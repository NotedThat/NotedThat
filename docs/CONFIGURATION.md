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
their variables, and `notedthat-mcp-stdio --help` lists its two.

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
| `NOTEDTHAT_API_TOKEN` | `--api-token` | string (non-empty) | Static Bearer token for authenticated API access and every HTTP write. A read request may omit it only for its configured manifest `public_read` capability. | `s3cr3t-token` |
| `NOTEDTHAT_WEBDAV_USERNAME` | `--webdav-username` | string (non-empty) | HTTP Basic auth username for the WebDAV listener. Required and must not be empty. | `webdav-user` |
| `NOTEDTHAT_WEBDAV_PASSWORD` | `--webdav-password` | string (non-empty) | HTTP Basic auth password for the WebDAV listener. Required and must not be empty. | (use a strong random value) |
| `NOTEDTHAT_KBS` | `--kbs` | comma-separated slugs | One or more knowledge base slugs to declare. Each slug must match `[a-z0-9-]{1,40}`. Duplicates are rejected. At least one slug is required. | `notes,scratch,work` |

## Optional environment variables

These have defaults and can be omitted.

| Variable | Flag | Type | Default | Description |
|----------|------|------|---------|-------------|
| `NOTEDTHAT_LISTEN_ADDR` | `--listen-addr` | `host:port` (SocketAddr) | `0.0.0.0:8080` | Address and port the HTTP server binds to. Use `127.0.0.1:8080` to restrict to localhost. |
| `NOTEDTHAT_LOG_FORMAT` | `--log-format` | `pretty` or `json` | `pretty` | Log output format. `pretty` produces human-readable multi-line output. `json` produces one JSON object per log event, suitable for log aggregators. |
| `RUST_LOG` | *(none — see above)* | tracing filter string | `info,notedthat=debug` | Controls log verbosity. Uses the standard `tracing-subscriber` filter syntax. Examples: `debug`, `warn`, `info,notedthat_api_http=trace`. |
| `NOTEDTHAT_MAX_PATCHABLE_SIZE` | `--max-patchable-size` | positive integer (u64 bytes) | 104857600 (100 MiB) | Maximum object size eligible for PATCH operations, in bytes. Objects larger than this are rejected before any splice. PATCH results larger than this limit are also rejected (checked arithmetic, no allocation). Applies to PATCH only — PUT uses the router body limit. Must be ≤ 5 GiB (MAX_UPLOAD_BYTES). |
| `NOTEDTHAT_UPLOAD_TMP_DIR` | `--upload-tmp-dir` | existing writable directory | platform temporary directory | Shared private staging directory for WebDAV upload spooling and indexer snapshots. Startup validates it before opening listeners or provisioning storage. |

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
| `NOTEDTHAT_S3_FORCE_PATH_STYLE` | `--s3-force-path-style` | No | `false` | Path-style addressing (`endpoint/bucket/key`). Set `true` for SeaweedFS, MinIO and most self-hosted stores. |
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

Turning watching off with `NOTEDTHAT_FS_WATCH=false` restores the older behaviour, where only
writes through the API, WebDAV or MCP update the search index.

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

## Manifest-controlled anonymous reads

Anonymous access is configured in each knowledge base's existing
`s3://<kb_bucket>/.notedthat/manifest.json`, not with an environment variable. One knowledge base
uses one bucket, so one bucket is one public-read policy boundary; there are no namespace or
path-prefix grants.

`public_read` is an additive optional field in manifest version `1`:

```json
"public_read": ["discover", "browse", "content", "search"]
```

Missing `public_read` and `public_read: []` both keep the knowledge base private. The only accepted
string values are `discover`, `browse`, `content`, and `search`; the field must be an array of those
strings. Unknown names and wrong JSON types make startup fail. Duplicate values are harmless and
are stored in canonical order when the manifest is serialized.

| Capability | Anonymous HTTP behavior | Anonymous WebDAV behavior |
| --- | --- | --- |
| `discover` | `GET /api/v1/knowledgebases` includes this knowledge base | Root `PROPFIND` includes this knowledge base |
| `browse` | `GET /api/v1/knowledgebases/{kb_slug}` lists object metadata | `PROPFIND` within the knowledge base is allowed |
| `content` | `GET` and `HEAD` on object paths are allowed | `GET` and `HEAD` are allowed |
| `search` | `POST /api/v1/knowledgebases/{kb_slug}/search` is allowed | Not applicable |

Capabilities are independent. For example, `search` can expose matching object paths and snippets
without granting anonymous browse or content access. Conversely, discovery does not imply browse.
For anonymous callers, `.notedthat` and all of its descendants are hidden from direct reads,
listings, WebDAV `PROPFIND`, and search.

The server validates and loads every policy once during startup provisioning. It does not watch
manifests or hot-reload policy changes: edit the manifest through your storage administration
workflow, then restart the server. A valid HTTP Bearer token or valid WebDAV Basic credential still
has full access to every declared knowledge base. If a client supplies an invalid credential, the
server returns `401 unauthorized` (and WebDAV supplies its Basic challenge) instead of treating that
request as anonymous. Every HTTP and WebDAV write remains authenticated.

`/healthz`, `/readyz`, and `/llms.txt` are globally public. MCP authentication is unchanged and
always requires its Bearer token; public-read capabilities do not grant MCP access. Anonymous
WebDAV `OPTIONS` returns only the read methods allowed at that path, while authenticated `OPTIONS`
advertises the normal method set.

There are no built-in public-read rate or burst settings. Before enabling anonymous `search`, set
rate and burst controls at the reverse proxy for that route, and tune them to the capacity of the
embedding and search backends. Do not add an application configuration variable for this control.

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
```

Each names the environment variable and the flag that overrides it, because which one you used is
not knowable from inside the check.

With the filesystem backend, the storage root is checked and claimed straight after the staging
directory, before any backend client is built:

```
Error: failed to claim NOTEDTHAT_FS_ROOT: configuration error: NOTEDTHAT_FS_ROOT does not exist: /srv/notedthat
Error: failed to claim NOTEDTHAT_FS_ROOT: configuration error: the storage root /srv/notedthat is already in use by another notedthat-server process (PID 4213)
```

This fail-fast behavior means misconfigured deployments fail loudly at startup rather than
silently misbehaving at runtime.

WebDAV credentials (`NOTEDTHAT_WEBDAV_USERNAME` and `NOTEDTHAT_WEBDAV_PASSWORD`) are required and
must not be empty strings. Setting either to an empty string is treated the same as leaving it unset
and causes a non-zero exit before any listener binds.

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
- **Multiple tokens:** Only one API token is supported. Per-KB tokens and scopes are planned for
  a later release.
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
- Source tombstone failed: the destination object IS stored, the source object IS deleted from S3, and the source search-index tombstone is missing. Search may return stale entries for the source path whose object_key now 404s. Because v1 has no public reindex endpoint and retrying the whole MOVE will 404 on GET(src), treat the 503 as final for storage state and monitor search-quality until a retry/reindex path exists.

Since v1 has no public reindex endpoint (D42), operators should treat repeated DELETE or MOVE-tombstone 503 with no retry/reindex path as a search-quality issue requiring monitoring. Clients SHOULD implement retry-with-backoff for DELETE, matching PUT.

Conditional writes under backpressure: retry semantics interact with 412. Conditional writes (`If-Match`, `If-None-Match`) that succeed at S3 but return 503 at the indexer queue leave a naive retry in a state where S3 may return 412 because the object now exists or its ETag changed. Clients using conditional headers MUST detect the 503 → 412 sequence and either accept the ghost state or use a stronger consistency mechanism.

The 503 response carries `Retry-After: 5` as a hint (not a guarantee). All three write surfaces (HTTP API, WebDAV, MCP-via-HTTP) surface this the same way: HTTP 503, error code `backend_unavailable`, and (for HTTP API + WebDAV) `Retry-After: 5`.

---

## MCP HTTP listener

NotedThat includes a built-in MCP-over-HTTP surface that exposes the same tools and resources as
the stdio transport. It is always mounted as streamable HTTP at `POST /mcp` on the single
`NOTEDTHAT_LISTEN_ADDR` listener, alongside the API at `/api/v1` and WebDAV at `/webdav`.

### MCP HTTP environment variables

| Variable | Flag | Type | Default | Description |
|----------|------|------|---------|-------------|
| `NOTEDTHAT_MCP_HTTP_ALLOWED_ORIGINS` | `--mcp-http-allowed-origins` | comma-separated strings | (unset) | Allowed `Origin` header values. When unset or empty, defaults to `["null"]` (loopback-only). Non-empty values replace the default entirely and form an exclusive allowlist. |
| `NOTEDTHAT_MCP_HTTP_ALLOWED_HOSTS` | `--mcp-http-allowed-hosts` | comma-separated strings | (unset) | Allowed `Host` header values. When unset or empty, defaults to `["127.0.0.1", "localhost", "::1"]` (loopback-only). Non-empty values replace the default entirely and form an exclusive allowlist. |

`NOTEDTHAT_API_TOKEN` is reused for MCP HTTP Bearer authentication. Every request to `POST /mcp`
must present this token in an `Authorization: Bearer` header.

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

The unified listener mounts streamable MCP at `POST /mcp`. Legacy SSE paths (`GET /mcp`,
`POST /sse`, `GET /sse`, `/sse/*`) return HTTP 405 with a JSON error body directing clients to use
`POST /mcp`.

### Example: public MCP through the shared TLS upstream

```sh
NOTEDTHAT_MCP_HTTP_ALLOWED_ORIGINS=https://mcp.example.com
NOTEDTHAT_MCP_HTTP_ALLOWED_HOSTS=mcp.example.com
# Reverse proxy terminates TLS and forwards all routes to 127.0.0.1:8080
```

---

## MCP stdio client (`notedthat-mcp-stdio`)

The `notedthat-mcp-stdio` binary takes both settings either way — `--url` and `--token`, or the variables beside them, with the flag winning. This matters for MCP client configuration files, which often make passing arguments to a child process easier than setting variables for it. It refuses to start if either setting is unsupplied, empty (after trimming whitespace), or if the URL is not a valid http/https URL.

```json
{ "command": "notedthat-mcp-stdio", "args": ["--url", "http://localhost:8080"], "env": { "NOTEDTHAT_TOKEN": "..." } }
```

Install it with `cargo install notedthat`, which ships both this binary and `notedthat-server`.

| Variable | Flag | Required | Description |
|----------|------|----------|-------------|
| `NOTEDTHAT_URL` | `--url` | Yes | HTTP base URL of the running `notedthat-server` (e.g., `http://localhost:8080`). Trailing slash is stripped automatically. |
| `NOTEDTHAT_TOKEN` | `--token` | Yes | Bearer token matching the server's `NOTEDTHAT_API_TOKEN`. Whitespace is trimmed; empty-after-trim is rejected. |

Note: `NOTEDTHAT_TOKEN` (MCP client) is distinct from the server-side `NOTEDTHAT_API_TOKEN`. The MCP client sends `NOTEDTHAT_TOKEN` as a `Bearer` header to the server, which validates it against `NOTEDTHAT_API_TOKEN`.
