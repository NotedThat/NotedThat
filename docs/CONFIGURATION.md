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
- Use the HTTP cursor API (`GET /v1/knowledgebases/{kb_slug}?cursor=...`) for programmatic access to large knowledge bases.

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
- If the queue is full, the object is stored to S3 but the write returns HTTP 503 `backend_unavailable` with `Retry-After: 5` and `INDEX_QUEUE_FULL` is logged. The client should retry to re-enqueue the indexing event.
- **Conditional writes under backpressure: retry semantics interact with 412.** A conditional `PUT`/`DELETE` using `If-Match` or `If-None-Match` can complete the S3 mutation and then return HTTP 503 because the indexer queue is full. A naive retry with the same conditional headers may then return HTTP 412 `precondition_failed` because the object now exists or its ETag changed. Clients that use conditional headers must treat a 503 → 412 sequence as a possible stored-but-not-indexed ghost state and either accept that state or use a stronger consistency mechanism; v1 does not automatically replay or repair it.
- If Qdrant is unreachable during indexing, `INDEXING_FAILED` is logged and the write still succeeds. The next write of the same object re-enqueues automatically.
- On graceful shutdown (SIGTERM), the server drains the queue with a **30-second bounded timeout** before stopping.

No search endpoint or MCP search tool is exposed in M4 — search arrives in M5.

See [SPECIFICATIONS.md](../SPECIFICATIONS.md) §6.4 (embeddings), §6.11 (startup provisioning), §6.12 (indexing queue) for full details.

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

NotedThat (M8+) includes a built-in MCP-over-HTTP listener that exposes the same tools and resources as the stdio transport, but over streamable HTTP. It runs as a third listener alongside the HTTP API (port 8080) and WebDAV (port 8081).

### MCP HTTP environment variables

| Variable | Type | Default | Description |
|----------|------|---------|-------------|
| `NOTEDTHAT_MCP_HTTP_ENABLED` | `true` or `false` | `true` | Whether to start the MCP HTTP listener. When `false`, the listener is not bound and `/readyz` does not check it. |
| `NOTEDTHAT_MCP_HTTP_BIND` | `host:port` (SocketAddr) | `0.0.0.0:8082` | Address and port the MCP HTTP listener binds to. Use `127.0.0.1:8082` to restrict to localhost. |
| `NOTEDTHAT_MCP_HTTP_ALLOWED_ORIGINS` | comma-separated strings | (unset) | Allowed `Origin` header values. When unset or empty, defaults to `["null"]` (loopback-only). Non-empty values replace the default entirely and form an exclusive allowlist. |
| `NOTEDTHAT_MCP_HTTP_ALLOWED_HOSTS` | comma-separated strings | (unset) | Allowed `Host` header values. When unset or empty, defaults to `["127.0.0.1", "localhost", "::1"]` (loopback-only). Non-empty values replace the default entirely and form an exclusive allowlist. |

`NOTEDTHAT_API_TOKEN` is reused for MCP HTTP Bearer authentication. Every request to the MCP HTTP listener must present this token in an `Authorization: Bearer` header. If `NOTEDTHAT_MCP_HTTP_ENABLED` is `true` and `NOTEDTHAT_API_TOKEN` is empty or whitespace-only, the server exits at startup with a non-zero status.

### Disabled listener behavior

When `NOTEDTHAT_MCP_HTTP_ENABLED=false`:

- No socket is bound on port 8082 (or whatever `NOTEDTHAT_MCP_HTTP_BIND` specifies).
- `/readyz` returns `{"status":"ok"}` without probing or requiring the MCP listener.
- All other listeners (HTTP API, WebDAV) start normally.

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

For any public-facing deployment, terminate TLS at a reverse proxy before traffic reaches the MCP listener:

- **nginx:** `proxy_pass http://127.0.0.1:8082;` behind an `ssl` server block
- **Traefik:** route the MCP service through a TLS entrypoint
- **Caddy:** `reverse_proxy 127.0.0.1:8082` inside a `tls` site block

Do not expose port 8082 directly to the internet without TLS termination.

### MCP endpoint

The MCP HTTP listener mounts the streamable HTTP transport at `POST /mcp`. Legacy SSE paths (`GET /mcp`, `POST /sse`, `GET /sse`, `/sse/*`) return HTTP 405 with a JSON error body directing clients to use `POST /mcp`.

### Example: MCP HTTP with loopback defaults

```sh
# MCP HTTP is enabled by default; these are the implicit values
NOTEDTHAT_MCP_HTTP_ENABLED=true
NOTEDTHAT_MCP_HTTP_BIND=0.0.0.0:8082
# NOTEDTHAT_MCP_HTTP_ALLOWED_ORIGINS not set -> ["null"]
# NOTEDTHAT_MCP_HTTP_ALLOWED_HOSTS not set -> ["127.0.0.1","localhost","::1"]
```

### Example: disable MCP HTTP

```sh
NOTEDTHAT_MCP_HTTP_ENABLED=false
```

### Example: public MCP HTTP behind a reverse proxy

```sh
NOTEDTHAT_MCP_HTTP_ENABLED=true
NOTEDTHAT_MCP_HTTP_BIND=127.0.0.1:8082
NOTEDTHAT_MCP_HTTP_ALLOWED_ORIGINS=https://mcp.example.com
NOTEDTHAT_MCP_HTTP_ALLOWED_HOSTS=mcp.example.com
# Reverse proxy terminates TLS and forwards to 127.0.0.1:8082
```

---

## MCP stdio client (`notedthat-mcp-stdio`)

The `notedthat-mcp-stdio` binary is configured exclusively via environment variables. It refuses to start if either variable is missing, empty (after trimming whitespace), or if `NOTEDTHAT_URL` is not a valid http/https URL.

| Variable | Required | Description |
|----------|----------|-------------|
| `NOTEDTHAT_URL` | Yes | HTTP base URL of the running `notedthat-server` (e.g., `http://localhost:8080`). Trailing slash is stripped automatically. |
| `NOTEDTHAT_TOKEN` | Yes | Bearer token matching the server's `NOTEDTHAT_API_TOKEN`. Whitespace is trimmed; empty-after-trim is rejected. |

Note: `NOTEDTHAT_TOKEN` (MCP client) is distinct from the server-side `NOTEDTHAT_API_TOKEN`. The MCP client sends `NOTEDTHAT_TOKEN` as a `Bearer` header to the server, which validates it against `NOTEDTHAT_API_TOKEN`.
