//! Hand-written S3 client: SigV4 (`UNSIGNED-PAYLOAD` for file bodies), single PUT (multipart above
//! 5 GiB), ListObjectsV2, (ranged) GET, DELETE; path-style or virtual-hosted addressing.

use std::fs::File;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use log::{debug, warn};
use sha2::{Digest, Sha256};
use ureq::http;

use crate::bail;
use crate::store::{Body, Cond, FileSection, Get, ObjInfo, Store, StoreError, join_key};
use crate::util::{Context, Result, msg};

pub const UNSIGNED: &str = "UNSIGNED-PAYLOAD";
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const MAX_SINGLE_PUT: u64 = 5 * 1024 * 1024 * 1024;
const RETRIES: u32 = 3;

#[derive(Debug, Clone)]
pub struct S3Config {
    /// `scheme://host[:port]`, no path.
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    pub path_style: bool,
}

pub struct S3Store {
    cfg: S3Config,
    scheme: String,
    /// Endpoint authority (host[:port]) without the bucket.
    authority: String,
    agent: ureq::Agent,
    /// Cleared when the provider answers 501 to a conditional header.
    guards: AtomicBool,
    pub multipart_threshold: u64,
    pub min_part_size: u64,
    pub retry_base: Duration,
    /// ListObjectsV2 page size (None = server default); small values exercise pagination in tests.
    pub max_keys: Option<u32>,
}

#[derive(Default)]
struct Req<'a> {
    method: &'static str,
    /// Full object key (prefix included); None addresses the bucket.
    key: Option<&'a str>,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    body: ReqBody<'a>,
}

#[derive(Default, Clone, Copy)]
enum ReqBody<'a> {
    #[default]
    Empty,
    Bytes(&'a [u8]),
    File(&'a File, u64, u64),
}

type Resp = http::Response<ureq::Body>;

/// Percent-encodes per SigV4: everything but `A-Za-z0-9-._~` (and `/` when `keep_slash`).
pub fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn sha256_hex(b: &[u8]) -> String {
    hex(&Sha256::digest(b))
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m =
        <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("hmac accepts any key length");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

/// SigV4 signer. `headers` must already contain every header to sign (including host,
/// x-amz-date and x-amz-content-sha256). Returns the Authorization header value.
pub fn sign_v4(
    access_key: &str,
    secret: &str,
    region: &str,
    method: &str,
    canonical_uri: &str,
    query: &[(String, String)],
    headers: &[(String, String)],
    payload_hash: &str,
    now: DateTime<Utc>,
) -> String {
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date = &amz_date[..8];
    let mut q: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (uri_encode(k, false), uri_encode(v, false)))
        .collect();
    q.sort();
    let cq = q
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let mut h: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| {
            (
                k.to_ascii_lowercase(),
                v.split_whitespace().collect::<Vec<_>>().join(" "),
            )
        })
        .collect();
    h.sort();
    let ch: String = h.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed = h
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let creq = format!("{method}\n{canonical_uri}\n{cq}\n{ch}\n{signed}\n{payload_hash}");
    let scope = format!("{date}/{region}/s3/aws4_request");
    let sts = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(creq.as_bytes())
    );
    let k = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k = hmac(&k, region.as_bytes());
    let k = hmac(&k, b"s3");
    let k = hmac(&k, b"aws4_request");
    let sig = hex(&hmac(&k, sts.as_bytes()));
    format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/{scope},SignedHeaders={signed},Signature={sig}"
    )
}

/// Flat list of (element name, text) for every leaf element.
fn xml_leaves(body: &[u8]) -> Result<Vec<(String, String)>> {
    use quick_xml::events::Event;
    let mut r = quick_xml::Reader::from_reader(body);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    let mut cur: Option<(String, String)> = None;
    loop {
        match r
            .read_event_into(&mut buf)
            .context("parsing XML response")?
        {
            Event::Start(e) => {
                cur = Some((e.local_name().as_ref().to_string(), String::new()));
            }
            Event::Text(t) => {
                if let Some((_, s)) = cur.as_mut() {
                    s.push_str(&t.xml10_content());
                }
            }
            Event::GeneralRef(g) => {
                if let Some((_, s)) = cur.as_mut() {
                    if let Some(c) = g.resolve_char_ref().context("bad XML char ref")? {
                        s.push(c);
                    } else {
                        let name = g.xml10_content();
                        match quick_xml::escape::resolve_predefined_entity(&name) {
                            Some(v) => s.push_str(v),
                            None => bail!("unknown XML entity &{name};"),
                        }
                    }
                }
            }
            Event::CData(c) => {
                if let Some((_, s)) = cur.as_mut() {
                    s.push_str(&c.xml10_content());
                }
            }
            Event::End(_) => {
                if let Some(leaf) = cur.take() {
                    out.push(leaf);
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

fn header<'a>(r: &'a Resp, name: &str) -> Option<&'a str> {
    r.headers().get(name).and_then(|v| v.to_str().ok())
}

impl S3Store {
    pub fn new(cfg: S3Config) -> Result<S3Store> {
        let (scheme, rest) = cfg.endpoint.split_once("://").ok_or_else(|| {
            msg(format!(
                "endpoint {:?} must start with http:// or https://",
                cfg.endpoint
            ))
        })?;
        let scheme = scheme.to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            bail!("endpoint {:?} must use http or https", cfg.endpoint);
        }
        let mut authority = rest.trim_end_matches('/').to_string();
        if authority.contains('/') {
            bail!(
                "endpoint {:?} must not contain a path; put it in the prefix",
                cfg.endpoint
            );
        }
        let default_port = if scheme == "https" { ":443" } else { ":80" };
        if let Some(a) = authority.strip_suffix(default_port) {
            authority = a.to_string();
        }
        if cfg.bucket.is_empty() {
            bail!("bucket is empty");
        }
        let tls = ureq::tls::TlsConfig::builder()
            .provider(ureq::tls::TlsProvider::Rustls)
            .root_certs(ureq::tls::RootCerts::WebPki)
            .build();
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .tls_config(tls)
            .user_agent(concat!("s4cp/", env!("CARGO_PKG_VERSION")))
            .max_redirects(0)
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_recv_response(Some(Duration::from_secs(120)))
            .timeout_recv_body(Some(Duration::from_secs(3600)))
            .build()
            .new_agent();
        Ok(S3Store {
            cfg,
            scheme,
            authority,
            agent,
            guards: AtomicBool::new(true),
            multipart_threshold: MAX_SINGLE_PUT,
            min_part_size: 64 * 1024 * 1024,
            retry_base: Duration::from_millis(500),
            max_keys: None,
        })
    }

    fn full_key(&self, key: &str) -> String {
        join_key(&[&self.cfg.prefix, key])
    }

    fn host(&self) -> String {
        if self.cfg.path_style {
            self.authority.clone()
        } else {
            format!("{}.{}", self.cfg.bucket, self.authority)
        }
    }

    fn canonical_uri(&self, key: Option<&str>) -> String {
        let mut p = String::new();
        if self.cfg.path_style {
            p.push('/');
            p.push_str(&uri_encode(&self.cfg.bucket, false));
        }
        if let Some(k) = key {
            p.push('/');
            p.push_str(&uri_encode(k, true));
        }
        if p.is_empty() {
            p.push('/');
        }
        p
    }

    fn send_once(&self, req: &Req<'_>) -> Result<Resp, ureq::Error> {
        let uri = self.canonical_uri(req.key);
        let mut query: Vec<(String, String)> = req.query.clone();
        query.sort();
        let qs = query
            .iter()
            .map(|(k, v)| format!("{}={}", uri_encode(k, false), uri_encode(v, false)))
            .collect::<Vec<_>>()
            .join("&");
        let url = format!(
            "{}://{}{}{}{}",
            self.scheme,
            self.host(),
            uri,
            if qs.is_empty() { "" } else { "?" },
            qs
        );
        let payload_hash = match req.body {
            ReqBody::Empty => EMPTY_SHA256.to_string(),
            ReqBody::Bytes(b) => sha256_hex(b),
            ReqBody::File(..) => UNSIGNED.to_string(),
        };
        let now = Utc::now();
        let mut headers = req.headers.clone();
        headers.push(("host".into(), self.host()));
        headers.push((
            "x-amz-date".into(),
            now.format("%Y%m%dT%H%M%SZ").to_string(),
        ));
        headers.push(("x-amz-content-sha256".into(), payload_hash.clone()));
        if let Some(t) = &self.cfg.session_token {
            headers.push(("x-amz-security-token".into(), t.clone()));
        }
        let auth = sign_v4(
            &self.cfg.access_key_id,
            &self.cfg.secret_access_key,
            &self.cfg.region,
            req.method,
            &uri,
            &query,
            &headers,
            &payload_hash,
            now,
        );
        let mut b = http::Request::builder().method(req.method).uri(&url);
        for (k, v) in &headers {
            if k != "host" {
                b = b.header(k.as_str(), v.as_str());
            }
        }
        b = b.header("authorization", auth);
        let len = match req.body {
            ReqBody::Empty => 0,
            ReqBody::Bytes(x) => x.len() as u64,
            ReqBody::File(_, _, n) => n,
        };
        if req.method == "PUT" || req.method == "POST" {
            b = b.header("content-length", len.to_string());
        }
        match req.body {
            ReqBody::Empty => self.agent.run(b.body(()).map_err(ureq::Error::from)?),
            ReqBody::Bytes(x) => self.agent.run(b.body(x).map_err(ureq::Error::from)?),
            ReqBody::File(f, off, n) => {
                let mut sec = FileSection {
                    file: f,
                    pos: off,
                    end: off + n,
                };
                let body = ureq::SendBody::from_reader(&mut sec);
                self.agent.run(b.body(body).map_err(ureq::Error::from)?)
            }
        }
    }

    /// Sends with retries on network errors, 5xx and 429.
    fn send(&self, req: &Req<'_>) -> Result<Resp> {
        let what = format!("{} {}", req.method, req.key.unwrap_or("/"));
        let mut attempt = 0;
        loop {
            let res = self.send_once(req);
            let retryable = match &res {
                Ok(r) => r.status().is_server_error() || r.status().as_u16() == 429,
                Err(e) => !matches!(e, ureq::Error::BadUri(_) | ureq::Error::Http(_)),
            };
            if !retryable || attempt >= RETRIES {
                return match res {
                    Ok(r) => Ok(r),
                    Err(e) => Err(Box::new(e)).with_context(|| format!("{what} failed")),
                };
            }
            let why = match &res {
                Ok(r) => format!("HTTP {}", r.status().as_u16()),
                Err(e) => e.to_string(),
            };
            let delay = self.retry_base * 2u32.pow(attempt);
            warn!(
                "{what}: {why}; retrying in {delay:?} ({}/{RETRIES})",
                attempt + 1
            );
            std::thread::sleep(delay);
            attempt += 1;
        }
    }

    /// Converts a non-success response into an error, parsing the S3 XML error body.
    fn fail(&self, mut r: Resp, what: &str) -> crate::util::Error {
        let status = r.status().as_u16();
        let body = r
            .body_mut()
            .with_config()
            .limit(64 * 1024)
            .read_to_vec()
            .unwrap_or_default();
        let leaves = xml_leaves(&body).unwrap_or_default();
        let get = |n: &str| leaves.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
        let code = get("Code").unwrap_or_default();
        if status == 412 || code == "ConditionalRequestConflict" {
            return Box::new(StoreError::PreconditionFailed(what.to_string()));
        }
        if status == 404 && (code.is_empty() || code == "NoSuchKey") {
            return Box::new(StoreError::NotFound(what.to_string()));
        }
        let m = get("Message")
            .unwrap_or_else(|| String::from_utf8_lossy(&body).chars().take(200).collect());
        msg(format!("{what}: HTTP {status} {code} {m}")
            .trim_end()
            .to_string())
    }

    fn guard_headers(&self, cond: &Cond) -> Vec<(String, String)> {
        if !self.guards.load(Ordering::Relaxed) {
            return vec![];
        }
        match cond {
            Cond::None => vec![],
            Cond::Absent => vec![("if-none-match".into(), "*".into())],
            Cond::Match(e) => vec![("if-match".into(), e.clone())],
        }
    }

    /// Sends a conditional write; on 501 disables guards for this target and resends without them.
    fn send_guarded(&self, mut req: Req<'_>, cond: &Cond) -> Result<Resp> {
        let base = req.headers.clone();
        req.headers.extend(self.guard_headers(cond));
        let r = self.send(&req)?;
        if r.status().as_u16() == 501 && *cond != Cond::None && self.guards.load(Ordering::Relaxed)
        {
            warn!(
                "{}: provider does not implement conditional writes (HTTP 501); continuing without \
                 overwrite guards for this target",
                self.describe()
            );
            self.guards.store(false, Ordering::Relaxed);
            req.headers = base;
            return self.send(&req);
        }
        Ok(r)
    }

    fn put_multipart(&self, key: &str, f: &File, len: u64, cond: &Cond) -> Result<Option<String>> {
        let part = len
            .div_ceil(10_000)
            .max(self.min_part_size)
            .div_ceil(1 << 20)
            << 20;
        let r = self.send(&Req {
            method: "POST",
            key: Some(key),
            query: vec![("uploads".into(), String::new())],
            ..Default::default()
        })?;
        if !r.status().is_success() {
            return Err(self.fail(r, &format!("create multipart upload {key}")));
        }
        let body = r.into_body().read_to_vec()?;
        let upload_id = xml_leaves(&body)?
            .into_iter()
            .find(|(k, _)| k == "UploadId")
            .map(|(_, v)| v)
            .ok_or_else(|| msg("CreateMultipartUpload response has no UploadId"))?;
        let result = (|| -> Result<Option<String>> {
            let mut etags = Vec::new();
            let mut off = 0;
            let mut n = 1;
            while off < len {
                let size = part.min(len - off);
                debug!("uploading part {n} of {key} ({size} bytes)");
                let r = self.send(&Req {
                    method: "PUT",
                    key: Some(key),
                    query: vec![
                        ("partNumber".into(), n.to_string()),
                        ("uploadId".into(), upload_id.clone()),
                    ],
                    body: ReqBody::File(f, off, size),
                    ..Default::default()
                })?;
                if !r.status().is_success() {
                    return Err(self.fail(r, &format!("upload part {n} of {key}")));
                }
                etags.push(
                    header(&r, "etag")
                        .ok_or_else(|| msg("UploadPart response has no ETag"))?
                        .to_string(),
                );
                off += size;
                n += 1;
            }
            let mut xml = String::from("<CompleteMultipartUpload>");
            for (i, e) in etags.iter().enumerate() {
                xml.push_str(&format!(
                    "<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>",
                    i + 1,
                    quick_xml::escape::escape(e.as_str())
                ));
            }
            xml.push_str("</CompleteMultipartUpload>");
            let r = self.send_guarded(
                Req {
                    method: "POST",
                    key: Some(key),
                    query: vec![("uploadId".into(), upload_id.clone())],
                    headers: vec![("content-type".into(), "application/xml".into())],
                    body: ReqBody::Bytes(xml.as_bytes()),
                },
                cond,
            )?;
            if !r.status().is_success() {
                return Err(self.fail(r, &format!("complete multipart upload {key}")));
            }
            // CompleteMultipartUpload can fail with 200 and an <Error> body.
            let body = r.into_body().read_to_vec()?;
            let leaves = xml_leaves(&body)?;
            if let Some((_, code)) = leaves.iter().find(|(k, _)| k == "Code") {
                if code == "PreconditionFailed" || code == "ConditionalRequestConflict" {
                    return Err(Box::new(StoreError::PreconditionFailed(key.to_string())));
                }
                bail!("complete multipart upload {key}: {code}");
            }
            Ok(leaves
                .into_iter()
                .find(|(k, _)| k == "ETag")
                .map(|(_, v)| v))
        })();
        if result.is_err() {
            let r = self.send(&Req {
                method: "DELETE",
                key: Some(key),
                query: vec![("uploadId".into(), upload_id)],
                ..Default::default()
            });
            if let Err(e) = r {
                warn!("aborting multipart upload of {key} failed: {e}");
            }
        }
        result
    }
}

impl S3Store {
    /// Creates the bucket (used by tests; s4cp never creates buckets in normal operation).
    pub fn create_bucket(&self) -> Result<()> {
        let r = self.send(&Req {
            method: "PUT",
            key: None,
            ..Default::default()
        })?;
        if r.status().is_success() || r.status().as_u16() == 409 {
            return Ok(());
        }
        Err(self.fail(r, &format!("create bucket {}", self.cfg.bucket)))
    }
}

impl Store for S3Store {
    fn describe(&self) -> String {
        format!(
            "s3://{}/{} ({})",
            self.cfg.bucket,
            self.cfg.prefix.trim_matches('/'),
            self.cfg.endpoint
        )
    }

    fn put(&self, key: &str, body: Body<'_>, cond: &Cond) -> Result<Option<String>> {
        let full = self.full_key(key);
        if let Body::File(f, len) = body
            && len > self.multipart_threshold
        {
            return self.put_multipart(&full, f, len, cond);
        }
        let rb = match body {
            Body::Bytes(b) => ReqBody::Bytes(b),
            Body::File(f, n) => ReqBody::File(f, 0, n),
        };
        let r = self.send_guarded(
            Req {
                method: "PUT",
                key: Some(&full),
                body: rb,
                ..Default::default()
            },
            cond,
        )?;
        if !r.status().is_success() {
            return Err(self.fail(r, &full));
        }
        Ok(header(&r, "etag").map(str::to_string))
    }

    fn get(&self, key: &str, if_none_match: Option<&str>) -> Result<Get> {
        let full = self.full_key(key);
        let mut headers = vec![];
        if let Some(e) = if_none_match {
            headers.push(("if-none-match".to_string(), e.to_string()));
        }
        let r = self.send(&Req {
            method: "GET",
            key: Some(&full),
            headers,
            ..Default::default()
        })?;
        match r.status().as_u16() {
            304 => Ok(Get::NotModified),
            404 => Ok(Get::NotFound),
            s if (200..300).contains(&s) => {
                let etag = header(&r, "etag").map(str::to_string);
                let len = header(&r, "content-length").and_then(|v| v.parse().ok());
                let body = r.into_body().into_with_config().limit(u64::MAX).reader();
                Ok(Get::Found {
                    body: Box::new(body),
                    etag,
                    len,
                })
            }
            _ => Err(self.fail(r, &full)),
        }
    }

    fn get_range(&self, key: &str, start: i64, len: u64) -> Result<Vec<u8>> {
        let full = self.full_key(key);
        let range = if start < 0 {
            format!("bytes={start}")
        } else {
            format!("bytes={}-{}", start, start as u64 + len.max(1) - 1)
        };
        let r = self.send(&Req {
            method: "GET",
            key: Some(&full),
            headers: vec![("range".into(), range)],
            ..Default::default()
        })?;
        if !r.status().is_success() {
            return Err(self.fail(r, &full));
        }
        let mut v = Vec::new();
        r.into_body()
            .into_with_config()
            .limit(u64::MAX)
            .reader()
            .read_to_end(&mut v)?;
        Ok(v)
    }

    fn list(&self, prefix: &str) -> Result<Vec<ObjInfo>> {
        let base = self.full_key("");
        let full_prefix = if base.is_empty() {
            prefix.to_string()
        } else {
            format!("{base}/{prefix}")
        };
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query = vec![
                ("list-type".to_string(), "2".to_string()),
                ("prefix".to_string(), full_prefix.clone()),
            ];
            if let Some(m) = self.max_keys {
                query.push(("max-keys".into(), m.to_string()));
            }
            if let Some(t) = &token {
                query.push(("continuation-token".into(), t.clone()));
            }
            let r = self.send(&Req {
                method: "GET",
                key: None,
                query,
                ..Default::default()
            })?;
            if !r.status().is_success() {
                return Err(self.fail(r, &format!("list {full_prefix}")));
            }
            let body = r
                .into_body()
                .with_config()
                .limit(64 * 1024 * 1024)
                .read_to_vec()?;
            let mut truncated = false;
            token = None;
            for (k, v) in xml_leaves(&body)? {
                match k.as_str() {
                    "Key" => {
                        let rel = if base.is_empty() {
                            v
                        } else {
                            v.strip_prefix(&format!("{base}/"))
                                .unwrap_or(&v)
                                .to_string()
                        };
                        out.push(ObjInfo { key: rel, size: 0 });
                    }
                    "Size" => {
                        if let Some(o) = out.last_mut() {
                            o.size = v.parse().unwrap_or(0);
                        }
                    }
                    "IsTruncated" => truncated = v == "true",
                    "NextContinuationToken" => token = Some(v),
                    _ => {}
                }
            }
            if !truncated {
                break;
            }
            if token.is_none() {
                bail!("truncated ListObjectsV2 response without a continuation token");
            }
        }
        Ok(out)
    }

    fn delete(&self, key: &str) -> Result<()> {
        let full = self.full_key(key);
        let r = self.send(&Req {
            method: "DELETE",
            key: Some(&full),
            ..Default::default()
        })?;
        if !r.status().is_success() && r.status().as_u16() != 404 {
            return Err(self.fail(r, &full));
        }
        Ok(())
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    const AK: &str = "AKIAIOSFODNN7EXAMPLE";
    const SK: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

    fn t() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2013-05-24T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn h(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    // Test vectors from the AWS S3 SigV4 documentation ("Examples: Signature Calculations").
    #[test]
    fn sigv4_get_object() {
        let a = sign_v4(
            AK,
            SK,
            "us-east-1",
            "GET",
            "/test.txt",
            &[],
            &h(&[
                ("Host", "examplebucket.s3.amazonaws.com"),
                ("Range", "bytes=0-9"),
                ("x-amz-content-sha256", EMPTY_SHA256),
                ("x-amz-date", "20130524T000000Z"),
            ]),
            EMPTY_SHA256,
            t(),
        );
        assert!(
            a.ends_with(
                "Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
            ),
            "{a}"
        );
        assert!(a.contains("SignedHeaders=host;range;x-amz-content-sha256;x-amz-date"));
    }

    #[test]
    fn sigv4_put_object() {
        let ph = "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072";
        let a = sign_v4(
            AK,
            SK,
            "us-east-1",
            "PUT",
            &format!("/{}", uri_encode("test$file.text", true)),
            &[],
            &h(&[
                ("Host", "examplebucket.s3.amazonaws.com"),
                ("Date", "Fri, 24 May 2013 00:00:00 GMT"),
                ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
                ("x-amz-content-sha256", ph),
                ("x-amz-date", "20130524T000000Z"),
            ]),
            ph,
            t(),
        );
        assert!(
            a.ends_with(
                "Signature=98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd"
            ),
            "{a}"
        );
    }

    #[test]
    fn sigv4_list_and_subresource() {
        let hd = h(&[
            ("Host", "examplebucket.s3.amazonaws.com"),
            ("x-amz-content-sha256", EMPTY_SHA256),
            ("x-amz-date", "20130524T000000Z"),
        ]);
        let q = h(&[("prefix", "J"), ("max-keys", "2")]);
        let a = sign_v4(AK, SK, "us-east-1", "GET", "/", &q, &hd, EMPTY_SHA256, t());
        assert!(
            a.ends_with(
                "Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
            ),
            "{a}"
        );
        let q = h(&[("lifecycle", "")]);
        let a = sign_v4(AK, SK, "us-east-1", "GET", "/", &q, &hd, EMPTY_SHA256, t());
        assert!(
            a.ends_with(
                "Signature=fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543"
            ),
            "{a}"
        );
    }

    /// Live checks against an S3 server, configured by S4CP_TEST_S3_ENDPOINT / _KEY / _SECRET.
    pub fn live_store(bucket: &str, prefix: &str) -> Option<S3Store> {
        let endpoint = std::env::var("S4CP_TEST_S3_ENDPOINT").ok()?;
        let s = S3Store::new(S3Config {
            endpoint,
            region: std::env::var("S4CP_TEST_S3_REGION").unwrap_or("us-east-1".into()),
            bucket: bucket.into(),
            prefix: prefix.into(),
            access_key_id: std::env::var("S4CP_TEST_S3_KEY").unwrap(),
            secret_access_key: std::env::var("S4CP_TEST_S3_SECRET").unwrap(),
            session_token: None,
            path_style: true,
        })
        .unwrap();
        s.create_bucket().unwrap();
        Some(s)
    }

    #[test]
    fn live_s3_semantics() {
        let Some(mut s) = live_store(
            "s4cp-test",
            &format!("unit/{}", Utc::now().timestamp_nanos_opt().unwrap()),
        ) else {
            eprintln!("S4CP_TEST_S3_ENDPOINT not set; skipping");
            return;
        };
        let guards = std::env::var("S4CP_TEST_S3_NO_GUARDS").is_err();
        let e1 = s
            .put("a b+c/obj", Body::Bytes(b"hello world"), &Cond::Absent)
            .unwrap();
        assert!(e1.is_some(), "PUT returned no ETag");
        let e1 = e1.unwrap();
        if guards {
            let err = s
                .put("a b+c/obj", Body::Bytes(b"x"), &Cond::Absent)
                .unwrap_err();
            assert!(
                crate::util::find_cause::<StoreError>(&*err).is_some(),
                "If-None-Match ignored: {err}"
            );
            let err = s
                .put(
                    "a b+c/obj",
                    Body::Bytes(b"x"),
                    &Cond::Match("\"0123\"".into()),
                )
                .unwrap_err();
            assert!(
                crate::util::find_cause::<StoreError>(&*err).is_some(),
                "If-Match ignored: {err}"
            );
        }
        let e2 = s
            .put(
                "a b+c/obj",
                Body::Bytes(b"HELLO WORLD"),
                &Cond::Match(e1.clone()),
            )
            .unwrap()
            .unwrap();
        assert_ne!(e1, e2);
        assert!(
            matches!(s.get("a b+c/obj", Some(&e2)).unwrap(), Get::NotModified),
            "conditional GET did not 304"
        );
        match s.get("a b+c/obj", Some(&e1)).unwrap() {
            Get::Found { mut body, etag, .. } => {
                let mut v = Vec::new();
                body.read_to_end(&mut v).unwrap();
                assert_eq!(v, b"HELLO WORLD");
                assert_eq!(etag.as_deref(), Some(e2.as_str()));
            }
            _ => panic!("expected body"),
        }
        assert!(matches!(s.get("missing", None).unwrap(), Get::NotFound));
        assert_eq!(s.get_range("a b+c/obj", -5, 0).unwrap(), b"WORLD");
        assert_eq!(s.get_range("a b+c/obj", 2, 3).unwrap(), b"LLO");
        for i in 0..7 {
            s.put(&format!("many/{i:02}"), Body::Bytes(b"z"), &Cond::None)
                .unwrap();
        }
        s.max_keys = Some(3);
        let l = s.list("many/").unwrap();
        assert_eq!(
            l.iter().map(|o| o.key.as_str()).collect::<Vec<_>>(),
            (0..7).map(|i| format!("many/{i:02}")).collect::<Vec<_>>()
        );
        assert!(l.iter().all(|o| o.size == 1));
        // Multipart path with a small threshold: 11 MiB in 5 MiB parts.
        s.multipart_threshold = 1;
        s.min_part_size = 5 << 20;
        let mut f = tempfile::tempfile().unwrap();
        let data: Vec<u8> = (0..11u32 << 20).map(|i| (i * 7 % 251) as u8).collect();
        std::io::Write::write_all(&mut f, &data).unwrap();
        s.put("big", Body::File(&f, data.len() as u64), &Cond::Absent)
            .unwrap();
        if guards {
            assert!(
                s.put("big", Body::File(&f, data.len() as u64), &Cond::Absent)
                    .is_err(),
                "multipart If-None-Match ignored"
            );
        }
        assert_eq!(s.get_range("big", -3, 0).unwrap(), &data[data.len() - 3..]);
        for o in s.list("").unwrap() {
            s.delete(&o.key).unwrap();
        }
        assert!(s.list("").unwrap().is_empty());
        s.delete("missing").unwrap();
    }

    #[test]
    fn encoding_and_xml() {
        assert_eq!(uri_encode("a b/c~d=e", true), "a%20b/c~d%3De");
        assert_eq!(uri_encode("a/b", false), "a%2Fb");
        let x = br#"<?xml version="1.0"?><ListBucketResult><IsTruncated>true</IsTruncated><Contents><Key>a&amp;b</Key><Size>3</Size></Contents><NextContinuationToken>t/=</NextContinuationToken></ListBucketResult>"#;
        let l = xml_leaves(x).unwrap();
        assert_eq!(l[1], ("Key".to_string(), "a&b".to_string()));
        assert_eq!(l[3].1, "t/=");
    }
}
