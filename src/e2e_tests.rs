//! Property tests: random writes / VACUUMs / page_size changes → backup chain → every backup restores byte-equal.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use rusqlite::Connection;

use crate::backup::{self, Dirs, Job, Outcome, Policy, Target, load_catalog};
use crate::restore::{self, Options};
use crate::store::{FsStore, Store};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
}

fn is_wal(c: &Connection) -> bool {
    let m: String = c
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    m == "wal"
}

/// The logical image of the database right now (checkpoints the WAL first; no writes in between).
fn image(c: &Connection, path: &Path) -> Vec<u8> {
    if is_wal(c) {
        c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .unwrap();
    }
    std::fs::read(path).unwrap()
}

fn random_op(rng: &mut Rng, c: &Connection) {
    match rng.below(10) {
        0..=3 => {
            let n = 1 + rng.below(60);
            let sz = 10 + rng.below(3000);
            c.execute_batch(&format!(
                "WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM s WHERE i<{n}) \
                 INSERT INTO t(k, v) SELECT abs(random()) % 100000, randomblob({sz}) FROM s;"
            ))
            .unwrap();
        }
        4 => {
            c.execute_batch(&format!(
                "UPDATE t SET v = randomblob(length(v)) WHERE rowid % {} = 0;",
                2 + rng.below(9)
            ))
            .unwrap();
        }
        5 => {
            c.execute_batch(&format!(
                "DELETE FROM t WHERE rowid % {} = 0;",
                1 + rng.below(4)
            ))
            .unwrap();
        }
        6 => c.execute_batch("VACUUM;").unwrap(),
        7 => {
            // page_size can only change outside WAL mode.
            let wal = is_wal(c);
            let ps = [512, 1024, 2048, 4096, 8192][rng.below(5) as usize];
            if wal {
                c.execute_batch("PRAGMA journal_mode=DELETE;").unwrap();
            }
            c.execute_batch(&format!("PRAGMA page_size={ps}; VACUUM;"))
                .unwrap();
            if wal {
                c.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
            }
        }
        8 => {
            let n = rng.below(5);
            c.execute_batch(&format!(
                "CREATE TABLE IF NOT EXISTS x{n}(a); INSERT INTO x{n} VALUES (randomblob(9000)); \
                 DROP TABLE IF EXISTS x{};",
                (n + 1) % 5
            ))
            .unwrap();
        }
        _ => {} // no change
    }
}

fn run_scenario(seed: u64, stores: Vec<Arc<dyn Store>>, with_state: bool) {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("app.db");
    let c = Connection::open(&db).unwrap();
    let ps = [1024, 4096][rng.below(2) as usize];
    c.execute_batch(&format!(
        "PRAGMA page_size={ps}; CREATE TABLE t(k, v); CREATE INDEX tk ON t(k);"
    ))
    .unwrap();
    if rng.chance(70) {
        c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
            .unwrap();
    }
    let dirs = Dirs {
        state_dir: with_state.then(|| dir.path().join("state")),
        spool_dir: dir.path().to_path_buf(),
    };
    let targets: Vec<Target> = stores
        .iter()
        .enumerate()
        .map(|(i, s)| Target {
            name: format!("t{i}"),
            store: s.clone(),
        })
        .collect();
    let policy = Policy {
        max_chain: 1 + rng.below(4) as u32,
        keep_full: rng.chance(50).then_some(2),
        ..Policy::default()
    };
    let mut expected: Vec<HashMap<String, Vec<u8>>> = vec![HashMap::new(); targets.len()];

    for step in 0..40 {
        for _ in 0..1 + rng.below(3) {
            random_op(&mut rng, &c);
        }
        // Second target joins late so its hash table differs from the first one's.
        let active: Vec<usize> = (0..targets.len())
            .filter(|&i| i == 0 || step >= 10)
            .collect();
        let job = Job {
            db: "app".into(),
            path: db.clone(),
            policy: policy.clone(),
            targets: active.iter().map(|&i| targets[i].clone()).collect(),
        };
        let force = rng.chance(5);
        let res = backup::backup(&job, &dirs, force).unwrap();
        if with_state && rng.chance(20) {
            // Unchanged database: second run must be a no-op.
            let res2 = backup::backup(&job, &dirs, false).unwrap();
            assert!(
                res2.iter().all(|(_, r)| matches!(r, Ok(Outcome::Skipped))),
                "seed {seed}: expected skip, got {res2:?} after {res:?}"
            );
        }
        let img = image(&c, &db);
        for (k, (name, r)) in res.into_iter().enumerate() {
            let r = r.unwrap_or_else(|e| panic!("seed {seed} step {step} target {name}: {e}"));
            if let Outcome::Uploaded { id, .. } = r {
                expected[active[k]].insert(id, img.clone());
            }
        }
    }

    for (ti, t) in targets.iter().enumerate() {
        let loaded = load_catalog(&*t.store, "app", None).unwrap();
        let cat = loaded.catalog;
        assert!(!cat.meta.backups.is_empty());
        if let Some(k) = policy.keep_full {
            assert!(cat.chains().len() <= k as usize);
        }
        // Every key in storage is referenced by the catalog (retention deleted the rest).
        let keys: Vec<String> = t
            .store
            .list("app/")
            .unwrap()
            .into_iter()
            .map(|o| o.key)
            .collect();
        assert_eq!(
            keys.len(),
            cat.meta.backups.len() + 1,
            "seed {seed}: orphans {keys:?}"
        );
        for e in &cat.meta.backups {
            let want = &expected[ti][&e.id];
            let dest = dir.path().join(format!("restored-{ti}.db"));
            restore::restore(
                &*t.store,
                "app",
                &cat,
                e,
                &dest,
                &Options {
                    force: true,
                    integrity_check: true,
                },
            )
            .unwrap_or_else(|err| panic!("seed {seed}: restore {} failed: {err}", e.id));
            let got = std::fs::read(&dest).unwrap();
            assert!(
                got == *want,
                "seed {seed}: restore of {} differs ({} vs {} bytes)",
                e.id,
                got.len(),
                want.len()
            );
        }
    }
}

fn fs_store(d: &tempfile::TempDir, name: &str) -> Arc<dyn Store> {
    Arc::new(FsStore::new(&d.path().join(name)))
}

#[test]
fn random_chains_restore_byte_equal() {
    let seeds: u64 = std::env::var("S4CP_PROPTEST_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(12);
    for seed in 1..=seeds {
        let d = tempfile::tempdir().unwrap();
        run_scenario(
            seed,
            vec![fs_store(&d, "a"), fs_store(&d, "b")],
            seed % 2 == 0,
        );
    }
}

#[test]
fn live_s3_chain() {
    let Some(s) = crate::s3::tests::live_store(
        "s4cp-test",
        &format!("e2e/{}", chrono::Utc::now().timestamp_nanos_opt().unwrap()),
    ) else {
        return;
    };
    let d = tempfile::tempdir().unwrap();
    run_scenario(7, vec![Arc::new(s), fs_store(&d, "b")], true);
}

#[test]
fn restore_refuses_existing_and_detects_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("app.db");
    let c = Connection::open(&db).unwrap();
    c.execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES (randomblob(100000));")
        .unwrap();
    let store: Arc<dyn Store> = Arc::new(FsStore::new(&dir.path().join("s")));
    let dirs = Dirs {
        state_dir: None,
        spool_dir: dir.path().to_path_buf(),
    };
    let job = Job {
        db: "app".into(),
        path: db.clone(),
        policy: Policy::default(),
        targets: vec![Target {
            name: "s".into(),
            store: store.clone(),
        }],
    };
    backup::backup(&job, &dirs, false).unwrap();
    let cat = load_catalog(&*store, "app", None).unwrap().catalog;
    let e = cat.latest().unwrap().clone();
    let opts = Options {
        force: false,
        integrity_check: true,
    };
    assert!(
        restore::restore(&*store, "app", &cat, &e, &db, &opts).is_err(),
        "must refuse existing file"
    );

    // Unchanged scan without prefilter state → no upload.
    let res = backup::backup(&job, &dirs, false).unwrap();
    assert_eq!(res[0].1.as_ref().unwrap(), &Outcome::Unchanged);

    // Flip one byte inside the compressed body: restore must fail and leave no temp file.
    let obj = dir.path().join("s").join(backup::object_key("app", &e));
    let mut b = std::fs::read(&obj).unwrap();
    let mid = b.len() / 2;
    b[mid] ^= 0x55;
    std::fs::write(&obj, &b).unwrap();
    let dest = dir.path().join("out.db");
    assert!(restore::restore(&*store, "app", &cat, &e, &dest, &opts).is_err());
    assert!(!dest.exists());
    assert!(!dir.path().join("out.db.s4cp-tmp").exists());
}

#[test]
fn state_is_keyed_by_target_location() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("app.db");
    let c = Connection::open(&db).unwrap();
    c.execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES (randomblob(5000));")
        .unwrap();
    let dirs = Dirs {
        state_dir: Some(dir.path().join("state")),
        spool_dir: dir.path().to_path_buf(),
    };
    // Same target name, different locations (e.g. two ad-hoc URLs); the database doesn't change in between.
    for loc in ["a", "b"] {
        let store: Arc<dyn Store> = fs_store(&dir, loc);
        let job = Job {
            db: "app".into(),
            path: db.clone(),
            policy: Policy::default(),
            targets: vec![Target {
                name: "adhoc".into(),
                store: store.clone(),
            }],
        };
        let res = backup::backup(&job, &dirs, false).unwrap();
        assert!(
            matches!(res[0].1, Ok(Outcome::Uploaded { .. })),
            "{loc}: {:?}",
            res[0].1
        );
        assert!(load_catalog(&*store, "app", None).unwrap().exists);
    }
}

#[test]
fn restore_refuses_leftover_wal() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("app.db");
    Connection::open(&db)
        .unwrap()
        .execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES (1);")
        .unwrap();
    let store: Arc<dyn Store> = fs_store(&dir, "s");
    let dirs = Dirs {
        state_dir: None,
        spool_dir: dir.path().to_path_buf(),
    };
    let job = Job {
        db: "app".into(),
        path: db.clone(),
        policy: Policy::default(),
        targets: vec![Target {
            name: "s".into(),
            store: store.clone(),
        }],
    };
    backup::backup(&job, &dirs, false).unwrap();
    let cat = load_catalog(&*store, "app", None).unwrap().catalog;
    let e = cat.latest().unwrap().clone();
    let dest = dir.path().join("out.db");
    std::fs::write(dir.path().join("out.db-wal"), b"stale").unwrap();
    let opts = Options {
        force: false,
        integrity_check: true,
    };
    let err = restore::restore(&*store, "app", &cat, &e, &dest, &opts).unwrap_err();
    assert!(err.to_string().contains("-wal"), "{err}");
    restore::restore(
        &*store,
        "app",
        &cat,
        &e,
        &dest,
        &Options {
            force: true,
            integrity_check: true,
        },
    )
    .unwrap();
    assert!(!dir.path().join("out.db-wal").exists());
}
