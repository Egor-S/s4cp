//! YAML configuration (named `targets` and `databases`), `${VAR}` interpolation, AWS_* env defaults.

use std::fmt;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use serde::de::{Deserializer, MapAccess, Visitor};

use crate::backup::{Policy, Target};
use crate::bail;
use crate::s3::{S3Config, S3Store};
use crate::schedule::Cron;
use crate::store::{FsStore, Store};
use crate::util::{Context, Result};

pub const DEFAULT_STATE_DIR: &str = "/var/lib/s4cp";
pub const DEFAULT_SPOOL_DIR: &str = "/tmp";

/// A YAML mapping that keeps its key order (the first listed target is the default for restore).
#[derive(Debug, Clone)]
pub struct Ordered<T>(pub Vec<(String, T)>);

impl<T> Default for Ordered<T> {
    fn default() -> Self {
        Ordered(Vec::new())
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Ordered<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
            type Value = Ordered<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a mapping")
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut m: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut out: Vec<(String, T)> = Vec::new();
                while let Some((k, v)) = m.next_entry::<String, T>()? {
                    if out.iter().any(|(e, _)| *e == k) {
                        return Err(serde::de::Error::custom(format!("duplicate key {k:?}")));
                    }
                    out.push((k, v));
                }
                Ok(Ordered(out))
            }
        }
        d.deserialize_map(V(PhantomData))
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub state_dir: Option<String>,
    pub spool_dir: Option<String>,
    pub log_level: Option<String>,
    pub log_format: Option<String>,
    #[serde(default)]
    pub defaults: DbSettings,
    #[serde(default)]
    pub targets: Ordered<TargetConfig>,
    #[serde(default)]
    pub databases: Ordered<DbConfig>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    /// `s3://bucket[/prefix]` or `file:///path`.
    pub url: String,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    pub session_token: Option<String>,
    pub path_style: Option<bool>,
}

/// Per-database settings that can also be set under `defaults`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct DbSettings {
    pub schedule: Option<String>,
    pub timezone: Option<String>,
    pub full_schedule: Option<String>,
    pub max_chain: Option<u32>,
    pub max_chain_ratio: Option<f64>,
    pub keep_full: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct DbConfig {
    pub path: String,
    pub targets: Option<Vec<String>>,
    pub ping_url: Option<String>,
    pub schedule: Option<String>,
    pub timezone: Option<String>,
    pub full_schedule: Option<String>,
    pub max_chain: Option<u32>,
    pub max_chain_ratio: Option<f64>,
    pub keep_full: Option<u32>,
}

/// A fully resolved database.
pub struct Db {
    pub name: String,
    pub path: PathBuf,
    pub targets: Vec<Target>,
    pub policy: Policy,
    pub schedule: Option<Cron>,
    pub ping_url: Option<String>,
}

/// Expands `${VAR}` and `${VAR:-default}`; `$$` is a literal `$`. Unset variables without a default are errors.
pub fn interpolate(s: &str) -> Result<String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        if let Some(t) = tail.strip_prefix("$$") {
            out.push('$');
            rest = t;
        } else if let Some(t) = tail.strip_prefix("${") {
            let Some(end) = t.find('}') else {
                bail!("unterminated ${{ in {s:?}")
            };
            let inner = &t[..end];
            let (name, default) = match inner.split_once(":-") {
                Some((n, d)) => (n, Some(d)),
                None => (inner, None),
            };
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                bail!("invalid variable name {name:?} in {s:?}");
            }
            match (std::env::var(name).ok().filter(|v| !v.is_empty()), default) {
                (Some(v), _) => out.push_str(&v),
                (None, Some(d)) => out.push_str(d),
                (None, None) => bail!("environment variable {name} is not set"),
            }
            rest = &t[end + 1..];
        } else {
            out.push('$');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    Ok(out)
}

fn interp_opt(v: &Option<String>) -> Result<Option<String>> {
    v.as_deref().map(interpolate).transpose()
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

pub fn valid_name(kind: &str, n: &str) -> Result<()> {
    if n.is_empty()
        || n.starts_with('.')
        || !n
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
    {
        bail!("invalid {kind} name {n:?} (allowed: letters, digits, '.', '_', '-')");
    }
    Ok(())
}

/// Builds a store from a target definition. Missing settings fall back to AWS_* env vars, then defaults.
pub fn build_store(t: &TargetConfig) -> Result<Arc<dyn Store>> {
    let url = interpolate(&t.url)?;
    if let Some(path) = url.strip_prefix("file://") {
        if path.is_empty() {
            bail!("file:// URL needs a path");
        }
        return Ok(Arc::new(FsStore::new(Path::new(path))));
    }
    let Some(rest) = url.strip_prefix("s3://") else {
        bail!("target url {url:?} must start with s3:// or file://");
    };
    let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
    let region = interp_opt(&t.region)?
        .or_else(|| env("AWS_REGION"))
        .or_else(|| env("AWS_DEFAULT_REGION"))
        .unwrap_or_else(|| "us-east-1".into());
    let custom = interp_opt(&t.endpoint)?
        .or_else(|| env("AWS_ENDPOINT_URL_S3"))
        .or_else(|| env("AWS_ENDPOINT_URL"));
    let path_style = t.path_style.unwrap_or(custom.is_some());
    let endpoint = custom.unwrap_or_else(|| format!("https://s3.{region}.amazonaws.com"));
    let access_key_id = interp_opt(&t.access_key_id)?
        .or_else(|| env("AWS_ACCESS_KEY_ID"))
        .ok_or_else(|| {
            crate::util::msg("no access_key_id (set it in the target or AWS_ACCESS_KEY_ID)")
        })?;
    let secret_access_key = interp_opt(&t.secret_access_key)?
        .or_else(|| env("AWS_SECRET_ACCESS_KEY"))
        .ok_or_else(|| {
            crate::util::msg("no secret_access_key (set it in the target or AWS_SECRET_ACCESS_KEY)")
        })?;
    let session_token = interp_opt(&t.session_token)?.or_else(|| env("AWS_SESSION_TOKEN"));
    Ok(Arc::new(S3Store::new(S3Config {
        endpoint,
        region,
        bucket: bucket.to_string(),
        prefix: prefix.trim_matches('/').to_string(),
        access_key_id,
        secret_access_key,
        session_token,
        path_style,
    })?))
}

impl FileConfig {
    pub fn load(path: &Path) -> Result<FileConfig> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("in config {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<FileConfig> {
        let c: FileConfig =
            serde_saphyr::from_str(text).map_err(|e| crate::util::msg(e.to_string()))?;
        for (n, _) in &c.targets.0 {
            valid_name("target", n)?;
        }
        for (n, d) in &c.databases.0 {
            valid_name("database", n)?;
            for t in d.targets.iter().flatten() {
                if !c.targets.0.iter().any(|(k, _)| k == t) {
                    bail!("database {n:?} refers to unknown target {t:?}");
                }
            }
        }
        Ok(c)
    }

    pub fn target(&self, name: &str) -> Result<Target> {
        let Some((_, t)) = self.targets.0.iter().find(|(k, _)| k == name) else {
            bail!("unknown target {name:?}");
        };
        let store = build_store(t).with_context(|| format!("target {name:?}"))?;
        Ok(Target {
            name: name.to_string(),
            store,
        })
    }

    pub fn db_names(&self) -> Vec<String> {
        self.databases.0.iter().map(|(k, _)| k.clone()).collect()
    }

    /// Resolves a database and its targets (all targets, in order, unless it lists some).
    pub fn db(&self, name: &str) -> Result<Db> {
        let Some((_, d)) = self.databases.0.iter().find(|(k, _)| k == name) else {
            bail!(
                "unknown database {name:?} (configured: {})",
                self.db_names().join(", ")
            );
        };
        let names: Vec<String> = match &d.targets {
            Some(t) => t.clone(),
            None => self.targets.0.iter().map(|(k, _)| k.clone()).collect(),
        };
        if names.is_empty() {
            bail!("database {name:?} has no targets");
        }
        let targets = names
            .iter()
            .map(|t| self.target(t))
            .collect::<Result<Vec<_>>>()?;
        let s = d;
        let df = &self.defaults;
        let tz = s.timezone.clone().or(df.timezone.clone());
        let cron = |e: &Option<String>, what: &str| -> Result<Option<Cron>> {
            e.as_deref()
                .map(|e| {
                    Cron::parse(e, tz.as_deref())
                        .with_context(|| format!("database {name:?} {what}"))
                })
                .transpose()
        };
        let policy = Policy {
            max_chain: s.max_chain.or(df.max_chain).unwrap_or(24),
            max_chain_ratio: s.max_chain_ratio.or(df.max_chain_ratio).unwrap_or(0.5),
            full_schedule: cron(
                &s.full_schedule.clone().or(df.full_schedule.clone()),
                "full_schedule",
            )?,
            keep_full: s.keep_full.or(df.keep_full),
        };
        if policy.keep_full == Some(0) {
            bail!("database {name:?}: keep_full must be at least 1");
        }
        Ok(Db {
            name: name.to_string(),
            path: PathBuf::from(interpolate(&d.path)?),
            targets,
            policy,
            schedule: cron(&s.schedule.clone().or(df.schedule.clone()), "schedule")?,
            ping_url: interp_opt(&d.ping_url)?,
        })
    }
}

/// Splits `s3://bucket/prefix/db` (or `file:///dir/db`) into a target URL and a db name.
pub fn split_db_url(url: &str) -> Result<(String, String)> {
    let trimmed = url.trim_end_matches('/');
    let Some((base, db)) = trimmed.rsplit_once('/') else {
        bail!("bad URL {url:?}")
    };
    if base.ends_with(':') || base.ends_with("//") || base.ends_with(":/") {
        bail!("{url:?} must name the database directory, e.g. s3://bucket/prefix/app");
    }
    valid_name("database", db)?;
    Ok((base.to_string(), db.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const YAML: &str = r#"
state_dir: /tmp/s4cp-state
defaults:
  schedule: "*/15 * * * *"
  keep_full: 3
targets:
  zeta:
    url: s3://bkt/prefix
    endpoint: http://localhost:9000
    access_key_id: ${S4CP_TEST_AK}
    secret_access_key: ${S4CP_TEST_SK:-fallback}
  alpha:
    url: file:///tmp/s4cp-alpha
databases:
  app:
    path: /data/app.db
    max_chain: 5
    ping_url: https://hc-ping.com/x
  other:
    path: /data/other.db
    targets: [alpha]
    schedule: "0 3 * * 1"
    timezone: Europe/Berlin
"#;

    #[test]
    fn parse_and_resolve() {
        unsafe { std::env::set_var("S4CP_TEST_AK", "ak") };
        let c = FileConfig::parse(YAML).unwrap();
        assert_eq!(
            c.targets
                .0
                .iter()
                .map(|(k, _)| k.as_str())
                .collect::<Vec<_>>(),
            vec!["zeta", "alpha"]
        );
        let app = c.db("app").unwrap();
        assert_eq!(
            app.targets
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            vec!["zeta", "alpha"]
        );
        assert_eq!(app.policy.max_chain, 5);
        assert_eq!(app.policy.keep_full, Some(3));
        assert_eq!(app.schedule.as_ref().unwrap().expr(), "*/15 * * * *");
        let other = c.db("other").unwrap();
        assert_eq!(other.targets.len(), 1);
        assert_eq!(other.schedule.as_ref().unwrap().expr(), "0 3 * * 1");
        assert!(c.db("nope").is_err());
    }

    #[test]
    fn example_config_is_valid() {
        for v in [
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "R2_ACCESS_KEY_ID",
            "R2_SECRET_ACCESS_KEY",
        ] {
            unsafe { std::env::set_var(v, "x") };
        }
        let c = FileConfig::parse(include_str!("../config.example.yaml")).unwrap();
        let app = c.db("app").unwrap();
        assert_eq!(app.targets.len(), 2);
        assert_eq!(app.policy.keep_full, Some(7));
        assert_eq!(c.db("analytics").unwrap().policy.keep_full, Some(3));
    }

    #[test]
    fn rejects_bad_config() {
        assert!(FileConfig::parse("targets:\n  a:\n    url: s3://b\n    bogus: 1\n").is_err());
        assert!(
            FileConfig::parse("databases:\n  a:\n    path: x\n    targets: [missing]\n").is_err()
        );
        assert!(FileConfig::parse("targets:\n  a/b:\n    url: s3://b\n").is_err());
        assert!(
            FileConfig::parse("targets:\n  a:\n    url: s3://b\n  a:\n    url: s3://c\n").is_err()
        );
    }

    #[test]
    fn interpolation() {
        unsafe { std::env::set_var("S4CP_TEST_X", "val") };
        assert_eq!(interpolate("a${S4CP_TEST_X}b").unwrap(), "avalb");
        assert_eq!(interpolate("${S4CP_TEST_UNSET:-d}").unwrap(), "d");
        assert_eq!(interpolate("$$x $y").unwrap(), "$x $y");
        assert!(interpolate("${S4CP_TEST_UNSET}").is_err());
        assert!(interpolate("${oops").is_err());
    }

    #[test]
    fn db_urls() {
        assert_eq!(
            split_db_url("s3://b/p/app").unwrap(),
            ("s3://b/p".into(), "app".into())
        );
        assert_eq!(
            split_db_url("s3://b/app/").unwrap(),
            ("s3://b".into(), "app".into())
        );
        assert_eq!(
            split_db_url("file:///x/app").unwrap(),
            ("file:///x".into(), "app".into())
        );
        assert!(split_db_url("s3://b").is_err());
    }
}
