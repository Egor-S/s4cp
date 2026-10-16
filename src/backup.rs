//! The backup engine: one scan per database, a spool file per target, independent per-target uploads.

use std::fs;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::catalog::{self, Catalog, Entry, Meta};
use crate::format::{FLAG_FULL, Header, NO_PARENT, ObjectWriter, VERSION, page_hash};
use crate::schedule::Cron;
use crate::snapshot::{self, DbStat, Snapshot};
use crate::store::{Body, Cond, Get, Store, StoreError, join_key};
use crate::util::{Context, Error, Result, find_cause, human_bytes, msg};

#[derive(Debug, Clone)]
pub struct Policy {
    /// Max deltas after a full before a new full starts.
    pub max_chain: u32,
    /// New full when the chain's deltas add up to more than this fraction of its full.
    pub max_chain_ratio: f64,
    pub full_schedule: Option<Cron>,
    /// Keep the newest N chains (None = keep everything).
    pub keep_full: Option<u32>,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            max_chain: 24,
            max_chain_ratio: 0.5,
            full_schedule: None,
            keep_full: None,
        }
    }
}

#[derive(Clone)]
pub struct Target {
    pub name: String,
    pub store: Arc<dyn Store>,
}

#[derive(Debug, Clone)]
pub struct Dirs {
    /// Prefilter state + catalog cache; None disables both.
    pub state_dir: Option<PathBuf>,
    pub spool_dir: PathBuf,
}

pub struct Job {
    pub db: String,
    pub path: PathBuf,
    pub policy: Policy,
    pub targets: Vec<Target>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Stat prefilter matched: the database was not read.
    Skipped,
    /// Scanned, no page changed.
    Unchanged,
    Uploaded {
        id: String,
        full: bool,
        pages: u32,
        bytes: u64,
    },
}

pub fn catalog_key(db: &str) -> String {
    join_key(&[db, "catalog"])
}

pub fn object_key(db: &str, e: &Entry) -> String {
    join_key(&[db, &e.key()])
}

// ---------------------------------------------------------------------------------------------
// Local state: prefilter + catalog cache, under <state_dir>/<target>/<db>/

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prefilter {
    pub stat: DbStat,
    pub last_ok: DateTime<Utc>,
}

pub fn state_path(dirs: &Dirs, target: &str, db: &str) -> Option<PathBuf> {
    dirs.state_dir.as_ref().map(|d| d.join(target).join(db))
}

pub fn read_prefilter(dir: &Path) -> Option<Prefilter> {
    let b = fs::read(dir.join("prefilter.json")).ok()?;
    serde_json::from_slice(&b).ok()
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path.parent().unwrap();
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut t = NamedTempFile::new_in(dir)?;
    t.write_all(data)?;
    t.persist(path).map_err(|e| e.error)?;
    Ok(())
}

fn write_prefilter(dir: &Path, p: &Prefilter) -> Result<()> {
    write_atomic(&dir.join("prefilter.json"), &serde_json::to_vec(p)?)
}

/// A catalog as loaded from a target.
pub struct Loaded {
    pub catalog: Catalog,
    /// ETag of the remote catalog; None if it doesn't exist yet (or the provider sends no ETag).
    pub etag: Option<String>,
    pub exists: bool,
}

/// Loads the catalog, using the local cache (conditional GET → 304) when available.
pub fn load_catalog(store: &dyn Store, db: &str, cache: Option<&Path>) -> Result<Loaded> {
    let key = catalog_key(db);
    let cached_etag = cache
        .and_then(|d| fs::read_to_string(d.join("catalog.etag")).ok())
        .filter(|_| cache.is_some_and(|d| d.join("catalog").exists()));
    let res = store
        .get(&key, cached_etag.as_deref())
        .with_context(|| format!("fetching {key}"))?;
    let loaded = match res {
        Get::NotFound => {
            if let Some(d) = cache {
                let _ = fs::remove_file(d.join("catalog"));
                let _ = fs::remove_file(d.join("catalog.etag"));
            }
            Loaded {
                catalog: Catalog::new(db),
                etag: None,
                exists: false,
            }
        }
        Get::NotModified => {
            let d = cache.unwrap();
            debug!("{key}: not modified, using cached copy");
            let f = fs::File::open(d.join("catalog"))?;
            let catalog =
                Catalog::read_from(BufReader::new(f)).context("reading cached catalog")?;
            Loaded {
                catalog,
                etag: cached_etag,
                exists: true,
            }
        }
        Get::Found { mut body, etag, .. } => {
            let catalog = match (cache, &etag) {
                (Some(d), Some(e)) => {
                    fs::create_dir_all(d)?;
                    let mut t = NamedTempFile::new_in(d)?;
                    std::io::copy(&mut body, &mut t)
                        .with_context(|| format!("downloading {key}"))?;
                    t.flush()?;
                    let mut f = t.reopen()?;
                    let c = Catalog::read_from(BufReader::new(&mut f))
                        .with_context(|| format!("parsing {key}"))?;
                    t.persist(d.join("catalog")).map_err(|e| e.error)?;
                    write_atomic(&d.join("catalog.etag"), e.as_bytes())?;
                    c
                }
                _ => Catalog::read_from(BufReader::new(&mut body))
                    .with_context(|| format!("parsing {key}"))?,
            };
            Loaded {
                catalog,
                etag,
                exists: true,
            }
        }
    };
    if loaded.exists && loaded.catalog.meta.db != db {
        crate::bail!(
            "{key} belongs to database {:?}, not {db:?}",
            loaded.catalog.meta.db
        );
    }
    Ok(loaded)
}

/// Writes a catalog (meta + hashes) with the guard implied by `prev_etag`/`exists`; updates the cache.
pub fn write_catalog(
    store: &dyn Store,
    db: &str,
    meta: &Meta,
    hashes: &[u64],
    exists: bool,
    prev_etag: Option<&str>,
    spool_dir: &Path,
    cache: Option<&Path>,
) -> Result<Option<String>> {
    let key = catalog_key(db);
    let dir = match cache {
        Some(d) => {
            fs::create_dir_all(d)?;
            d.to_path_buf()
        }
        None => spool_dir.to_path_buf(),
    };
    let mut t = tempfile::Builder::new()
        .prefix(&spool_prefix())
        .tempfile_in(&dir)?;
    catalog::write_parts(meta, hashes, BufWriter::new(t.as_file_mut()))?;
    let len = t.as_file().metadata()?.len();
    let cond = match (exists, prev_etag) {
        (false, _) => Cond::Absent,
        (true, Some(e)) => Cond::Match(e.to_string()),
        (true, None) => {
            warn!("{key}: provider returned no ETag; writing catalog without an If-Match guard");
            Cond::None
        }
    };
    let etag = store
        .put(&key, Body::File(t.as_file(), len), &cond)
        .with_context(|| format!("writing {key}"))?;
    if let Some(d) = cache {
        match &etag {
            Some(e) => {
                t.persist(d.join("catalog")).map_err(|e| e.error)?;
                write_atomic(&d.join("catalog.etag"), e.as_bytes())?;
            }
            None => {
                let _ = fs::remove_file(d.join("catalog.etag"));
            }
        }
    }
    Ok(etag)
}

// ---------------------------------------------------------------------------------------------
// Spool files

pub fn spool_prefix() -> String {
    format!("s4cp-{}-", std::process::id())
}

/// Deletes spool files left behind by dead s4cp processes (or by an earlier life of this PID).
pub fn sweep_spools(spool_dir: &Path) {
    let me = std::process::id();
    let Ok(rd) = fs::read_dir(spool_dir) else {
        return;
    };
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix("s4cp-") else {
            continue;
        };
        let pid: Option<u32> = rest.split('-').next().and_then(|p| p.parse().ok());
        let stale = match pid {
            Some(p) => p == me || !Path::new(&format!("/proc/{p}")).exists(),
            None => false,
        };
        if stale {
            match fs::remove_file(ent.path()) {
                Ok(()) => info!("removed stale spool file {}", ent.path().display()),
                Err(e) => warn!(
                    "cannot remove stale spool file {}: {e}",
                    ent.path().display()
                ),
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Backup

struct TState {
    target: Target,
    cache: Option<PathBuf>,
    loaded: Loaded,
    group: usize,
    full_reason: Option<String>,
    writer: Option<ObjectWriter<NamedTempFile>>,
    header: Option<Header>,
}

fn full_reason(
    policy: &Policy,
    c: &Catalog,
    page_size: u32,
    force: bool,
    now: DateTime<Utc>,
) -> Option<String> {
    if force {
        return Some("forced".into());
    }
    let Some(last) = c.latest() else {
        return Some("no previous backup".into());
    };
    if last.page_size != page_size {
        return Some(format!(
            "page_size changed {} → {page_size}",
            last.page_size
        ));
    }
    let chain = c.current_chain();
    let deltas = chain.len() as u32 - 1;
    if deltas >= policy.max_chain {
        return Some(format!(
            "chain has {deltas} deltas (max_chain {})",
            policy.max_chain
        ));
    }
    let full_size = chain[0].size as f64;
    let delta_size: u64 = chain[1..].iter().map(|e| e.size).sum();
    if delta_size as f64 > policy.max_chain_ratio * full_size {
        return Some(format!(
            "chain deltas total {} > {} × full {}",
            human_bytes(delta_size),
            policy.max_chain_ratio,
            human_bytes(chain[0].size)
        ));
    }
    if let Some(fs) = &policy.full_schedule
        && fs.fired_between(chain[0].time, now)
    {
        return Some(format!("full_schedule {:?} fired", fs.expr()));
    }
    None
}

fn new_chain_id(c: &Catalog, now: DateTime<Utc>) -> String {
    let mut t = now;
    let last = c.chains().pop();
    loop {
        let id = catalog::chain_id(t);
        if last.as_ref().is_none_or(|l| id > *l) {
            return id;
        }
        t += chrono::Duration::seconds(1);
    }
}

/// Backs up one database to all its targets. Returns one result per target (in target order).
pub fn backup(job: &Job, dirs: &Dirs, force_full: bool) -> Result<Vec<(String, Result<Outcome>)>> {
    let db = &job.db;
    let st = snapshot::stat(&job.path)?;
    let now = Utc::now();
    let mut results: Vec<(String, Option<Result<Outcome>>)> =
        job.targets.iter().map(|t| (t.name.clone(), None)).collect();

    // 1. Prefilter: targets whose last successful run saw the same stat are skipped.
    let mut todo = Vec::new();
    for (i, t) in job.targets.iter().enumerate() {
        let sdir = state_path(dirs, &t.name, db);
        let unchanged = !force_full
            && sdir
                .as_deref()
                .and_then(read_prefilter)
                .is_some_and(|p| p.stat == st);
        if unchanged {
            debug!(
                "{db} → {}: unchanged since last backup (stat), skipping",
                t.name
            );
            results[i].1 = Some(Ok(Outcome::Skipped));
        } else {
            todo.push(i);
        }
    }
    if todo.is_empty() {
        info!("{db}: unchanged, nothing to do");
        return Ok(results.into_iter().map(|(n, r)| (n, r.unwrap())).collect());
    }

    // 2. Load catalogs; group targets whose page-hash tables are identical (one table in memory per group).
    let mut groups: Vec<Vec<u64>> = Vec::new();
    let mut ts: Vec<(usize, TState)> = Vec::new();
    for i in todo {
        let t = &job.targets[i];
        let cache = state_path(dirs, &t.name, db);
        match load_catalog(&*t.store, db, cache.as_deref()) {
            Ok(mut loaded) => {
                let hashes = std::mem::take(&mut loaded.catalog.hashes);
                let ps = loaded.catalog.latest().map(|e| e.page_size);
                let g = ts
                    .iter()
                    .find(|(_, o)| {
                        o.loaded.catalog.latest().map(|e| e.page_size) == ps
                            && groups[o.group] == hashes
                    })
                    .map(|(_, o)| o.group);
                let group = match g {
                    Some(g) => g,
                    None => {
                        groups.push(hashes);
                        groups.len() - 1
                    }
                };
                ts.push((
                    i,
                    TState {
                        target: t.clone(),
                        cache,
                        loaded,
                        group,
                        full_reason: None,
                        writer: None,
                        header: None,
                    },
                ));
            }
            Err(e) => results[i].1 = Some(Err(e)),
        }
    }

    // 3. Scan once, feeding every target's spool.
    if !ts.is_empty() {
        let conn = snapshot::open(&job.path)?;
        let mut snap = Snapshot::begin(&conn)?;
        let info = snap.info;
        for (_, s) in ts.iter_mut() {
            let c = &s.loaded.catalog;
            s.full_reason = full_reason(&job.policy, c, info.page_size, force_full, now);
            let (chain, seq, parent) = match (&s.full_reason, c.latest()) {
                (None, Some(l)) => (l.chain.clone(), l.seq + 1, l.seq),
                _ => (new_chain_id(c, now), 0, NO_PARENT),
            };
            let header = Header {
                version: VERSION,
                flags: if s.full_reason.is_some() {
                    FLAG_FULL
                } else {
                    0
                },
                page_size: info.page_size,
                page_count: info.page_count,
                record_count: 0,
                seq,
                parent_seq: parent,
                chain,
                created_ms: now.timestamp_millis(),
                body_len: 0,
            };
            let spool = tempfile::Builder::new()
                .prefix(&spool_prefix())
                .tempfile_in(&dirs.spool_dir)
                .with_context(|| format!("creating spool file in {}", dirs.spool_dir.display()))?;
            s.writer = Some(ObjectWriter::new(spool, header.clone())?);
            s.header = Some(header);
        }
        let mut changed = vec![false; groups.len()];
        let image_hash = snap.for_each_page(|pgno, data| {
            let h = page_hash(data);
            let idx = (pgno - 1) as usize;
            for (g, table) in groups.iter_mut().enumerate() {
                changed[g] = match table.get_mut(idx) {
                    Some(old) if *old == h => false,
                    Some(old) => {
                        *old = h;
                        true
                    }
                    None => {
                        table.push(h);
                        true
                    }
                };
            }
            for (_, s) in ts.iter_mut() {
                if s.full_reason.is_some() || changed[s.group] {
                    s.writer.as_mut().unwrap().add_page(pgno, data)?;
                }
            }
            Ok(())
        })?;
        snap.commit()?;
        drop(conn);
        for table in groups.iter_mut() {
            table.truncate(info.page_count as usize);
        }

        // 4. Upload per target, independently.
        for (i, mut s) in ts {
            let g = s.group;
            let r = finish_target(
                job,
                dirs,
                db,
                &mut s,
                &groups[g],
                info.page_count,
                image_hash,
                now,
            );
            if r.is_ok()
                && let Some(d) = &s.cache
                && let Err(e) = write_prefilter(
                    d,
                    &Prefilter {
                        stat: st.clone(),
                        last_ok: now,
                    },
                )
            {
                warn!("{db} → {}: cannot save prefilter state: {e}", s.target.name);
            }
            results[i].1 = Some(r);
        }
    }
    Ok(results.into_iter().map(|(n, r)| (n, r.unwrap())).collect())
}

#[allow(clippy::too_many_arguments)]
fn finish_target(
    job: &Job,
    dirs: &Dirs,
    db: &str,
    s: &mut TState,
    hashes: &[u64],
    page_count: u32,
    image_hash: u128,
    now: DateTime<Utc>,
) -> Result<Outcome> {
    let tname = s.target.name.clone();
    let store = s.target.store.clone();
    let writer = s.writer.take().unwrap();
    let full = s.full_reason.is_some();
    let prev = s.loaded.catalog.latest().cloned();
    if !full
        && writer.record_count() == 0
        && prev.as_ref().is_some_and(|p| p.page_count == page_count)
    {
        info!("{db} → {tname}: no pages changed");
        return Ok(Outcome::Unchanged);
    }
    let (spool, header, size) = writer.finish(page_count, image_hash)?;
    let entry = Entry {
        id: catalog::backup_id(&header.chain, header.seq),
        chain: header.chain.clone(),
        seq: header.seq,
        full,
        time: now,
        page_size: header.page_size,
        page_count,
        pages: header.record_count,
        size,
        image_hash: crate::util::hex128(image_hash),
    };
    let okey = object_key(db, &entry);
    put_object(&*store, &okey, spool.as_file(), size)?;
    drop(spool);

    let mut meta = s.loaded.catalog.meta.clone();
    meta.backups.push(entry.clone());
    let etag = match write_catalog(
        &*store,
        db,
        &meta,
        hashes,
        s.loaded.exists,
        s.loaded.etag.as_deref(),
        &dirs.spool_dir,
        s.cache.as_deref(),
    ) {
        Ok(e) => e,
        Err(e) => {
            // The object is unreferenced now; remove it so the next run can reuse its key.
            if let Err(de) = store.delete(&okey) {
                warn!("{db} → {tname}: cannot remove unreferenced {okey}: {de}");
            }
            return Err(e);
        }
    };
    let kind = match &s.full_reason {
        Some(r) => format!("full ({r})"),
        None => "delta".to_string(),
    };
    info!(
        "{db} → {tname}: {kind} {}: {} of {page_count} pages, {}",
        entry.id,
        entry.pages,
        human_bytes(size)
    );

    if let Some(keep) = job.policy.keep_full {
        let mut cat = Catalog {
            meta,
            hashes: Vec::new(),
        };
        if let Err(e) = retain(
            &*store,
            db,
            &mut cat,
            hashes,
            etag,
            keep,
            &dirs.spool_dir,
            s.cache.as_deref(),
            false,
        ) {
            warn!("{db} → {tname}: retention failed (will retry after the next backup): {e}");
        }
    }
    Ok(Outcome::Uploaded {
        id: entry.id,
        full,
        pages: entry.pages,
        bytes: size,
    })
}

/// Objects older than this that exist but aren't in the catalog are leftovers of a failed run.
const ORPHAN_AGE: chrono::Duration = chrono::Duration::hours(1);

/// PUTs a data object with `If-None-Match: *`. A key that already exists is not in our catalog (we
/// never reuse ids), so it is either a leftover of a run whose catalog write failed (replace it when
/// it is old enough) or the work of a concurrent writer (abort).
fn put_object(store: &dyn Store, key: &str, f: &fs::File, size: u64) -> Result<()> {
    let e = match store.put(key, Body::File(f, size), &Cond::Absent) {
        Ok(_) => return Ok(()),
        Err(e) if is_conflict(&e) => e,
        Err(e) => return Err(e).with_context(|| format!("uploading {key}")),
    };
    let created = store
        .get_range(key, 0, crate::format::HEADER_LEN as u64)
        .ok()
        .and_then(|b| Header::decode(&b).ok())
        .and_then(|h| DateTime::from_timestamp_millis(h.created_ms));
    match created {
        Some(t) if Utc::now() - t > ORPHAN_AGE => {
            warn!(
                "{key} exists but is not in the catalog (left by a failed run at {t}); replacing it"
            );
            store.delete(key)?;
            store
                .put(key, Body::File(f, size), &Cond::Absent)
                .with_context(|| format!("uploading {key}"))?;
            Ok(())
        }
        _ => Err(e).with_context(|| {
            format!("uploading {key}: object exists and is recent (concurrent writer?)")
        }),
    }
}

/// Drops all but the newest `keep` chains: rewrites the catalog first, then deletes the objects.
/// Returns the deleted keys.
#[allow(clippy::too_many_arguments)]
pub fn retain(
    store: &dyn Store,
    db: &str,
    cat: &mut Catalog,
    hashes: &[u64],
    etag: Option<String>,
    keep: u32,
    spool_dir: &Path,
    cache: Option<&Path>,
    dry_run: bool,
) -> Result<Vec<String>> {
    let chains = cat.chains();
    let keep = keep.max(1) as usize;
    if chains.len() <= keep {
        return Ok(vec![]);
    }
    let drop_chains = &chains[..chains.len() - keep];
    let (gone, kept): (Vec<Entry>, Vec<Entry>) = cat
        .meta
        .backups
        .drain(..)
        .partition(|e| drop_chains.contains(&e.chain));
    cat.meta.backups = kept;
    let keys: Vec<String> = gone.iter().map(|e| object_key(db, e)).collect();
    if dry_run {
        return Ok(keys);
    }
    write_catalog(
        store,
        db,
        &cat.meta,
        hashes,
        true,
        etag.as_deref(),
        spool_dir,
        cache,
    )?;
    for k in &keys {
        store.delete(k).with_context(|| format!("deleting {k}"))?;
    }
    info!(
        "{db}: retention removed {} chain(s) ({} objects)",
        drop_chains.len(),
        keys.len()
    );
    Ok(keys)
}

/// Whether an error is a lost conditional write.
pub fn is_conflict(e: &Error) -> bool {
    matches!(
        find_cause::<StoreError>(&**e),
        Some(StoreError::PreconditionFailed(_))
    )
}

/// Reads a whole object into memory-free streaming form, for callers that need a body reader.
pub fn open_object(store: &dyn Store, key: &str) -> Result<Box<dyn Read + Send>> {
    match store.get(key, None)? {
        Get::Found { body, .. } => Ok(body),
        _ => Err(msg(format!("{key}: object missing"))),
    }
}
