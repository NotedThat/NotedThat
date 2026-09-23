# NotedThat Configuration

Every NotedThat setting can be given two ways: as an environment variable, or as the command-line
flag named after it. There are no config files and no `.env` auto-loading.

## Precedence

**Command-line flag > environment variable > default.**

A flag overrides the variable it mirrors for that one run, and leaves the deployment's own
configuration untouched. Nothing else changes: a process given no arguments — which is what a
container gets — behaves exactly as it did when variables were the only source.

```sh
# The deployment's address, from the environment
NOTEDTHAT_LISTEN_ADDR=0.0.0.0:8080 notedthat-server

# Same deployment, bound to loopback for one run
NOTEDTHAT_LISTEN_ADDR=0.0.0.0:8080 notedthat-server --listen-addr 127.0.0.1:8080
```

The flag name is the variable name with the `NOTEDTHAT_` prefix dropped, lowercased, underscores
turned into dashes: `NOTEDTHAT_S3_ENDPOINT_URL` is `--s3-endpoint-url`, and `EMBEDDING_MODEL` —
which has no such prefix — is `--embedding-model`. `notedthat-server --help` lists all of them with
their variables.

Startup diagnostics name both forms, so an error is actionable whichever one you reached for:

```
Error: configuration error: NOTEDTHAT_FS_ROOT (--fs-root) is required when NOTEDTHAT_STORAGE_BACKEND=fs
```

## Passing secrets

> **A command line is not private.** Arguments are visible to every user on the host through `ps`,
> are recorded in shell history, and are kept in `docker inspect` output for the life of the
> container. Prefer the environment variable — or a secret manager — for `--api-token`,
> `--webdav-password`, `--s3-access-key-id`, `--s3-secret-access-key`, `--qdrant-api-key`,
> `--embedding-api-key` and `--token`. The flags exist for local development and one-off runs.

`--help` never prints a credential's value. It names each setting's variable and, for
non-credentials, shows the value currently in effect; for the settings above it shows the variable
name alone.

## The one exception: `RUST_LOG`

`RUST_LOG` has no flag. It is read by `tracing-subscriber` rather than by NotedThat's own
configuration, and it is a convention shared across the Rust ecosystem, so it stays where every
other Rust program keeps it. Every other setting on this page has a flag.

Which storage variables are required depends on the selected backend — see
[Storage backend](#storage-backend).

## Required environment variables

These must be set. The server exits with a non-zero status and a descriptive error message if any
are missing or invalid.

| Variable | Flag | Type | Description | Example |
|----------|------|------|-------------|---------|
| `NOTEDTHAT_API_TOKEN` | `--api-token` | string (non-empty) | The deployment's own Bearer token — the *service token*. Accepted on every surface, bound by the manifest's rules like any credential, and the only principal that reaches `.notedthat`. Identity-provider users authenticate with their own tokens instead (see [OIDC authentication](#oidc-authentication)). | `s3cr3t-token` |
| `NOTEDTHAT_WEBDAV_USERNAME` | `--webdav-username` | string (non-empty) | HTTP Basic auth username the WebDAV surface accepts. Resolves to the same service-token principal as `NOTEDTHAT_API_TOKEN`; WebDAV also accepts `Bearer`. Required and must not be empty. | `webdav-user` |
| `NOTEDTHAT_WEBDAV_PASSWORD` | `--webdav-password` | string (non-empty) | HTTP Basic auth password for the WebDAV listener. Required and must not be empty. | (use a strong random value) |
| `NOTEDTHAT_KBS` | `--kbs` | comma-separated slugs | One or more knowledge base slugs to declare. Each slug must match `[a-z0-9-]{1,40}`. Duplicates are rejected. At least one slug is required. | `notes,scratch,work` |

## Optional environment variables

These have defaults and can be omitted.

| Variable | Flag | Type | Default | Description |
|----------|------|------|---------|-------------|
| `NOTEDTHAT_LISTEN_ADDR` | `--listen-addr` | `host:port` (SocketAddr) | `0.0.0.0:8080` | Address and port the HTTP server binds to. Use `127.0.0.1:8080` to restrict to localhost. |
| `NOTEDTHAT_METRICS_ENABLED` | `--metrics-enabled` | `true` or `false` | `false` | Serve Prometheus metrics on a second listener of their own. `true` or `false` exactly; anything else refuses startup and names the setting. Off means nothing is bound *and nothing is recorded* — enabling it later starts the counters at that restart. See [Metrics](#metrics). |
| `NOTEDTHAT_METRICS_LISTEN_ADDR` | `--metrics-listen-addr` | `host:port` (SocketAddr) | `127.0.0.1:9090` | Address the metrics listener binds to. Loopback by default, because the exposition is unauthenticated. Supplying it while metrics are off refuses startup rather than binding nothing. See [Metrics](#metrics). |
| `NOTEDTHAT_LOG_FORMAT` | `--log-format` | `pretty` or `json` | `pretty` | Log output format. `pretty` produces human-readable multi-line output. `json` produces one JSON object per log event, suitable for log aggregators. |
| `RUST_LOG` | *(none — see above)* | tracing filter string | `info,notedthat=debug` | Controls log verbosity. Uses the standard `tracing-subscriber` filter syntax. Examples: `debug`, `warn`, `info,notedthat_api_http=trace`. |
| `NOTEDTHAT_MAX_PATCHABLE_SIZE` | `--max-patchable-size` | positive integer (u64 bytes) | 104857600 (100 MiB) | Maximum object size eligible for PATCH operations, in bytes. Objects larger than this are rejected before any splice. PATCH results larger than this limit are also rejected (checked arithmetic, no allocation). Applies to PATCH only — PUT uses the router body limit. Must be ≤ 5 GiB (MAX_UPLOAD_BYTES). |
| `NOTEDTHAT_UPLOAD_TMP_DIR` | `--upload-tmp-dir` | existing writable directory | platform temporary directory | Shared private staging directory for WebDAV upload spooling and indexer snapshots. Startup validates it before opening listeners or provisioning storage. |
| `NOTEDTHAT_READY_PROBE_INTERVAL_MS` | `--ready-probe-interval-ms` | positive integer (ms) | `5000` | How often `/readyz`'s background prober checks the storage backend and Qdrant; also each probe's deadline, so keep it well above the backends' round-trip time (the in-process e2e uses `20`; a networked S3 or Qdrant needs hundreds of milliseconds at least, or every probe times out and `/readyz` sits at `503 timeout`). An outage is reported within twice this value. Must be > 0. See [Readiness](#readiness). |

## Storage backend

NotedThat keeps objects either in an S3-compatible object store or in a local directory tree.
One backend is active per process, chosen at startup.

| Variable | Flag | Required | Default | Description |
|----------|------|----------|---------|-------------|
| `NOTEDTHAT_STORAGE_BACKEND` | `--storage-backend` | No | `s3` | `s3` or `fs`. Any other value is a startup error — it is not silently defaulted, because pointing the server at the wrong store produces a deployment that looks healthy while serving nothing. |
| `NOTEDTHAT_S3_REGION` | `--s3-region` | Yes, when `s3` | — | AWS region. Required even with a custom endpoint. |
| `NOTEDTHAT_S3_ACCESS_KEY_ID` | `--s3-access-key-id` | Yes, when `s3` | — | Access key ID. No credential chain is consulted; this value is used directly. |
| `NOTEDTHAT_S3_SECRET_ACCESS_KEY` | `--s3-secret-access-key` | Yes, when `s3` | — | Secret access key for the key ID above. |
| `NOTEDTHAT_S3_ENDPOINT_URL` | `--s3-endpoint-url` | No | (AWS default) | Custom S3-compatible endpoint. Required for SeaweedFS, MinIO, Ceph, Garage and R2. |
| `NOTEDTHAT_S3_FORCE_PATH_STYLE` | `--s3-force-path-style` | No | `false` | Path-style addressing (`endpoint/bucket/key`). Set `true` for SeaweedFS, MinIO and most self-hosted stores. `true` or `false` exactly; anything else refuses startup and names the setting. |
| `NOTEDTHAT_S3_RECONCILE` | `--s3-reconcile` | No | `true` | Compare every knowledge base's bucket against the search index once at startup, re-indexing objects changed outside NotedThat. `true` or `false` exactly. The on-demand pass (`POST …/index/reconcile`) is available either way. See [S3 reconciliation](#s3-reconciliation). |
| `NOTEDTHAT_FS_ROOT` | `--fs-root` | Yes, when `fs` | — | Absolute path of the storage root. |
| `NOTEDTHAT_FS_METADATA` | `--fs-metadata` | No | `sidecar` | Where per-object metadata is kept. `sidecar` is the only accepted value today. |
| `NOTEDTHAT_FS_FILE_MODE` | `--fs-file-mode` | No | `0644` | Octal mode for created object files. |
| `NOTEDTHAT_FS_DIR_MODE` | `--fs-dir-mode` | No | `0755` | Octal mode for created directories. |
| `NOTEDTHAT_FS_ALLOW_LOSSY_NAMES` | `--fs-allow-lossy-names` | No | `false` | Start even on a filesystem that folds case or normalizes Unicode. See the warning below. |
| `NOTEDTHAT_FS_WATCH` | `--fs-watch` | No | `true` | Watch the tree and re-index objects changed outside NotedThat. See below. |
| `NOTEDTHAT_FS_WATCH_DEBOUNCE_MS` | `--fs-watch-debounce-ms` | No | `500` | How long a file must go quiet before a change to it is acted on. Accepted range `50`–`60000`. |

### Settings belonging to the unselected backend are rejected

Supplying `NOTEDTHAT_FS_ROOT` or `--fs-root` while the `s3` backend is selected — including by
leaving the selector unset — refuses startup rather than ignoring the setting, and the error names
every conflicting one at once:

```
Error: configuration error: NOTEDTHAT_STORAGE_BACKEND (--storage-backend) is fs, but these
settings belong to the s3 backend and would be ignored: NOTEDTHAT_S3_REGION (--s3-region),
NOTEDTHAT_S3_ACCESS_KEY_ID (--s3-access-key-id). Unset them or set
NOTEDTHAT_STORAGE_BACKEND=s3 to start the server.
```

The alternative is an operator who believes their notes are on a disk that nothing is reading.
The check is over resolved values, so a flag counts exactly as a variable does.

**An empty value is still a value** — both `NOTEDTHAT_S3_REGION=` and `--s3-region ""` count as
supplied, matching how removed settings are checked. Note this if you pass variables through
Compose with the `${VAR-}` form.

`AWS_*` variables are never considered: the S3 client uses the credentials given above and never
consults the ambient credential chain, so ambient AWS variables have no effect either way.

## Events backend

NotedThat can publish every object change as an event and stream it to subscribers over
`GET /api/v1/knowledgebases/{kb_slug}/events` ([API.md](API.md#get-apiv1knowledgebaseskb_slugevents)).
Where the log lives is chosen at startup, and selected the same way the storage backend is:
strictly, with the unselected backends' variables refused rather than ignored.

| Variable | Flag | Required | Default | Description |
|----------|------|----------|---------|-------------|
| `NOTEDTHAT_EVENTS_BACKEND` | `--events-backend` | No | `none` | `none`, `memory` or `nats`. Any other value is a startup error. With `none`, nothing is published and the events route answers `404`. |
| `NOTEDTHAT_EVENTS_MEMORY_CAPACITY` | `--events-memory-capacity` | No | `10000` | With `memory`: how many events the ring keeps for replay. A `Last-Event-ID` older than the oldest retained event answers `410`. |
| `NOTEDTHAT_NATS_URL` | `--nats-url` | Yes, when `nats` | — | NATS server URL, `nats://[user:pass@]host:4222`. Credentials travel in the URL, so `--help` hides this value. |
| `NOTEDTHAT_NATS_STREAM` | `--nats-stream` | No | `notedthat-events` | The JetStream stream holding the log. Created on startup if absent; a stream by this name that captures other subjects refuses startup. Letters, digits, `_` and `-`. |
| `NOTEDTHAT_NATS_MAX_AGE_SECS` | `--nats-max-age-secs` | No | `604800` (7 days) | How long the stream retains an event. Changing it updates the existing stream on the next start. A subscriber resuming from a position that has aged out answers `410`. |

**`memory`** is a process-local ring: replay survives a subscriber's reconnect but not a server
restart, and two replicas each have their own log. That makes it the right choice for
development and for any single-process deployment — including every `fs` deployment, which is one
process per root by construction. Ids are a counter that starts again at 1 on every start; a
client that reconnects after a restart with a `Last-Event-ID` from before it answers `410` and
must resync by listing, since the changes made while the server was down are exactly what it
would otherwise miss.

**`nats`** is one JetStream stream shared by every replica: ids are the stream sequence, so they
are strictly increasing across replicas and a client reconnecting to any replica with
`Last-Event-ID` receives exactly what it missed. The server publishes to
`notedthat.events.<kb>.<written|deleted>` and needs a JetStream-enabled server (`nats-server
-js`). At startup an unreachable broker refuses to start; at runtime a lost connection turns
`/readyz` into `503` and each write into `503 backend_unavailable` with `Retry-After: 5` after
the bytes are stored, so the client's retry publishes the event. The Compose overlay
`docker-compose.events.yml` runs a broker beside the server.

**A write that stored its bytes but could not publish its event fails the request** with
`503 backend_unavailable` and `Retry-After: 5`, on the HTTP API and on WebDAV alike, and logs
`EVENT_PUBLISH_FAILED` with the knowledge base and key. This mirrors indexer backpressure below:
the write is idempotent, the client is the retry mechanism, and delivery is at least once. A
change the `fs` watcher detects has no client to hand a `503` to; a failed publish there is logged
and the change is found again by the next comparison of the tree against the index.

The rejection of unselected settings works as for storage, and groups the offenders by the
backend that owns them:

```
Error: configuration error: NOTEDTHAT_EVENTS_BACKEND (--events-backend) is unset, so the default
none backend is selected, but these settings belong to the nats backend and would be ignored:
NOTEDTHAT_NATS_URL (--nats-url). Unset them or set NOTEDTHAT_EVENTS_BACKEND=nats to start the
server.
```

## Metrics

NotedThat can export Prometheus metrics. They are off by default, and when they are on they are
served on a **second listener** — not on `NOTEDTHAT_LISTEN_ADDR`.

| Variable | Flag | Required | Default | Description |
|----------|------|----------|---------|-------------|
| `NOTEDTHAT_METRICS_ENABLED` | `--metrics-enabled` | No | `false` | `true` or `false` exactly. Any other value is a startup error. |
| `NOTEDTHAT_METRICS_LISTEN_ADDR` | `--metrics-listen-addr` | No | `127.0.0.1:9090` | Where the metrics listener binds. Refused unless metrics are enabled. |

**A separate listener, and loopback by default**, because the exposition is unauthenticated: there
is no credential a scraper presents and no principal the exporter could evaluate. Putting
`/metrics` on the product listener would mean everything that reaches NotedThat also reaches a
description of its traffic, held off only by a path rule somebody could get wrong. A separate
socket makes that boundary a property of the bind. `GET /metrics` on `NOTEDTHAT_LISTEN_ADDR` is
`404`, with or without a credential, and every path other than `/metrics` on the metrics listener
is `404` too.

To be scraped from another host, bind it there deliberately —
`NOTEDTHAT_METRICS_LISTEN_ADDR=0.0.0.0:9090` — and keep it behind whatever your network already
uses to keep operator surfaces operator-only. A container is its own network namespace, so inside
Compose or Kubernetes `0.0.0.0:9090` with no published port is reachable by the scraper and by
nothing outside; `docker-compose.metrics.yml` does exactly that.

**Nothing is recorded while metrics are off.** The instrumented code records through the `metrics`
facade, whose macros are a no-op until a recorder is installed, and no recorder is installed unless
`NOTEDTHAT_METRICS_ENABLED=true`. There is no accumulated history to collect after the fact; the
counters begin at the restart that enabled them.

**Setting the address while metrics are off refuses startup**, naming both settings, rather than
binding nothing and leaving an operator to discover from an empty dashboard that their scrape
target never existed:

```
Error: configuration error: NOTEDTHAT_METRICS_ENABLED (--metrics-enabled) is unset, so metrics are
off, but NOTEDTHAT_METRICS_LISTEN_ADDR (--metrics-listen-addr) would be ignored: no metrics
listener is opened. Unset it or set NOTEDTHAT_METRICS_ENABLED=true to start the server.
```

As everywhere else, **an empty value is still a value**: `NOTEDTHAT_METRICS_LISTEN_ADDR=` counts as
supplied, and `NOTEDTHAT_METRICS_ENABLED=` is refused rather than read as off.

### What the exposition carries, and what it never does

Labels are bounded by construction: a knowledge base slug (bounded by `NOTEDTHAT_KBS`), a surface,
an HTTP method, a route *template*, a status, an operation, an outcome, a backend name, a phase.
**Never** an object key, a principal, a credential, a search query, a request id, a URL path or a
subscriber's filters. Two reasons, both operational: an exposition is retained for months by
whoever scrapes it and read by people who were granted nothing under your manifests' access rules,
and an unbounded label is how a Prometheus falls over. `tests/metrics_labels_e2e.rs` puts named
sentinel values into a running server — a unique object key, the service token, the WebDAV
principal and password, a search phrase, a subscriber's prefix — exercises writes, a deletion, a
search, an index failure, a subscriber and a queue-full refusal, and asserts every one of them is
absent from the scraped text.

Request duration is time to **response head**, not to last byte. The events route returns at once
and then streams for hours, so measuring body completion would put hour-long observations in the
histogram and pin the in-flight gauge at the subscriber count. A live stream's cost is
`notedthat_events_subscribers`; a large read's backend cost is `notedthat_storage_*`.

A storage call that answers `404`, `304` or `412` is counted as an outcome, not an error: those are
how an idempotent delete and every conditional request work, and counting them would make the error
rate track client caching rather than the backend's health. The vector store is treated the same
way — a missing collection is `outcome="not_found"`, which both reconcilers handle by skipping the
pass. What is left is the backend being unreachable, so that is what to alert on:

```promql
rate(notedthat_storage_operations_total{outcome="unavailable"}[5m]) > 0
rate(notedthat_vector_store_operations_total{outcome="unavailable"}[5m]) > 0
```

`outcome="cancelled"` is the third kind: the caller gave up before the call answered — an
abandoned search, a connection dropped mid-`GET`. It is recorded because the alternative is
losing those calls entirely, and losing them selectively: a dropped future records nothing after
its await, and the calls most likely to be abandoned are the slow ones, so the duration histograms
would go quiet exactly when a backend is degrading. It is not a fault on its own — clients give
up for their own reasons — but a rising share of it beside rising latency is the same story as
`unavailable`, told from the caller's side.

### Metric catalogue

The full table is in `SPECIFICATIONS.md` §6.15. The families are: HTTP requests
(`notedthat_http_*`), search (`notedthat_search_*`), embedding (`notedthat_embedding_*`), the index
queue and worker (`notedthat_index_*`), the vector store (`notedthat_vector_store_*`), object
change events (`notedthat_events_*`), reconciliation (`notedthat_reconcile_*`), the filesystem
watcher (`notedthat_fs_watch_lost_total`), storage (`notedthat_storage_*`) and
`notedthat_build_info`.

### Scraping it

```yaml
scrape_configs:
  - job_name: notedthat
    static_configs:
      - targets: ["notedthat-server:9090"]
```

Search P95 over five minutes, which is what §7.3's latency target is to be set from:

```promql
histogram_quantile(0.95, sum by (le) (rate(notedthat_search_duration_seconds_bucket[5m])))
```

`sum by (le, kb)` breaks it down per knowledge base. `docker-compose.metrics.yml` runs a Prometheus
with this scrape config already pointed at the server.

## Filesystem storage backend

With `NOTEDTHAT_STORAGE_BACKEND=fs`, an object's key is its path under the root:

```
$NOTEDTHAT_FS_ROOT/
  .notedthat.lock                 process lock
  .notedthat-meta/                per-object metadata, outside every knowledge base
  nt-default-notes/               one directory per knowledge base
    notes/hello.md                the object `notes/hello.md`
    .notedthat/manifest.json      the knowledge base manifest
```

The tree is meant to be read. Open it in an editor, `grep` it, `rsync` it, put it under version
control. Nothing but objects appears inside a knowledge base directory — metadata and the lock
live above them.

The knowledge base directories themselves are created at startup and are expected to stay. Remove
one while the server runs and that knowledge base answers `404 not_found` on every surface — reads,
listings and writes alike; a write that finds it gone refuses rather than recreating it — until the directory is put back or the
server is restarted, which provisions it again. This is the same answer the `s3` backend gives for a
bucket deleted out from under it.

**Editing files in place works.** An object changed outside the server is detected on the next
request and its `ETag` recomputed from content, so clients are never served stale validators — and
the search index keeps up as well. NotedThat watches each knowledge base's directory and re-indexes
what changes, so a note edited in an editor, restored by `git`, or copied in by a script becomes
searchable shortly afterwards. Every knowledge base is also compared against the index once at
startup, which is how changes made while the server was not running are picked up.

Almost all of that costs nothing when nothing has changed: an object already indexed from exactly
the bytes on disk is recognised and left alone, so a startup comparison over an unchanged knowledge
base reads no file content and sends nothing to the embedding endpoint.

### What is watched, and what is not

The watcher deliberately looks at less than the API will store, because it sees files nobody asked
NotedThat to hold:

- Paths containing a `.git`, `.svn` or `.hg` component are ignored, so a knowledge base kept under
  version control does not re-index on every commit.
- Symbolic links are not followed and their targets are not indexed, matching how the backend
  already refuses to serve a symlink as an object.
- Everything under `.notedthat/`, including the manifest, stays private.
- Reading an object is never treated as changing it, so `grep -r` over the tree costs nothing.

Known limits:

- A file being written continuously — an in-place `rsync` of a large one — may be indexed from
  partial content and corrected on a later pass.
- Hardlinked objects are indexed under the name that was written, not under other names for the
  same file.
- On a filesystem needing `NOTEDTHAT_FS_ALLOW_LOSSY_NAMES`, watching inherits the same
  key-collision hazard.
- Changing `EMBEDDING_MODEL` or `EMBEDDING_DIMENSIONS` still re-indexes nothing. Recognising
  unchanged content does not repair vectors built by a different model — drop the Qdrant collection
  to rebuild.
- Watching is supported on Linux and macOS. Other platforms compile but are untested; on BSD,
  kqueue needs one file descriptor per file, so a large tree will exhaust `kern.maxfiles`. Set
  `NOTEDTHAT_FS_WATCH=false` there.

### Operating it

Linux keeps one watch per **directory** — not per file — so a knowledge base of 100,000 notes in
500 directories costs 500 watches. If the tree is deep enough to exceed `fs.inotify.max_user_watches`
(often 8192, sometimes 65536), the server refuses to start and says so, naming both the limit and
the number of watches it needed. Raise the limit, or set `NOTEDTHAT_FS_WATCH=false` to serve without
watching:

```console
$ sudo sysctl -w fs.inotify.max_user_watches=524288
```

Two log codes on the `notedthat::watch` target are worth alerting on:

| Code | Meaning |
|------|---------|
| `FS_WATCH_LOST` | A watch could not be kept. Changes below newly created directories may go unnoticed until the next comparison. Usually the watch limit. |
| `FS_WATCH_RESCAN` | Events were dropped — by the kernel, or because more changed at once than was worth tracking individually — so the affected knowledge base is being compared against the index in full. Informational unless it repeats. |

Both are also visible without the log: while a rescan is pending the knowledge base reports
`"state": "stale"` at `GET /api/v1/knowledgebases/{kb}/index`, and every completed pass is
its `last_reconcile` (see [the API's state model](API.md#get-apiv1knowledgebaseskb_slugindex)).

Turning watching off with `NOTEDTHAT_FS_WATCH=false` restores the older behaviour, where only
writes through the API, WebDAV or MCP update the search index.

### S3 reconciliation

An S3 bucket can change through anything that speaks S3 — `aws s3 cp`, a sync job, another
application — and S3 has no change feed NotedThat can subscribe to portably, so unlike the `fs`
backend nothing is watched. Instead the `s3` backend **compares** each knowledge base's bucket
against the search index (D67):

- **at startup**, for every declared knowledge base, while `NOTEDTHAT_S3_RECONCILE` is `true`
  (the default); until that pass completes the knowledge base reports `"state": "stale"` at
  `GET /api/v1/knowledgebases/{kb}/index`;
- **on demand**, whenever the service token `POST`s
  `/api/v1/knowledgebases/{kb}/index/reconcile` — `202`, and the result lands in `/index`'s
  `last_reconcile` ([the API's description](API.md#post-apiv1knowledgebaseskb_slugindexreconcile)).

What a pass costs: one `ListObjectsV2` per thousand keys (each entry carries the object's
`ETag`), one scroll of the index, and then a re-read and re-embed of only the objects whose
`ETag` differs or which the index lacks. Objects gone from the bucket are removed from the
index. An unchanged object is neither fetched nor embedded, so a pass over an up-to-date bucket
reads no content. One pass per knowledge base runs at a time; a second request while one runs
answers `409 conflict` — retry once `last_reconcile.at` moves.

While a large pass is enqueueing, the indexing queue (1024 events) fills and writes through
the API, WebDAV and MCP answer `503` with `Retry-After` until it drains — the same as during the
`fs` startup pass. Schedule a requested pass accordingly.

**What a pass costs in memory.** A pass holds both sides of the comparison resident for its
whole duration: every key and `ETag` the bucket listing returned, and every key and `ETag` the
index holds. Budget roughly **150 bytes per object**, counted once per knowledge base being
compared — so about 150 MiB for a bucket of a million objects, and low single-digit GiB at ten
million. The startup pass walks every declared knowledge base one after another, so the ceiling
is the largest single knowledge base rather than their sum; a requested pass is one knowledge
base. Size the container accordingly, or run the largest buckets with
`NOTEDTHAT_S3_RECONCILE=false` and request passes when the memory is available.

```console
$ aws s3 cp meeting.md s3://nt-default-notes/meeting.md
$ curl -X POST -H "Authorization: Bearer $NOTEDTHAT_API_TOKEN" \
       http://localhost:8080/api/v1/knowledgebases/notes/index/reconcile
{"kb_slug":"notes","status":"started"}
```

Two log codes on the `notedthat::reconcile` target are worth alerting on:

| Code | Meaning |
|------|---------|
| `S3_RECONCILE_SKIPPED` | The index could not be read for this knowledge base, so nothing was compared and nothing enqueued; it stays `stale`. Usually the search collection is gone (provisioned at startup and dropped since — restart to re-provision it). |
| `S3_RECONCILE_INCOMPLETE` | The bucket could not be listed, so the pass stopped before comparing; nothing enqueued, the knowledge base stays `stale`. Fix the bucket or the credentials, then request a pass. |

With `NOTEDTHAT_S3_RECONCILE=false`, only writes through the API, WebDAV or MCP update the
search index until the operator asks for a pass.

**One process per root.** Conditional writes (`If-Match`, `If-None-Match`) are made atomic by an
in-process lock, so a second server on the same root would reintroduce the lost writes that
[§8.1 of the specification](../SPECIFICATIONS.md) records against backends without a consensus
mechanism. The server takes an exclusive lock on `$NOTEDTHAT_FS_ROOT/.notedthat.lock` at startup
and refuses to start if another process holds it. For the same reason, **network filesystems
(NFS, SMB) are not supported** — their advisory locking is unreliable.

**The filesystem must preserve names byte-for-byte.** On a case-folding filesystem (macOS APFS
and Windows NTFS by default) the keys `Foo.md` and `foo.md` become one file, and writing either
destroys the other; a Unicode-normalizing filesystem does the same to composed and decomposed
spellings. Startup probes for both and refuses rather than lose a note silently. Use a
case-sensitive filesystem, or set `NOTEDTHAT_FS_ALLOW_LOSSY_NAMES=true` to accept the risk.

**Durability and backups are yours.** The root is the only copy; NotedThat does not replicate it.
Size it for the knowledge bases plus growth, and back it up like any other data directory —
an ordinary file-level backup is sufficient and restores to a working store.

**Permissions.** Objects are created `0644` and directories `0755`, so the tree is readable by a
backup job or a person. Adjust with `NOTEDTHAT_FS_FILE_MODE` and `NOTEDTHAT_FS_DIR_MODE`. In the
container image the server runs as uid **10001**, so a bind-mounted root must be writable by that
uid; a named volume avoids the question.

**Two keys a filesystem cannot hold at once.** S3 allows an object `a/b` alongside `a/b/c`; a
filesystem cannot make `a/b` both a file and a directory, so the second write is refused with an
error naming the conflict. This is the one place the two backends genuinely differ.

## Knowledge base description

A knowledge base can say what it is for. The manifest's optional `description` is shown, with the
`display_name`, by `GET /api/v1/knowledgebases` and the MCP `list_knowledgebases` tool, so an agent
can choose where to search before it searches. `NOTEDTHAT_KBS` stays a list of slugs; the words
live in the manifest, next to the access rules:

```json
{
  "display_name": "Engineering notes",
  "description": "Design notes, ADRs and meeting minutes of the platform team.",
  "access": [ … ]
}
```

The description is plain metadata written by an operator — never derived from the objects — and
it is validated at startup like the access rules: one line (no newlines, tabs or other control
characters), not blank, at most 500 characters. A manifest that breaks those limits stops the
server booting with a message naming the problem; a manifest without the field is unchanged.

To set it, edit the manifest and restart (policies and descriptions are one startup snapshot):

- `fs` backend: edit `$NOTEDTHAT_FS_ROOT/nt-default-<kb>/.notedthat/manifest.json` in place (the
  [directory layout](#filesystem-storage-backend) above).
- `s3` backend: fetch the manifest, edit it, and put it back with the service token — it alone
  reaches `.notedthat`:

  ```sh
  curl -H "Authorization: Bearer $TOKEN" \
       http://localhost:8080/api/v1/knowledgebases/notes/.notedthat/manifest.json > manifest.json
  # add "description": "…"
  curl -X PUT -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
       --data-binary @manifest.json \
       http://localhost:8080/api/v1/knowledgebases/notes/.notedthat/manifest.json
  ```

  The write checks what startup would check — valid JSON, the limits above, the access rules,
  and that the manifest names this knowledge base — and answers `400 invalid_request` with the
  same message a refused boot would print, so a typo is found here and not by whoever restarts
  the server next. The check lives in the shared write path, not in this route: a `PATCH`, a
  `POST …/replace/`, a WebDAV `PUT`, `COPY` or `MOVE` onto the key (`400`), and so every MCP
  tool that writes, are refused the same way. What is stored still takes effect at the next
  restart.

A description is shown exactly when its knowledge base is listed, so it follows the same
visibility rule as everything else: a caller who holds no grant in a knowledge base is told nothing
about it.

## Manifest access rules

Access is configured in each knowledge base's `.notedthat/manifest.json`, not with an environment
variable. One knowledge base is one bucket, and that bucket is the policy boundary.

```json
"access": [
  { "who": "anyone",        "may": ["list", "read"], "under": ["public/**"] },
  { "who": "anyone",        "may": ["search"] },
  { "who": "signed-in",     "may": ["list", "read", "search"] },
  { "who": "group:editors", "may": ["write", "delete"] },
  { "who": "group:interns", "may_not": ["read", "search"], "under": ["hr/**"] }
]
```

Each rule names **who** it applies to, **what** verbs it grants (`may`) or revokes (`may_not`), and
**where**:

| Subject | Who it is |
| --- | --- |
| `anyone` | A caller supplying no credential |
| `signed-in` | Any caller whose credential verified: `NOTEDTHAT_API_TOKEN`, the WebDAV Basic credential, and every identity-provider user |
| `group:<name>` | An identity-provider user whose token places them in group `<name>` |
| `user:<name>` | An identity-provider user whose username claim is exactly `<name>` |

`group:` and `user:` rules match only callers authenticated through an OIDC provider (see
[OIDC authentication](#oidc-authentication)). The configured token is in no group and has no
username, so it is never matched by them; a manifest naming such a rule on a deployment without an
OIDC issuer starts, and logs `ACCESS_RULES_IDENTITY_WITHOUT_OIDC` naming the knowledge base.

| Verb | HTTP | WebDAV |
| --- | --- | --- |
| `list` | `GET /api/v1/knowledgebases/{kb_slug}` | `PROPFIND` |
| `read` | `GET`, `HEAD` on an object | `GET`, `HEAD` |
| `write` | `PUT`, `PATCH`, `POST .../replace/...` | `PUT`, `MKCOL`, `COPY`, `MOVE` destination |
| `delete` | `DELETE` | `DELETE`, `MOVE` source |
| `search` | `POST /api/v1/knowledgebases/{kb_slug}/search` | Not applicable |

A rule carries exactly one of `may` and `may_not`. Private by default; a verb is allowed on a key
when some matching `may` rule covers the key **and no matching `may_not` rule does** — deny
overrides, and both sides are unions, so the order of the rules never changes a decision. Omit
`under` to scope a rule to the whole knowledge base. A `may_not` scoped to the whole knowledge base
also removes the base from the caller's listings for that verb.

Pattern syntax: `*` matches within one segment and never `/`; `**` matches whole segments and must
be an entire segment; `?` matches one non-`/` character; `{a,b}` alternates. So `public/*` grants
the files directly in `public/`, and `public/**` grants everything beneath it as well. There is no
escape character, so a key containing a literal `*`, `?` or `{` cannot be matched.

A knowledge base shows up in `GET /api/v1/knowledgebases`, in the WebDAV root and on `/browse/` when
the caller holds any grant in it. There is no separate discovery capability to enable.

### Things worth knowing before you write one

**Verbs are independent, and `search` is filtered by its own patterns rather than by `read`.** A
broad `search` grant with a narrow `read` grant returns object paths, heading paths and preview text
for keys the caller cannot fetch. Previews are content — if you configure that combination, you are
publishing excerpts.

**The rules bind the credential holder too.** A manifest can narrow what `NOTEDTHAT_API_TOKEN` may
do. That is useful — `{"who": "signed-in", "may": ["list", "read", "search"]}` gives you a read-only
deployment, MCP included — and it means a mistake can lock you out of your own knowledge base.

**The way back in.** `.notedthat` is not addressable by any rule — naming it in `under` refuses
startup. `NOTEDTHAT_API_TOKEN` always reaches it, and nobody else ever does: not `anyone`, and not
an identity-provider user however broad their grants, because the manifest carries the policy, group
names included. So a manifest that revokes everything else is still repairable with that token:

```sh
curl -X PUT -H "Authorization: Bearer $TOKEN" \
  --data-binary @fixed-manifest.json \
  "http://localhost:8080/api/v1/knowledgebases/notes/.notedthat%2Fmanifest.json"
# then restart the server — policies are a startup snapshot
```

**Anonymous writes are refused at startup, not silently ignored.** A rule granting — or revoking —
`write` or `delete` for `anyone` stops the server booting with a message naming the rule. So does a
rule with an empty `may`/`may_not`, one naming both, or one naming neither.

**An empty `access: []` grants nobody anything.** The knowledge base becomes inert — reachable only
through the `.notedthat` repair path above — and startup logs `ACCESS_RULES_EMPTY` naming it.

**A manifest with no `access` field** means the credential holder may do everything and anonymous
callers nothing. That is what every manifest written before this model already meant, so upgrading
changes nothing for credentialed access.

### Upgrading from `public_read`

The `public_read` array is removed. A manifest still carrying it parses, the field is ignored, and
**the knowledge base comes up private to anonymous callers**. Nothing in the server warns about
this. If you had published a knowledge base, translate it before upgrading:

| Old capability | New equivalent |
| --- | --- |
| `discover` | No equivalent — visibility follows from holding any grant |
| `browse` | `{"who": "anyone", "may": ["list"]}` |
| `content` | `{"who": "anyone", "may": ["read"]}` |
| `search` | `{"who": "anyone", "may": ["search"]}` |

The new model can also do what the old one could not: scope any of those to part of the knowledge
base with `under`.

### Operational notes

Policies are validated and loaded once during startup provisioning. The server does not watch
manifests or hot-reload them: edit the manifest through your storage administration workflow, then
restart. Keeping authorization off the storage path is deliberate — a policy decision never waits on
a bucket.

Authorization failures answer `403` when a valid credential is not granted, and `404` both when the
knowledge base is not declared and when an anonymous caller is refused — the two are deliberately
indistinguishable, body included, so the status cannot be used to enumerate private knowledge bases
or prefixes. `/browse` and `/api/v1` agree on this. `401` is reserved for a credential that is
missing where one is unconditionally required or that failed to verify; the one exception is
`GET /api/v1/knowledgebases`, which names no knowledge base and so has no existence to conceal. See
*Authorization failures* in `docs/API.md` for the trade-off this accepts.

MCP acts as whoever called it: the bearer presented to `/mcp` — the service token or an identity
token — is the one its API calls carry, so a tool call inherits that principal's rules, including
any restriction placed on them.

There are no built-in rate or burst settings. Before enabling anonymous `search`, set rate and burst
controls at the reverse proxy for that route **and for `/mcp`**, tuned to the capacity of the
embedding and search backends: an anonymous MCP `search` with `kb` omitted fans out to every
knowledge base anonymous callers may see, up to eight at a time. Do not add an application
configuration variable for this control.

`/healthz`, `/readyz` and `/llms.txt` are globally public.

## OIDC authentication

NotedThat mints no tokens of its own. Point it at an OpenID Connect issuer and it accepts that
issuer's signed JWT access tokens as bearers, on every surface: the HTTP API, WebDAV, the browse
pages and `/mcp`. The token's username claim becomes the caller's subject and its groups claim
becomes the caller's groups, which is what `user:` and `group:` rules in a manifest match. Supported
and documented providers: [Authentik](#authentik), [Authelia](#authelia) and [Zitadel](#zitadel).
Any issuer that publishes discovery and a JWKS and can mint JWT access tokens works the same way.

| Variable | Flag | Type | Default | Description |
|----------|------|------|---------|-------------|
| `NOTEDTHAT_OIDC_ISSUER` | `--oidc-issuer` | `http(s)` URL | *(unset — identity tokens refused)* | The issuer, spelled **exactly** as the provider spells its `iss` claim, trailing slash included. Setting it turns identity tokens on; discovery runs at `{issuer}/.well-known/openid-configuration` during startup. |
| `NOTEDTHAT_OIDC_AUDIENCE` | `--oidc-audience` | comma-separated strings | *(required with the issuer)* | The audiences a token may carry; one of them must match its `aud`. Usually the client id the provider registered for NotedThat, plus the id of any MCP client that obtains tokens with `resource` set to this server. |
| `NOTEDTHAT_OIDC_USERNAME_CLAIM` | `--oidc-username-claim` | claim name | `preferred_username` | The claim `user:<name>` rules match, and what logs identify a caller by. Falls back to `sub` when the claim is absent. |
| `NOTEDTHAT_OIDC_GROUPS_CLAIM` | `--oidc-groups-claim` | claim name | `groups` | The claim `group:<name>` rules match. Its value may be an array of strings, a single string, or an object whose keys are the group names (Zitadel's roles shape). Absent or unreadable means "in no group". |
| `NOTEDTHAT_OIDC_HTTP_TIMEOUT_MS` | `--oidc-http-timeout-ms` | positive integer | `5000` | Timeout for the discovery and key-set requests to the issuer. |
| `NOTEDTHAT_OIDC_RESOURCE` | `--oidc-resource` | `http(s)` URL | *(unset — nothing published)* | This deployment's public URL. When set, the server publishes RFC 9728 metadata at `/.well-known/oauth-protected-resource` and names it in a `WWW-Authenticate: Bearer resource_metadata="…"` challenge on every `401` from `/api/v1` and `/mcp`, which is how an MCP client finds the authorization server. |
| `NOTEDTHAT_OIDC_CA_CERT` | `--oidc-ca-cert` | path to a PEM bundle | *(unset — public roots only)* | Extra CA certificates to trust when reaching the issuer, on top of the built-in Mozilla roots. A self-hosted provider is usually behind an internal or self-signed CA, and the server does not read the operating system's trust store. The file must exist at startup; a bundle that is not PEM, or holds no certificate, refuses startup. |

Any `NOTEDTHAT_OIDC_*` setting other than the issuer, with the issuer unset, refuses startup rather
than being ignored: a deployment that set an audience believed it had configured identity tokens.

### How a token is checked

The bearer is compared with `NOTEDTHAT_API_TOKEN` first, in constant time; only something that is
not the service token is treated as an identity token. It must be a JWT signed with `RS256`,
`RS384`, `RS512`, `ES256` or `ES384` by a key the issuer publishes — never `HS*`, since the server
shares no secret with the issuer. `iss` must equal the configured issuer, `aud` must contain one of
the configured audiences, `exp` is required and `nbf` honoured, both with 60 seconds of leeway. A
token that fails any of this is `401`, never anonymous, and the reason — never the token — is
logged at `debug`.

Keys are fetched at startup and cached by `kid`. A token with an unknown `kid` triggers a refetch,
at most once every 30 seconds, so a key rotation is picked up without a restart and a flood of
bogus tokens is not a flood of requests to the issuer. A cached set older than an hour is refreshed
before the next use. A refetch that fails keeps the previous keys and logs a warning.

**Only JWTs are accepted.** There is no introspection call, so a provider that mints opaque access
tokens by default has to be told to mint JWTs — the per-provider notes below say how. Configure the
access token, which is what a client is meant to present to an API. The check above is all the
verifier does, though: it does not look at the `typ` header, so an ID token minted for the
configured audience — the client id, in the setups below — verifies exactly like an access token.
Treat an ID token for that audience as a bearer credential for this API, not only as proof of
login for the client that received it.

### What an identity can and cannot do

An identity-provider user is `signed-in`, so every `signed-in` rule applies to them, plus every
`group:` rule naming one of their groups and every `user:` rule naming their subject; a `may_not`
rule removes what the others gave. What no rule can give them is `.notedthat`: the manifest carries
the policy, group names included, and only `NOTEDTHAT_API_TOKEN` reads or writes it.

MCP acts as the caller. The bearer presented to `/mcp` is the bearer the server's own API call
carries, so a tool call is bound by exactly the rules a direct request would be.

### MCP clients

An MCP client that supports OAuth (Claude Code, Cursor, VS Code, the MCP Inspector) discovers the
authorization server from the `401` challenge and the metadata document, then runs the
authorization-code flow with PKCE against it. None of the supported providers offers dynamic
client registration, so register a public client for the MCP client on the provider, allow its
redirect URI (the client documents it — `http://127.0.0.1:<port>/callback` or similar), and give
the client that id. Add the id to `NOTEDTHAT_OIDC_AUDIENCE` if the provider puts the client id in
`aud` (Authentik and Authelia do). Set `NOTEDTHAT_OIDC_RESOURCE` to the URL the client connects to,
scheme and host exactly as it will use them.

The `401` is the trigger, and on a deployment where some knowledge base grants `anyone` a verb
there is none: `/mcp` admits a request with no credential as the anonymous caller
([D59](../SPECIFICATIONS.md#2-decisions-log)), so an OAuth client that connects without a token
is served the public knowledge bases and is never prompted to sign in. Either sign the client in
explicitly (Claude Code: `/mcp` → *Authenticate*) or set `NOTEDTHAT_MCP_ANONYMOUS=never`, which
keeps the `401` on `/mcp` — and gives up anonymous MCP — while the HTTP API, WebDAV and the browse
pages keep honouring the `anyone` rules. See [MCP HTTP listener](#mcp-http-listener).

### Authentik

1. **Provider** → *OAuth2/OpenID Provider*. Client type *Confidential* for a server-side client, or
   *Public* for an MCP client. Note the client id. Signing key: the RS256 certificate (the default
   *authentik Self-signed Certificate* is fine). Access tokens are JWTs by default.
2. **Scopes**: the built-in `openid`, `profile` and `email` mappings. `profile` carries
   `preferred_username` and `groups` (the user's group names), so the defaults for both claim
   settings work.
3. **Application** bound to the provider; the slug decides the issuer.
4. Settings:

```sh
NOTEDTHAT_OIDC_ISSUER=https://auth.example.com/application/o/notedthat/   # note the trailing slash
NOTEDTHAT_OIDC_AUDIENCE=<client id>
```

Verify by decoding an access token (`jwt.io` or `cut -d. -f2 | base64 -d`): `iss` must equal the
setting byte for byte, `groups` must be present.

### Authelia

Three things about Authelia (4.39) matter here, and each was found by running it rather than
reading about it:

- It serves OIDC **only over https** — a plain-http discovery request is refused outright — so a
  local or internal deployment needs `NOTEDTHAT_OIDC_CA_CERT` pointing at whatever signed its
  certificate.
- Access tokens are **opaque unless the client sets `access_token_signed_response_alg`**, and
  `groups` reaches the access token **only through a claims policy**.
- A JWT access token carries **no `aud` unless the client is allowed an audience and requests
  it**; `requested_audience_mode: implicit` requests it on every call. Without this the token is
  refused for a missing `aud`.

In `configuration.yml`:

```yaml
identity_providers:
  oidc:
    claims_policies:
      notedthat:
        access_token:
          - groups
          - preferred_username
    clients:
      - client_id: notedthat
        client_secret: '<pbkdf2 hash>'
        access_token_signed_response_alg: RS256   # a JWT rather than an opaque token
        audience: [notedthat]                     # what `aud` may carry …
        requested_audience_mode: implicit         # … and ask for it every time
        claims_policy: notedthat
        scopes: [openid, profile, groups]
        redirect_uris: [...]
        authorization_policy: two_factor
```

Settings:

```sh
NOTEDTHAT_OIDC_ISSUER=https://auth.example.com      # Authelia's issuer has no trailing slash
NOTEDTHAT_OIDC_AUDIENCE=notedthat
NOTEDTHAT_OIDC_CA_CERT=/etc/notedthat/internal-ca.pem   # if the certificate is not publicly trusted
```

The issuer Authelia writes into a token is derived from the request's host, so the server and
every client must reach it by the same name and port. `docker-compose.auth.yml` runs this exact
shape against Authelia's file user backend; `docker/authelia/configuration.yml` is the working
configuration and the [manual QA script](manual-qa/oidc-mcp.sh) walks it. The in-repo
`oidc_authelia_e2e` test (Docker, `--ignored`) runs the same flow.

### Zitadel

1. **Project** → *Settings*: enable *Assert Roles on Authentication*, and *Check authorization on
   authentication* if only users holding a role may sign in. Define the roles you will name in
   manifests.
2. **Application** in the project: type *API* or *Web*, **Auth Token Type: JWT** — the default is
   opaque. Note the client id.
3. Zitadel carries roles, not groups, under a claim named
   `urn:zitadel:iam:org:project:roles` whose value is an object keyed by role name. The client must
   request the scope of the same name. Settings:

```sh
NOTEDTHAT_OIDC_ISSUER=https://example.zitadel.cloud
NOTEDTHAT_OIDC_AUDIENCE=<project id>,<client id>     # Zitadel lists both in aud
NOTEDTHAT_OIDC_GROUPS_CLAIM=urn:zitadel:iam:org:project:roles
```

Rules then name roles: `{ "who": "group:editor", "may": ["write"] }`.

### Startup log lines

- `OIDC issuer discovered` — with the issuer, the JWKS URL and the number of keys, at `info`.
- `OIDC key set refreshed` / `OIDC key set refresh failed; keeping the previous keys`.
- `ACCESS_RULES_IDENTITY_WITHOUT_OIDC` — a manifest names a `group:` or `user:` rule and no
  issuer is configured; the rule can never match, and the base is named. Not a refusal, because
  manifests live in buckets that outlive one deployment's configuration.
- `MCP_ANONYMOUS enabled` / `MCP_ANONYMOUS disabled_no_anonymous_grants` /
  `MCP_ANONYMOUS disabled_by_setting` — whether `/mcp` admits a request with no credential, with
  the mode and whether any manifest grants `anyone` something. See
  [MCP HTTP listener](#mcp-http-listener).

## Upload and index staging directory

`NOTEDTHAT_UPLOAD_TMP_DIR` selects one private directory used by both WebDAV uploads and
the background indexer. If it is unset, NotedThat uses the platform temporary directory.
The server refuses startup when the selected path is missing, is not a directory, or is not
writable; this happens before listener binding and storage provisioning.

Size the selected filesystem for at least **5 GiB per concurrently accepted maximum-size upload**,
plus space for the corresponding index snapshot, backend working space, and image/build needs.
The default temporary directory is sufficient when it has that capacity; set the variable to use
a specific directory. A `tmpfs` consumes host RAM and can turn a burst of uploads into memory
pressure or an out-of-memory kill.

Each staged file is created privately. Successful uploads, failures, and cancelled requests clean
up their temporary files automatically. A forced `SIGKILL` prevents cleanup, so operators should
periodically inspect an otherwise idle staging directory for orphaned files after unclean stops.

OKF metadata recognition reads at most 16 MiB of frontmatter. Unusually large frontmatter falls
back to raw Markdown indexing rather than requiring more staging memory.

## Shutdown behaviour

NotedThat performs a staged graceful shutdown when it receives SIGTERM or SIGINT:

1. **The unified listener stops accepting new connections and finishes accepted work** — Axum
   graceful shutdown completes API, WebDAV, and MCP requests on `NOTEDTHAT_LISTEN_ADDR` before
   the bounded indexer drain begins.
2. **Indexer drain (up to 31 seconds)** — once the listeners have completed, the background indexer
   worker is signalled to stop and given up to 31 seconds to flush its queue. Any events not
   processed within this window are abandoned.

Size a container's `terminationGracePeriodSeconds` (Kubernetes) or `stop_grace_period` (Docker
Compose) for the longest allowed in-flight request plus the 31-second indexer drain and operational
margin. The bundled Compose configuration uses `45s`, leaving 14 seconds beyond the drain budget;
deployments that permit longer requests must configure a larger grace period. Standalone Docker
users can set the equivalent timeout with `docker run --stop-timeout 45`.

## WebDAV operational notes

### WebDAV PROPFIND on large knowledge bases

WebDAV `PROPFIND` on large knowledge bases walks the storage cursor server-side, making multiple paginated requests to the storage backend before returning a single `207 Multi-Status` response.

**Reverse-proxy timeout:** For knowledge bases with more than 1 000 objects, set your reverse proxy's read timeout to at least 120 seconds:

- **nginx:** `proxy_read_timeout 120s;`
- **Traefik:** `readTimeout = "120s"` in the service configuration
- **Caddy:** `read_timeout 120s` in the reverse proxy directive

**v1 safety cap (`PROPFIND_MAX_ENTRIES = 10 000`):** To avoid memory exhaustion and proxy timeouts on very large knowledge bases, PROPFIND is capped at 10 000 objects per response. This is a hardcoded v1 operational hedge — it is not a correctness guarantee for knowledge bases larger than 10 000 objects.

When a PROPFIND would return more than 10 000 objects, the server instead returns:

```
HTTP 507 Insufficient Storage
Content-Type: application/xml; charset=utf-8

<?xml version="1.0" encoding="utf-8"?>
<D:error xmlns:D="DAV:" xmlns:nt="urn:notedthat:error">
  <nt:propfind-too-large/>
</D:error>
```

The `<nt:propfind-too-large/>` element uses a custom XML namespace URI `urn:notedthat:error` (this is used as an XML namespace identifier only, not a formal IANA-registered URN per RFC 8141). This is compliant with RFC 4918 §17, which requires that new WebDAV condition elements live outside the `DAV:` namespace.

When the cap is hit, the server also logs `PROPFIND_TRUNCATED` so operators are not silently surprised.

**Recommended action for clients receiving 507:**
- Split the knowledge base into smaller units (each under 10 000 objects), or
- Use the HTTP cursor API (`GET /api/v1/knowledgebases/{kb_slug}?cursor=...`) for programmatic access to large knowledge bases.

Post-v1 versions may raise or remove the cap.

## Example: local development with SeaweedFS

Copy this block and export the variables in your shell, or save it as `.env` and load it with
`direnv` or `source .env`.

```sh
NOTEDTHAT_API_TOKEN=dev-token-please-change
NOTEDTHAT_KBS=notes,scratch
NOTEDTHAT_LISTEN_ADDR=127.0.0.1:8080
NOTEDTHAT_S3_ENDPOINT_URL=http://127.0.0.1:8333
NOTEDTHAT_S3_REGION=us-east-1
NOTEDTHAT_S3_ACCESS_KEY_ID=any
NOTEDTHAT_S3_SECRET_ACCESS_KEY=any
NOTEDTHAT_S3_FORCE_PATH_STYLE=true
NOTEDTHAT_WEBDAV_USERNAME=webdav-user-please-change
NOTEDTHAT_WEBDAV_PASSWORD=webdav-pass-please-change
RUST_LOG=info,notedthat=debug
```

> **Note:** This file is for reference only. The server does **not** auto-load `.env` files. Export
> these variables manually or use a tool like [direnv](https://direnv.net/) to load them
> automatically when you enter the project directory.

The same configuration as one command, with nothing exported:

```sh
notedthat-server \
  --api-token dev-token-please-change \
  --kbs notes,scratch \
  --listen-addr 127.0.0.1:8080 \
  --s3-endpoint-url http://127.0.0.1:8333 \
  --s3-region us-east-1 \
  --s3-access-key-id any \
  --s3-secret-access-key any \
  --s3-force-path-style \
  --webdav-username webdav-user-please-change \
  --webdav-password webdav-pass-please-change
```

`--s3-force-path-style` takes no value when you mean `true`; write `--s3-force-path-style=false` to
turn it off for a run where the variable sets it. The same applies to `--fs-allow-lossy-names`.

## Example: AWS S3

```sh
NOTEDTHAT_API_TOKEN=<your-api-token>
NOTEDTHAT_KBS=notes
NOTEDTHAT_S3_REGION=eu-west-1
NOTEDTHAT_S3_ACCESS_KEY_ID=<your-access-key-id>
NOTEDTHAT_S3_SECRET_ACCESS_KEY=<your-secret-access-key>
```

No endpoint URL or path-style override needed for real AWS S3.

## Example: single node, no object store

```sh
export NOTEDTHAT_STORAGE_BACKEND=fs
export NOTEDTHAT_FS_ROOT=/srv/notedthat
export NOTEDTHAT_API_TOKEN=change-me
export NOTEDTHAT_KBS=notes
export NOTEDTHAT_WEBDAV_USERNAME=webdav-user
export NOTEDTHAT_WEBDAV_PASSWORD=change-me
export NOTEDTHAT_QDRANT_URL=http://127.0.0.1:6334
# plus the four EMBEDDING_* variables
```

Do not also set `NOTEDTHAT_S3_*`; see
[Settings belonging to the unselected backend are rejected](#settings-belonging-to-the-unselected-backend-are-rejected).

## Readiness

`/healthz` says the process is alive; `/readyz` says it can do its job. Every
`NOTEDTHAT_READY_PROBE_INTERVAL_MS` (default `5000`) a background task probes the storage
backend and Qdrant, each probe bounded by that same interval, and publishes the result;
`/readyz` reads the latest result and never probes on request, so an orchestrator can poll it
as often as it likes. An outage is reported within twice the interval, recovery within one,
and neither needs a restart. The body names each check, its backend, a status of `ok`, `degraded` or `unavailable`,
and — when it is not `ok` — one of `timeout`, `unreachable`, `not_found` or `disconnected`; the
backend's own error goes to the log (`READINESS_LOST` or `READINESS_DEGRADED`, once per failure,
and `READINESS_RESTORED`), never to the unauthenticated response. See [`docs/API.md`](API.md#get-readyz) for the body.

What each probe is:

- **`s3`** — `HeadBucket` on the bucket of the knowledge base whose slug sorts first
  (`NOTEDTHAT_KBS` is held sorted, so `notes,archive` probes `archive`). It needs
  `s3:ListBucket` on that bucket, which `ListObjectsV2` already requires, so no new grant in
  practice. Some S3-compatible stores answer `403` rather than `404` for a bucket that does not
  exist; that is reported as `unreachable` rather than `not_found`, and is still a `503`.
- **`fs`** — a stat of that same knowledge base's directory under `NOTEDTHAT_FS_ROOT`.
  It cannot see a read-only remount or a full disk; readiness means reachable, not writable.
- **`qdrant`** — the `HealthCheck` RPC, through the same channel and API key as every other call.
- **events** — the broker's connection state, when an events backend is configured.

A `not_found` on `storage` means the bucket or directory that existed at startup has since been
deleted; nothing re-creates it while the process runs, so restart to re-provision. It is
`degraded`, not `unavailable`: the backend answered, so it is up and the other knowledge bases
keep serving, and `/readyz` stays `200` rather than pulling the replica out of the load balancer
for a data problem a restart fixes. Not covered:
the `fs` change watcher (a lost watch is [`FS_WATCH_LOST`](#operating-it) and a rescan), the
embedding endpoint, and how fresh the index is — that is per knowledge base, at
`GET /api/v1/knowledgebases/{kb_slug}/index` (see [`docs/API.md`](API.md#get-apiv1knowledgebaseskb_slugindex)).

## Startup validation

The server validates all configuration before binding to any port or reaching any backend. If a
required variable is missing, empty, or invalid, the process exits immediately with a non-zero
status code and prints a descriptive error to stderr. For example:

```
Error: NOTEDTHAT_API_TOKEN (--api-token) is required
Error: NOTEDTHAT_KBS (--kbs) must declare at least one knowledge base
Error: configuration error: NOTEDTHAT_S3_REGION (--s3-region) is required
Error: NOTEDTHAT_LISTEN_ADDR (--listen-addr) is invalid: invalid socket address syntax
Error: invalid KB slug "My Notes": slugs must match [a-z0-9-]{1,40}
Error: duplicate KB slug in NOTEDTHAT_KBS (--kbs): "notes"
Error: configuration error: NOTEDTHAT_STORAGE_BACKEND (--storage-backend) is invalid: expected "s3" or "fs", got "filesystem"
Error: configuration error: NOTEDTHAT_FS_ROOT (--fs-root) is required when NOTEDTHAT_STORAGE_BACKEND=fs
Error: configuration error: NOTEDTHAT_FS_ROOT (--fs-root) must be an absolute path, got 'data'
Error: configuration error: NOTEDTHAT_EVENTS_BACKEND (--events-backend) is invalid: expected "none", "memory" or "nats", got "kafka"
Error: configuration error: NOTEDTHAT_NATS_URL (--nats-url) is required
Error: configuration error: NOTEDTHAT_NATS_MAX_AGE_SECS (--nats-max-age-secs) is invalid: expected a positive number of seconds, got "7d"
```

Each names the environment variable and the flag that overrides it, because which one you used is
not knowable from inside the check.

With the filesystem backend, the storage root is checked and claimed straight after the staging
directory, before any backend client is built:

```
Error: failed to claim NOTEDTHAT_FS_ROOT: configuration error: NOTEDTHAT_FS_ROOT does not exist: /srv/notedthat
Error: failed to claim NOTEDTHAT_FS_ROOT: configuration error: the storage root /srv/notedthat is already in use by another notedthat-server process (PID 4213)
```

Provisioning is part of the same pass: every declared knowledge base's bucket, manifest and
Qdrant collection is ensured before any listener binds, and any of them failing exits non-zero
with the knowledge base and the setting to look at:

```
Error: failed to provision the qdrant collection for knowledge base 'notes' via NOTEDTHAT_QDRANT_URL (--qdrant-url): …
```

Under Docker Compose the server is `restart: unless-stopped`, so a Qdrant that is still coming up
costs a restart or two rather than a server that serves without a search collection.

This fail-fast behavior means misconfigured deployments fail loudly at startup rather than
silently misbehaving at runtime.

WebDAV credentials (`NOTEDTHAT_WEBDAV_USERNAME` and `NOTEDTHAT_WEBDAV_PASSWORD`) are required and
must not be empty strings. Setting either to an empty string is treated the same as leaving it unset
and causes a non-zero exit before any listener binds.

The OIDC settings are checked in the same pass, and the issuer is contacted before any listener
binds:

```
Error: NOTEDTHAT_OIDC_ISSUER (--oidc-issuer) is unset, so identity tokens are not accepted, but NOTEDTHAT_OIDC_AUDIENCE (--oidc-audience) is set; set the issuer or unset it
Error: NOTEDTHAT_OIDC_AUDIENCE (--oidc-audience) is required when NOTEDTHAT_OIDC_ISSUER (--oidc-issuer) is set: name the audience the provider puts in its tokens, usually the client id
Error: failed to reach NOTEDTHAT_OIDC_ISSUER (--oidc-issuer): GET https://auth.example.com/.well-known/openid-configuration: error sending request
Error: failed to reach NOTEDTHAT_OIDC_ISSUER (--oidc-issuer): https://auth.example.com/.well-known/openid-configuration reports issuer `https://auth.example.com/` but NOTEDTHAT_OIDC_ISSUER is `https://auth.example.com`; they must match exactly, trailing slash included
```

A manifest access rule scoped to `.notedthat`, naming both `may` and `may_not`, or naming neither
also refuses startup, with a message naming the rule's subject.

With `NOTEDTHAT_EVENTS_BACKEND=nats`, the broker is contacted before any listener binds, and the
stream is created or checked:

```
Error: failed to reach NOTEDTHAT_NATS_URL (--nats-url): could not connect to NATS: failed to connect to NATS server
Error: failed to reach NOTEDTHAT_NATS_URL (--nats-url): JetStream stream notedthat-events exists with subjects ["orders.>"], not ["notedthat.events.>"]; point NOTEDTHAT_NATS_STREAM at a stream NotedThat owns
```

### Removed variables

Three settings from the era of separate listeners no longer exist. The server refuses to start
while any of them is supplied, naming the replacement, rather than ignoring them. Their flags are
still accepted by the parser, and hidden from `--help`, so that reaching for one gets the same
explanation rather than "unexpected argument":

| Removed variable | Replacement |
|---|---|
| `NOTEDTHAT_WEBDAV_LISTEN_ADDR` | WebDAV is always served at `/webdav` on `NOTEDTHAT_LISTEN_ADDR` |
| `NOTEDTHAT_MCP_HTTP_BIND` | MCP HTTP is always served at `/mcp` on `NOTEDTHAT_LISTEN_ADDR` |
| `NOTEDTHAT_MCP_HTTP_ENABLED` | MCP HTTP is always served at `/mcp` on `NOTEDTHAT_LISTEN_ADDR` |

```
Error: NOTEDTHAT_MCP_HTTP_ENABLED was removed: MCP HTTP is always served at /mcp on NOTEDTHAT_LISTEN_ADDR. Unset NOTEDTHAT_MCP_HTTP_ENABLED to start the server.
```

Each of these encoded a decision about which network surface was reachable. Silently ignoring one
would widen exposure on upgrade — a WebDAV listener bound to `127.0.0.1` becoming reachable at
`/webdav` on a public address, or a disabled MCP transport becoming mounted at `/mcp`. Unset the
variable to acknowledge the new layout; a startup failure is a five-second fix, an unnoticed
exposure change is not. See the upgrade notes in [API.md](API.md).

## What's not configurable in M2

- **Tenant slug:** Hardcoded to `"default"`. There is no `NOTEDTHAT_TENANT_SLUG` variable and no flag.
- **Upload buffer sizes:** Fixed at 16 MiB. Configurable buffer sizes are planned for a later
  release.
- **Rate limits:** No built-in per-client or global rate limiter. Operators exposing anonymous
  search must configure rate and burst controls at their reverse proxy.
- **TLS:** The server speaks plain HTTP. Terminate TLS at a reverse proxy (Traefik, nginx, Caddy).
- **Multiple service tokens:** There is one `NOTEDTHAT_API_TOKEN`. Per-person credentials come
  from an OIDC provider (see [OIDC authentication](#oidc-authentication)), not from a second token.
- **Token introspection and browser login:** Only signed JWT bearers are accepted; there is no
  introspection call for opaque tokens and no session cookie for `/browse`. Put a forward-auth
  proxy in front of `/browse` if people need to sign in with a browser.
- **Multiple processes over one filesystem root:** The `fs` backend supports exactly one server
  process per `NOTEDTHAT_FS_ROOT`, enforced by a lock at startup. See
  [Filesystem storage backend](#filesystem-storage-backend).

---

## Qdrant

NotedThat uses [Qdrant](https://qdrant.tech/) for vector search indexing (M4+). Qdrant v1.15.2 or later is required for server-side `qdrant/bm25` sparse inference.

| Variable | Flag | Required | Default | Description |
|---|---|---|---|---|
| `NOTEDTHAT_QDRANT_URL` | `--qdrant-url` | Yes | | Qdrant gRPC endpoint (e.g. `http://127.0.0.1:6334`) |
| `NOTEDTHAT_QDRANT_API_KEY` | `--qdrant-api-key` | No | | API key for authenticated Qdrant instances |
| `NOTEDTHAT_QDRANT_TIMEOUT_MS` | `--qdrant-timeout-ms` | No | `30000` | Per-RPC timeout in milliseconds. Must be > 0. |
| `NOTEDTHAT_QDRANT_CONNECT_TIMEOUT_MS` | `--qdrant-connect-timeout-ms` | No | `10000` | Connection-establishment timeout in milliseconds. Must be > 0. |

> **Why the timeout is set explicitly.** `qdrant-client` defaults to **5 seconds
> for every RPC**, and NotedThat did not override it. That is too tight for this
> workload: a full embedding batch upserted with `wait(true)`, against a collection
> that is still building payload indexes, on a busy host, can exceed it. When it
> does, the failure surfaces as an opaque `Cancelled: Timeout expired` — which
> reads like a Qdrant fault rather than a deadline — and the document silently
> goes unindexed. Raise `NOTEDTHAT_QDRANT_TIMEOUT_MS` further if you run large
> batches on slow storage.

**Example**:

```env
NOTEDTHAT_QDRANT_URL=http://127.0.0.1:6334
# NOTEDTHAT_QDRANT_API_KEY=your-key-here  # optional
```

---

## Embedding

NotedThat uses an external OpenAI-compatible embedding endpoint to index markdown content (M4+). Indexing is **async best-effort** — see [Indexing behavior](#indexing-behavior) below.

| Variable | Flag | Required | Default | Description |
|---|---|---|---|---|
| `EMBEDDING_ENDPOINT_URL` | `--embedding-endpoint-url` | Yes | | Base URL of the OpenAI-compatible endpoint (e.g. `https://api.openai.com`) |
| `EMBEDDING_MODEL` | `--embedding-model` | Yes | | Model name (e.g. `text-embedding-3-small`, `voyage-3`, `BAAI/bge-m3`) |
| `EMBEDDING_API_KEY` | `--embedding-api-key` | Yes | | Bearer token / API key for the endpoint |
| `EMBEDDING_DIMENSIONS` | `--embedding-dimensions` | Yes | | Output vector dimensions. Must match the model's actual output and is baked into the Qdrant collection at first provisioning. |
| `EMBEDDING_BATCH_SIZE` | `--embedding-batch-size` | No | `32` | Number of text chunks per HTTP embedding request |
| `EMBEDDING_TIMEOUT_MS` | `--embedding-timeout-ms` | No | `30000` | Per-request HTTP timeout (milliseconds) |
| `EMBEDDING_MAX_RETRIES` | `--embedding-max-retries` | No | `3` | Number of retry attempts on HTTP 429 or 5xx responses |
| `EMBEDDING_MAX_INPUT_TOKENS` | `--embedding-max-input-tokens` | No | `8192` | Chunks exceeding this character count are dropped (with a WARN log) rather than truncated |

### Examples

**OpenAI** (`text-embedding-3-small`, 1536 dimensions):

```env
EMBEDDING_ENDPOINT_URL=https://api.openai.com
EMBEDDING_MODEL=text-embedding-3-small
EMBEDDING_API_KEY=sk-...
EMBEDDING_DIMENSIONS=1536
```

**Voyage AI** (`voyage-3`, 1024 dimensions):

```env
EMBEDDING_ENDPOINT_URL=https://api.voyageai.com
EMBEDDING_MODEL=voyage-3
EMBEDDING_API_KEY=pa-...
EMBEDDING_DIMENSIONS=1024
```

**Self-hosted TEI** (Text Embeddings Inference, `BAAI/bge-m3`, 1024 dimensions):

```env
EMBEDDING_ENDPOINT_URL=http://tei:80
EMBEDDING_MODEL=BAAI/bge-m3
EMBEDDING_API_KEY=any          # TEI doesn't require a key; set to any value
EMBEDDING_DIMENSIONS=1024
```

### Changing the embedding model

Changing `EMBEDDING_MODEL` or `EMBEDDING_DIMENSIONS` after initial provisioning will cause the server to fail at startup with a `ManifestMismatch` error. Re-indexing after a model change requires:

1. Stop the server
2. Delete the Qdrant collection(s) manually
3. Update env vars
4. Restart — collections are re-provisioned automatically

---

## Indexing behavior

Indexing in NotedThat (M4+) is **async best-effort** per design decision D38:

- Writes commit to S3 first, then enqueue an index event. Search results may be **stale** briefly after a write.
- Queue capacity is fixed at **1024 events** in v1 (not configurable).
- If the queue is full, the object is stored to S3 but the write returns HTTP 503 `backend_unavailable` with `Retry-After: 5` and `INDEX_QUEUE_FULL` is logged. The client should retry to re-enqueue the indexing event.
- **Conditional writes under backpressure: retry semantics interact with 412.** A conditional `PUT`/`DELETE` using `If-Match` or `If-None-Match` can complete the S3 mutation and then return HTTP 503 because the indexer queue is full. A naive retry with the same conditional headers may then return HTTP 412 `precondition_failed` because the object now exists or its ETag changed. Clients that use conditional headers must treat a 503 → 412 sequence as a possible stored-but-not-indexed ghost state and either accept that state or use a stronger consistency mechanism; v1 does not automatically replay or repair it.
- If Qdrant is unreachable during indexing, `INDEXING_FAILED` is logged and the write still succeeds. The next write of the same object re-enqueues automatically.
- On graceful shutdown (SIGTERM), the server drains the queue with a **31-second bounded timeout** after in-flight listener work completes.

No search endpoint or MCP search tool is exposed in M4 — search arrives in M5.

See [SPECIFICATIONS.md](../SPECIFICATIONS.md) §6.4 (embeddings), §6.11 (startup provisioning), §6.12 (indexing queue) for full details.

### PATCH memory model

PATCH loads the full object into RAM to perform the splice. At the peak of step 9 (splice construction), three buffers coexist per worker:

1. **Current object bytes** fetched via GET — up to `NOTEDTHAT_MAX_PATCHABLE_SIZE`
2. **Request body** buffered by axum — up to `NOTEDTHAT_MAX_PATCHABLE_SIZE`
3. **Spliced result** under construction — up to `NOTEDTHAT_MAX_PATCHABLE_SIZE`

The **rough upper bound on PATCH memory footprint per worker is `~3 × NOTEDTHAT_MAX_PATCHABLE_SIZE`**. Under high concurrency, aggregate worst-case RAM is approximately:

```
worker_concurrency × 3 × NOTEDTHAT_MAX_PATCHABLE_SIZE
```

**Rejected patches** (result would exceed the size cap) are detected via checked arithmetic before allocating the third buffer, so a flood of oversized PATCH requests cannot exhaust 3× RAM per worker — rejected patches peak at `~2× NOTEDTHAT_MAX_PATCHABLE_SIZE`.

Operators should lower `NOTEDTHAT_MAX_PATCHABLE_SIZE` or add a concurrency-limit layer around the PATCH route when running with limited memory. See `SPECIFICATIONS.md D46` for the full concurrency contract.

---

## Indexer backpressure

**HTTP 503 on write operations**
If write requests return HTTP 503 with `"error": "backend_unavailable"`, the internal indexing queue is full. The object was successfully stored; only the search index update is delayed. Clients should retry with exponential backoff. Repeated 503s indicate the embedder or Qdrant is processing slower than the write rate — investigate embedder throughput and Qdrant ingestion latency. Queue capacity is fixed in v1; tuning is post-v1.

DELETE 503 semantics:
- The object IS deleted from S3 (storage)
- The Qdrant index still contains the object
- Search will return the object until either (a) the client retries DELETE, or (b) a later re-index operation

MOVE 503 comes in two flavors; the response body distinguishes which failure occurred and what the client should do:
- Destination index event failed: the destination object IS stored, the destination search-index upsert is missing, and the source is unchanged. Retry MOVE to re-enqueue the destination index event; the destination write is idempotent.
- Source tombstone failed: the destination object IS stored, the source object IS deleted from S3, and the source search-index tombstone is missing. Search may return stale entries for the source path whose object_key now 404s. Retrying the whole MOVE will 404 on GET(src); on `s3`, a `POST …/index/reconcile` (D67) clears the stale source entry, since the object is gone from the bucket; on `fs`, the watcher's next pass does.

A repeated DELETE or MOVE-tombstone 503 leaves a stale entry the next reconciliation pass removes (on `s3`, request one with `POST …/index/reconcile`; on `fs`, the watcher's pass). Clients SHOULD still implement retry-with-backoff for DELETE, matching PUT.

Conditional writes under backpressure: retry semantics interact with 412. Conditional writes (`If-Match`, `If-None-Match`) that succeed at S3 but return 503 at the indexer queue leave a naive retry in a state where S3 may return 412 because the object now exists or its ETag changed. Clients using conditional headers MUST detect the 503 → 412 sequence and either accept the ghost state or use a stronger consistency mechanism.

The 503 response carries `Retry-After: 5` as a hint (not a guarantee). All three write surfaces (HTTP API, WebDAV, MCP-via-HTTP) surface this the same way: HTTP 503, error code `backend_unavailable`, and (for HTTP API + WebDAV) `Retry-After: 5`.

**Event publish failure** takes the same shape. With an events backend configured, a write whose bytes are stored but whose change event could not be published (the broker is down) answers HTTP 503 `backend_unavailable` with `Retry-After: 5` and a message beginning `object stored; change event not published` or `deleted from storage; change event not published`. The indexing event was already enqueued. Retry the idempotent write and the event is published; a retry may publish the same change twice, which is the at-least-once contract subscribers are asked to handle. The server logs `EVENT_PUBLISH_FAILED` with the knowledge base and key, and `/readyz` reports `503` while the broker is unreachable. See [Events backend](#events-backend).

---

## MCP HTTP listener

NotedThat's MCP surface is streamable HTTP, always mounted at `POST /mcp` on the single
`NOTEDTHAT_LISTEN_ADDR` listener, alongside the API at `/api/v1` and WebDAV at `/webdav`. There
is no stdio binary; a client that can only spawn a command is bridged with `mcp-remote` (see
[Connecting clients](CLIENTS.md#clients-that-only-spawn-a-command)).

### MCP HTTP environment variables

| Variable | Flag | Type | Default | Description |
|----------|------|------|---------|-------------|
| `NOTEDTHAT_MCP_HTTP_ALLOWED_ORIGINS` | `--mcp-http-allowed-origins` | comma-separated strings | (unset) | Allowed `Origin` header values. When unset or empty, defaults to `["null"]` (loopback-only). Non-empty values replace the default entirely and form an exclusive allowlist. |
| `NOTEDTHAT_MCP_HTTP_ALLOWED_HOSTS` | `--mcp-http-allowed-hosts` | comma-separated strings | (unset) | Allowed `Host` header values. When unset or empty, defaults to `["127.0.0.1", "localhost", "::1"]` (loopback-only). Non-empty values replace the default entirely and form an exclusive allowlist. |
| `NOTEDTHAT_MCP_ANONYMOUS` | `--mcp-anonymous` | `auto` or `never` | `auto` (unset or empty means `auto`) | Whether `POST /mcp` admits a request with no `Authorization` header. `auto`: yes, when at least one declared knowledge base grants `anyone` some verb; the tools then act as the anonymous caller and the `anyone` rules decide. `never`: always `401`, so an OAuth-capable client is challenged on connect even on a deployment with public knowledge bases. Any other value refuses startup. |
| `NOTEDTHAT_MCP_MAX_READ_BYTES` | `--mcp-max-read-bytes` | positive integer (u64 bytes) | 16777216 (16 MiB) | Most bytes one MCP object read (`read` tool, `resources/read`, the copy inside `move`) may fetch from the API. A larger object is refused with `response_too_large`, whose message names the `read` tool's slice arguments; a slice within the budget is served. The default equals the API body cap, so anything written through the API reads back whole — only objects that arrived over WebDAV or straight into an `fs` tree can be larger. A binary resource is base64-encoded on top of this, about four thirds of the budget at most; a text `read` returns its text in both halves of the result (`content` and `structuredContent`), so that response is up to twice this. |
| `NOTEDTHAT_MCP_MAX_SESSIONS` | `--mcp-max-sessions` | positive integer | `256` (unset or empty means 256) | Most MCP sessions one process holds at once. A `POST` that would open another — one carrying no `Mcp-Session-Id` — is refused `503` with `Retry-After: 5` until a session ends or idles out. A **soft** bound: the count and the session's creation are not atomic, so concurrent `initialize`s can overshoot it by however many were in flight, and it is per process rather than per caller. Zero, a negative number or a non-empty non-number refuses startup; an empty or blank value is the default, as it is for its siblings — Compose expands an unset `${NOTEDTHAT_MCP_MAX_SESSIONS-}` to the empty string. See [what a session costs](#what-a-session-costs) before raising it. |

#### What a session costs

A session is not one connection. While it lives it holds:

- one inbound `GET /mcp` connection for its notification leg, for as long as the client keeps it
  open — and that leg is itself a request on the same listener;
- one loopback `GET /api/v1/knowledgebases/{kb}/events` stream per knowledge base it has
  subscriptions in, so `1 + kbs` connections rather than one, each of which is another inbound
  request on that same listener.

And it does not necessarily idle out. rmcp's five-minute idle timer counts *messages*, not an open
leg, so a session with live subscriptions is kept alive by a server `ping` every 60 s and holds its
slot until the client disappears; a session with none is closed after five minutes idle. Size
`NOTEDTHAT_MCP_MAX_SESSIONS` against the process's file-descriptor limit and the number of
knowledge bases clients subscribe in, not against the number of users.

### Who may call `/mcp`

`POST /mcp` accepts the same bearers as the HTTP API — `NOTEDTHAT_API_TOKEN`, or an identity
token when an [OIDC issuer](#oidc-authentication) is configured — and acts as that caller on its
loopback API call. A request with **no** credential is admitted as the anonymous caller when some
declared knowledge base grants `anyone` a verb and `NOTEDTHAT_MCP_ANONYMOUS` is `auto`: the
loopback call carries no credential either, so `list_knowledgebases` names only the knowledge
bases anonymous callers may see, the read-only tools work where `anyone` holds the verb, a denial
is a `not_found` tool error and a mutating tool is `unauthorized`. Otherwise a missing credential
is `401` with the bearer challenge. A supplied credential that does not verify is `401` in every
mode; it never becomes the anonymous caller. The decision is taken once at startup, as the access
rules themselves are, and logged as `MCP_ANONYMOUS`.

### Origin and Host allow-list semantics

The empty-string default is intentionally safe. Leaving `NOTEDTHAT_MCP_HTTP_ALLOWED_ORIGINS` or `NOTEDTHAT_MCP_HTTP_ALLOWED_HOSTS` unset does **not** mean "allow all" — it means "loopback only":

- **Origins:** unset or empty → `["null"]`. This matches requests from `null` origin (local file or same-host loopback) and rejects cross-origin browser requests.
- **Hosts:** unset or empty → `["127.0.0.1", "localhost", "::1"]`. This rejects requests with a `Host` header pointing at a public hostname.

Setting either variable to a non-empty comma-separated list replaces the loopback default with your explicit allowlist. Values are trimmed of whitespace.

```sh
# Allow two specific origins
NOTEDTHAT_MCP_HTTP_ALLOWED_ORIGINS=https://app.example.com,https://staging.example.com

# Allow a public hostname in addition to localhost
NOTEDTHAT_MCP_HTTP_ALLOWED_HOSTS=localhost,mcp.example.com
```

### HTTPS requirement for public deployments

The MCP HTTP listener speaks plain HTTP. Bearer tokens sent over plaintext HTTP are acceptable only on loopback or private trusted links (e.g., within a container network or VPN).

For any public-facing deployment, terminate TLS at one reverse proxy before traffic reaches the
unified listener. Forward the complete path space so that `/api/v1`, `/webdav`, `/mcp`,
`/healthz`, `/readyz`, and `/llms.txt` share the same TLS upstream:

- **nginx:** `proxy_pass http://127.0.0.1:8080;` behind an `ssl` server block
- **Traefik:** route the unified listener through a TLS entrypoint
- **Caddy:** `reverse_proxy 127.0.0.1:8080` inside a `tls` site block

Do not expose the unified listener directly to the internet without TLS termination.

### MCP endpoint

The unified listener mounts streamable MCP at `/mcp`: `POST` for requests, `GET` for the
session's notification leg, `DELETE` to end a session ([sessions](API.md#streamable-http-transport)).
Legacy SSE paths (`POST /sse`, `GET /sse`, `/sse/*`) return HTTP 405 with a JSON error body
directing clients to use `POST /mcp`. A process holds at most `NOTEDTHAT_MCP_MAX_SESSIONS` MCP sessions (default 256, and
[what a session costs](#what-a-session-costs) is what to size it against); with an
[events backend](#events-backend) configured, sessions may subscribe to resources.

### Example: public MCP through the shared TLS upstream

```sh
NOTEDTHAT_MCP_HTTP_ALLOWED_ORIGINS=https://mcp.example.com
NOTEDTHAT_MCP_HTTP_ALLOWED_HOSTS=mcp.example.com
# Reverse proxy terminates TLS and forwards all routes to 127.0.0.1:8080
```
