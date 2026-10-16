# s4cp: cheap incremental SQLite backups to S3

s4cp takes scheduled, page-level snapshots of SQLite databases and uploads them to S3-compatible storage.
It is a "dumb and cheap Litestream": there is no WAL streaming, so the recovery point is your schedule
interval. In exchange, it is a single static binary that uses about 15 MB of RAM and needs almost no
S3 requests.

- **Consistent snapshots of live databases.** Pages are read through SQLite (`sqlite_dbpage`) inside a
  read transaction, so WAL frames are included and concurrent writers never produce a torn copy.
- **Incremental.** Every page is hashed, and only changed pages are uploaded. Backups form chains: one full
  backup followed by deltas. A new full starts after `max_chain` deltas, when the deltas grow past
  `max_chain_ratio` of the full, or on an optional `full_schedule`.
- **Cheap on S3.** A backup costs 1 conditional GET and 2 PUTs. An unchanged database costs nothing,
  because a local stat check skips it without reading it.
- **Safe restores.** A restore checks every object's hashes and the whole-image xxh3-128, runs
  `PRAGMA integrity_check`, and then renames the file into place atomically.
- **Several targets per database.** Each target succeeds or fails on its own, and the database is scanned
  only once per run.

> **v3 is a rewrite.** It shares nothing with v2 (`s4cp PATH KEY`): the CLI, the key layout and the image
> tags are all new. v2 backups are plain SQLite files, so you can still download and use them directly.

## Quick start (ad hoc)

```shell
export AWS_ACCESS_KEY_ID=… AWS_SECRET_ACCESS_KEY=…
export AWS_ENDPOINT_URL=https://s3.eu-central-1.amazonaws.com   # omit for AWS us-east-1

s4cp backup ./app.db s3://my-bucket/backups          # full the first time, deltas afterwards
s4cp ls s3://my-bucket/backups/app
s4cp restore s3://my-bucket/backups/app ./restored.db
```

The database name in storage is the file stem (`app`); use `--name` to change it.

## Configuration

Point `--config` or `S4CP_CONFIG` at a YAML file; `/etc/s4cp/config.yaml` is used if present.
See [`config.example.yaml`](config.example.yaml) for every option.

```yaml
targets:
  s3:
    url: s3://my-bucket/backups
    endpoint: https://s3.eu-central-1.amazonaws.com
    access_key_id: ${AWS_ACCESS_KEY_ID}
    secret_access_key: ${AWS_SECRET_ACCESS_KEY}

databases:
  app:
    path: /data/app.db
    schedule: "*/15 * * * *"     # 5-field cron, UTC unless `timezone` is set
    keep_full: 7                 # keep the newest 7 chains
    ping_url: https://hc-ping.com/<uuid>
```

- **Precedence:** command line > environment > YAML > defaults.
- **Environment variables:** `S4CP_CONFIG`, `S4CP_LOG_LEVEL`, `S4CP_LOG_FORMAT`. `AWS_ACCESS_KEY_ID`,
  `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, `AWS_REGION` and `AWS_ENDPOINT_URL` fill in any target
  setting the YAML leaves out.
- **Interpolation:** `${VAR}` is replaced by the variable and fails if it is unset; use `${VAR:-default}`
  to give a fallback. `$$` is a literal `$`.
- **Targets:** a database is backed up to every target unless it lists `targets`. Restore, ls and verify
  use the first target listed unless you pass `--target`.
- **Addressing:** a custom endpoint uses path-style addressing by default, and AWS uses virtual-hosted.
  Set `path_style` to override either.
- **Local targets:** `file:///path` targets work too, which is handy for local copies and tests.

## Commands

| Command | What it does |
|---|---|
| `backup [DB…] [--full]` | Back up the named configured databases (all by default), or ad hoc: `backup <file> <s3://bucket/prefix>` |
| `watch` | Run every database that has a `schedule`. Overdue jobs run on start. Missed runs are skipped, not queued. |
| `restore <DB\|URL> [DEST] [--at ID\|TIME] [--target T] [--force]` | Restore the latest backup, or the latest one at or before `--at` |
| `ls [DB\|URL] [--json]` | List backups |
| `verify [DB\|URL] [--at …\|--all-chains]` | Download a backup and check its hashes and `integrity_check`, without writing a database |
| `prune [DB\|URL…] [--keep-full N] [--dry-run]` | Apply retention and delete orphaned objects older than an hour |
| `inspect <file\|s3://bucket/key> [--pages] [--json]` | Show a catalog or data object. Remote data objects need only 2 ranged GETs. |

`restore` refuses to overwrite an existing database. It also refuses when a leftover `-wal` or `-journal`
file sits next to the destination, because SQLite would replay it over the restored image.
`restore --force` replaces the file and deletes its `-wal`, `-shm` and `-journal` files. Stop the
application before you do this.

## Docker

```shell
docker run -d --name s4cp \
  --user 1000:1000 \
  -v /srv/app/data:/data \
  -v s4cp-state:/var/lib/s4cp \
  -v ./s4cp.yaml:/etc/s4cp/config.yaml:ro \
  -e AWS_ACCESS_KEY_ID -e AWS_SECRET_ACCESS_KEY \
  ghcr.io/egor-s/s4cp:3
```

The image is built on `gcr.io/distroless/static:nonroot` for amd64 and arm64, and its default command
is `watch`.

- **User:** run as a user that can write the database directory. Reading a WAL database needs its `-shm` file.
- **State volume:** `/var/lib/s4cp` holds the prefilter state and the catalog cache.

- **Volumes:** without the `/var/lib/s4cp` volume, s4cp still works. After a restart it just has to scan
  every database once more and download each catalog again.
- **Spool files:** these go to `/tmp` (`spool_dir`). They need about as much space as one compressed
  backup per target.
- **Logs:** written to stderr, as text by default or as JSON with `--log-format json`.

## How it works

Each (target, database) pair has its own storage layout:

```text
<prefix>/<db>/catalog                       source of truth: backup list + page-hash table
<prefix>/<db>/<chain-start-UTC>/<seq>.s4    immutable data objects (full = seq 000000)
```

**Backup**
1. The **prefilter** compares the database file and `-wal` (size, mtime, inode, WAL salts) with the
   last successful run for that target. If nothing changed, it stops here.
2. It fetches the catalog with a conditional GET. When the local cache is current, the server answers 304.
3. It opens a read transaction and runs `SELECT pgno, data FROM sqlite_dbpage`. Each page is hashed with
   xxh3-64 and compared with the catalog's table. Changed pages go into a zstd **spool file**, one per
   target.
4. It commits, which releases the snapshot, and then uploads. The data object is PUT with
   `If-None-Match: *` and the new catalog with `If-Match: <etag>`. If either PUT returns 412, the run for
   that target stops. The catalog is written only after its data object is stored.
5. If `keep_full` is set, retention rewrites the catalog and then deletes old chains.

**Data object format v1** (frozen at 3.0.0)
- A 64-byte header: magic, version, flags (a full bit, and a bit reserved for encryption), `page_size`,
  `page_count`, record count, chain, seq, parent seq, created time and body length.
- One zstd frame of `(pgno u32 LE, page)` records, sorted by `pgno`.
- A 32-byte trailer holding the body length and the image xxh3-128.

**Catalog format v1:** a 24-byte header, zstd-compressed JSON metadata, then the raw `u64` page hashes.
That is about 2 MB per GB of database.

**Journal modes:** WAL and rollback-journal databases are both supported. In rollback-journal mode,
writers are blocked while the scan runs. s4cp waits up to 30 s for locks and never changes
`journal_mode`.

### Provider compatibility

Conditional writes (`If-None-Match` and `If-Match` on PUT) protect against a second writer, but s4cp
assumes only one writer per (target, database). If a provider answers `501`, s4cp turns the guards off
for that target and logs a warning. Some providers ignore the headers silently; backups still work there,
but the guards cannot detect a second writer.

| Provider | Guards | Notes |
|---|---|---|
| AWS S3, Cloudflare R2, MinIO | honoured | according to provider documentation (not tested by s4cp CI) |
| versitygw, RustFS | honoured | tested in CI |
| Garage v2.1 | **silently ignored** | tested manually: both headers are accepted and have no effect |
| Backblaze B2, Hetzner | unknown | not documented by the providers |

## Building

```shell
cargo build --release      # needs a C compiler (bundled SQLite and zstd)
cargo test                 # unit and property tests
S4CP_TEST_S3_ENDPOINT=http://localhost:9000 S4CP_TEST_S3_KEY=… S4CP_TEST_S3_SECRET=… cargo test live_
```

The bundled SQLite is compiled with `SQLITE_ENABLE_DBPAGE_VTAB`; `.cargo/config.toml` sets this.
Static binaries are built with `cargo zigbuild --release --target x86_64-unknown-linux-musl`
(or `aarch64-unknown-linux-musl`). The Dockerfile uses the same toolchain.
