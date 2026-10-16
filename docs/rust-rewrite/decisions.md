# s4cp Rust rewrite — design interview & decision log

Branch: `rust-rewrite`. Started 2026-10-16.
Method: [grilling](https://github.com/mattpocock/skills/blob/main/skills/productivity/grilling/SKILL.md) —
the design tree is worked in rounds; each round asks only questions whose prerequisites are settled.

Status values: `open` · `decided` · `deferred` · `superseded`

## Design summary (confirmed by the user on 2026-10-16)

**What:** s4cp v3 is a Rust rewrite and a fresh start (D1). It is a "dumb and cheap Litestream": it takes scheduled page-level
snapshots of SQLite databases and uploads them to S3, with no WAL streaming. The recovery point is the schedule interval (D8).

**Snapshot (D9, D11, D13, D16)**
- Inside a read transaction, run `SELECT pgno, data FROM sqlite_dbpage` on bundled SQLite built with `SQLITE_ENABLE_DBPAGE_VTAB`.
  This gives a consistent snapshot that includes WAL frames, read one page at a time.
- Each page gets an xxh3-64 hash, and the whole resulting image gets an xxh3-128 hash (checked on restore).
- A change in page_size forces a full. A shrinking page_count is recorded in the delta, and restore truncates to it.
- A local stat prefilter (db/wal size + mtime + WAL salts) skips unchanged DBs without reading them. Its state is kept per (db, target)
  and advanced only for targets that succeeded (D37). A scan that finds no changes uploads nothing.
- Rollback-journal DBs are supported, but writers are blocked during the scan. `busy_timeout` is 30 s, and the tool never changes journal_mode (D29).

**Delta model & policy (D2, D15)**
- Backups form a hybrid chain: a full, then deltas, each delta relative to the previous backup.
- A new full starts on `max_chain` (24), `max_chain_ratio` (0.5) or an optional `full_schedule` cron, whichever fires first. `backup --full` forces one.

**Storage layout (D3, D10, D23, D24, D25)**
- Per (target, db): `<prefix>/<db>/catalog` and `<prefix>/<db>/<chain-start-UTC>/<seq>.s4`.
- The **catalog** is the source of truth and the index of backups: magic + version, zstd-compressed JSON backup list,
  then the raw u64 page-hash table of the latest state (about 2 MB per GB of DB).
- **Data objects** are immutable: a header (magic, version, flags with a reserved encryption bit, page_size, page_count, parent id),
  one zstd frame of sorted `(pgno u32 LE, page)` records, and a trailer holding the image xxh3-128. A full is a delta containing every page.
- A local cache keyed by the catalog's ETag means a steady-state backup costs 1 conditional GET + 2 PUTs.

**Data path & S3 (D7, D12, D14, D27)**
- Fully synchronous, with no tokio. `watch` runs jobs one at a time and skips a run if the previous run of the same job is still going.
- Scan once and write a zstd spool file per target. Commit to release the snapshot, then upload. Each spool is deleted as soon as its upload finishes,
  and stale spools are swept on start.
- Hand-written S3 client: SigV4 with `UNSIGNED-PAYLOAD` over TLS, a single PUT (multipart only above 5 GiB), ListObjectsV2, ranged GET, DELETE,
  and configurable path-style or virtual-hosted addressing.
- Concurrency: one writer per (target, db) is assumed. Guards: `If-Match` on catalog writes and `If-None-Match: *` on data objects. On a 412 the run aborts.
  On 501 the guard is turned off for that target with a warning, and providers that ignore the headers are documented.
- Each target succeeds or fails independently (D4). A request is retried up to 3 times with backoff on 5xx, 429 or network errors.
  The catalog is written only after its data PUT succeeds.

**Retention (D5, D22)**
- `keep_full: N` keeps the newest N chains. It runs after each successful backup.
- The catalog is rewritten first, then old objects are deleted. `prune` also sweeps orphans via LIST.

**Restore (D17, D28)**
- Restore writes the full and then the deltas into `<dest>.s4cp-tmp`, truncates, checks the image hash and runs `integrity_check` (on by default),
  then fsyncs and renames atomically.
- It refuses an existing file unless given `--force`. `--at <ts|id>` selects the point, and `--target` the source (default: the first target listed).

**CLI (D18, D26, D32)**
- Commands: `backup [--full]`, `restore`, `watch`, `ls`, `prune`, `verify`, and `inspect <file|s3://key> [--pages] [--json]`.
  Remote inspect needs only 2 ranged GETs.
- Ad-hoc mode without YAML: `s4cp backup ./app.db s3://bucket/prefix`.

**Config (D19, D20)**
- Precedence is CLI > env > YAML > defaults.
- The YAML has named `targets` and `databases`, with `${VAR}` interpolation.
- Env vars: `S4CP_CONFIG`, `S4CP_LOG_LEVEL`, `S4CP_LOG_FORMAT`, and `AWS_*` as defaults for targets.
- Schedules are 5-field cron in UTC, with an optional `timezone`. There is no catch-up after downtime, but a backup runs on start if one is overdue. No jitter.

**Compression/encryption (D6):** zstd is always on. Encryption is deferred, with a flag bit reserved for it.

**Ops & packaging (D21, D30, D34, D38)**
- A static musl binary on `gcr.io/distroless/static:nonroot`, built for amd64 and arm64 (D38).
- `state_dir` (default `/var/lib/s4cp`, mount a volume) holds the prefilter state and catalog ETag cache. `spool_dir` defaults to `/tmp`.
- Logs go to stderr as text, or JSON.
- Each DB can have a `ping_url` (start/success/fail).
- Budgets: binary ≤ 10 MB; RSS ≤ 64 MB plus the hash tables; tz database bundled.
- Versioning: git tag `v3.0.0`, image tags `3.0.0`/`3.0`/`3`/`latest`. On-disk format v1 is frozen at 3.0.0.

**Code (D33, D35, D36)**
- One crate with modules `cli`, `config`, `schedule`, `snapshot`, `format`, `catalog`, `s3` and `restore`.
- Crates: clap, serde, serde-saphyr, cron, chrono, chrono-tz, ureq 3, rustls (ring), webpki-roots, rusqlite (bundled), zstd, twox-hash,
  hmac, sha2, quick-xml, log, env_logger, tempfile.

**Testing (D31)**
- SigV4 test vectors.
- Property tests: random writes and VACUUMs, then a backup chain, then a restore that must be byte-equal.
- MinIO integration tests in CI.
- A `watch` run under `--memory 128m`.

**Known risks / verify before implementing**
- The ⚠ facts above (they came from the agents' prior knowledge): Garage/B2/Hetzner conditional-write behaviour and R2/B2 streaming-signature gaps.
  The D9 foundations (dbpage reads through the pager; `LIBSQLITE3_FLAGS`) are verified.
- The catalog hash table is rewritten on every backup: about 21 MB per backup for a 10 GB DB.

## Initial brief (from the user)

- Rewrite s4cp in Rust. UX: a Docker image with a CLI inside.
- Config from a YAML file, CLI arguments and env variables (env only for some generic options).
- Minimal commands (names not final): `backup` (full or delta), `restore`, `watch`, `ls`.
- `watch` is the only long-running command: it copies several SQLite files on a cron-like schedule, to one or more S3 buckets.
- Small and resource-efficient: must run on a VPS with less than 512 MB RAM.
- S3 protocol implemented by hand (no SDK).
- Instead of `.backup`, iterate over DB pages and hash each one, so incremental deltas can be produced.
- S3-ops efficient: fewer objects is better.

## Current Go implementation (baseline, v2)

- One command: `s4cp PATH KEY`. Flags/env: endpoint-url, region, bucket, access-key-id, secret-access-key, `--gz`.
- Uses the SQLite online backup API to write a temp file, gzips it to a second temp file if asked, then does a single PutObject.
- No-overwrite check is HEAD then PUT, which races.
- The key supports strftime.

## Facts gathered (by Claude)

- Memory math: a 10 GB DB with 4 KiB pages has about 2.6M pages; at 16 B per hash the index is about 42 MB.
  A 1 GB DB gives about 4 MB. It fits within 512 MB, but the index must not be duplicated per target in memory.
- SQLite facts (from the agent's prior knowledge, not live-verified; ⚠ = verify):
  - **Reading the raw `.db` file is wrong in WAL mode.** Committed pages can live only in `-wal`, and a checkpoint can
    run during our read transaction, so raw bytes are a torn mix rather than a snapshot.
  - ✅ **Verified** (sqlite.org/dbpage.html; `src/dbpage.c` reads via `sqlite3PagerGet`): `sqlite_dbpage` reads through the pager. `BEGIN; SELECT pgno, data FROM sqlite_dbpage; COMMIT` therefore gives a
    consistent snapshot that includes WAL frames. It can also *write* pages (UPDATE/INSERT), which rules out
    `SQLITE_DBCONFIG_DEFENSIVE`, and newer versions can truncate ⚠.
  - ✅ **Verified** (rusqlite `libsqlite3-sys/build.rs`, which reads `LIBSQLITE3_FLAGS` and accepts `-D…`/`SQLITE_…`): it needs `-DSQLITE_ENABLE_DBPAGE_VTAB`. rusqlite `bundled` takes `LIBSQLITE3_FLAGS="-DSQLITE_ENABLE_DBPAGE_VTAB"`
    (set via `.cargo/config.toml` `[env]`). There is no cargo feature for it.
  - Change detection: the header change counter (offset 24) is **not reliable in WAL mode**. `PRAGMA data_version` only
    works within one long-lived connection. Across runs, use the WAL header salts / checkpoint seq + `-wal` size + db size/mtime,
    or just rely on the page hashes.
  - `page_size` changes only on VACUUM, and never while in WAL mode. `page_count` can shrink (VACUUM, auto_vacuum).
  - Bundled SQLite adds roughly 0.8–1.5 MB to a musl binary.
- S3 facts (from the agent's prior knowledge, not live-verified; items marked ⚠ need a check before relying on them):
  - Single PUT is at most 5 GiB on AWS, R2, B2, MinIO, Garage and Hetzner. Multipart parts are 5 MiB–5 GiB, with at most 10,000 parts. R2 requires all parts except the last to be the same size.
  - `UNSIGNED-PAYLOAD` over HTTPS is accepted everywhere. Chunked streaming signing has gaps on R2 and B2 ⚠, so avoid it.
  - `If-None-Match: *` on PUT works on AWS (since Aug 2024), R2 and MinIO. `If-Match` works on AWS (since Nov 2024), R2 and MinIO.
    Garage is partial ⚠, and B2 and Hetzner are undocumented. **An unsupported provider may silently ignore the header.**
  - ListObjectsV2 works everywhere. Path-style suits custom endpoints, and AWS prefers virtual-hosted addressing. Make the style configurable.
  - Checksum headers (the CRC32 default in the 2025 SDKs) are optional, and omitting them is the most portable choice. DeleteObjects (batch) needs Content-MD5 on some providers.
  - The HTTP stack (ureq + rustls with ring) adds about 1–2 MB. ring cross-builds cleanly to musl x86_64 and aarch64.

## Decisions

### Round 1

| ID | Question | Recommendation | Answer | Status |
|----|----------|----------------|--------|--------|
| D1 | Clean break: new major (v3), no compatibility with the v2 CLI or key layout | Yes | Yes — start fresh; v2 repo is inspiration only, not a contract | decided |
| D2 | Delta model: chain of deltas vs cumulative (differential) against the last full | Hybrid: chain, with an auto-full after N deltas or when the chain's size exceeds X% of a full | Hybrid; policy configurable with defaults | decided |
| D3 | Source of truth for page-hash state | S3 (stored with each backup); optional local cache | Yes; idea: the same S3 file can also serve as the index of backups/deltas | decided |
| D4 | Multiple targets: independent chain and state per target | Yes | Yes | decided |
| D5 | Retention/pruning in scope for v1 | Yes, chain-aware (whole chains are deleted) | Yes | decided |
| D6 | Compression and encryption | zstd always on; encryption deferred to after v1 | Yes | decided |
| D7 | Runtime model: sync, no tokio; watch runs jobs sequentially | Yes | Yes | decided |
| D8 | Scope vs Litestream: no WAL streaming; RPO = schedule interval | Yes | Yes — "dumb and cheap Litestream", not a clone | decided |
| D9 | Snapshot mechanism: `sqlite_dbpage` inside a read txn, bundled SQLite with DBPAGE_VTAB | Yes | Yes, exactly | decided |

#### Notes per question

**D2 — delta model.** With a chain, each delta = pages changed since the *previous* backup. Uploads are the
smallest, but a restore needs 1 + k GETs, and losing one object breaks everything after it. With
cumulative, each delta = pages changed since the *last full*. A restore is always 2 GETs and there's no fragile
chain, but deltas grow until the next full. The hybrid caps the chain length and still keeps uploads small.

### Round 2

| ID | Recommended proposal | Answer | Status |
|----|----------------------|--------|--------|
| D10 | One small catalog ("manifest") per (target, db): backup list + current page-hash table; immutable data objects; 2 PUTs per backup | Yes | decided |
| D11 | Hashes: xxh3-64 per page for change detection; whole-image xxh3-128 per backup, verified on restore | Yes | decided |
| D12 | Writer concurrency: one writer per (target, db) assumed; conditional PUT (If-Match / If-None-Match) used as a guard, with a fallback when the provider doesn't support it | Yes | decided |
| D13 | page_size change → forced full; page_count shrink → recorded in the delta, restore truncates | Yes | decided |
| D14 | Data path: scan once → per-target compressed spool file → release txn → upload (single PUT, multipart only above 5 GiB) | Yes, but spool files should be cleaned up / recycled right after completion | decided |
| D15 | Full-backup policy knobs: `max_chain`, `max_chain_ratio`, optional `full_schedule` cron | Yes | decided |
| D16 | Skip unchanged: stat prefilter (db+wal size/mtime) in local state, else scan; no-change → 0 PUTs | Yes | decided |
| D17 | Restore safety: temp file → apply chain → checksum + integrity_check → atomic rename; refuse an existing file without `--force`; `--at` point selection | Yes | decided |
| D18 | Command set: `backup`, `restore`, `watch`, `ls`, `prune`, `verify` | Yes | decided |
| D19 | Config: precedence CLI > env > YAML > defaults; YAML has named `targets` + `databases`; `${VAR}` interpolation for secrets | Yes | decided |
| D20 | Schedule: 5-field cron, UTC default + optional `timezone`; no catch-up except a run-on-start if overdue | Yes | decided |
| D21 | Image & ops: static musl `FROM scratch` (webpki-roots) — base image superseded by D38, amd64+arm64; text logs (`--log-format json`); per-db optional `ping_url` (healthchecks.io-style) | Yes | decided |

#### Notes per question

**D10 — catalog vs. hash table size.** A 10 GB DB at 4 KiB pages with 8 B hashes needs a ~21 MB table, rewritten on every backup;
at 1 GB it's ~2 MB. Alternative: hashes only inside the full objects + deltas. That avoids rewriting the table, but the next delta then needs
1 + k GETs. A local cache keyed by the catalog ETag (conditional GET → 304) makes a steady-state run cost 1 GET + 2 PUTs.

**D14 — spool vs. stream.** Streaming holds the read txn open for the whole upload, so the WAL can't be checkpointed past it and grows,
and it's awkward with several targets in one sync thread. Spooling costs local disk ≈ compressed delta size × targets.

**D14 — follow-up (user).** Each spool file is deleted as soon as its upload completes (success or final failure);
spool files are created as anonymous/auto-deleting temp files where possible; `watch` sweeps stale `s4cp-*` spool files on start.

### Round 3

| ID | Recommended proposal | Answer | Status |
|----|----------------------|--------|--------|
| D22 | Retention: `keep_full: N` keeps last N chains; prune after each successful backup; catalog updated first, then objects deleted; `prune` also sweeps orphans via LIST | Yes | decided |
| D23 | Key layout: `<prefix>/<db>/catalog`, `<prefix>/<db>/<chain-ts>/<seq>.s4`; db name defaults to file stem; strftime dropped | Yes | decided |
| D24 | Data object format: fixed header (magic, version, flags, page_size, page_count, parent id) + one zstd frame of sorted `(pgno u32, page)` records + trailer (image xxh3-128) | Yes; CLI needs cheap inspection command(s) for these binaries, local and maybe remote (→ D32) | decided |
| D25 | Catalog format: magic + zstd(JSON metadata) + raw hash array, versioned | Yes; same inspection requirement (→ D32) | decided |
| D26 | Ad-hoc mode without YAML: `s4cp backup ./app.db s3://bucket/prefix` with endpoint/creds from flags/env | Yes | decided |
| D27 | Failure handling: per-target isolation; retries with backoff (3×) per request; catalog only written after data PUT succeeds | Yes | decided |
| D28 | Restore/ls/verify pick a target via `--target` (default: first one listed for that db) | Yes | decided |
| D29 | Rollback-journal DBs supported (documented: writers blocked during scan); `busy_timeout` 30 s; tool never changes journal_mode | Yes | decided |
| D30 | Budgets: stripped binary ≤ 10 MB; RSS ≤ 64 MB + hash tables; tz database bundled (scratch has none) | Yes | decided |
| D31 | Testing: unit + property tests (random writes → backup chain → restore byte-equal), MinIO integration in CI, a run under `--memory 128m` | Yes | decided |

### Round 4

| ID | Recommended proposal | Answer | Status |
|----|----------------------|--------|--------|
| D32 | `inspect <file|s3://key>`: magic-sniffs catalog vs data object; prints header/trailer (remote: 2 ranged GETs) and the backup list; `--pages` decodes the body; `--json` | Yes | decided |
| D33 | Crates: clap, serde + serde_yaml_ng, croner, jiff (bundled tzdb), ureq 3 + rustls/ring + webpki-roots, rusqlite (bundled), zstd, xxhash-rust, hmac/sha2, quick-xml, log + tiny logger | Yes, but only well-adopted crates (Claude checked crates.io, see the adoption table; revised list below) | decided |
| D34 | Repo & release: Rust replaces Go in the repo root on this branch; tags `v3.x.y`, image `ghcr.io/egor-s/s4cp:v3`; on-disk format v1 frozen at v3.0; README migration note | Use the most common scheme → git tag `v3.0.0` (v-prefixed), Cargo `3.0.0`, image tags `3.0.0`/`3.0`/`3`/`latest` (docker/metadata-action semver default; drops the old `:v2` style) | decided |
| D35 | Code layout: one crate, modules `cli`, `config`, `schedule`, `snapshot`, `format`, `catalog`, `s3`, `restore` — no workspace | Yes | decided |

#### D33 — crate adoption check (crates.io API, 2026-10-16)

| Crate | Total DL | Last 90 d | Latest | Updated | Verdict |
|---|---|---|---|---|---|
| clap | 1233M | 258M | 4.6.7 | 2026-09 | ✅ |
| serde | 1540M | 366M | 1.0.229 | 2026-07 | ✅ |
| ureq | 231M | 70M | 3.4.2 | 2026-09 | ✅ |
| rustls / ring / webpki-roots | 1009M / 810M / 825M | 236M / 184M / 215M | | 2026 / 2025-03 / 2026 | ✅ |
| rusqlite | 127M | 45M | 0.40.2 | 2026-08 | ✅ |
| zstd | 428M | 96M | 0.14.2 | 2026-10 | ✅ |
| hmac / sha2 | 657M / 1070M | 183M / 308M | | 2026 | ✅ |
| quick-xml | 474M | 138M | 0.42.0 | 2026-08 | ✅ |
| log / env_logger | 1378M / 591M | 328M / 87M | | 2026 | ✅ env_logger replaces "tiny custom logger" (custom JSON formatter via builder) |
| tempfile | 895M | 212M | 3.27.0 | 2026-03 | ✅ (spool files) |
| xxhash-rust vs **twox-hash** | 119M vs 268M | 37M vs 64M | | 2026 | → **twox-hash** 2.x (xxh3-64/128, pure Rust, more adopted) |
| croner vs **cron** | 10.6M vs 26.6M | 4.5M vs 9.1M | 4.0.1 / 0.17.0 | 2026 | → **cron** (more adopted, fewer deps; croner pulls chrono *and* jiff + derive_builder + strum) |
| jiff vs **chrono + chrono-tz** | 229M vs 886M + 173M | 87M vs 203M + 43M | | 2026 / 2025-07 | → **chrono + chrono-tz** (`cron` is chrono-based; avoid two time libs) |
| serde_yaml | 438M | 97M | 0.9.34+deprecated | 2024-03 | ⚠ deprecated/archived, still most used |
| serde_yaml_ng | 17M | 11M | 0.10.0 | 2024-05 | ⚠ stale |
| serde_yml | 25M | 5M | 0.0.13 | 2026-05 | ❌ RUSTSEC-2025-0068 (unsound) |
| serde_norway | 15M | 7M | 0.9.42 | 2024-12 | ⚠ stale |
| serde-saphyr | 11M | 8M | 2.0.0 | 2026-10 | maintained, pure Rust, fast-growing |

YAML has no option that is both well adopted and maintained → follow-up D36.

### Round 5

| ID | Recommended proposal | Answer | Status |
|----|----------------------|--------|--------|
| D36 | YAML crate → rec: serde-saphyr (maintained, pure Rust; config is a small, trusted input; trivial to swap behind `config` module) | Yes — serde-saphyr | decided |

### Round 6 (gaps found while writing the summary)

| ID | Recommended proposal | Answer | Status |
|----|----------------------|--------|--------|
| D37 | Prefilter state (D16) keyed per (db, target), advanced only for targets that succeeded, so a failed target can't be skipped forever while the DB is idle | Yes | decided |
| D38 | Base image `gcr.io/distroless/static:nonroot` (has `/tmp`, CA certs, a nonroot user; about 2 MB more than scratch); new `state_dir` setting (default `/var/lib/s4cp`) for the prefilter state + catalog ETag cache, documented as "mount a volume or lose the cache on restart (only costs a rescan + GET)"; `spool_dir` default `/tmp` | Yes | decided |

## Deferred / later rounds (blocked on the above or on facts)

- _(none — frontier empty; design summary confirmed. Implemented on 2026-10-16, see below.)_


## Implementation notes (2026-10-16)

The design was implemented as specified. Where the code differs from the text above, or where a fact was checked
during implementation, it is recorded here.

### Facts verified while building

| Fact | Result |
|---|---|
| D9 dbpage snapshot | ✅ A WAL DB whose committed pages exist only in `-wal` gives a full, consistent image. A commit made by another connection during the scan is not seen (unit test `snapshot::tests`). |
| Readers create an empty `-wal` | ⚠ New finding: opening a WAL DB that has no `-wal` creates a 0-byte one. The prefilter treats an empty WAL as absent, otherwise every run after a clean shutdown would rescan. |
| SigV4 | ✅ Four AWS documentation vectors pass: GET with Range, PUT with `$` in the key, list with query, `?lifecycle` sub-resource. |
| Conditional writes | ✅ versitygw and RustFS honour `If-None-Match: *` and `If-Match` on PUT and on CompleteMultipartUpload, and answer conditional GET with 304. ❌ **Garage v2.1.0 silently ignores all three** (it returns 200, not 501). Documented in the README. |
| ListObjectsV2 field order | ⚠ versitygw sends `<LastModified>` before `<Key>`. The parser handles each `<Contents>` element as a unit. |
| `cron` crate | ⚠ It counts days of week 1–7 starting on Sunday and has no Vixie DOM/DOW OR rule. `schedule` parses the Unix day-of-week field itself and builds two schedules when both fields are restricted. Tested. |
| MinIO in CI | ❌ `minio/minio` (Docker Hub) and `quay.io/minio/minio` can no longer be pulled. CI uses **versitygw + RustFS** instead (D31). |
| TLS (rustls + ring + webpki-roots) | ✅ Checked against real AWS S3 with fake credentials (virtual-hosted and path-style, from both the host binary and the distroless image; the result was `InvalidAccessKeyId`), against Backblaze B2 and Hetzner, and against healthchecks.io for pings. |
| Budgets (D30) | ✅ The static musl binary is 8.1 MB (x86_64). The image is 18.4 MB. Max RSS: 14 MB for a full backup of a 1 GB DB (256k pages), 13 MB for a delta, 11 MB for a restore. A `watch` container under `--memory 128m` peaked at about 13 MB. |

### Deviations and additions

- **Modules (D35):** besides the eight planned modules there are `backup` (the engine), `store` (a storage trait
  plus a `file://` directory store used by tests and for local copies) and `util`.
- **Crates (D33):** `libc` was added for the SIGTERM/SIGINT handler. It was already in the tree transitively.
  `watch` finishes the current job and exits; a second signal exits at once. Without a handler, PID 1 in a container ignores SIGTERM.
  `serde_json` was added for the catalog metadata. chrono's `serde` feature is enabled.
- **Config:** a top-level `defaults:` block holds per-database settings shared by all databases.
  Unknown keys are rejected. The default config path is `/etc/s4cp/config.yaml`.
- **Data object ids:** `<chain>/<seq:06>`. The chain id is `YYYYMMDDTHHMMSSZ`, bumped by a second when needed so it stays unique.
- **Orphaned data objects (D12/D27):** if the catalog PUT fails after the data PUT, the data object is deleted
  best-effort. If a later data PUT hits 412 on an unreferenced object older than one hour, s4cp treats it as such a
  leftover and replaces it; a newer one aborts the run as a possible concurrent writer.
  `prune` sweeps unreferenced `*.s4` objects older than one hour.
- **Retention failures** after a successful backup are logged as warnings and retried after the next backup.
- **Spool files** are named `s4cp-<pid>-*`. On startup, files left by dead PIDs (or by our own PID from an earlier
  container life) are deleted.
- **Source connection:** opened read-write without create (falling back to read-only), with `query_only` set and
  `SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE`, so s4cp never checkpoints or deletes the application's WAL.
- **Local state** lives at `<state_dir>/<target>-<hash of location>/<db>/` (prefilter.json, catalog, catalog.etag).
  It is keyed by where the target points, so another ad-hoc URL, or a target whose bucket or prefix changed, never reuses
  stale prefilter state.
- **A 404 is "missing" only for `NoSuchKey`.** `NoSuchBucket` and similar errors are configuration errors, so a mistyped
  bucket fails loudly instead of showing "no backups".
- **HTTP timeouts:** connect 15 s, response headers 120 s, and a 24 h total budget for any body. ureq's body timeout is a
  total budget, not an idle timeout.
- **Restore without `--force`** refuses when `<dest>-wal` or `<dest>-journal` exists, because SQLite would replay it.
- **Restore `--force`** also deletes the destination's `-wal`, `-shm` and `-journal`. A stale WAL would otherwise be
  replayed over the restored image.

### Not verified locally

- The multi-arch image push and the GitHub Actions run. The workflow passes actionlint, and an amd64 image was built
  and run locally. The aarch64 binary was cross-built with zig; it was not executed.
- AWS S3, R2, B2 and Hetzner behaviour. The ⚠ items for B2 and Hetzner remain open.
