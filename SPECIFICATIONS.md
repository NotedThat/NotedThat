# NotedThat — Specifications (Working Draft)

> **Status**: DRAFT — actively being fleshed out via Q&A.
> **Legend**: `[DECIDED]` = locked in, `[ASSUMED]` = my interpretation (correct me), `[OPEN]` = still pending.

---

## 1. Product Summary

NotedThat is a **markdown-first knowledgebase system** exposed as an **HTTP API**, an **MCP server**, and a **WebDAV** surface — all three read-write.

Three logical layers:
1. **Durable content store** (S3-compatible) — source of truth
2. **Search index** (Qdrant-compatible) — derived, rebuildable from (1) alone
3. **Access layer** — API + MCP + WebDAV, multi-tenant with per-KB isolation and ACL

**Reference deployment**: NotedThat itself uses **SeaweedFS ≥ 4.18** + Qdrant + an external embedding endpoint. Any S3-compatible backend works — see §9 for guidance to operators picking their own.

---

## 2. Decisions Log

| ID | Area | Decision |
|----|------|----------|
| D1 | Deployment shape | **Multi-tenant SaaS architecture; single-tenant in practice.** All isolation primitives exist from day one, but we ship with a single active tenant. |
| D2 | Isolation model | **Bucket-per-KB + collection-per-KB.** Each knowledgebase gets its own S3 bucket and its own Qdrant collection. Storage adapter is trait-based so a shared-bucket / prefix-per-KB fallback can be added later (§9.4) but is not built now. |
| D3 | Content model | **Markdown-first.** Markdown is the primary citizen; other MIME types are second-class fallback. |
| D4 | Write→search consistency | **Async best-effort.** Writes return fast; indexing happens in the background. **Job internals are NOT exposed** to API/MCP/WebDAV callers. |
| D5 | Scale target | **1k–100k documents per KB.** Design for this range; do not over-engineer for millions. |
| D6 | Byte-range reads | **First-class capability.** API and MCP forward S3-style `Range`/`Content-Range` semantics. Search chunks also carry `byte_start`/`byte_end` so a search hit dereferences to an exact byte-range read. |
| D7 | Access surface capabilities | **API, MCP, and WebDAV are all read-write.** All three surfaces can create, read, update, and delete objects. |
| D8 | Language / runtime | **Rust.** `rmcp` is production-ready per user veto — no blocker to picking Rust over TypeScript. |
| D9 | Concurrency model | **Optimistic concurrency via pass-through of conditional PUT headers.** NotedThat forwards `If-Match` / `If-None-Match` / `If-*-Since` from the client to the S3 backend and returns whatever the backend returns. We do not validate, gate, or compensate. If the operator picks a backend that doesn't enforce the header correctly, that is a property of the deployment — see §9.1 for the informational compatibility matrix. |
| D10 | Distribution | **Single static binary + Docker.** Configuration via environment variables only. No config files. |
| D11 | Tenancy hierarchy | **Tenant → many KBs.** v1 has one tenant and static credentials with access to all declared KBs. v2 JWT scopes tokens to KB + path-prefix (ACL granularity = KB + prefix). |
| D12 | Chunking strategy | **Heading-aware.** Split at markdown H1/H2/H3 boundaries with a soft size cap; every chunk carries `byte_start`/`byte_end`. |
| D13 | Search shape | **Hybrid BM25 + dense vector,** fused. Qdrant is the vector store. |
| D14 | Qdrant hybrid impl | **Server-side BM25 via `qdrant/bm25` inference model** (no client tokenizer). Named vectors: `dense` + `sparse_bm25` (with `Modifier::Idf`). Query API with `prefetch` + `fusion: RRF` (default) or `DBSF`. `qdrant-client` Rust crate ≥ 1.10 (current: 1.18). |
| D15 | Markdown parsing stack | v1 uses `pulldown-cmark` byte-offset iteration + a heading-aware chunker (§6.3). `comrak` rejected: line/column spans, not bytes. `gray_matter` / frontmatter parsing is `[POST-v1]` per D33. |
| D16 | WebDAV crate | **`dav-server` v0.11** (github.com/messense/dav-server-rs). Custom `DavFileSystem` backed by our HTTP API (per D29 all surfaces wrap the API). Reference impl: RustFS `WebDavDriver`. Streaming PUT accumulates into a write buffer (S3 has no streaming PUT). |
| D17 | WebDAV LOCK | **Not implemented, ever.** S3 Object Lock is a retention primitive, not a coordination primitive — semantically incompatible with WebDAV LOCK. Optimistic concurrency (D9) is the only concurrency contract we offer. v1 rejects `LOCK`/`UNLOCK`; FakeLs is deferred (D34). |
| D18 | Embeddings | **External endpoints only.** No local embedding models. Pluggable adapter over an OpenAI-compatible HTTP interface (works with OpenAI, Voyage, Cohere, self-hosted vLLM/Ollama/TEI). Config via env vars per D10. Same endpoint used at index time and query time. |
| D19 | S3 backend config | **Standard S3 client config only — no capability flags, no profiles.** Env vars are just what the AWS S3 SDK needs: endpoint URL, region, access key, secret, path-style flag. Everything else the backend does or doesn't support surfaces via the backend's HTTP responses. §9.1 exists only as **guidance for operators choosing a backend**. |
| D20 | Bucket naming | **Slug-based, no UUID.** Deterministic: `nt-{tenant_slug}-{kb_slug}`. Idempotent from `(tenant_slug, kb_slug)` alone — no persisted id, no state lookup. DNS-safe, ≤ 63 chars (validated at KB creation per D39). See §6.6. |
| D21 | API auth (v1) | **`[TEMPORARY]` Static Bearer token** from `NOTEDTHAT_API_TOKEN` env var. Single value, single tenant. No claims, no expiry, no rotation surface. Comparison-only auth check on every request. Replaced by JWT (D27) in v2. |
| D22 | WebDAV auth (v1) | **`[TEMPORARY]` Static HTTP Basic credentials** from `NOTEDTHAT_WEBDAV_USERNAME` + `NOTEDTHAT_WEBDAV_PASSWORD`. Same user/pass for every WebDAV connection. Replaced by JWT-over-Basic (D27) in v2. |
| D23 | WebDAV URL scheme | **Unified path-based root.** One mount URL (`https://dav.host/`). In v1, root `PROPFIND` returns every KB declared in `NOTEDTHAT_KBS`, listed by KB `slug` (no per-token filtering until JWT v2). Nested paths route to `/<kb_slug>/<object_path>`. No subdomain sharding, no per-KB URLs. |
| D24 | KB identity | Every KB has a stable `slug` (`[a-z0-9-]{1,40}`, immutable in v1) **and** a mutable `display_name` (Unicode-friendly, shown as WebDAV `DAV:displayname`). The `slug` is the internal identifier — used directly for the S3 bucket name (D20) and Qdrant collection name. No separate UUID identifier in v1; single-tenant, and slugs are unique per tenant (§6.8). |
| D25 | MCP tool surface | Each MCP tool takes a `kb` (slug) argument. In v1, one static token can address every KB declared in `NOTEDTHAT_KBS`. A **`list_knowledgebases()`** discovery tool returns that declared KB list (matching WebDAV root PROPFIND). JWT-filtered visibility is v2. Full list in §6.10. |
| D26 | KB manifest | `s3://<kb_bucket>/.notedthat/manifest.json` — small, human-readable boot record (§6.7). Written at KB create; updated when the shape of the collection changes. Not on the hot path; recoverable from operational config. |
| D27 | JWT model (v2, deferred) | **`[POST-v1]`** When we outgrow D21/D22's static tokens: HS256 self-signed, self-contained claims, no denylist DB. See §6.9 for the design. v1 ships with the static-token flow instead. |
| D28 | Repository layout | **Cargo workspace, multiple crates.** Core / storage / indexer / api-http / webdav / mcp are separate crates; `notedthat-server` binary wires them; a tiny `notedthat-mcp-stdio` binary is shipped for local MCP use. See §6.11. |
| D29 | MCP stdio mode | **stdio wraps the HTTP API** — it's a thin MCP-over-stdio → HTTP client adapter. Config = `NOTEDTHAT_URL` + `NOTEDTHAT_TOKEN`. No S3/Qdrant deps in this binary. |
| D30 | Reference backend | **NotedThat's own reference deployment uses SeaweedFS ≥ 4.18 + Qdrant.** This is what we test against and what we ship containers for. Other backends (§9.1) are supported at deployer-choice; NotedThat itself makes no runtime distinction. |
| D31 | MCP transport (v1) | **stdio only in v1.** All current MCP clients (Claude Desktop, Cursor, Zed) use stdio locally. HTTP transport (streamable) is post-v1. |
| D32 | KB provisioning (v1) | **`[TEMPORARY]` KBs declared in env vars** — no admin API, no CLI in v1. A `NOTEDTHAT_KBS` env var lists the KBs (slug + display name) to ensure exist at startup. Bucket + Qdrant collection created idempotently on boot. **KB deletion is not implemented in v1** (§7.6). |
| D33 | Frontmatter handling (v1) | **Fully raw.** Frontmatter is not parsed, skipped, mapped, or interpreted in v1. If a markdown file starts with YAML/TOML/JSON frontmatter, those bytes are treated as ordinary markdown text for chunking, indexing, and byte offsets. Frontmatter-aware tag extraction and payload mapping are `[POST-v1]`. |
| D34 | WebDAV FakeLs | **`[POST-v1]`** Not enabled in v1. Consequence: WebDAV clients that require `LOCK` before `PUT` (macOS Finder for saving, some Office suites, some mobile Files apps) will treat the mount as read-only or refuse to save. Read-only browsing works. API + MCP writes are unaffected. Add `FakeLs` when a real client scenario demands it. |
| D35 | Upload buffering | In-memory upload cap **16 MiB** before spooling to a temp file. Max upload size **5 GiB** (matches S3's non-multipart PUT ceiling). **Values hardcoded in v1; env-var tuning `[POST-v1]`.** |
| D36 | Multipart upload | Switch to S3 multipart above **32 MiB** total size; part size **8 MiB** (matches `aws-sdk-s3` defaults). **Values hardcoded in v1; env-var tuning `[POST-v1]`.** |
| D37 | MCP Resources | **`[POST-v1]` Confirmed on the roadmap.** Expose `notedthat://<kb_slug>/<path>` as browsable MCP Resources for clients like Claude Desktop and MCP Inspector. Not in M4; lands with MCP HTTP transport (D31) or shortly after. |
| D38 | Indexing queue (v1) | **Simple best-effort in-process queue.** Writes commit to S3 first, enqueue an indexing event to a bounded in-memory channel, then return. If the queue is full or the embedder/Qdrant path fails, log the failure and mark the object stale/missing in search until a later write or future reindex. No durable queue, no job IDs, no caller-visible indexing status in v1. |
| D39 | Startup provisioning (v1) | **Fail fast.** At startup, parse `NOTEDTHAT_KBS`, validate every slug, ensure each S3 bucket, manifest, and Qdrant collection exists, and exit non-zero if any declared KB cannot be provisioned or validated. No partial startup with missing KBs in v1. |
| D40 | Path normalization (v1) | **Simple object-path rules.** Object paths are UTF-8 strings normalized by stripping one leading `/`, rejecting empty file paths, rejecting `.` / `..` segments, rejecting backslashes, preserving case, and using `/` as the only separator. Directories are virtual prefixes; only object bytes are stored. |
| D41 | Pagination (v1) | **Simple limits.** `list` uses S3 lexicographic order and an opaque continuation cursor passed through from the storage adapter. `search` is top-k only: `limit` controls the number of hits; no search pagination/cursor in v1. |
| D42 | Reindex (v1) | **No public reindex endpoint/tool in v1.** Qdrant remains rebuildable by design, but rebuild is an operator/internal future operation. If indexing falls behind or data is stale, v1 accepts temporary search staleness. |
| D43 | Error contract (v1) | **Small stable mapping.** HTTP mirrors normal status codes (`400` invalid input/path/range syntax, `401` auth, `403` forbidden in v2 ACL cases, `404` KB/object missing, `409` conflicts that are not preconditions, `412` S3 precondition failed, `416` unsatisfiable range, `413` upload too large, `502/503` backend unavailable). MCP maps these to typed tool errors with the same code strings; WebDAV uses the nearest HTTP/WebDAV status. |
| D44 | HTTP API path shape | Routes under `/v1/knowledgebases/{kb_slug}[/{object_path}]`, mirroring the WebDAV path shape (D23) so all three surfaces share URL structure. Namespaced under `/v1/` and versioned for future evolution. Full route table in §6.12. |

---

## 3. Core Capabilities `[DECIDED]`

- **Persist** content to per-KB S3 buckets (source of truth)
- **Index** content into per-KB Qdrant collections (rebuildable from S3)
- **Search** hybrid (BM25 + dense) per KB with payload filters
- **Multi-tenant-ready scoping**: v1 single tenant/static full access; v2 per-KB + per-prefix ACL
- **WebDAV** — read-write, `Range`-honoring, unified root
- **HTTP API** — read-write, byte-range aware
- **MCP server** — read-write, byte-range aware; v1 stdio wrapper only, HTTP transport post-v1
- **Byte-range reads** everywhere; search hits carry `object_key + byte_start + byte_end` for exact re-fetch
- **Pass-through optimistic concurrency** — conditional PUT headers forwarded to the backend verbatim (D9)

### 3.1 Single write path across three surfaces `[DECIDED]`
API `POST`, MCP `write`, and WebDAV `PUT/MOVE/COPY/DELETE/MKCOL` all route through one internal `commit(kb, path, bytes, conditional_headers)` primitive. Path normalization, MIME sniffing, size limits, ACL check, S3 put (with client headers forwarded verbatim), and indexing-event emission live there — not per surface.

---

## 4. Architectural Sketch

```
                        ┌─────────────────────────────────────┐
                        │             Clients                 │
                        │  HTTP API │  MCP  │  WebDAV         │  ← all read-write
                        │           │       │                 │
                        │  Static Bearer    ├── stdio wrapper │  ← notedthat-mcp-stdio
                        │  Basic user/pass ─┘   → HTTP API    │     wraps HTTP API (D29)
                        └───────┬─────────────────────────────┘
                                │  auth → resolve KB → ACL check
                        ┌───────▼─────────────────────────────┐
                        │      notedthat-server binary        │
                        │  ┌──────────┐   ┌───────────┐       │
                        │  │  read    │   │  commit   │       │  ← single write path
                        │  │ (range)  │   │ (forwards │       │
                        │  │          │   │  headers) │       │
                        │  └────┬─────┘   └─────┬─────┘       │
                        └───────┼───────────────┼─────────────┘
                                │               │
                     ┌──────────▼──┐       ┌────▼─────────────┐
                     │ S3 buckets  │       │ indexing event   │
                     │ 1 per KB    │       │ (in-proc channel)│
                     │ (truth)     │       └────┬─────────────┘
                     └─────┬───────┘            │
                           │             ┌──────▼───────────────┐
                           │             │  async indexer       │
                           │             │  (best-effort)       │
                           │             │  ┌─ chunker          │
                           │             │  ├─ embedder ─────► external endpoint (D18)
                           │             │  └─ Qdrant upsert    │
                           │             └───┬──────────────────┘
                           │                 │
                           │            ┌────▼──────────────┐
                           └───────────►│  Qdrant collect.  │
                                        │  1 per KB         │
                                        │  named vectors:   │
                                        │  dense + sparse   │
                                        └───────────────────┘
```

No SQLite. No app-layer arbiter. No capability probes. The S3 backend is truth and answers for its own capabilities.

---

## 5. Design Principles

1. **S3 is authoritative.** Qdrant is a derived index. A future `reindex(kb)` operation reconstructs Qdrant from S3 alone; v1 does not expose it publicly (D42).
2. **Isolation by construction.** Bucket-per-KB + collection-per-KB — cross-tenant leaks require explicit misconfiguration.
3. **Markdown-first.** Chunking, metadata, and search UX designed for markdown; other MIME types get a fallback path.
4. **Async best-effort indexing.** Writes are fast. Callers never see job IDs.
5. **Byte-range everywhere.** Any read path is range-capable.
6. **One write path, three front ends.** Cross-surface behavior is identical by construction.
7. **Thin over the backend.** NotedThat does not simulate, compensate for, or hide backend capabilities. It forwards headers and status codes honestly. The deployer picks the backend; we document what each one does (§9.1).
8. **No local state that isn't derived.** No SQLite arbiters, no token denylist DB, no in-memory ETag mirrors. State lives in S3 (truth) or Qdrant (derived, rebuildable).
9. **Single binary, single process.** `notedthat-server` hosts HTTP API + WebDAV in one process for v1. A separate small stdio binary provides MCP by wrapping the HTTP API. MCP-HTTP is post-v1.
10. **MCP wraps the HTTP API, always.** The v1 stdio wrapper, and any future MCP HTTP transport, go through the HTTP API for business logic — never bypass to the storage layer directly. One source of truth for auth, ACL, and validation.
11. **KISS.** When a choice is between "solve it for the user" and "document it and let the deployer choose", we document.

---

## 6. Data Model & Layout

### 6.1 S3 layout (per KB)
```
s3://<kb_bucket>/
├── objects/<path/to/file.md>              # user content (authoritative)
└── .notedthat/manifest.json               # KB boot record (D26, §6.7)
```
No sidecars. All derivable metadata lives in Qdrant payload.

### 6.2 Qdrant collection (per KB) `[DECIDED — D14]`

Named vectors:
- `dense` — cosine; dimensionality from `EMBEDDING_DIMENSIONS`
- `sparse_bm25` — `Modifier::Idf`; sparse vectors generated server-side from raw text via the `qdrant/bm25` inference model

Payload schema:
| Field | Type | Purpose |
|-------|------|---------|
| `object_key`   | string   | S3 key |
| `chunk_index`  | int      | position of chunk in doc |
| `byte_start`   | int      | byte offset where chunk begins in source |
| `byte_end`     | int      | byte offset where chunk ends |
| `content_hash` | string   | sha256 of source object — idempotent reindex |
| `mtime`        | int      | last-modified Unix timestamp |
| `mime`         | string   | source MIME |
| `heading_path` | string[] | markdown headings, e.g. `["Introduction", "Motivation"]` |
| `tags`         | string[] | user tags (API-only in v1; frontmatter extraction deferred) |

Search: `prefetch` on `dense` + `prefetch` on `sparse_bm25` fused via RRF. Sparse prefetch limit bumped when a selective payload filter is present (§9.6).

### 6.3 Chunking pipeline `[DECIDED — D15]`

```
raw markdown bytes
     │
     ▼
[pulldown-cmark::into_offset_iter()]  ── (Event, Range<usize>) in bytes over the full raw file
     │
     ▼   accumulate heading stack [H1, H2, H3] per span
[heading-aware chunker]
     │   split at H1/H2/H3, soft cap ≈ 800 tokens (~3000 chars)
     │   emit (text, byte_start, byte_end, heading_path)
     ▼
[Chunk { text, byte_start, byte_end, heading_path }]   byte offsets are absolute offsets in the original raw file
     │
     ▼
[external embedder]  ── POST /v1/embeddings → dense vec<f32>
     │
     ▼
Qdrant upsert  (dense vector + Document{text, "qdrant/bm25"} sparse + payload)
```

Chunker implementation `[DECIDED]`:
- `text-splitter` v0.32 with `MarkdownSplitter` — stable default; byte offsets tracked in the driver loop.
- `julienne` v0.1 rejected for v1 because it is too early/beta.

Wiki-links `[[note]]` — deferred to post-M2 (candidate: `turbovault-parser` v1.5).

### 6.4 Embedding pipeline `[DECIDED — D18]`

External endpoints only. OpenAI-compatible HTTP:
```
POST {EMBEDDING_ENDPOINT_URL}/v1/embeddings
Authorization: Bearer {EMBEDDING_API_KEY}
{ "model": "{EMBEDDING_MODEL}", "input": ["chunk1", ...] }
```

Env vars:
- `EMBEDDING_ENDPOINT_URL` — e.g. `https://api.openai.com`, `https://api.voyageai.com`, `http://tei:8080`
- `EMBEDDING_MODEL` — e.g. `text-embedding-3-small`, `voyage-3`, `BAAI/bge-m3`
- `EMBEDDING_API_KEY`
- `EMBEDDING_DIMENSIONS` — must match Qdrant `dense` vector size at KB-create
- `EMBEDDING_BATCH_SIZE` (default 32), `EMBEDDING_TIMEOUT_MS`, `EMBEDDING_MAX_RETRIES`

Switching models requires a full reindex — different model = different vector space; `dense` size is baked into the Qdrant collection.

Internal trait:
```rust
#[async_trait]
pub trait Embedder: Send + Sync {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedderError>;
    fn dim(&self) -> usize;
    fn max_input_tokens(&self) -> usize;
    fn model_id(&self) -> &str;
}
```

### 6.5 S3 connection config `[DECIDED — D19]`

Standard AWS S3 SDK config. No NotedThat-specific capability flags.

S3 env vars:
- `NOTEDTHAT_S3_ENDPOINT_URL` (optional for AWS; required for MinIO/Ceph/SeaweedFS/Garage/RustFS/R2)
- `NOTEDTHAT_S3_REGION`
- `NOTEDTHAT_S3_ACCESS_KEY_ID`
- `NOTEDTHAT_S3_SECRET_ACCESS_KEY`
- `NOTEDTHAT_S3_FORCE_PATH_STYLE` (bool; usually true for non-AWS)

Server listen config (not S3-specific — colocated here so all runtime env vars are in one place):
- `NOTEDTHAT_LISTEN_ADDR` — `host:port` the HTTP API binds to. Default `0.0.0.0:8080`. Standard `SocketAddr` parsing (IPv4, IPv6 in brackets, or hostname).

At startup we log which endpoint URL we're pointed at. Operators are responsible for choosing a backend that supports the features their clients depend on — see §9.1 for guidance.

### 6.6 Bucket naming `[DECIDED — D20]`

```
nt-{tenant_slug}-{kb_slug}
```
- `tenant_slug`: 1–20 chars, `[a-z0-9-]`, no leading/trailing hyphen
- `kb_slug`: 1–40 chars, `[a-z0-9-]`, no leading/trailing hyphen (§6.8)
- Combined bucket name must be ≤ 63 chars (S3 DNS-name limit). Any `(tenant_slug, kb_slug)` combination whose `nt-{tenant_slug}-{kb_slug}` exceeds 63 chars is rejected at KB provisioning per D39. Worst-case with the max slug lengths (`3 + 20 + 1 + 40 = 64`) is one char over, so at least one slug must be one char shorter — validated at boot, not at type level.
- Deterministic + idempotent: same `(tenant_slug, kb_slug)` → same bucket name; `CreateBucket` treats `BucketAlreadyOwnedByYou` as success.
- **No UUID, no persisted id, no state lookup** — the bucket name is fully recoverable from `NOTEDTHAT_KBS` alone. This is what makes D39's fail-fast startup provisioning possible without any external state.

### 6.7 KB manifest `[DECIDED — D26]`

`s3://<kb_bucket>/.notedthat/manifest.json`:

```json
{
  "notedthat_version": "0.1",
  "manifest_version": 1,
  "tenant_slug": "default",
  "kb_slug": "my-notes",
  "display_name": "My Notes",
  "created_at": "2026-07-02T12:00:00Z",
  "embedding": {
    "endpoint_url_hint": "https://api.openai.com",
    "model": "text-embedding-3-small",
    "dimensions": 1536
  },
  "chunker": {
    "version": "v1",
    "strategy": "heading-aware",
    "soft_cap_chars": 3000
  },
  "qdrant_collection": "kb_my-notes_v1"
}
```

The `(tenant_slug, kb_slug)` pair *is* the identifier — no separate UUID field. Manifest is a sanity-check record, not the source of truth for identity (D20, D24).

Read at KB open time to sanity-check config vs deployment env. Not on hot path. Rebuildable if lost.

### 6.8 KB identity `[DECIDED — D24]`

| Name | Shape | Use | Stability |
|---|---|---|---|
| `kb_slug` | `[a-z0-9-]{1,40}` | S3 bucket name (D20), Qdrant collection name, WebDAV URL path, MCP `kb` arg, HTTP API URL | immutable in v1 |
| `display_name` | Unicode string, ≤ 128 chars | WebDAV `DAV:displayname`, UI-friendly | mutable |

Slug user-supplied at create (auto-derived from `display_name` if omitted). Uniqueness scope: per tenant.

**No separate UUID identifier in v1.** The slug *is* the identifier. This is safe because v1 is single-tenant, slugs are immutable, and slugs are unique per tenant. If multi-tenant KB rename ever lands post-v1, an internal UUID may reappear then — not now.

### 6.9 Auth

#### 6.9.1 v1 — static tokens `[DECIDED — D21, D22, D32]`

All auth in v1 is via static env-var-configured secrets. Single tenant. No user model, no ACL granularity, no expiry.

Env vars:
- `NOTEDTHAT_API_TOKEN` — the Bearer token for the HTTP API. `notedthat-mcp-stdio` uses this value via its own `NOTEDTHAT_TOKEN` env var when calling the API. String comparison, constant-time.
- `NOTEDTHAT_WEBDAV_USERNAME` — HTTP Basic username the server accepts
- `NOTEDTHAT_WEBDAV_PASSWORD` — HTTP Basic password the server accepts
- `NOTEDTHAT_KBS` — comma-separated `slug:Display Name` pairs; the server ensures these KBs exist at startup (bucket + Qdrant collection created idempotently)

The static-token holder has full access to every declared KB. No per-KB / per-prefix scoping until v2.

Example:
```
NOTEDTHAT_API_TOKEN=sk_live_9f8c…
NOTEDTHAT_WEBDAV_USERNAME=notedthat
NOTEDTHAT_WEBDAV_PASSWORD=change-me
NOTEDTHAT_KBS=my-notes:My Notes,work-kb:Work KB
```

This is `[TEMPORARY]` — replaced by D27 (JWT) in v2.

#### 6.9.2 v2 — JWT `[POST-v1 — D27]`

**Signer**: internal (we mint). No OIDC in v2 initial ship — hook OIDC later if needed.

**Algorithm**: `HS256` with a single signing key.

**Claims** — self-contained (ACL travels in the token):

```json
{
  "iss": "notedthat",
  "sub": "user@example.com",
  "aud": "notedthat",
  "iat": 1719936000,
  "exp": 1722528000,
  "tenant": "default",
  "kbs": [
    { "slug": "my-notes",       "prefix": "",         "perms": "rws" },
    { "slug": "work-kb",        "prefix": "shared/",  "perms": "rs"  }
  ]
}
```

Perm chars: `r` read, `w` write, `s` search, `a` admin.
`prefix`: literal string applied as an object-key prefix filter inside the KB.
Revocation: rotate the signing key (nuclear) or wait for `exp` (default lifetime 30 days).

Env vars (v2 additions):
- `NOTEDTHAT_JWT_SIGNING_KEY` — base64, ≥ 256 bits
- `NOTEDTHAT_JWT_ISSUER`, `NOTEDTHAT_JWT_AUDIENCE`
- `NOTEDTHAT_JWT_DEFAULT_LIFETIME_DAYS`

### 6.10 MCP tool surface `[DECIDED — D25]`

All tools take `kb` (the slug) where relevant. `if_match` / `if_none_match` args map directly to HTTP conditional headers (per D9 — forwarded to backend, no capability check).

| Tool | Purpose |
|---|---|
| `list_knowledgebases()` | Returns `[{kb_slug, display_name, description?, perms}]`; in v1 this is every KB declared in `NOTEDTHAT_KBS` |
| `search(kb, query, filters?, limit?)` | Hybrid search; returns chunks with `{object_key, byte_start, byte_end, heading_path, score, preview}` |
| `read(kb, path, byte_start?, byte_end?)` | Byte-range read of an object |
| `write(kb, path, content, if_match?, if_none_match?)` | Create/update object |
| `list(kb, prefix?, limit?, cursor?)` | List objects under a prefix |
| `delete(kb, path, if_match?)` | Delete object |
| `move(kb, from, to, if_match?)` | Rename/move object |

Resources: expose `notedthat://<kb_slug>/<path>` as MCP Resources for browsable clients — **`[POST-v1]`**. Tools alone in M4; Resources added later.

Transports: **stdio only in v1** (D31), via the `notedthat-mcp-stdio` binary (D29). Streamable HTTP transport added post-v1 if remote MCP hosting matters.

### 6.11 Repository layout `[DECIDED — D28]`

Cargo workspace:

```
notedthat/
├── Cargo.toml                    # workspace root
├── crates/
│   ├── notedthat-core/           # domain types, traits, static auth checks; JWT verify post-v1
│   ├── notedthat-storage-s3/     # S3 adapter (aws-sdk-s3); implements Storage trait
│   ├── notedthat-indexer/        # chunker + embedder client + Qdrant integration
│   ├── notedthat-api-http/       # HTTP API surface (axum handlers over core)
│   ├── notedthat-webdav/         # WebDAV surface (dav-server DavFileSystem impl)
│   ├── notedthat-mcp/            # MCP tool definitions (rmcp) + HTTP-client-backed impl
│   └── notedthat-server/         # main binary — wires all listeners in one process
└── bin/
    └── notedthat-mcp-stdio/      # small binary — MCP over stdio → HTTP API of a running server
```

Dep graph:
- `notedthat-core` — no deps on other workspace crates
- `notedthat-storage-s3`, `notedthat-indexer` — depend on core
- `notedthat-api-http` — depends on core + storage + indexer
- `notedthat-webdav` — depends on core + an HTTP client to the local API
- `notedthat-mcp` — depends on core (for types) + an HTTP client
- `notedthat-server` — depends on api-http + webdav; runs HTTP API + WebDAV listeners in v1 (MCP HTTP post-v1)
- `notedthat-mcp-stdio` — depends on notedthat-mcp only

Per D10, `notedthat-server` is the main artifact. `notedthat-mcp-stdio` ships in the same Docker image and separately as an installable (`cargo install notedthat-mcp-stdio`).

### 6.12 v1 operational contracts `[DECIDED — D38–D44]`

#### HTTP API path shape `[DECIDED — D44]`

Routes are namespaced under `/v1/` and follow the WebDAV path shape (D23) so all three surfaces share the same URL structure.

| Method | Route | Purpose |
|---|---|---|
| `GET` | `/v1/knowledgebases` | List every KB declared in `NOTEDTHAT_KBS` (v1: static-token holder sees them all) |
| `GET` | `/v1/knowledgebases/{kb_slug}` | List objects in a KB with `?prefix=`, `?limit=`, `?cursor=` |
| `HEAD` | `/v1/knowledgebases/{kb_slug}/{object_path}` | Existence + metadata check (no body) |
| `GET` | `/v1/knowledgebases/{kb_slug}/{object_path}` | Read object; `Range` header honored per §6.12 Byte ranges |
| `PUT` | `/v1/knowledgebases/{kb_slug}/{object_path}` | Create/update object; conditional headers per D9 |
| `DELETE` | `/v1/knowledgebases/{kb_slug}/{object_path}` | Delete object; conditional headers per D9 |
| `GET` | `/healthz` | Liveness probe (server up) |
| `GET` | `/readyz` | Readiness probe (KB provisioning complete, S3 reachable) |

`{object_path}` is a URL-percent-encoded path with `/` used as a nested-key separator, normalized per §6.12 "Path normalization". Health probes are unauthenticated; every other route requires the static Bearer token (D21).

#### Startup provisioning
1. Parse `NOTEDTHAT_KBS` as comma-separated `slug:Display Name` pairs.
2. Validate every slug (`[a-z0-9-]{1,40}`, no leading/trailing hyphen; reject empty display names). Reject any `(tenant_slug, kb_slug)` whose derived bucket name (§6.6) exceeds 63 chars.
3. Ensure each bucket exists; `BucketAlreadyOwnedByYou` is success.
4. Ensure each `.notedthat/manifest.json` exists and matches the declared slug/display name/embedding dimensions.
5. Ensure each Qdrant collection exists with the expected dense dimension and sparse BM25 vector.
6. If any step fails: log the exact KB + backend error and exit non-zero. No partial startup.

#### Path normalization
- Input path is UTF-8 text after URL percent-decoding.
- Strip exactly one leading `/` if present at a surface boundary; internally paths are relative.
- Reject empty file paths for file operations.
- Reject `.` and `..` path segments instead of resolving them.
- Reject backslashes and NUL bytes.
- Preserve case and Unicode exactly.
- `/` is the only separator.
- Directories are virtual prefixes; there are no directory marker objects unless a client explicitly writes one.

#### Byte ranges
- HTTP API accepts normal `Range: bytes=start-end` and forwards equivalent range semantics to S3.
- Successful partial reads return `206` + `Content-Range`.
- Full reads return `200`.
- Malformed ranges return `400`; unsatisfiable ranges return `416`.
- MCP `read(kb, path, byte_start?, byte_end?)` uses zero-based byte offsets with `byte_end` exclusive, matching internal chunk offsets. The MCP wrapper converts to HTTP's inclusive `Range` header when calling the API.

#### List/search pagination
- `list`: lexicographic S3 object order. Default limit `100`, max `1000`. Cursor is opaque and maps to the backend continuation token.
- `search`: top-k only in v1. Default limit `10`, max `50`. No cursor/offset/search pagination.

#### Indexing queue
- Write path: S3 commit succeeds first, then enqueue `{kb, object_key, etag/content_hash, mtime}` onto a bounded in-memory channel.
- Queue capacity: fixed implementation constant in v1 (recommended `1024` events); env tuning post-v1.
- If the queue is full, log `INDEX_QUEUE_FULL` with KB/path and return write success anyway.
- Embedder or Qdrant failures are logged as `INDEXING_FAILED`; callers do not receive job IDs or indexing status.
- Search may be stale in v1. A later write to the same object re-enqueues it.

#### Reindex
- No public HTTP endpoint, MCP tool, WebDAV action, or CLI for reindex in v1.
- The storage/index design remains rebuildable from S3; operational reindex becomes a v2/admin feature.

#### Error mapping
| Condition | HTTP | MCP | WebDAV |
|---|---:|---|---:|
| Invalid input/path/range syntax | `400` | `invalid_request` | `400` |
| Missing/invalid auth | `401` | `unauthorized` | `401` |
| Forbidden by ACL (v2+) | `403` | `forbidden` | `403` |
| Missing KB/object | `404` | `not_found` | `404` |
| Upload too large | `413` | `payload_too_large` | `413` |
| S3 precondition failed (`If-Match`, `If-None-Match`) | `412` | `precondition_failed` | `412` |
| Unsatisfiable range | `416` | `range_not_satisfiable` | `416` |
| Backend unavailable / timeout | `503` | `backend_unavailable` | `503` |
| Unexpected internal error | `500` | `internal_error` | `500` |

HTTP error bodies are JSON: `{ "error": "code", "message": "human readable", "request_id": "..." }`. MCP tool errors use the same `error` code string and include the human message as tool error content.

---

## 7. Open Questions

### 7.1 WebDAV micro-decisions (closed for v1)
- ✅ Basic-auth username: v1 uses static `NOTEDTHAT_WEBDAV_USERNAME` (D22).
- ✅ `FakeLs`: not in v1 (D34); Finder / Office save-workflow limitations documented.
- ✅ Upload buffer: 16 MiB in-memory, 5 GiB max — hardcoded (D35).
- KB deletion + open WebDAV mount — moot for v1 (D32).

### 7.2 MCP micro-decisions (closed for v1)
- ✅ Resources: post-v1 (D37).
- ✅ HTTP transport: post-v1 (D31).
- Legacy SSE MCP transport: don't implement — deprecated in the MCP spec.

### 7.3 Non-functional targets `[OPEN]`
- Search P50/P95 latency budget? — set with data during M2.
- Write ack latency budget? — set with data during M1.
- Availability target? — SLA depends on deployment shape; not a v1 concern.

### 7.4 v2+ deferred items
- **Auth**: JWT (D27), per-KB + per-prefix ACL, HTTP admin endpoints for KB create / token mint.
- **WebDAV**: `FakeLs` for save-workflow clients (D34).
- **KB lifecycle**: delete + rename (D32).
- **Content**: frontmatter parsing → payload mapping (D33).
- **MCP**: HTTP transport (D31), Resources (D37).
- **Tuning**: env-var overrides for upload buffer / multipart thresholds (D35, D36).
- **Storage**: full-rebuild-from-S3 as a first-class operation.

---

## 8. Milestones

### v1 (static-token, single-tenant, KB-declared-in-env)
1. **M0** — Spec locked (this doc)
2. **M1** — `notedthat-core` + `notedthat-storage-s3` + `notedthat-api-http` + `notedthat-server`: put/get/delete with `Range` (pass-through conditional headers), static-Bearer auth (D21), env-declared KB list (D32), idempotent bucket provisioning
3. **M2** — `notedthat-indexer` + Qdrant hybrid search
4. **M3** — `notedthat-webdav` (unified root, no LOCK/FakeLs in v1, Basic auth per D22, routed through the HTTP API)
5. **M4** — `notedthat-mcp` tool surface + `notedthat-mcp-stdio` binary (stdio transport only)

### v2 (post-v1, in rough order)
- JWT auth (D27), per-KB + per-prefix ACL
- HTTP admin surface (KB create/delete, token mint)
- KB deletion + rename
- Frontmatter parsing (D33)
- MCP HTTP transport + Resources (D31)
- Full-rebuild-from-S3 as a first-class operation
- Multi-tenant polish, observability

---

## 9. Backend Selection Guidance (informational)

Per D9 / D19 / D30: NotedThat runs against any S3-compatible backend without capability checks. What the backend supports is what the deployment gets. This section exists so **operators can make an informed choice**.

Concrete honest problem: if a client sends `If-Match: <etag>` to NotedThat, the client will either:
- get a proper `412 Precondition Failed` on mismatch (AWS, MinIO, R2, Ceph ≥20.2.1, SeaweedFS ≥4.09, RustFS) — correct behavior, or
- get a silent `200 OK` even on mismatch under contention (Garage always; SeaweedFS <4.09; RustFS under high-concurrency lock-timeout — see §9.1).

The client won't know it got the wrong answer until concurrent conflicts show up as silent lost writes. **This is a property of the deployment, not of NotedThat.** We document; the deployer picks.

### 9.1 Compatibility matrix (as of Q3 2026)

Two distinct primitives, different levels across the ecosystem:
- **`If-None-Match: *`** on `PUT` — create-only / no-clobber (AWS Aug 2024)
- **`If-Match: <etag>`** on `PUT` — CAS on overwrite (AWS Nov 2024) — the harder one

Accepting the header is easy; **atomicity under concurrent writers** requires consensus or a distributed lock inside the backend. Otherwise the header is theatre.

| Backend | Range GET | ETag | `If-None-Match: *` | `If-Match: <etag>` | Atomicity mechanism | Object Lock | Notes |
|---|---|---|---|---|---|---|---|
| **AWS S3** | ✅ | ✅ | ✅ (Aug 2024) | ✅ (Nov 2024) | S3 internals | ✅ | Full support |
| **MinIO** | ✅ | ✅ | ✅ | ✅ | Distributed lock + EC quorum | ✅ | Full support. **License**: AGPL v3 since 2024–25 — check compatibility with your product before choosing |
| **Ceph RGW** ≥ v20.2.1 (Tentacle) / v20.3.0+ (Squid) | ✅ | ✅ | ✅ | ✅ | RADOS transactions | ✅ | Full support; heavy ops (recommend Rook on K8s) |
| **Cloudflare R2** | ✅ | ✅ | ✅ | ✅ | R2 internals | ❌ | Full support; zero egress bonus |
| **SeaweedFS** ≥ 4.09 (Feb 2026, PR #7154), **recommended ≥ 4.18** (Apr 2026, PR #8802 — atomic mutations) | ✅ | ✅ | ✅ | ✅ | Filer-level distributed lock | ❌ | Full support on non-versioned buckets (NotedThat's default). **NotedThat's own reference backend.** |
| **SeaweedFS** < 4.09 | ✅ | ✅ | ⚠️ header parsed, not enforced | ⚠️ | (n/a) | ❌ | Upgrade required |
| **RustFS** 1.0.0-beta.8 (Q2 2026) | ✅ | ✅ | ✅ | ✅ | Per-PUT distributed lock — **lock RPC timeouts under commit-storm concurrency** (issue #3097) + **disk-full metadata-corruption** report (#2737) | ⚠️ | Apache 2.0 (attractive vs MinIO's AGPL) but beta — pilot only |
| **Garage** | ✅ | ✅ | ⚠️ parses but no atomicity | ⚠️ parses but no atomicity | **none — structurally impossible per Garage docs** ("cannot be safely implemented due to the lack of a consensus algorithm") | ❌ | Works fine for single-writer / best-effort deployments. Silent lost writes under contention. Not a bug — a design choice. |

### 9.2 Rough recommendations to operators

- **You want it easy, want CAS, and are okay self-hosting**: **SeaweedFS ≥ 4.18** — small footprint, single binary, non-versioned buckets by default (matches NotedThat), full RFC 7232 conditional-write correctness, active project. **This is what NotedThat itself uses.**
- **You want CAS + zero egress + no self-hosting the object store**: **Cloudflare R2**.
- **You already run Kubernetes and want Ceph-grade correctness**: **Ceph RGW on Rook** (≥ v20.2.1).
- **You already run Ceph on bare metal**: **Ceph RGW** direct.
- **You need geo-distributed, mostly single-writer, don't need CAS**: **Garage**. Small, elegant. Pick this only if you accept last-writer-wins for concurrent writes.
- **AGPL is fine and you want a super-mature single-cluster S3**: **MinIO**.
- **You want Rust-native and are willing to pilot**: **RustFS** — track #3097 and #2737 before production.
- **You're on AWS anyway**: **AWS S3**. Cost-check bucket-per-KB at scale (§9.4).

### 9.3 What NotedThat does NOT do
- No compensating layer for missing backend features (no SQLite ETag mirror, no app-layer CAS arbiter).
- No capability probes at startup.
- No feature flags to disable header forwarding — headers are always forwarded verbatim.
- No object-lock / retention / legal-hold surface; WebDAV LOCK is refused (D17).
- No test-your-backend probe at startup. If you want to verify your backend really does honor `If-Match` under concurrency, use `ceph/s3-tests` — that's a deployer-side gate, not ours.

### 9.4 Bucket-per-KB at scale
Bucket-per-KB is safe on SeaweedFS, Garage, R2, RustFS (no limits, no per-bucket cost). On AWS the default quota is 10,000 buckets per account (raised from 100 in Nov 2024), free up to 2,000, ~$0.10/bucket/month above that. `ListBuckets` above 10k requires pagination.

Plan: ship bucket-per-KB only. Storage adapter is a trait so a prefix-per-KB fallback can be added if AWS deployments approach the quota. Do not build it now.

### 9.5 IDF drift on small BM25 corpora
Qdrant computes IDF at query time from live stats. On corpora < 10k documents, IDF shifts noticeably as documents are added — subtle search-quality regression. Mitigation: monitor eval scores; periodic full reindex (M6); prefer RRF over DBSF (rank-based, more robust to IDF drift).

### 9.6 Filter selectivity vs sparse prefetch
Qdrant applies payload filters *after* sparse top-k. Selective filters starve fusion input. Mitigation: bump sparse prefetch limit (e.g. 100) when a filter is present.

---

## 10. Reference implementations to mine

- **`dav-server` v0.11** (github.com/messense/dav-server-rs) — `DavFileSystem` / `DavFile` / `DavMetaData` traits
- **RustFS `WebDavDriver`** (github.com/rustfs/rustfs → `crates/protocols/src/webdav/driver.rs`) — S3-backed `DavFileSystem` with write-buffer PUT pattern
- **`qdrant-client` v1.18** (github.com/qdrant/rust-client) — Query API with `PrefetchQueryBuilder`, `RrfBuilder`, `DocumentBuilder("qdrant/bm25")`
- **`pulldown-cmark` v0.13** — `Parser::new(...).into_offset_iter()` for byte-offset iteration
- **`gray_matter` v0.3** — frontmatter YAML/TOML/JSON (`[POST-v1]`)
- **`aws-sdk-s3`** — with `path_style` for Garage/SeaweedFS/MinIO/Ceph/RustFS
- **`rmcp`** — official Rust MCP SDK
- **`jsonwebtoken`** (github.com/Keats/jsonwebtoken) — HS256 sign/verify
