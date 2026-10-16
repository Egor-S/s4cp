//! Object storage abstraction: S3 (see `s3`) and a local directory store (`file://`, used for tests and local copies).

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use crate::util::{Context, Result};

/// Conditional-write guard for PUT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cond {
    None,
    /// `If-None-Match: *` — fail if the key exists.
    Absent,
    /// `If-Match: <etag>` — fail unless the key still has this ETag.
    Match(String),
}

pub enum Body<'a> {
    Bytes(&'a [u8]),
    /// A whole file of the given length, read with positional reads (so retries can re-read it).
    File(&'a File, u64),
}

impl Body<'_> {
    pub fn len(&self) -> u64 {
        match self {
            Body::Bytes(b) => b.len() as u64,
            Body::File(_, n) => *n,
        }
    }
}

pub enum Get {
    NotFound,
    NotModified,
    Found {
        body: Box<dyn Read + Send>,
        etag: Option<String>,
        len: Option<u64>,
    },
}

#[derive(Debug, Clone)]
pub struct ObjInfo {
    /// Relative to the store root.
    pub key: String,
    pub size: u64,
}

/// Errors callers need to tell apart.
#[derive(Debug)]
pub enum StoreError {
    /// 412 / 409: a conditional write lost (another writer, or our view is stale).
    PreconditionFailed(String),
    NotFound(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::PreconditionFailed(k) => write!(
                f,
                "precondition failed writing {k} (another writer, or a stale catalog); aborting this target"
            ),
            StoreError::NotFound(k) => write!(f, "{k}: not found"),
        }
    }
}

impl std::error::Error for StoreError {}

pub trait Store: Send + Sync {
    /// Human-readable location of the root, for logs.
    fn describe(&self) -> String;
    /// Returns the new ETag (quoted, as the server sent it), if any.
    fn put(&self, key: &str, body: Body<'_>, cond: &Cond) -> Result<Option<String>>;
    fn get(&self, key: &str, if_none_match: Option<&str>) -> Result<Get>;
    /// Reads a byte range; `start < 0` means a suffix range of `-start` bytes (`len` ignored).
    fn get_range(&self, key: &str, start: i64, len: u64) -> Result<Vec<u8>>;
    fn list(&self, prefix: &str) -> Result<Vec<ObjInfo>>;
    fn delete(&self, key: &str) -> Result<()>;
}

/// Reads a section of a file with positional reads, leaving the file offset alone.
pub struct FileSection<'a> {
    pub file: &'a File,
    pub pos: u64,
    pub end: u64,
}

impl Read for FileSection<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let want = buf.len().min((self.end - self.pos) as usize);
        if want == 0 {
            return Ok(0);
        }
        let n = self.file.read_at(&mut buf[..want], self.pos)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "spool file shorter than expected",
            ));
        }
        self.pos += n as u64;
        Ok(n)
    }
}

/// A directory used as an object store. Not meant for concurrent writers.
pub struct FsStore {
    root: PathBuf,
}

impl FsStore {
    pub fn new(root: &Path) -> FsStore {
        FsStore {
            root: root.to_path_buf(),
        }
    }

    fn path(&self, key: &str) -> Result<PathBuf> {
        if key.split('/').any(|s| s == ".." || s == ".") || key.starts_with('/') {
            crate::bail!("invalid key {key:?}");
        }
        Ok(self.root.join(key))
    }

    fn etag_of(p: &Path) -> Result<Option<String>> {
        match fs::read(p) {
            Ok(b) => Ok(Some(format!(
                "\"{:032x}\"",
                twox_hash::XxHash3_128::oneshot(&b)
            ))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

impl Store for FsStore {
    fn describe(&self) -> String {
        format!("file://{}", self.root.display())
    }

    fn put(&self, key: &str, body: Body<'_>, cond: &Cond) -> Result<Option<String>> {
        let p = self.path(key)?;
        let cur = Self::etag_of(&p)?;
        let ok = match cond {
            Cond::None => true,
            Cond::Absent => cur.is_none(),
            Cond::Match(e) => cur.as_deref() == Some(e.as_str()),
        };
        if !ok {
            return Err(Box::new(StoreError::PreconditionFailed(key.to_string())));
        }
        let dir = p.parent().unwrap();
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut tmp = tempfile::Builder::new()
            .prefix(".s4cp-put-")
            .tempfile_in(dir)?;
        match body {
            Body::Bytes(b) => tmp.write_all(b)?,
            Body::File(f, n) => {
                io::copy(
                    &mut FileSection {
                        file: f,
                        pos: 0,
                        end: n,
                    },
                    &mut tmp,
                )?;
            }
        }
        tmp.as_file().sync_all()?;
        tmp.persist(&p).map_err(|e| e.error)?;
        Self::etag_of(&p)
    }

    fn get(&self, key: &str, if_none_match: Option<&str>) -> Result<Get> {
        let p = self.path(key)?;
        let Some(etag) = Self::etag_of(&p)? else {
            return Ok(Get::NotFound);
        };
        if if_none_match == Some(etag.as_str()) {
            return Ok(Get::NotModified);
        }
        let f = File::open(&p)?;
        let len = f.metadata()?.len();
        Ok(Get::Found {
            body: Box::new(io::BufReader::new(f)),
            etag: Some(etag),
            len: Some(len),
        })
    }

    fn get_range(&self, key: &str, start: i64, len: u64) -> Result<Vec<u8>> {
        let p = self.path(key)?;
        let f = File::open(&p).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                Box::new(StoreError::NotFound(key.to_string())) as crate::util::Error
            } else {
                e.into()
            }
        })?;
        let size = f.metadata()?.len();
        let (off, n) = if start < 0 {
            let n = (-start as u64).min(size);
            (size - n, n)
        } else {
            let s = (start as u64).min(size);
            (s, len.min(size - s))
        };
        let mut buf = vec![0u8; n as usize];
        f.read_exact_at(&mut buf, off)?;
        Ok(buf)
    }

    fn list(&self, prefix: &str) -> Result<Vec<ObjInfo>> {
        fn walk(dir: &Path, rel: &str, out: &mut Vec<ObjInfo>) -> io::Result<()> {
            let rd = match fs::read_dir(dir) {
                Ok(r) => r,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e),
            };
            for ent in rd {
                let ent = ent?;
                let name = ent.file_name().to_string_lossy().into_owned();
                if name.starts_with(".s4cp-put-") {
                    continue;
                }
                let key = if rel.is_empty() {
                    name.clone()
                } else {
                    format!("{rel}/{name}")
                };
                let ft = ent.file_type()?;
                if ft.is_dir() {
                    walk(&ent.path(), &key, out)?;
                } else {
                    out.push(ObjInfo {
                        key,
                        size: ent.metadata()?.len(),
                    });
                }
            }
            Ok(())
        }
        let mut out = Vec::new();
        walk(&self.root, "", &mut out)?;
        out.retain(|o| o.key.starts_with(prefix));
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    fn delete(&self, key: &str) -> Result<()> {
        let p = self.path(key)?;
        match fs::remove_file(&p) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// Joins key segments, skipping empty ones.
pub fn join_key(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|p| p.trim_matches('/'))
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fs_store_conditions() {
        let d = tempfile::tempdir().unwrap();
        let s = FsStore::new(d.path());
        let e1 = s
            .put("a/catalog", Body::Bytes(b"one"), &Cond::Absent)
            .unwrap()
            .unwrap();
        assert!(
            s.put("a/catalog", Body::Bytes(b"x"), &Cond::Absent)
                .is_err()
        );
        assert!(
            s.put(
                "a/catalog",
                Body::Bytes(b"x"),
                &Cond::Match("\"nope\"".into())
            )
            .is_err()
        );
        let e2 = s
            .put("a/catalog", Body::Bytes(b"two"), &Cond::Match(e1.clone()))
            .unwrap()
            .unwrap();
        assert!(matches!(
            s.get("a/catalog", Some(&e2)).unwrap(),
            Get::NotModified
        ));
        assert!(matches!(
            s.get("a/catalog", Some(&e1)).unwrap(),
            Get::Found { .. }
        ));
        assert!(matches!(s.get("a/nope", None).unwrap(), Get::NotFound));
        assert_eq!(s.get_range("a/catalog", -2, 0).unwrap(), b"wo");
        assert_eq!(s.get_range("a/catalog", 0, 2).unwrap(), b"tw");
        assert_eq!(s.list("a/").unwrap().len(), 1);
        s.delete("a/catalog").unwrap();
        assert!(s.list("").unwrap().is_empty());
        assert_eq!(join_key(&["", "p/", "db", "catalog"]), "p/db/catalog");
    }
}
