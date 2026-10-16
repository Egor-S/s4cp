//! Command-line interface: backup, restore, watch, ls, prune, verify, inspect.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use clap::{Args, Parser, Subcommand};
use log::{debug, error, info, warn};

use crate::backup::{self, Dirs, Job, Policy, Target, load_catalog, state_path};
use crate::bail;
use crate::catalog::{Catalog, Entry};
use crate::config::{self, Db, FileConfig, TargetConfig};
use crate::format::{self, HEADER_LEN, Header, Kind, ObjectReader, TRAILER_LEN, Trailer};
use crate::restore;
use crate::store::Get;
use crate::util::{Context, Result, human_bytes, msg};

const DEFAULT_CONFIG: &str = "/etc/s4cp/config.yaml";

#[derive(Parser, Debug)]
#[command(
    name = "s4cp",
    version,
    about = "Cheap, page-level incremental SQLite backups to S3",
    long_about = "Cheap, page-level incremental SQLite backups to S3.\n\n\
        s4cp takes consistent snapshots of SQLite databases (WAL or rollback journal) by reading\n\
        pages inside a read transaction, and uploads only the pages that changed since the\n\
        previous backup. Backups form chains: a full, then deltas.\n\n\
        Configure databases and targets in a YAML file (--config / S4CP_CONFIG, default\n\
        /etc/s4cp/config.yaml), or use ad-hoc mode:\n\n  \
        s4cp backup ./app.db s3://bucket/prefix\n  \
        s4cp restore s3://bucket/prefix/app ./restored.db\n\n\
        Credentials and endpoint default to AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY,\n\
        AWS_SESSION_TOKEN, AWS_REGION and AWS_ENDPOINT_URL."
)]
pub struct Cli {
    /// YAML config file [default: /etc/s4cp/config.yaml if present]
    #[arg(short, long, env = "S4CP_CONFIG", global = true)]
    config: Option<PathBuf>,
    /// error, warn, info, debug or trace [default: info]
    #[arg(long, env = "S4CP_LOG_LEVEL", global = true)]
    log_level: Option<String>,
    /// Log format on stderr [default: text]
    #[arg(long, env = "S4CP_LOG_FORMAT", global = true, value_parser = ["text", "json"])]
    log_format: Option<String>,
    /// Local state: prefilter + catalog cache [default: /var/lib/s4cp]
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    /// Where spool files are written during a backup [default: /tmp]
    #[arg(long, global = true)]
    spool_dir: Option<PathBuf>,
    #[command(flatten)]
    s3: S3Args,
    #[command(subcommand)]
    cmd: Cmd,
}

/// S3 settings for ad-hoc URLs (s3://…) given on the command line.
#[derive(Args, Debug, Clone, Default)]
struct S3Args {
    /// S3 endpoint, e.g. https://s3.eu-central-1.amazonaws.com [env: AWS_ENDPOINT_URL]
    #[arg(long, global = true, help_heading = "Ad-hoc S3 options")]
    endpoint: Option<String>,
    /// [env: AWS_REGION] [default: us-east-1]
    #[arg(long, global = true, help_heading = "Ad-hoc S3 options")]
    region: Option<String>,
    /// [env: AWS_ACCESS_KEY_ID]
    #[arg(long, global = true, help_heading = "Ad-hoc S3 options")]
    access_key_id: Option<String>,
    /// [env: AWS_SECRET_ACCESS_KEY]
    #[arg(long, global = true, help_heading = "Ad-hoc S3 options")]
    secret_access_key: Option<String>,
    /// Use path-style addressing (default with a custom endpoint)
    #[arg(
        long,
        global = true,
        conflicts_with = "virtual_hosted",
        help_heading = "Ad-hoc S3 options"
    )]
    path_style: bool,
    /// Use virtual-hosted addressing (default on AWS)
    #[arg(long, global = true, help_heading = "Ad-hoc S3 options")]
    virtual_hosted: bool,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Back up configured databases (all, or the named ones), or ad hoc: `backup <db-file> <s3://bucket/prefix>`
    Backup {
        /// Database names from the config, or `<db-file> <url>`
        args: Vec<String>,
        /// Start a new chain with a full backup
        #[arg(long)]
        full: bool,
        /// Ad hoc: database name in storage [default: file stem]
        #[arg(long)]
        name: Option<String>,
        /// Keep the newest N chains (overrides the config)
        #[arg(long)]
        keep_full: Option<u32>,
        /// Max deltas per chain (ad hoc) [default: 24]
        #[arg(long)]
        max_chain: Option<u32>,
        /// New full when deltas exceed this fraction of the full (ad hoc) [default: 0.5]
        #[arg(long)]
        max_chain_ratio: Option<f64>,
    },
    /// Restore a database: `restore <db> [dest]` or `restore <s3://bucket/prefix/db> <dest>`
    Restore {
        /// Database name from the config, or the URL of a database in storage
        source: String,
        /// Output file [default: the configured database path]
        dest: Option<PathBuf>,
        /// Backup id (e.g. 20261016T120000Z/000003) or time (RFC 3339 / `YYYY-MM-DD HH:MM`, UTC); latest at or before it
        #[arg(long)]
        at: Option<String>,
        /// Target to restore from [default: the first one listed for the database]
        #[arg(long)]
        target: Option<String>,
        /// Replace an existing file (also removes its -wal/-shm/-journal)
        #[arg(long)]
        force: bool,
        /// Skip PRAGMA integrity_check
        #[arg(long)]
        no_integrity_check: bool,
    },
    /// Run scheduled backups for all databases with a `schedule`
    Watch,
    /// List backups
    Ls {
        /// Database name or URL [default: all configured databases]
        source: Option<String>,
        #[arg(long)]
        target: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Apply retention (keep_full) and delete orphaned objects
    Prune {
        /// Database names or URLs [default: all configured databases]
        sources: Vec<String>,
        /// Only this target [default: all targets of each database]
        #[arg(long)]
        target: Option<String>,
        /// Keep the newest N chains (overrides the config)
        #[arg(long)]
        keep_full: Option<u32>,
        /// Show what would be deleted
        #[arg(long)]
        dry_run: bool,
    },
    /// Download and check a backup (hashes + integrity_check) without restoring it
    Verify {
        /// Database name or URL [default: all configured databases]
        source: Option<String>,
        #[arg(long)]
        target: Option<String>,
        /// Backup id or time [default: latest]
        #[arg(long)]
        at: Option<String>,
        /// Verify the latest backup of every chain
        #[arg(long, conflicts_with = "at")]
        all_chains: bool,
        #[arg(long)]
        no_integrity_check: bool,
    },
    /// Show what is inside a catalog or data object: a local file or s3://bucket/key
    Inspect {
        what: String,
        /// Decode the body: list every page record (data) or page hash (catalog)
        #[arg(long)]
        pages: bool,
        #[arg(long)]
        json: bool,
    },
}

// ---------------------------------------------------------------------------------------------
// Logging

fn init_logging(level: &str, json: bool) {
    let mut b = env_logger::Builder::new();
    b.parse_filters(level);
    b.target(env_logger::Target::Stderr);
    if json {
        b.format(|f, r| {
            let v = serde_json::json!({
                "ts": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
                "level": r.level().as_str().to_ascii_lowercase(),
                "target": r.target(),
                "msg": r.args().to_string(),
            });
            writeln!(f, "{v}")
        });
    } else {
        b.format(|f, r| {
            writeln!(
                f,
                "{} {:<5} {}",
                Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
                r.level(),
                r.args()
            )
        });
    }
    let _ = b.try_init();
}

// ---------------------------------------------------------------------------------------------
// Signals (watch exits between jobs on SIGTERM/SIGINT; a second signal exits immediately)

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    if STOP.swap(true, Ordering::SeqCst) {
        unsafe { libc::_exit(130) };
    }
}

fn install_signal_handlers() {
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }
}

// ---------------------------------------------------------------------------------------------
// Pings (healthchecks.io style: <url>/start, <url>, <url>/fail)

fn ping(url: &str, suffix: &str) {
    let full = format!("{}{}", url.trim_end_matches('/'), suffix);
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::Rustls)
        .root_certs(ureq::tls::RootCerts::WebPki)
        .build();
    let agent = ureq::Agent::config_builder()
        .tls_config(tls)
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .new_agent();
    match agent.get(&full).call() {
        Ok(_) => debug!("pinged {full}"),
        Err(e) => warn!("ping {full} failed: {e}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Context

struct Ctx {
    cli_s3: S3Args,
    file: Option<FileConfig>,
    dirs: Dirs,
}

impl Ctx {
    fn cfg(&self) -> Result<&FileConfig> {
        self.file.as_ref().ok_or_else(|| {
            msg(format!("no config file (use --config, S4CP_CONFIG or {DEFAULT_CONFIG}), or give an s3:// URL"))
        })
    }

    fn adhoc_target(&self, url: &str) -> Result<Target> {
        let s = &self.cli_s3;
        let t = TargetConfig {
            url: url.to_string(),
            endpoint: s.endpoint.clone(),
            region: s.region.clone(),
            access_key_id: s.access_key_id.clone(),
            secret_access_key: s.secret_access_key.clone(),
            session_token: None,
            path_style: if s.path_style {
                Some(true)
            } else if s.virtual_hosted {
                Some(false)
            } else {
                None
            },
        };
        Ok(Target {
            name: "adhoc".into(),
            store: config::build_store(&t)?,
        })
    }

    /// Resolves `<db name | url>` + `--target` to (db name, target, configured db).
    fn source(&self, source: &str, target: Option<&str>) -> Result<(String, Target, Option<Db>)> {
        if source.contains("://") {
            if target.is_some() {
                bail!("--target cannot be combined with a URL");
            }
            let (base, db) = config::split_db_url(source)?;
            return Ok((db, self.adhoc_target(&base)?, None));
        }
        let db = self.cfg()?.db(source)?;
        let t = match target {
            Some(t) => db
                .targets
                .iter()
                .find(|x| x.name == t)
                .cloned()
                .ok_or_else(|| {
                    msg(format!(
                        "database {source:?} is not backed up to target {t:?}"
                    ))
                })?,
            None => db.targets[0].clone(),
        };
        Ok((db.name.clone(), t, Some(db)))
    }

    fn sources(&self, given: Option<&str>) -> Result<Vec<String>> {
        match given {
            Some(s) => Ok(vec![s.to_string()]),
            None => Ok(self.cfg()?.db_names()),
        }
    }
}

fn prepare_dir(p: &Path, what: &str) -> Option<PathBuf> {
    match std::fs::create_dir_all(p) {
        Ok(()) => {
            let probe = p.join(".s4cp-probe");
            match std::fs::write(&probe, b"") {
                Ok(()) => {
                    let _ = std::fs::remove_file(&probe);
                    Some(p.to_path_buf())
                }
                Err(e) => {
                    warn!("{what} {} is not writable ({e})", p.display());
                    None
                }
            }
        }
        Err(e) => {
            warn!("cannot create {what} {} ({e})", p.display());
            None
        }
    }
}

pub fn main() -> i32 {
    let cli = Cli::parse();

    // CLI > env > YAML > defaults. The config is read before logging is set up, so report its errors directly.
    let config_path = cli.config.clone().or_else(|| {
        let p = PathBuf::from(DEFAULT_CONFIG);
        p.exists().then_some(p)
    });
    let file = match config_path.as_deref().map(FileConfig::load).transpose() {
        Ok(f) => f,
        Err(e) => {
            init_logging("info", cli.log_format.as_deref() == Some("json"));
            error!("{e}");
            return 1;
        }
    };
    let level = cli
        .log_level
        .clone()
        .or(file.as_ref().and_then(|f| f.log_level.clone()))
        .unwrap_or("info".into());
    let format = cli
        .log_format
        .clone()
        .or(file.as_ref().and_then(|f| f.log_format.clone()))
        .unwrap_or("text".into());
    if format != "text" && format != "json" {
        init_logging("info", false);
        error!("log_format must be text or json, not {format:?}");
        return 1;
    }
    init_logging(&level, format == "json");

    let state = cli
        .state_dir
        .clone()
        .or(file
            .as_ref()
            .and_then(|f| f.state_dir.clone().map(PathBuf::from)))
        .unwrap_or(PathBuf::from(config::DEFAULT_STATE_DIR));
    let spool = cli
        .spool_dir
        .clone()
        .or(file
            .as_ref()
            .and_then(|f| f.spool_dir.clone().map(PathBuf::from)))
        .unwrap_or(PathBuf::from(config::DEFAULT_SPOOL_DIR));
    let needs_state = matches!(cli.cmd, Cmd::Backup { .. } | Cmd::Watch | Cmd::Prune { .. });
    let state_dir = if needs_state {
        let s = prepare_dir(&state, "state_dir");
        if s.is_none() {
            warn!(
                "running without local state: every run scans every database and downloads every catalog"
            );
        }
        s
    } else {
        None
    };
    let ctx = Ctx {
        cli_s3: cli.s3.clone(),
        file,
        dirs: Dirs {
            state_dir,
            spool_dir: spool,
        },
    };

    let res = match cli.cmd {
        Cmd::Backup {
            args,
            full,
            name,
            keep_full,
            max_chain,
            max_chain_ratio,
        } => cmd_backup(
            &ctx,
            args,
            full,
            name,
            keep_full,
            max_chain,
            max_chain_ratio,
        ),
        Cmd::Restore {
            source,
            dest,
            at,
            target,
            force,
            no_integrity_check,
        } => cmd_restore(
            &ctx,
            &source,
            dest,
            at.as_deref(),
            target.as_deref(),
            force,
            !no_integrity_check,
        ),
        Cmd::Watch => cmd_watch(&ctx),
        Cmd::Ls {
            source,
            target,
            json,
        } => cmd_ls(&ctx, source.as_deref(), target.as_deref(), json),
        Cmd::Prune {
            sources,
            target,
            keep_full,
            dry_run,
        } => cmd_prune(&ctx, sources, target.as_deref(), keep_full, dry_run),
        Cmd::Verify {
            source,
            target,
            at,
            all_chains,
            no_integrity_check,
        } => cmd_verify(
            &ctx,
            source.as_deref(),
            target.as_deref(),
            at.as_deref(),
            all_chains,
            !no_integrity_check,
        ),
        Cmd::Inspect { what, pages, json } => cmd_inspect(&ctx, &what, pages, json),
    };
    match res {
        Ok(()) => 0,
        Err(e) => {
            error!("{e}");
            1
        }
    }
}

// ---------------------------------------------------------------------------------------------
// backup / watch

/// Runs one database backup with pings; returns whether every target succeeded.
fn run_db(ctx: &Ctx, db: &Db, force_full: bool) -> bool {
    if let Some(u) = &db.ping_url {
        ping(u, "/start");
    }
    let job = Job {
        db: db.name.clone(),
        path: db.path.clone(),
        policy: db.policy.clone(),
        targets: db.targets.clone(),
    };
    let ok = match backup::backup(&job, &ctx.dirs, force_full) {
        Ok(results) => {
            let mut ok = true;
            for (t, r) in results {
                if let Err(e) = r {
                    error!("{} → {t}: backup failed: {e}", db.name);
                    ok = false;
                }
            }
            ok
        }
        Err(e) => {
            error!("{}: backup failed: {e}", db.name);
            false
        }
    };
    if let Some(u) = &db.ping_url {
        ping(u, if ok { "" } else { "/fail" });
    }
    ok
}

fn cmd_backup(
    ctx: &Ctx,
    args: Vec<String>,
    full: bool,
    name: Option<String>,
    keep_full: Option<u32>,
    max_chain: Option<u32>,
    max_chain_ratio: Option<f64>,
) -> Result<()> {
    backup::sweep_spools(&ctx.dirs.spool_dir);
    let dbs: Vec<Db> = if args.len() == 2 && args[1].contains("://") {
        let path = PathBuf::from(&args[0]);
        let name = match name {
            Some(n) => n,
            None => path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .ok_or_else(|| msg("cannot derive a database name; use --name"))?,
        };
        config::valid_name("database", &name)?;
        let d = Policy::default();
        vec![Db {
            name,
            path,
            targets: vec![ctx.adhoc_target(&args[1])?],
            policy: Policy {
                max_chain: max_chain.unwrap_or(d.max_chain),
                max_chain_ratio: max_chain_ratio.unwrap_or(d.max_chain_ratio),
                full_schedule: None,
                keep_full,
            },
            schedule: None,
            ping_url: None,
        }]
    } else {
        if name.is_some() || max_chain.is_some() || max_chain_ratio.is_some() {
            bail!(
                "--name, --max-chain and --max-chain-ratio only apply to ad-hoc backups (set them in the config)"
            );
        }
        if args.iter().any(|a| a.contains("://")) {
            bail!("ad-hoc usage: s4cp backup <db-file> <s3://bucket/prefix>");
        }
        let cfg = ctx.cfg()?;
        let names = if args.is_empty() {
            cfg.db_names()
        } else {
            args
        };
        if names.is_empty() {
            bail!("no databases configured");
        }
        names
            .iter()
            .map(|n| {
                let mut db = cfg.db(n)?;
                if keep_full.is_some() {
                    db.policy.keep_full = keep_full;
                }
                Ok(db)
            })
            .collect::<Result<_>>()?
    };
    let failed: Vec<&str> = dbs
        .iter()
        .filter(|d| !run_db(ctx, d, full))
        .map(|d| d.name.as_str())
        .collect();
    if !failed.is_empty() {
        bail!("backup failed for: {}", failed.join(", "));
    }
    Ok(())
}

fn sleep_until(t: DateTime<Utc>) {
    while !STOP.load(Ordering::SeqCst) {
        let left = t - Utc::now();
        if left <= chrono::Duration::zero() {
            return;
        }
        std::thread::sleep(
            left.to_std()
                .unwrap_or_default()
                .min(Duration::from_secs(1)),
        );
    }
}

fn last_ok(ctx: &Ctx, db: &Db) -> Option<DateTime<Utc>> {
    // The oldest success across targets: a target that never succeeded makes the db overdue.
    let mut out: Option<DateTime<Utc>> = None;
    for t in &db.targets {
        let p = state_path(&ctx.dirs, &t.name, &db.name)?;
        let t = backup::read_prefilter(&p)?.last_ok;
        out = Some(out.map_or(t, |o| o.min(t)));
    }
    out
}

fn cmd_watch(ctx: &Ctx) -> Result<()> {
    install_signal_handlers();
    backup::sweep_spools(&ctx.dirs.spool_dir);
    let cfg = ctx.cfg()?;
    let mut jobs: Vec<(Db, DateTime<Utc>)> = Vec::new();
    let now = Utc::now();
    for n in cfg.db_names() {
        let db = cfg.db(&n)?;
        let Some(cron) = db.schedule.clone() else {
            warn!("{n}: no schedule, not watched");
            continue;
        };
        let overdue = match (cron.prev_at_or_before(now), last_ok(ctx, &db)) {
            (Some(prev), Some(last)) => last < prev,
            (Some(_), None) => true,
            (None, _) => false,
        };
        let next = if overdue {
            now
        } else {
            cron.next_after(now).context("schedule never fires")?
        };
        info!(
            "{n}: schedule {:?}, {}",
            cron.expr(),
            if overdue {
                "overdue, running now".to_string()
            } else {
                format!("next run at {next}")
            }
        );
        jobs.push((db, next));
    }
    if jobs.is_empty() {
        bail!("no database has a schedule; nothing to watch");
    }
    info!(
        "s4cp {} watching {} database(s)",
        env!("CARGO_PKG_VERSION"),
        jobs.len()
    );
    loop {
        let i = (0..jobs.len()).min_by_key(|&i| jobs[i].1).unwrap();
        sleep_until(jobs[i].1);
        if STOP.load(Ordering::SeqCst) {
            info!("signal received, exiting");
            return Ok(());
        }
        let (db, _) = &jobs[i];
        run_db(ctx, db, false);
        // Runs that were missed while this job ran are skipped, not queued.
        let cron = db.schedule.as_ref().unwrap();
        jobs[i].1 = cron
            .next_after(Utc::now())
            .context("schedule never fires")?;
        debug!("{}: next run at {}", jobs[i].0.name, jobs[i].1);
        if STOP.load(Ordering::SeqCst) {
            info!("signal received, exiting");
            return Ok(());
        }
    }
}

// ---------------------------------------------------------------------------------------------
// restore / ls / verify / prune

fn cmd_restore(
    ctx: &Ctx,
    source: &str,
    dest: Option<PathBuf>,
    at: Option<&str>,
    target: Option<&str>,
    force: bool,
    integrity: bool,
) -> Result<()> {
    let (db, t, conf) = ctx.source(source, target)?;
    let dest = match (dest, conf) {
        (Some(d), _) => d,
        (None, Some(c)) => c.path,
        (None, None) => bail!("a destination file is required when restoring from a URL"),
    };
    let cat = load_catalog(&*t.store, &db, None)?;
    if !cat.exists {
        bail!("no catalog for {db} on {}", t.store.describe());
    }
    let e = cat.catalog.resolve(at)?.clone();
    info!(
        "restoring {db} backup {} ({}) from {}",
        e.id,
        e.time.to_rfc3339_opts(SecondsFormat::Secs, true),
        t.store.describe()
    );
    restore::restore(
        &*t.store,
        &db,
        &cat.catalog,
        &e,
        &dest,
        &restore::Options {
            force,
            integrity_check: integrity,
        },
    )
}

fn entry_row(e: &Entry) -> String {
    format!(
        "{:<24} {:<20} {:<5} {:>9} {:>10} {:>10}",
        e.id,
        e.time.format("%Y-%m-%d %H:%M:%S"),
        if e.full { "full" } else { "delta" },
        e.pages,
        e.page_count,
        human_bytes(e.size)
    )
}

fn print_entries(c: &Catalog) {
    println!(
        "{:<24} {:<20} {:<5} {:>9} {:>10} {:>10}",
        "ID", "TIME (UTC)", "KIND", "PAGES", "PAGE_COUNT", "SIZE"
    );
    for e in &c.meta.backups {
        println!("{}", entry_row(e));
    }
}

fn cmd_ls(ctx: &Ctx, source: Option<&str>, target: Option<&str>, json: bool) -> Result<()> {
    let mut out = Vec::new();
    for s in ctx.sources(source)? {
        let (db, t, _) = ctx.source(&s, target)?;
        let cat = load_catalog(&*t.store, &db, None)?;
        if json {
            out.push(serde_json::json!({
                "db": db, "target": t.name, "location": t.store.describe(), "backups": cat.catalog.meta.backups,
            }));
            continue;
        }
        println!("{db} @ {} ({})", t.name, t.store.describe());
        if cat.catalog.meta.backups.is_empty() {
            println!("  (no backups)");
        } else {
            let total: u64 = cat.catalog.meta.backups.iter().map(|e| e.size).sum();
            print_entries(&cat.catalog);
            println!(
                "{} backup(s) in {} chain(s), {} stored",
                cat.catalog.meta.backups.len(),
                cat.catalog.chains().len(),
                human_bytes(total)
            );
        }
        println!();
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&out)?);
    }
    Ok(())
}

fn cmd_verify(
    ctx: &Ctx,
    source: Option<&str>,
    target: Option<&str>,
    at: Option<&str>,
    all_chains: bool,
    integrity: bool,
) -> Result<()> {
    let mut bad = 0;
    for s in ctx.sources(source)? {
        let (db, t, _) = ctx.source(&s, target)?;
        let cat = load_catalog(&*t.store, &db, None)?.catalog;
        let entries: Vec<Entry> = if all_chains {
            let mut v = Vec::new();
            for c in cat.chains() {
                v.push(
                    cat.meta
                        .backups
                        .iter()
                        .rev()
                        .find(|e| e.chain == c)
                        .unwrap()
                        .clone(),
                );
            }
            v
        } else {
            match cat.resolve(at) {
                Ok(e) => vec![e.clone()],
                Err(e) => {
                    error!("{db} @ {}: {e}", t.name);
                    bad += 1;
                    continue;
                }
            }
        };
        for e in entries {
            match restore::verify(&*t.store, &db, &cat, &e, &ctx.dirs.spool_dir, integrity) {
                Ok(()) => info!("{db} @ {}: {} OK", t.name, e.id),
                Err(err) => {
                    error!("{db} @ {}: {} FAILED: {err}", t.name, e.id);
                    bad += 1;
                }
            }
        }
    }
    if bad > 0 {
        bail!("{bad} backup(s) failed verification");
    }
    Ok(())
}

/// `<chain>/<seq>.s4` as written by s4cp.
fn is_data_key(rel: &str) -> bool {
    let Some((chain, file)) = rel.split_once('/') else {
        return false;
    };
    let Some(seq) = file.strip_suffix(".s4") else {
        return false;
    };
    chain.len() == 16
        && chain.as_bytes()[8] == b'T'
        && chain.ends_with('Z')
        && !seq.is_empty()
        && seq.bytes().all(|b| b.is_ascii_digit())
}

fn cmd_prune(
    ctx: &Ctx,
    sources: Vec<String>,
    target: Option<&str>,
    keep_full: Option<u32>,
    dry_run: bool,
) -> Result<()> {
    if keep_full == Some(0) {
        bail!("--keep-full must be at least 1");
    }
    let sources = if sources.is_empty() {
        ctx.cfg()?.db_names()
    } else {
        sources
    };
    let mut failed = 0;
    for s in sources {
        let pairs: Vec<(String, Target, Option<u32>)> = if s.contains("://") || target.is_some() {
            let (db, t, conf) = ctx.source(&s, target)?;
            vec![(db, t, keep_full.or(conf.and_then(|c| c.policy.keep_full)))]
        } else {
            let db = ctx.cfg()?.db(&s)?;
            db.targets
                .iter()
                .map(|t| {
                    (
                        db.name.clone(),
                        t.clone(),
                        keep_full.or(db.policy.keep_full),
                    )
                })
                .collect()
        };
        for (db, t, keep) in pairs {
            if let Err(e) = prune_one(ctx, &db, &t, keep, dry_run) {
                error!("{db} @ {}: prune failed: {e}", t.name);
                failed += 1;
            }
        }
    }
    if failed > 0 {
        bail!("prune failed for {failed} database/target pair(s)");
    }
    Ok(())
}

fn prune_one(ctx: &Ctx, db: &str, t: &Target, keep: Option<u32>, dry_run: bool) -> Result<()> {
    let cache = state_path(&ctx.dirs, &t.name, db);
    let loaded = load_catalog(&*t.store, db, cache.as_deref())?;
    let mut cat = loaded.catalog;
    if !loaded.exists {
        warn!("{db} @ {}: no catalog; only sweeping orphans", t.name);
    }
    let hashes = std::mem::take(&mut cat.hashes);
    let mut removed = Vec::new();
    if let (Some(k), true) = (keep, loaded.exists) {
        removed = backup::retain(
            &*t.store,
            db,
            &mut cat,
            &hashes,
            loaded.etag,
            k,
            &ctx.dirs.spool_dir,
            cache.as_deref(),
            dry_run,
        )?;
        for k in &removed {
            info!(
                "{db} @ {}: {} {k}",
                t.name,
                if dry_run { "would delete" } else { "deleted" }
            );
        }
    }
    // Orphans: data objects not referenced by the catalog. Recent ones may belong to a backup in flight.
    let referenced: std::collections::HashSet<String> =
        cat.meta.backups.iter().map(|e| e.key()).collect();
    let now = Utc::now();
    let mut orphans = 0;
    for o in t.store.list(&format!("{db}/"))? {
        let rel = o.key.strip_prefix(&format!("{db}/")).unwrap_or(&o.key);
        if !is_data_key(rel) || referenced.contains(rel) || removed.contains(&o.key) {
            continue;
        }
        if o.last_modified
            .is_some_and(|m| now - m < chrono::Duration::hours(1))
        {
            info!(
                "{db} @ {}: skipping recent unreferenced {} (backup in progress?)",
                t.name, o.key
            );
            continue;
        }
        orphans += 1;
        if dry_run {
            info!("{db} @ {}: would delete orphan {}", t.name, o.key);
        } else {
            t.store.delete(&o.key)?;
            info!("{db} @ {}: deleted orphan {}", t.name, o.key);
        }
    }
    info!(
        "{db} @ {}: {} chain object(s) and {orphans} orphan(s) {}",
        t.name,
        removed.len(),
        if dry_run {
            "would be deleted"
        } else {
            "deleted"
        }
    );
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// inspect

fn header_json(h: &Header, t: Option<&Trailer>, size: Option<u64>) -> serde_json::Value {
    serde_json::json!({
        "kind": "data",
        "version": h.version,
        "full": h.is_full(),
        "encrypted": h.flags & format::FLAG_ENCRYPTED != 0,
        "flags": h.flags,
        "chain": h.chain,
        "seq": h.seq,
        "parent_seq": (h.parent_seq != format::NO_PARENT).then_some(h.parent_seq),
        "page_size": h.page_size,
        "page_count": h.page_count,
        "records": h.record_count,
        "body_len": h.body_len,
        "size": size,
        "created": DateTime::from_timestamp_millis(h.created_ms).map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true)),
        "image_hash": t.map(|t| crate::util::hex128(t.image_hash)),
    })
}

fn print_header(h: &Header, t: Option<&Trailer>, size: Option<u64>) {
    println!("kind:        data object (format v{})", h.version);
    println!(
        "type:        {}",
        if h.is_full() { "full" } else { "delta" }
    );
    println!(
        "backup id:   {}",
        crate::catalog::backup_id(&h.chain, h.seq)
    );
    if h.parent_seq != format::NO_PARENT {
        println!(
            "parent:      {}",
            crate::catalog::backup_id(&h.chain, h.parent_seq)
        );
    }
    if let Some(c) = DateTime::from_timestamp_millis(h.created_ms) {
        println!(
            "created:     {}",
            c.to_rfc3339_opts(SecondsFormat::Millis, true)
        );
    }
    println!("page_size:   {}", h.page_size);
    println!(
        "page_count:  {} ({})",
        h.page_count,
        human_bytes(h.page_count as u64 * h.page_size as u64)
    );
    println!("records:     {}", h.record_count);
    println!("body:        {} zstd", human_bytes(h.body_len));
    if let Some(s) = size {
        println!("object size: {}", human_bytes(s));
    }
    if let Some(t) = t {
        println!(
            "image hash:  {} (xxh3-128)",
            crate::util::hex128(t.image_hash)
        );
    }
    if h.flags & format::FLAG_ENCRYPTED != 0 {
        println!("encrypted:   yes (not supported by this version)");
    }
}

fn inspect_data_stream<R: Read>(r: R, json: bool, size: Option<u64>) -> Result<()> {
    let mut rd = ObjectReader::new(r)?;
    let h = rd.header.clone();
    let mut pages = Vec::new();
    let mut buf = Vec::new();
    while let Some(p) = rd.next_page(&mut buf)? {
        pages.push((p, format::page_hash(&buf)));
    }
    let t = rd.finish()?;
    if json {
        let mut v = header_json(&h, Some(&t), size);
        v["pages"] = pages
            .iter()
            .map(|(p, x)| serde_json::json!({"pgno": p, "xxh3": format!("{x:016x}")}))
            .collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        print_header(&h, Some(&t), size);
        println!("\nPGNO       XXH3-64");
        for (p, x) in pages {
            println!("{p:<10} {x:016x}");
        }
    }
    Ok(())
}

fn inspect_catalog(c: &Catalog, json: bool, pages: bool, size: Option<u64>) -> Result<()> {
    if json {
        let mut v = serde_json::json!({
            "kind": "catalog", "db": c.meta.db, "size": size, "hash_count": c.hashes.len(), "backups": c.meta.backups,
        });
        if pages {
            v["hashes"] = c.hashes.iter().map(|h| format!("{h:016x}")).collect();
        }
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    println!("kind:        catalog (format v{})", format::VERSION);
    println!("database:    {}", c.meta.db);
    if let Some(s) = size {
        println!("size:        {}", human_bytes(s));
    }
    println!(
        "backups:     {} in {} chain(s)",
        c.meta.backups.len(),
        c.chains().len()
    );
    println!("hash table:  {} pages", c.hashes.len());
    println!();
    print_entries(c);
    if pages {
        println!("\nPGNO       XXH3-64");
        for (i, h) in c.hashes.iter().enumerate() {
            println!("{:<10} {h:016x}", i + 1);
        }
    }
    Ok(())
}

fn cmd_inspect(ctx: &Ctx, what: &str, pages: bool, json: bool) -> Result<()> {
    if let Some(rest) = what.strip_prefix("s3://") {
        let (bucket, key) = rest
            .split_once('/')
            .ok_or_else(|| msg("expected s3://bucket/key"))?;
        let t = ctx.adhoc_target(&format!("s3://{bucket}"))?;
        let head = t.store.get_range(key, 0, HEADER_LEN as u64)?;
        match format::sniff(&head) {
            Some(Kind::Data) if !pages => {
                // Two ranged GETs: header + trailer.
                let h = Header::decode(&head)?;
                let tail = t.store.get_range(key, -(TRAILER_LEN as i64), 0)?;
                let tr = Trailer::decode(&tail)?;
                let size = HEADER_LEN as u64 + h.body_len + TRAILER_LEN as u64;
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&header_json(&h, Some(&tr), Some(size)))?
                    );
                } else {
                    print_header(&h, Some(&tr), Some(size));
                }
                Ok(())
            }
            Some(kind) => {
                let Get::Found { body, len, .. } = t.store.get(key, None)? else {
                    bail!("{what}: not found")
                };
                match kind {
                    Kind::Data => inspect_data_stream(body, json, len),
                    Kind::Catalog => inspect_catalog(
                        &Catalog::read_from(std::io::BufReader::new(body))?,
                        json,
                        pages,
                        len,
                    ),
                }
            }
            None => bail!("{what} is not an s4cp catalog or data object"),
        }
    } else {
        let mut f = std::fs::File::open(what).with_context(|| format!("opening {what}"))?;
        let size = f.metadata()?.len();
        let mut head = [0u8; HEADER_LEN];
        let n = f.read(&mut head)?;
        f.seek(SeekFrom::Start(0))?;
        match format::sniff(&head[..n]) {
            Some(Kind::Data) if !pages => {
                let h = Header::decode(&head[..n])?;
                let mut tail = [0u8; TRAILER_LEN];
                f.seek(SeekFrom::End(-(TRAILER_LEN as i64)))?;
                f.read_exact(&mut tail)?;
                let tr = Trailer::decode(&tail)?;
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&header_json(&h, Some(&tr), Some(size)))?
                    );
                } else {
                    print_header(&h, Some(&tr), Some(size));
                }
                Ok(())
            }
            Some(Kind::Data) => inspect_data_stream(std::io::BufReader::new(f), json, Some(size)),
            Some(Kind::Catalog) => inspect_catalog(
                &Catalog::read_from(std::io::BufReader::new(f))?,
                json,
                pages,
                Some(size),
            ),
            None => bail!("{what} is not an s4cp catalog or data object"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_keys() {
        assert!(is_data_key("20261016T120000Z/000003.s4"));
        assert!(!is_data_key("catalog"));
        assert!(!is_data_key("20261016T120000Z/x.s4"));
        assert!(!is_data_key("notes/000001.s4"));
    }

    #[test]
    fn cli_parses() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
