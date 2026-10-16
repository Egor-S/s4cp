//! Data object format (v1, frozen at 3.0.0).
//!
//! ```text
//! header  (64 B)  magic "S4CPDATA", u16 version, u16 flags, u32 page_size, u32 page_count,
//!                 u32 record_count, u32 seq, u32 parent_seq (u32::MAX = none), [16] chain id,
//!                 i64 created (unix ms), u64 body_len
//! body            one zstd frame of sorted records: u32 pgno (LE) + page_size bytes
//! trailer (32 B)  u64 body_len, u128 image xxh3-128, magic "S4CPTAIL"
//! ```
//! All integers are little-endian. A full backup is a delta that contains every page (`FLAG_FULL`).

use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};

use crate::bail;
use crate::util::{Context, Result};

pub const DATA_MAGIC: &[u8; 8] = b"S4CPDATA";
pub const TRAILER_MAGIC: &[u8; 8] = b"S4CPTAIL";
pub const CATALOG_MAGIC: &[u8; 8] = b"S4CPCATL";
pub const VERSION: u16 = 1;
pub const HEADER_LEN: usize = 64;
pub const TRAILER_LEN: usize = 32;

/// Reserved for encryption (deferred); readers must refuse objects with this bit.
pub const FLAG_ENCRYPTED: u16 = 1 << 0;
pub const FLAG_FULL: u16 = 1 << 1;
pub const NO_PARENT: u32 = u32::MAX;

pub const ZSTD_LEVEL: i32 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub version: u16,
    pub flags: u16,
    pub page_size: u32,
    pub page_count: u32,
    pub record_count: u32,
    pub seq: u32,
    pub parent_seq: u32,
    pub chain: String,
    pub created_ms: i64,
    pub body_len: u64,
}

impl Header {
    pub fn is_full(&self) -> bool {
        self.flags & FLAG_FULL != 0
    }

    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0..8].copy_from_slice(DATA_MAGIC);
        b[8..10].copy_from_slice(&self.version.to_le_bytes());
        b[10..12].copy_from_slice(&self.flags.to_le_bytes());
        b[12..16].copy_from_slice(&self.page_size.to_le_bytes());
        b[16..20].copy_from_slice(&self.page_count.to_le_bytes());
        b[20..24].copy_from_slice(&self.record_count.to_le_bytes());
        b[24..28].copy_from_slice(&self.seq.to_le_bytes());
        b[28..32].copy_from_slice(&self.parent_seq.to_le_bytes());
        let c = self.chain.as_bytes();
        b[32..32 + c.len().min(16)].copy_from_slice(&c[..c.len().min(16)]);
        b[48..56].copy_from_slice(&self.created_ms.to_le_bytes());
        b[56..64].copy_from_slice(&self.body_len.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<Header> {
        if b.len() < HEADER_LEN || &b[0..8] != DATA_MAGIC {
            bail!("not an s4cp data object (bad magic)");
        }
        let u16_at = |o: usize| u16::from_le_bytes(b[o..o + 2].try_into().unwrap());
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let version = u16_at(8);
        if version != VERSION {
            bail!("unsupported data object version {version}");
        }
        let chain_raw = &b[32..48];
        let end = chain_raw.iter().position(|&c| c == 0).unwrap_or(16);
        let chain = std::str::from_utf8(&chain_raw[..end])
            .context("bad chain id")?
            .to_string();
        Ok(Header {
            version,
            flags: u16_at(10),
            page_size: u32_at(12),
            page_count: u32_at(16),
            record_count: u32_at(20),
            seq: u32_at(24),
            parent_seq: u32_at(28),
            chain,
            created_ms: i64::from_le_bytes(b[48..56].try_into().unwrap()),
            body_len: u64::from_le_bytes(b[56..64].try_into().unwrap()),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trailer {
    pub body_len: u64,
    pub image_hash: u128,
}

impl Trailer {
    pub fn encode(&self) -> [u8; TRAILER_LEN] {
        let mut b = [0u8; TRAILER_LEN];
        b[0..8].copy_from_slice(&self.body_len.to_le_bytes());
        b[8..24].copy_from_slice(&self.image_hash.to_le_bytes());
        b[24..32].copy_from_slice(TRAILER_MAGIC);
        b
    }

    pub fn decode(b: &[u8]) -> Result<Trailer> {
        if b.len() != TRAILER_LEN || &b[24..32] != TRAILER_MAGIC {
            bail!("bad data object trailer (truncated object?)");
        }
        Ok(Trailer {
            body_len: u64::from_le_bytes(b[0..8].try_into().unwrap()),
            image_hash: u128::from_le_bytes(b[8..24].try_into().unwrap()),
        })
    }
}

/// Counts bytes written through it.
struct Counting<W> {
    inner: W,
    n: u64,
}

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.n += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Streams a data object into a seekable sink (a spool file).
pub struct ObjectWriter<W: Write + Seek> {
    header: Header,
    enc: zstd::stream::write::Encoder<'static, Counting<io::BufWriter<W>>>,
    last_pgno: u32,
}

impl<W: Write + Seek> ObjectWriter<W> {
    pub fn new(mut sink: W, header: Header) -> Result<Self> {
        sink.write_all(&[0u8; HEADER_LEN])?;
        let enc = zstd::stream::write::Encoder::new(
            Counting {
                inner: io::BufWriter::with_capacity(256 * 1024, sink),
                n: 0,
            },
            ZSTD_LEVEL,
        )?;
        Ok(ObjectWriter {
            header,
            enc,
            last_pgno: 0,
        })
    }

    pub fn add_page(&mut self, pgno: u32, data: &[u8]) -> Result<()> {
        debug_assert!(
            pgno > self.last_pgno,
            "pages must be added in ascending order"
        );
        debug_assert_eq!(data.len(), self.header.page_size as usize);
        self.last_pgno = pgno;
        self.enc.write_all(&pgno.to_le_bytes())?;
        self.enc.write_all(data)?;
        self.header.record_count += 1;
        Ok(())
    }

    pub fn record_count(&self) -> u32 {
        self.header.record_count
    }

    /// Finishes the object; returns the sink, the final header and the total object length.
    pub fn finish(mut self, page_count: u32, image_hash: u128) -> Result<(W, Header, u64)> {
        let counting = self.enc.finish()?;
        let body_len = counting.n;
        let mut sink = counting.inner.into_inner().map_err(|e| e.into_error())?;
        self.header.body_len = body_len;
        self.header.page_count = page_count;
        sink.write_all(
            &Trailer {
                body_len,
                image_hash,
            }
            .encode(),
        )?;
        sink.seek(SeekFrom::Start(0))?;
        sink.write_all(&self.header.encode())?;
        sink.flush()?;
        let total = HEADER_LEN as u64 + body_len + TRAILER_LEN as u64;
        sink.seek(SeekFrom::Start(0))?;
        Ok((sink, self.header, total))
    }
}

/// Streams records out of a data object read sequentially (e.g. straight from an HTTP body).
pub struct ObjectReader<R: Read> {
    pub header: Header,
    dec: Option<zstd::stream::read::Decoder<'static, BufReader<io::Take<R>>>>,
    remaining: u32,
}

impl<R: Read> ObjectReader<R> {
    pub fn new(mut r: R) -> Result<Self> {
        let mut hb = [0u8; HEADER_LEN];
        r.read_exact(&mut hb)
            .context("reading data object header")?;
        let header = Header::decode(&hb)?;
        if header.flags & FLAG_ENCRYPTED != 0 {
            bail!("encrypted data objects are not supported by this version");
        }
        let dec = zstd::stream::read::Decoder::new(r.take(header.body_len))?.single_frame();
        let remaining = header.record_count;
        Ok(ObjectReader {
            header,
            dec: Some(dec),
            remaining,
        })
    }

    /// Reads the next record into `buf`; returns its pgno, or None at the end of the body.
    pub fn next_page(&mut self, buf: &mut Vec<u8>) -> Result<Option<u32>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let dec = self.dec.as_mut().expect("reader already finished");
        let mut pg = [0u8; 4];
        dec.read_exact(&mut pg)
            .context("reading data object body")?;
        buf.resize(self.header.page_size as usize, 0);
        dec.read_exact(buf).context("reading data object body")?;
        self.remaining -= 1;
        Ok(Some(u32::from_le_bytes(pg)))
    }

    /// Consumes the rest of the body and reads the trailer.
    pub fn finish(mut self) -> Result<Trailer> {
        let mut buf = Vec::new();
        while self.next_page(&mut buf)?.is_some() {}
        let dec = self.dec.take().unwrap();
        let mut take = dec.finish().into_inner();
        io::copy(&mut take, &mut io::sink())?;
        let mut r = take.into_inner();
        let mut tb = [0u8; TRAILER_LEN];
        r.read_exact(&mut tb)
            .context("reading data object trailer")?;
        let t = Trailer::decode(&tb)?;
        if t.body_len != self.header.body_len {
            bail!("data object header/trailer body length mismatch");
        }
        Ok(t)
    }
}

/// Which kind of s4cp file a byte prefix belongs to.
#[derive(Debug, PartialEq, Eq)]
pub enum Kind {
    Data,
    Catalog,
}

pub fn sniff(prefix: &[u8]) -> Option<Kind> {
    if prefix.len() < 8 {
        return None;
    }
    match &prefix[0..8] {
        m if m == DATA_MAGIC => Some(Kind::Data),
        m if m == CATALOG_MAGIC => Some(Kind::Catalog),
        _ => None,
    }
}

pub fn page_hash(data: &[u8]) -> u64 {
    twox_hash::XxHash3_64::oneshot(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(full: bool) -> Header {
        Header {
            version: VERSION,
            flags: if full { FLAG_FULL } else { 0 },
            page_size: 512,
            page_count: 0,
            record_count: 0,
            seq: 3,
            parent_seq: 2,
            chain: "20261016T120000Z".into(),
            created_ms: 1_760_000_000_000,
            body_len: 0,
        }
    }

    #[test]
    fn roundtrip() {
        let pages: Vec<(u32, Vec<u8>)> =
            vec![(1, vec![1; 512]), (5, vec![5; 512]), (9, vec![9; 512])];
        let mut w = ObjectWriter::new(io::Cursor::new(Vec::new()), header(false)).unwrap();
        for (p, d) in &pages {
            w.add_page(*p, d).unwrap();
        }
        let (cur, h, total) = w.finish(9, 0xdead_beef).unwrap();
        let bytes = cur.into_inner();
        assert_eq!(bytes.len() as u64, total);
        assert_eq!(h.record_count, 3);
        assert_eq!(sniff(&bytes), Some(Kind::Data));
        assert_eq!(Header::decode(&bytes[..HEADER_LEN]).unwrap(), h);
        let t = Trailer::decode(&bytes[bytes.len() - TRAILER_LEN..]).unwrap();
        assert_eq!(t.image_hash, 0xdead_beef);

        // Extra bytes after the object must not be consumed or confuse the reader.
        let mut padded = bytes.clone();
        padded.extend_from_slice(b"garbage");
        let mut r = ObjectReader::new(&padded[..]).unwrap();
        assert_eq!(r.header, h);
        let mut buf = Vec::new();
        for (p, d) in &pages {
            assert_eq!(r.next_page(&mut buf).unwrap(), Some(*p));
            assert_eq!(&buf, d);
        }
        assert_eq!(r.next_page(&mut buf).unwrap(), None);
        assert_eq!(r.finish().unwrap(), t);
    }

    #[test]
    fn truncated_object_fails() {
        let mut w = ObjectWriter::new(io::Cursor::new(Vec::new()), header(true)).unwrap();
        w.add_page(1, &[7; 512]).unwrap();
        let bytes = w.finish(1, 1).unwrap().0.into_inner();
        let cut = &bytes[..bytes.len() - 5];
        let r = ObjectReader::new(cut).unwrap();
        assert!(r.finish().is_err());
    }
}
