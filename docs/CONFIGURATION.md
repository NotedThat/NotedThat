# NotedThat Configuration

NotedThat is configured entirely through environment variables. There are no CLI flags, no config
files, and no `.env` file auto-loading. Every setting the server needs must be present in the
process environment before startup.

This keeps the configuration surface explicit and container-friendly: pass vars via `docker run -e`,
a Kubernetes `Secret`, or your shell's `export` statements.

## Required environment variables

These must be set. The server exits with a non-zero status and a descriptive error message if any
are missing or invalid.

| Variable | Type | Description | Example |
|----------|------|-------------|---------|
| `NOTEDTHAT_API_TOKEN` | string (non-empty) | Static Bearer token for API authentication. All `/v1/` requests must present this token in the `Authorization: Bearer` header. | `s3cr3t-token` |
| `NOTEDTHAT_WEBDAV_USERNAME` | string (non-empty) | HTTP Basic auth username for the WebDAV listener. Required and must not be empty. | `webdav-user` |
| `NOTEDTHAT_WEBDAV_PASSWORD` | string (non-empty) | HTTP Basic auth password for the WebDAV listener. Required and must not be empty. | (use a strong random value) |
| `NOTEDTHAT_KBS` | comma-separated slugs | One or more knowledge base slugs to declare. Each slug must match `[a-z0-9-]{1,40}`. Duplicates are rejected. At least one slug is required. | `notes,scratch,work` |
| `NOTEDTHAT_S3_REGION` | AWS region string | AWS region for the S3 bucket. Required even when using a custom endpoint. | `us-east-1` |
| `NOTEDTHAT_S3_ACCESS_KEY_ID` | string | AWS access key ID. No credential chain is consulted; this value is used directly. | `AKIAIOSFODNN7EXAMPLE` |
| `NOTEDTHAT_S3_SECRET_ACCESS_KEY` | string | AWS secret access key corresponding to the access key ID above. | `wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY` |

## Optional environment variables

These have defaults and can be omitted.

| Variable | Type | Default | Description |
|----------|------|---------|-------------|
| `NOTEDTHAT_LISTEN_ADDR` | `host:port` (SocketAddr) | `0.0.0.0:8080` | Address and port the HTTP server binds to. Use `127.0.0.1:8080` to restrict to localhost. |
| `NOTEDTHAT_WEBDAV_LISTEN_ADDR` | `host:port` (SocketAddr) | `0.0.0.0:8081` | Address and port the WebDAV listener binds to. Use `127.0.0.1:8081` to restrict to localhost. |
| `NOTEDTHAT_S3_ENDPOINT_URL` | URL | (unset, uses AWS default) | Custom S3-compatible endpoint. Required for SeaweedFS, MinIO, Ceph, Garage, and other S3-compatible stores. |
| `NOTEDTHAT_S3_FORCE_PATH_STYLE` | `true` or `false` | `false` | Use path-style S3 addressing (`endpoint/bucket/key`) instead of virtual-hosted style (`bucket.endpoint/key`). Set to `true` for SeaweedFS, MinIO, and most self-hosted S3-compatible stores. |
| `NOTEDTHAT_LOG_FORMAT` | `pretty` or `json` | `pretty` | Log output format. `pretty` produces human-readable multi-line output. `json` produces one JSON object per log event, suitable for log aggregators. |
| `RUST_LOG` | tracing filter string | `info,notedthat=debug` | Controls log verbosity. Uses the standard `tracing-subscriber` filter syntax. Examples: `debug`, `warn`, `info,notedthat_api_http=trace`. |

## Shutdown behaviour

NotedThat performs a staged graceful shutdown when it receives SIGTERM or SIGINT:

1. **Both listeners stop accepting new connections** — the HTTP API listener (port 8080) and the
   WebDAV listener (port 8081) both stop accepting immediately.
2. **WebDAV in-flight grace period (60 seconds)** — in-flight WebDAV uploads have up to 60 seconds
   to complete their storage writes and enqueue index events. This window is hardcoded as
   `WEBDAV_INFLIGHT_GRACE` and is not configurable.
3. **Indexer drain (31 seconds)** — the background indexer worker is signalled to stop and given
   up to 31 seconds to flush its queue. Any events not processed within this window are abandoned.

**Total worst-case shutdown time: approximately 91 seconds** (60 s WebDAV grace + 31 s indexer drain).

Plan your container `terminationGracePeriodSeconds` (Kubernetes) or `stop_grace_period` (Docker
Compose) accordingly — a value of at least 120 seconds is recommended.

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
# NOTEDTHAT_WEBDAV_LISTEN_ADDR=0.0.0.0:8081  # optional, this is the default
RUST_LOG=info,notedthat=debug
```

> **Note:** This file is for reference only. The server does **not** auto-load `.env` files. Export
> these variables manually or use a tool like [direnv](https://direnv.net/) to load them
> automatically when you enter the project directory.

## Example: AWS S3

```sh
NOTEDTHAT_API_TOKEN=<your-api-token>
NOTEDTHAT_KBS=notes
NOTEDTHAT_S3_REGION=eu-west-1
NOTEDTHAT_S3_ACCESS_KEY_ID=<your-access-key-id>
NOTEDTHAT_S3_SECRET_ACCESS_KEY=<your-secret-access-key>
```

No endpoint URL or path-style override needed for real AWS S3.

## Startup validation

The server validates all configuration before binding to any port or connecting to S3. If a
required variable is missing, empty, or invalid, the process exits immediately with a non-zero
status code and prints a descriptive error to stderr. For example:

```
Error: NOTEDTHAT_API_TOKEN is required
Error: NOTEDTHAT_KBS must declare at least one knowledge base
Error: NOTEDTHAT_S3_REGION is required
Error: NOTEDTHAT_LISTEN_ADDR is invalid: invalid socket address syntax
Error: invalid KB slug "My Notes": slugs must match [a-z0-9-]{1,40}
Error: duplicate KB slug in NOTEDTHAT_KBS: "notes"
```

This fail-fast behavior means misconfigured deployments fail loudly at startup rather than
silently misbehaving at runtime.

WebDAV credentials (`NOTEDTHAT_WEBDAV_USERNAME` and `NOTEDTHAT_WEBDAV_PASSWORD`) are required and
must not be empty strings. Setting either to an empty string is treated the same as leaving it unset
and causes a non-zero exit before any listener binds.

## What's not configurable in M2

- **Tenant slug:** Hardcoded to `"default"`. There is no `NOTEDTHAT_TENANT_SLUG` variable.
- **Upload buffer sizes:** Fixed at 16 MiB. Configurable buffer sizes are planned for a later
  release.
- **Rate limits:** No per-client or global rate limiting in M2.
- **TLS:** The server speaks plain HTTP. Terminate TLS at a reverse proxy (Traefik, nginx, Caddy).
- **Multiple tokens:** Only one API token is supported. Per-KB tokens and scopes are planned for
  a later release.

---

## Qdrant

NotedThat uses [Qdrant](https://qdrant.tech/) for vector search indexing (M4+). Qdrant v1.15.2 or later is required for server-side `qdrant/bm25` sparse inference.

| Variable | Required | Default | Description |
|---|---|---|---|
| `NOTEDTHAT_QDRANT_URL` | Yes | | Qdrant gRPC endpoint (e.g. `http://127.0.0.1:6334`) |
| `NOTEDTHAT_QDRANT_API_KEY` | No | | API key for authenticated Qdrant instances |

**Example**:

```env
NOTEDTHAT_QDRANT_URL=http://127.0.0.1:6334
# NOTEDTHAT_QDRANT_API_KEY=your-key-here  # optional
```

---

## Embedding

NotedThat uses an external OpenAI-compatible embedding endpoint to index markdown content (M4+). Indexing is **async best-effort** — see [Indexing behavior](#indexing-behavior) below.

| Variable | Required | Default | Description |
|---|---|---|---|
| `EMBEDDING_ENDPOINT_URL` | Yes | | Base URL of the OpenAI-compatible endpoint (e.g. `https://api.openai.com`) |
| `EMBEDDING_MODEL` | Yes | | Model name (e.g. `text-embedding-3-small`, `voyage-3`, `BAAI/bge-m3`) |
| `EMBEDDING_API_KEY` | Yes | | Bearer token / API key for the endpoint |
| `EMBEDDING_DIMENSIONS` | Yes | | Output vector dimensions. Must match the model's actual output and is baked into the Qdrant collection at first provisioning. |
| `EMBEDDING_BATCH_SIZE` | No | `32` | Number of text chunks per HTTP embedding request |
| `EMBEDDING_TIMEOUT_MS` | No | `30000` | Per-request HTTP timeout (milliseconds) |
| `EMBEDDING_MAX_RETRIES` | No | `3` | Number of retry attempts on HTTP 429 or 5xx responses |
| `EMBEDDING_MAX_INPUT_TOKENS` | No | `8192` | Chunks exceeding this character count are dropped (with a WARN log) rather than truncated |

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
- If the queue is full, the write still succeeds and `INDEX_QUEUE_FULL` is logged.
- If Qdrant is unreachable during indexing, `INDEXING_FAILED` is logged and the write still succeeds. The next write of the same object re-enqueues automatically.
- On graceful shutdown (SIGTERM), the server drains the queue with a **30-second bounded timeout** before stopping.

No search endpoint or MCP search tool is exposed in M4 — search arrives in M5.

See [SPECIFICATIONS.md](../SPECIFICATIONS.md) §6.4 (embeddings), §6.11 (startup provisioning), §6.12 (indexing queue) for full details.
