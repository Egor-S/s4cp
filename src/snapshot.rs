//! Consistent page-level snapshots via `sqlite_dbpage` inside a read transaction, and the stat prefilter.

use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};

use crate::bail;
use crate::util::{Context, Result};

pub const BUSY_TIMEOUT: Duration = Duration::from_secs(30);

/// Cheap fingerprint of a database's on-disk state, used to skip unchanged databases without reading them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DbStat {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_ns: i128,
    pub wal: Option<WalStat>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalStat {
    pub size: u64,
    pub mtime_ns: i128,
    /// Checkpoint sequence + salts from the WAL header (bytes 12..24); change on every WAL reset.
    pub header: String,
}

fn mtime_ns(m: &fs::Metadata) -> i128 {
    m.mtime() as i128 * 1_000_000_000 + m.mtime_nsec() as i128
}

pub fn wal_path(db: &Path) -> PathBuf {
    let mut s = db.as_os_str().to_owned();
    s.push("-wal");
    PathBuf::from(s)
}

pub fn stat(db: &Path) -> Result<DbStat> {
    let m = fs::metadata(db).with_context(|| format!("stat {}", db.display()))?;
    if !m.is_file() {
        bail!("{} is not a regular file", db.display());
    }
    let wp = wal_path(db);
    let wal = match fs::metadata(&wp) {
        // An empty WAL holds no frames; readers create one, so treat it as absent.
        Ok(wm) if wm.len() == 0 => None,
        Ok(wm) => {
            let mut hdr = [0u8; 24];
            let header = match fs::File::open(&wp).and_then(|mut f| f.read_exact(&mut hdr)) {
                Ok(()) => hdr[12..24].iter().map(|b| format!("{b:02x}")).collect(),
                Err(_) => String::new(),
            };
            Some(WalStat {
                size: wm.len(),
                mtime_ns: mtime_ns(&wm),
                header,
            })
        }
        Err(_) => None,
    };
    Ok(DbStat {
        dev: m.dev(),
        ino: m.ino(),
        size: m.len(),
        mtime_ns: mtime_ns(&m),
        wal,
    })
}

/// Opens a source database for snapshot reads. Never creates the file and never changes journal_mode.
pub fn open(db: &Path) -> Result<Connection> {
    let base = OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI;
    let conn = match Connection::open_with_flags(db, base | OpenFlags::SQLITE_OPEN_READ_WRITE) {
        Ok(c) => c,
        // e.g. a read-only mount
        Err(_) => Connection::open_with_flags(db, base | OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("opening {}", db.display()))?,
    };
    conn.busy_timeout(BUSY_TIMEOUT)?;
    // Don't checkpoint/delete the WAL when our connection happens to be the last one to close.
    conn.set_db_config(
        rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
        true,
    )?;
    conn.pragma_update(None, "query_only", true)?;
    Ok(conn)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanInfo {
    pub page_size: u32,
    pub page_count: u32,
}

/// A read transaction pinned on a consistent snapshot of the database.
pub struct Snapshot<'c> {
    conn: &'c Connection,
    pub info: ScanInfo,
    open: bool,
}

impl<'c> Snapshot<'c> {
    pub fn begin(conn: &'c Connection) -> Result<Snapshot<'c>> {
        conn.execute_batch("BEGIN").context("BEGIN")?;
        let mut s = Snapshot {
            conn,
            info: ScanInfo {
                page_size: 0,
                page_count: 0,
            },
            open: true,
        };
        // The first read pins the snapshot (and takes the SHARED lock in rollback-journal mode).
        let page_size: i64 = conn
            .query_row("PRAGMA page_size", [], |r| r.get(0))
            .context("reading page_size")?;
        let page_count: i64 = conn
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .context("reading page_count")?;
        s.info = ScanInfo {
            page_size: page_size as u32,
            page_count: page_count as u32,
        };
        Ok(s)
    }

    /// Visits every page in order; returns the image xxh3-128.
    pub fn for_each_page(
        &mut self,
        mut visit: impl FnMut(u32, &[u8]) -> Result<()>,
    ) -> Result<u128> {
        let mut img = twox_hash::XxHash3_128::new();
        let mut st = self
            .conn
            .prepare("SELECT pgno, data FROM sqlite_dbpage('main')")
            .context("querying sqlite_dbpage (is SQLite built with SQLITE_ENABLE_DBPAGE_VTAB?)")?;
        let mut rows = st.query([])?;
        let mut expect = 1u32;
        while let Some(row) = rows.next()? {
            let pgno: i64 = row.get(0)?;
            let data = row.get_ref(1)?.as_blob()?;
            if pgno != expect as i64 || data.len() != self.info.page_size as usize {
                bail!(
                    "unexpected page {pgno} (len {}) from sqlite_dbpage, expected {expect}",
                    data.len()
                );
            }
            img.write(data);
            visit(expect, data)?;
            expect += 1;
        }
        if expect - 1 != self.info.page_count {
            bail!(
                "sqlite_dbpage returned {} pages, page_count is {}",
                expect - 1,
                self.info.page_count
            );
        }
        Ok(img.finish_128())
    }

    pub fn commit(mut self) -> Result<()> {
        self.open = false;
        self.conn.execute_batch("COMMIT").context("COMMIT")
    }
}

impl Drop for Snapshot<'_> {
    fn drop(&mut self) {
        if self.open {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
    }
}

/// Hashes a whole file as an image (xxh3-128), streaming.
pub fn hash_file(f: &mut fs::File) -> Result<u128> {
    use std::io::{Seek, SeekFrom};
    f.seek(SeekFrom::Start(0))?;
    let mut h = twox_hash::XxHash3_128::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.write(&buf[..n]);
    }
    Ok(h.finish_128())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_includes_wal_and_ignores_concurrent_writes() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.db");
        let w = Connection::open(&p).unwrap();
        w.pragma_update(None, "journal_mode", "wal").unwrap();
        w.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        w.execute_batch(
            "create table t(x); with recursive c(i) as (select 1 union all select i+1 from c where i<200) \
             insert into t select randomblob(500) from c;",
        )
        .unwrap();
        let st1 = stat(&p).unwrap();
        assert!(st1.wal.is_some());

        let c = open(&p).unwrap();
        let mut s = Snapshot::begin(&c).unwrap();
        let mut img = Vec::new();
        let mut wrote = false;
        let h = s
            .for_each_page(|_, d| {
                if !wrote {
                    w.execute_batch(
                        "insert into t values(randomblob(5000)); delete from t where rowid < 50;",
                    )
                    .unwrap();
                    wrote = true;
                }
                img.extend_from_slice(d);
                Ok(())
            })
            .unwrap();
        s.commit().unwrap();
        assert_eq!(h, twox_hash::XxHash3_128::oneshot(&img));
        assert_ne!(stat(&p).unwrap(), st1, "stat must change after a write");

        let q = d.path().join("img.db");
        fs::write(&q, &img).unwrap();
        let r = Connection::open(&q).unwrap();
        let n: i64 = r
            .query_row("select count(*) from t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 200);
        drop(c);
        assert!(
            wal_path(&p).exists(),
            "closing our connection must not remove the WAL"
        );
    }
}
