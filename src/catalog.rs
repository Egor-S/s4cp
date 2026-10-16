//! Catalog format (v1, frozen at 3.0.0): the source of truth and index of backups per (target, db).
//!
//! ```text
//! header (24 B)  magic "S4CPCATL", u16 version, u16 flags, u32 meta_len, u64 hash_count
//! meta           zstd(JSON `Meta`), meta_len bytes
//! hashes         hash_count × u64 LE: xxh3-64 of every page of the latest backup's image
//! ```

use std::io::{self, Read, Write};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::bail;
use crate::format::{CATALOG_MAGIC, VERSION};
use crate::util::{Context, Result};

pub const CATALOG_HEADER_LEN: usize = 24;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    /// `<chain>/<seq>`, e.g. `20261016T120000Z/000003`.
    pub id: String,
    pub chain: String,
    pub seq: u32,
    pub full: bool,
    pub time: DateTime<Utc>,
    pub page_size: u32,
    pub page_count: u32,
    /// Number of page records in the object.
    pub pages: u32,
    /// Object size in bytes.
    pub size: u64,
    /// xxh3-128 of the whole image after applying this backup, hex.
    pub image_hash: String,
}

impl Entry {
    /// Object key relative to the db prefix.
    pub fn key(&self) -> String {
        object_key(&self.chain, self.seq)
    }
}

pub fn object_key(chain: &str, seq: u32) -> String {
    format!("{chain}/{seq:06}.s4")
}

pub fn backup_id(chain: &str, seq: u32) -> String {
    format!("{chain}/{seq:06}")
}

pub fn chain_id(t: DateTime<Utc>) -> String {
    t.format("%Y%m%dT%H%M%SZ").to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Meta {
    pub db: String,
    /// Oldest first.
    pub backups: Vec<Entry>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Catalog {
    pub meta: Meta,
    /// Page hashes of the latest backup's image (index = pgno - 1).
    pub hashes: Vec<u64>,
}

impl Catalog {
    pub fn new(db: &str) -> Catalog {
        Catalog {
            meta: Meta {
                db: db.to_string(),
                backups: vec![],
            },
            hashes: vec![],
        }
    }

    pub fn latest(&self) -> Option<&Entry> {
        self.meta.backups.last()
    }

    /// Chain ids, oldest first.
    pub fn chains(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for e in &self.meta.backups {
            if out.last() != Some(&e.chain) {
                out.push(e.chain.clone());
            }
        }
        out
    }

    /// Entries of the chain of the latest backup.
    pub fn current_chain(&self) -> &[Entry] {
        let Some(last) = self.latest() else {
            return &[];
        };
        let start = self
            .meta
            .backups
            .iter()
            .rposition(|e| e.chain != last.chain)
            .map_or(0, |i| i + 1);
        &self.meta.backups[start..]
    }

    /// Entries needed to restore `id`: its chain from the full up to and including it.
    pub fn restore_path(&self, id: &str) -> Result<Vec<Entry>> {
        let Some(idx) = self.meta.backups.iter().position(|e| e.id == id) else {
            bail!("backup {id} not found in catalog");
        };
        let target = &self.meta.backups[idx];
        let start = self.meta.backups[..=idx]
            .iter()
            .rposition(|e| e.chain != target.chain)
            .map_or(0, |i| i + 1);
        let path = self.meta.backups[start..=idx].to_vec();
        if !path[0].full {
            bail!(
                "chain {} has no full backup (catalog damaged?)",
                target.chain
            );
        }
        for (i, e) in path.iter().enumerate() {
            if e.seq != i as u32 {
                bail!(
                    "chain {} is not contiguous at {} (catalog damaged?)",
                    target.chain,
                    e.id
                );
            }
        }
        Ok(path)
    }

    /// Resolves `--at`: a backup id, or a timestamp (the latest backup at or before it).
    pub fn resolve(&self, at: Option<&str>) -> Result<&Entry> {
        let Some(at) = at else {
            return self
                .latest()
                .ok_or_else(|| crate::util::msg("no backups in catalog"));
        };
        if let Some(e) = self.meta.backups.iter().find(|e| e.id == at) {
            return Ok(e);
        }
        let ts = parse_time(at)
            .with_context(|| format!("--at {at:?} is neither a backup id nor a timestamp"))?;
        self.meta
            .backups
            .iter()
            .rev()
            .find(|e| e.time <= ts)
            .ok_or_else(|| crate::util::msg(format!("no backup at or before {ts}")))
    }

    #[cfg(test)]
    pub fn write_to<W: Write>(&self, w: W) -> Result<()> {
        write_parts(&self.meta, &self.hashes, w)
    }

    #[cfg(test)]
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut v = Vec::with_capacity(CATALOG_HEADER_LEN + 1024 + self.hashes.len() * 8);
        self.write_to(&mut v)?;
        Ok(v)
    }

    /// Reads the header and metadata only; returns the number of hashes that follow.
    pub fn read_meta<R: Read>(r: &mut R) -> Result<(Meta, u64)> {
        let mut h = [0u8; CATALOG_HEADER_LEN];
        r.read_exact(&mut h).context("reading catalog header")?;
        if &h[0..8] != CATALOG_MAGIC {
            bail!("not an s4cp catalog (bad magic)");
        }
        let version = u16::from_le_bytes([h[8], h[9]]);
        if version != VERSION {
            bail!("unsupported catalog version {version}");
        }
        let meta_len = u32::from_le_bytes(h[12..16].try_into().unwrap()) as usize;
        let count = u64::from_le_bytes(h[16..24].try_into().unwrap());
        let mut meta = vec![0u8; meta_len];
        r.read_exact(&mut meta)
            .context("reading catalog metadata")?;
        let json = zstd::stream::decode_all(&meta[..]).context("decompressing catalog metadata")?;
        let meta: Meta = serde_json::from_slice(&json).context("parsing catalog metadata")?;
        Ok((meta, count))
    }

    pub fn read_from<R: Read>(mut r: R) -> Result<Catalog> {
        let (meta, count) = Self::read_meta(&mut r)?;
        let mut hashes = Vec::with_capacity(count as usize);
        let mut buf = vec![0u8; 64 * 1024];
        let mut left = count as usize * 8;
        while left > 0 {
            let n = left.min(buf.len());
            r.read_exact(&mut buf[..n])
                .context("reading catalog hash table")?;
            hashes.extend(
                buf[..n]
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|c| u64::from_le_bytes(*c)),
            );
            left -= n;
        }
        let mut extra = [0u8; 1];
        if r.read(&mut extra)? != 0 {
            bail!("trailing bytes after catalog hash table");
        }
        if let Some(l) = meta.backups.last()
            && l.page_count as usize != hashes.len()
        {
            bail!(
                "catalog hash table has {} entries, latest backup has {} pages",
                hashes.len(),
                l.page_count
            );
        }
        Ok(Catalog { meta, hashes })
    }
}

/// Serializes catalog parts (lets callers write without owning the hash table).
pub fn write_parts<W: Write>(meta: &Meta, hashes: &[u64], mut w: W) -> Result<()> {
    let json = serde_json::to_vec(meta)?;

    let meta = zstd::bulk::compress(&json, 3)?;
    let mut h = [0u8; CATALOG_HEADER_LEN];
    h[0..8].copy_from_slice(CATALOG_MAGIC);
    h[8..10].copy_from_slice(&VERSION.to_le_bytes());
    h[12..16].copy_from_slice(&(meta.len() as u32).to_le_bytes());
    h[16..24].copy_from_slice(&(hashes.len() as u64).to_le_bytes());
    w.write_all(&h)?;
    w.write_all(&meta)?;
    let mut buf = Vec::with_capacity(64 * 1024);
    for chunk in hashes.chunks(8192) {
        buf.clear();
        for v in chunk {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        w.write_all(&buf)?;
    }
    w.flush()?;
    Ok(())
}

/// Accepts RFC 3339, `YYYY-MM-DD HH:MM[:SS]` (UTC), `YYYY-MM-DD` (UTC midnight) and the chain id form.
pub fn parse_time(s: &str) -> Result<DateTime<Utc>> {
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(t.with_timezone(&Utc));
    }
    for f in [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M",
        "%Y%m%dT%H%M%SZ",
    ] {
        if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, f) {
            return Ok(t.and_utc());
        }
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(d.and_hms_opt(0, 0, 0).unwrap().and_utc());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("cannot parse time {s:?}"),
    )
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(chain: &str, seq: u32, mins: i64) -> Entry {
        Entry {
            id: backup_id(chain, seq),
            chain: chain.into(),
            seq,
            full: seq == 0,
            time: DateTime::from_timestamp(1_760_000_000 + mins * 60, 0).unwrap(),
            page_size: 4096,
            page_count: 3,
            pages: 1,
            size: 100,
            image_hash: "00".into(),
        }
    }

    fn sample() -> Catalog {
        Catalog {
            meta: Meta {
                db: "app".into(),
                backups: vec![
                    entry("A", 0, 0),
                    entry("A", 1, 1),
                    entry("B", 0, 2),
                    entry("B", 1, 3),
                    entry("B", 2, 4),
                ],
            },
            hashes: vec![1, 2, 3],
        }
    }

    #[test]
    fn roundtrip() {
        let c = sample();
        let b = c.to_bytes().unwrap();
        assert_eq!(Catalog::read_from(&b[..]).unwrap(), c);
        let mut bad = b.clone();
        bad.push(0);
        assert!(Catalog::read_from(&bad[..]).is_err());
        assert!(Catalog::read_from(&b[..b.len() - 1]).is_err());
    }

    #[test]
    fn chains_and_paths() {
        let c = sample();
        assert_eq!(c.chains(), vec!["A", "B"]);
        assert_eq!(c.current_chain().len(), 3);
        let p = c.restore_path("B/000001").unwrap();
        assert_eq!(
            p.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            vec!["B/000000", "B/000001"]
        );
        assert_eq!(c.restore_path("A/000001").unwrap().len(), 2);
        assert!(c.restore_path("C/000000").is_err());
    }

    #[test]
    fn resolve_at() {
        let c = sample();
        assert_eq!(c.resolve(None).unwrap().id, "B/000002");
        assert_eq!(c.resolve(Some("A/000001")).unwrap().id, "A/000001");
        let t = DateTime::from_timestamp(1_760_000_000 + 150, 0).unwrap();
        assert_eq!(c.resolve(Some(&t.to_rfc3339())).unwrap().id, "B/000000");
        assert!(c.resolve(Some("2000-01-01")).is_err());
        assert!(c.resolve(Some("garbage")).is_err());
    }
}
