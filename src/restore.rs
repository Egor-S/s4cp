//! Restore: apply a chain (full + deltas) into `<dest>.s4cp-tmp`, verify, then rename atomically.

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use log::{debug, info};
use rusqlite::{Connection, OpenFlags};

use crate::backup::object_key;
use crate::bail;
use crate::catalog::{Catalog, Entry};
use crate::format::ObjectReader;
use crate::snapshot::hash_file;
use crate::store::{Get, Store};
use crate::util::{Context, Result, hex128, human_bytes, parse_hex128};

pub struct Options {
    pub force: bool,
    pub integrity_check: bool,
}

fn sibling(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Rebuilds the image of `entry` into `file` (which must be empty); returns bytes downloaded.
pub fn rebuild(
    store: &dyn Store,
    db: &str,
    cat: &Catalog,
    entry: &Entry,
    file: &File,
) -> Result<u64> {
    let path = cat.restore_path(&entry.id)?;
    let mut downloaded = 0;
    let mut buf = Vec::new();
    for (i, e) in path.iter().enumerate() {
        let key = object_key(db, e);
        debug!("applying {key}");
        let body = match store
            .get(&key, None)
            .with_context(|| format!("fetching {key}"))?
        {
            Get::Found { body, .. } => body,
            _ => bail!("{key} is listed in the catalog but missing from storage"),
        };
        let mut r = ObjectReader::new(body).with_context(|| format!("reading {key}"))?;
        let h = r.header.clone();
        let expected_parent = if i == 0 {
            crate::format::NO_PARENT
        } else {
            path[i - 1].seq
        };
        if h.chain != e.chain
            || h.seq != e.seq
            || h.parent_seq != expected_parent
            || h.is_full() != (i == 0)
        {
            bail!(
                "{key}: header does not match the catalog (chain {} seq {} parent {})",
                h.chain,
                h.seq,
                h.parent_seq
            );
        }
        if h.page_size != e.page_size || h.page_count != e.page_count {
            bail!("{key}: page geometry does not match the catalog");
        }
        let ps = h.page_size as u64;
        while let Some(pgno) = r.next_page(&mut buf)? {
            if pgno == 0 || pgno > h.page_count {
                bail!("{key}: page {pgno} out of range 1..={}", h.page_count);
            }
            file.write_all_at(&buf, (pgno as u64 - 1) * ps)?;
        }
        let t = r.finish().with_context(|| format!("reading {key}"))?;
        if hex128(t.image_hash) != e.image_hash {
            bail!("{key}: trailer image hash does not match the catalog");
        }
        downloaded += e.size;
    }
    file.set_len(entry.page_count as u64 * entry.page_size as u64)?;
    let mut f = file.try_clone()?;
    let got = hash_file(&mut f)?;
    if got != parse_hex128(&entry.image_hash)? {
        bail!(
            "restored image hash {} does not match backup {} ({})",
            hex128(got),
            entry.id,
            entry.image_hash
        );
    }
    Ok(downloaded)
}

/// Runs `PRAGMA integrity_check` on a file without modifying it.
pub fn integrity_check(path: &Path) -> Result<()> {
    let enc: String = path
        .to_string_lossy()
        .chars()
        .map(|c| match c {
            '%' | '?' | '#' => format!("%{:02X}", c as u32),
            c => c.to_string(),
        })
        .collect();
    let uri = format!("file:{enc}?mode=ro&immutable=1");
    let conn = Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    let mut st = conn.prepare("PRAGMA integrity_check")?;
    let msgs: Vec<String> = st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
    if msgs != ["ok"] {
        bail!("integrity_check failed: {}", msgs.join("; "));
    }
    Ok(())
}

fn fsync_dir(p: &Path) -> Result<()> {
    let dir = p
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    File::open(dir)?.sync_all()?;
    Ok(())
}

pub fn restore(
    store: &dyn Store,
    db: &str,
    cat: &Catalog,
    entry: &Entry,
    dest: &Path,
    opts: &Options,
) -> Result<()> {
    if !opts.force {
        if dest.exists() {
            bail!(
                "{} already exists; use --force to replace it",
                dest.display()
            );
        }
        // SQLite would replay a leftover WAL/journal over the restored image.
        for suffix in ["-wal", "-journal"] {
            let p = sibling(dest, suffix);
            if p.exists() {
                bail!(
                    "{} exists (left over from a deleted database); use --force to remove it",
                    p.display()
                );
            }
        }
    }
    let tmp = sibling(dest, ".s4cp-tmp");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    let res = (|| -> Result<u64> {
        let n = rebuild(store, db, cat, entry, &file)?;
        if opts.integrity_check {
            integrity_check(&tmp)?;
        }
        file.sync_all()?;
        Ok(n)
    })();
    let downloaded = match res {
        Ok(n) => n,
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
    };
    drop(file);
    if opts.force {
        // A leftover WAL or journal from the old database would be applied on top of the restored image.
        for suffix in ["-wal", "-shm", "-journal"] {
            let p = sibling(dest, suffix);
            match fs::remove_file(&p) {
                Ok(()) => info!("removed stale {}", p.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("removing {}", p.display())),
            }
        }
    }
    fs::rename(&tmp, dest)
        .with_context(|| format!("renaming {} to {}", tmp.display(), dest.display()))?;
    fsync_dir(dest)?;
    info!(
        "restored {db} backup {} ({} pages, {} downloaded) to {}",
        entry.id,
        entry.page_count,
        human_bytes(downloaded),
        dest.display()
    );
    Ok(())
}

/// Rebuilds a backup into a scratch file and checks hashes (+ integrity_check).
pub fn verify(
    store: &dyn Store,
    db: &str,
    cat: &Catalog,
    entry: &Entry,
    scratch_dir: &Path,
    integrity: bool,
) -> Result<()> {
    let tmp = tempfile::Builder::new()
        .prefix(&crate::backup::spool_prefix())
        .tempfile_in(scratch_dir)?;
    rebuild(store, db, cat, entry, tmp.as_file())?;
    if integrity {
        integrity_check(tmp.path())?;
    }
    Ok(())
}
