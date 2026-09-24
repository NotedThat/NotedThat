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

**Reference deployment**: NotedThat itself uses **SeaweedFS ≥ 4.18** + Qdrant + an external embedding endpoint. Any S3-compatible backend works — see §8 for guidance to operators picking their own.

---

## 2. Decisions Log

| ID | Area | Decision |
|----|------|----------|
| D1 | Deployment shape | **Multi-tenant SaaS architecture; single-tenant in practice.** All isolation primitives exist from day one, but we ship with a single active tenant. |
| D2 | Isolation model | **Bucket-per-KB + collection-per-KB.** Each knowledgebase gets its own S3 bucket and its own Qdrant collection. Storage adapter is trait-based so a shared-bucket / prefix-per-KB fallback can be added later (§8.4) but is not built now. |
| D3 | Content model | **Markdown-first.** Markdown is the primary citizen; other MIME types are second-class fallback. |
| D4 | Write→search consistency | **Async best-effort.** Writes return fast; indexing happens in the background. **Job internals are NOT exposed** to API/MCP/WebDAV callers. Queue-full is the one narrowing of the best-effort contract: it surfaces as a transient error (HTTP 503) so the client can retry, because the object is stored but not searchable. |
| D5 | Scale target | **1k–100k documents per KB.** Design for this range; do not over-engineer for millions. |
| D6 | Byte-range reads | **First-class capability.** API and MCP forward S3-style `Range`/`Content-Range` semantics. Search chunks also carry `byte_start`/`byte_end` so a search hit dereferences to an exact byte-range read. |
| D7 | Access surface capabilities | **API, MCP, and WebDAV are all read-write.** All three surfaces can create, read, update, and delete objects. |
| D8 | Language / runtime | **Rust.** `rmcp` is production-ready per user veto — no blocker to picking Rust over TypeScript. |
| D9 | Concurrency model | **Optimistic concurrency via pass-through of conditional PUT headers.** NotedThat forwards `If-Match` / `If-None-Match` / `If-*-Since` from the client to the S3 backend and returns whatever the backend returns. We do not validate, gate, or compensate. If the operator picks a backend that doesn't enforce the header correctly, that is a property of the deployment — see §8.1 for the informational compatibility matrix. |
| D10 | Distribution | **Single static binary + Docker.** Configuration via environment variables only. No config files. |
| D11 | Tenancy hierarchy | **Tenant → many KBs.** v1 has one tenant. Access granularity is KB + object-key glob, declared in each manifest (D51) and matched against the caller's identity-provider groups (D53) rather than carried in a token. |
| D12 | Chunking strategy | **Heading-aware.** Split at markdown H1/H2/H3 boundaries with a soft size cap; every chunk carries `byte_start`/`byte_end`. |
| D13 | Search shape | **Hybrid BM25 + dense vector,** fused. Qdrant is the vector store. |
| D14 | Qdrant hybrid impl | **Server-side BM25 via `qdrant/bm25` inference model** (no client tokenizer). Named vectors: `dense` + `sparse_bm25` (with `Modifier::Idf`). Query API with `prefetch` + `fusion: RRF` (default) or `DBSF`. `qdrant-client` Rust crate ≥ 1.15.2 (minimum for server-side `qdrant/bm25` inference). Prefetch depth and the fused `limit` are set per request by D56. |
| D15 | Markdown parsing stack | `pulldown-cmark` byte-offset iteration + a heading-aware chunker (§6.3). OKF YAML metadata is parsed with `serde_yaml_ng` before body chunking, retaining source byte offsets (D33). |
| D16 | WebDAV crate | **`dav-server` v0.11** (github.com/messense/dav-server-rs). Custom `DavFileSystem` backed by our HTTP API (per D29 all surfaces wrap the API). Reference impl: RustFS `WebDavDriver`. Streaming PUT accumulates into a write buffer (S3 has no streaming PUT). |
| D17 | WebDAV LOCK | **Not implemented, ever.** S3 Object Lock is a retention primitive, not a coordination primitive — semantically incompatible with WebDAV LOCK. Optimistic concurrency (D9) is the only concurrency contract we offer. v1 rejects `LOCK`/`UNLOCK`; FakeLs is deferred (D34). |
| D18 | Embeddings | **External endpoints only.** No local embedding models. Pluggable adapter over an OpenAI-compatible HTTP interface (works with OpenAI, Voyage, Cohere, self-hosted vLLM/Ollama/TEI). Config via env vars per D10. Same endpoint used at index time and query time. |
| D19 | S3 backend config | **Standard S3 client config only — no capability flags, no profiles.** Applies to the `s3` backend; D49 adds the `fs` backend and its own variables. Env vars are just what the AWS S3 SDK needs: endpoint URL, region, access key, secret, path-style flag. Everything else the backend does or doesn't support surfaces via the backend's HTTP responses. §8.1 exists only as **guidance for operators choosing a backend**. |
| D20 | Bucket naming | **Slug-based, no UUID.** Deterministic: `nt-{tenant_slug}-{kb_slug}`. Idempotent from `(tenant_slug, kb_slug)` alone — no persisted id, no state lookup. DNS-safe, ≤ 63 chars (validated at KB creation per D39). See §6.6. **Amended by D58:** tenant slugs contain no hyphen, which is what makes the name injective. |
| D21 | API auth (v1) | **Static Bearer token** from `NOTEDTHAT_API_TOKEN` env var. Single value, single tenant. No claims, no expiry, no rotation surface. Comparison-only auth check on every request. **Amended by D53:** it stays as the deployment's *service token* — the operator's own credential and the `.notedthat` recovery path — beside identity-provider tokens, rather than being replaced by them. |
| D22 | WebDAV auth (v1) | **Static HTTP Basic credentials** from `NOTEDTHAT_WEBDAV_USERNAME` + `NOTEDTHAT_WEBDAV_PASSWORD`. Same user/pass for every WebDAV connection. **Amended by D53:** the pair resolves to the same service-token principal as D21's Bearer, `Basic` stays WebDAV-only, and WebDAV additionally accepts `Bearer` — a client that can set a header need not speak Basic, and an identity-provider token only exists as a bearer. |
| D23 | WebDAV URL scheme | **Unified path-based root.** WebDAV is mounted at `https://host/webdav/` on the same listener as the API and MCP. In v1, root `PROPFIND` returns every KB declared in `NOTEDTHAT_KBS`, listed by KB `slug` (no per-token filtering until JWT v2). Nested paths route to `/webdav/<kb_slug>/<object_path>`. No subdomain sharding, no per-KB URLs. |
| D24 | KB identity | Every KB has a stable `slug` (`[a-z0-9-]{1,40}`, immutable in v1) **and** a mutable `display_name` (Unicode-friendly, held in the manifest §6.7 and returned by `GET /api/v1/knowledgebases` and MCP `list_knowledgebases`, D60). The `slug` is the internal identifier — used directly for the S3 bucket name (D20) and Qdrant collection name. No separate UUID identifier in v1; single-tenant, and slugs are unique per tenant (§6.8). |
| D25 | MCP tool surface | Each MCP tool takes a `kb` (slug) argument — except `search`, whose `kb` is a list of slugs since D54. In v1, one static token can address every KB declared in `NOTEDTHAT_KBS`. A **`list_knowledgebases()`** discovery tool returns that declared KB list (matching WebDAV root PROPFIND). JWT-filtered visibility is v2. Full list in §6.10. |
| D26 | KB manifest | `s3://<kb_bucket>/.notedthat/manifest.json` — small, human-readable boot record (§6.7). Manifest v1 carries the knowledge base's `access` rules (D51). Written at KB create; updated when the shape of the collection changes. Not on the hot path; recoverable from operational config. |
| D27 | JWT model (v2, deferred) | **Superseded by D53.** The plan was HS256 self-signed tokens carrying their own ACL. D51 moved the policy into the manifest and D53 delegates identity to an external OIDC issuer, so NotedThat mints nothing and the self-contained-claims design is not needed. |
| D28 | Repository layout | **Cargo workspace, multiple crates.** Core / storage / indexer / api-http / webdav / mcp are separate crates; `notedthat-server` binary wires them; the stdio binary that once shipped beside it was removed by D63. See §6.11. |
| D29 | MCP stdio mode | **Superseded by D63.** stdio wrapped the HTTP API as a thin MCP-over-stdio → HTTP client adapter (`NOTEDTHAT_URL` + `NOTEDTHAT_TOKEN`, no S3/Qdrant deps). Removed; the principle it embodied — MCP goes through the HTTP API — survives in the HTTP transport (§5 principle 10). |
| D30 | Reference backend | **NotedThat's own reference deployment uses SeaweedFS ≥ 4.18 + Qdrant.** This is what we test against and what we ship containers for. Other S3-compatible backends (§8.1) are supported at deployer-choice; NotedThat makes no runtime distinction between them. The one runtime distinction it does make is D49's choice between the S3 and filesystem adapters. |
| D31 | MCP transport (v1) | **Streamable HTTP is the one transport (stdio shipped first, then was removed by D63).** HTTP transport is always mounted at `/mcp` on the `NOTEDTHAT_LISTEN_ADDR` listener with Bearer auth — or, since D59, as the anonymous caller where a manifest grants `anyone` something. It shipped as stateless JSON-response MCP; since D66 it is the stateful transport the MCP specification describes by default: `initialize` opens a session, every later request carries its `Mcp-Session-Id`, answers are SSE-framed, `GET /mcp` is the server-to-client notification leg and `DELETE /mcp` ends the session. Legacy `/sse` paths return 405. |
| D32 | KB provisioning (v1) | **`[TEMPORARY]` KBs declared at startup** — no admin API in v1. `NOTEDTHAT_KBS`, or the `--kbs` flag that overrides it, lists the KB slugs to ensure exist at startup; the manifest holds the `display_name` (§6.7, §6.8). Bucket + Qdrant collection created idempotently on boot. **KB deletion is not implemented in v1** (§7.6). |
| D33 | Frontmatter handling | **OKF-aware indexing, raw storage.** Non-reserved `.md` files with YAML frontmatter and a non-empty string `type` expose concept metadata and tags in search; only their bodies are chunked, with original source offsets. Other documents retain raw Markdown indexing. Unknown fields remain in the original bytes. See [OKF support](docs/OKF.md). |
| D34 | WebDAV FakeLs | **`[POST-v1]`** Not enabled in v1. Consequence: WebDAV clients that require `LOCK` before `PUT` (macOS Finder for saving, some Office suites, some mobile Files apps) will treat the mount as read-only or refuse to save. Read-only browsing works. API + MCP writes are unaffected. Add `FakeLs` when a real client scenario demands it. |
| D35 | Upload buffering | In-memory upload cap **16 MiB** before spooling to a temp file. Max upload size **5 GiB** (matches S3's non-multipart PUT ceiling). **Values hardcoded in v1; env-var tuning `[POST-v1]`.** |
| D36 | Multipart upload | Switch to S3 multipart above **32 MiB** total size; part size **8 MiB** (matches `aws-sdk-s3` defaults). **Values hardcoded in v1; env-var tuning `[POST-v1]`.** |
| D37 | MCP Resources | **Shipped in M8.** Expose `notedthat://<kb_slug>/<percent-encoded path>` as browsable MCP Resources for clients like Claude Desktop and MCP Inspector. Flat listing via `resources/list` with an opaque base64 M8 cursor across KB boundaries. `subscribe` and `listChanged` since D66, fed by the D55 events. Text objects return `TextResourceContents`; non-UTF-8 bytes return `BlobResourceContents` base64-encoded. Landed alongside MCP HTTP transport (D31) in M8. |
| D38 | Indexing queue (v1) | **Simple best-effort in-process queue.** Writes commit to S3 first, enqueue an indexing event to a bounded in-memory channel, then return. If the queue is full, return operation-specific `WriteError::IndexerBackpressureUpsert` or `WriteError::IndexerBackpressureTombstone` → HTTP 503 `backend_unavailable` with `Retry-After: 5`. The storage mutation IS committed to S3 first; the client should retry to re-enqueue the indexing event. Log `INDEX_QUEUE_FULL`. If the embedder or Qdrant path fails downstream of the queue, log `INDEXING_FAILED` and mark the object stale/missing in search until a later write or future reindex. No durable queue, no job IDs, no caller-visible indexing status in v1. D55 adds a durable *event* log beside this queue; moving indexing onto it is a follow-up, not a rewrite, because the publish hook sits at the same point. |
| D39 | Startup provisioning (v1) | **Fail fast.** At startup, parse `NOTEDTHAT_KBS`, validate every slug, ensure each S3 bucket, manifest, and Qdrant collection exists, and exit non-zero if any declared KB cannot be provisioned or validated. No partial startup with missing KBs in v1. **Amended by D64:** the Qdrant step was best-effort in code (a warning, then startup continued) until D64 made it exit non-zero as written here. |
| D40 | Path normalization (v1) | **Simple object-path rules.** Object paths are UTF-8 strings normalized by stripping one leading `/`, rejecting empty file paths, rejecting `.` / `..` segments, rejecting backslashes, preserving case, and using `/` as the only separator. Directories are virtual prefixes; only object bytes are stored. |
| D41 | Pagination (v1) | **Simple limits.** `list` uses S3 lexicographic order and an opaque continuation cursor passed through from the storage adapter. `search` is top-k only: `limit` controls the number of hits; no search pagination/cursor in v1. The order within the top-k, and the window it is cut from, are D56. |
| D42 | Reindex (v1) | **No public reindex endpoint/tool in v1.** Qdrant remains rebuildable by design, but rebuild is an operator/internal future operation. If indexing falls behind or data is stale, v1 accepts temporary search staleness. |
| D43 | Error contract (v1) | **Small stable mapping.** HTTP mirrors normal status codes (`400` invalid input/path/range syntax, `401` auth, `403` forbidden in v2 ACL cases, `404` KB/object missing, `409` conflicts that are not preconditions, `412` S3 precondition failed, `416` unsatisfiable range, `413` upload too large, `502/503` backend unavailable). MCP maps these to typed tool errors with the same code strings; WebDAV uses the nearest HTTP/WebDAV status. "KB missing" includes a declared knowledge base whose bucket or `fs` directory is gone at request time: every backend reports it as `StorageError::BucketNotFound` and every surface answers `404`, never a `5xx`, and a write never recreates it (#69). |
| D44 | HTTP API route shape | **Routes under `/api/v1/knowledgebases/{kb_slug}/{path}`** where `{path}` is a percent-encoded (URL-encoded) object key — a single URL segment. Any `/` within an object key becomes `%2F`, `?` becomes `%3F`, `#` becomes `%23`, etc. Single-segment matching avoids multi-segment wildcard routing and eliminates any KB-vs-object boundary ambiguity. KB discovery via `GET /api/v1/knowledgebases`. Health endpoints (`/healthz`, `/readyz`) are unauthenticated and unversioned (what `/readyz` covers: D64). The `v1` prefix reserves the option for a breaking API version later without disturbing WebDAV (D23) or MCP (D25). WebDAV keeps its native multi-segment path semantics; the HTTP API and WebDAV share the logical KB+object model but not the URL wire format. See §6.13 for the concrete route surface. |
| D45 | Line-range reads | **First-class line-range read capability.** HTTP GET accepts `Range: lines=<first>-<last>` (1-based inclusive) and returns 206 + `Content-Range: lines <first>-<last>/<total>` + `X-Content-Range-Bytes: <byte_start>-<byte_end>/<total_bytes>` (inclusive byte_end). MCP `read` gains `line_start`/`line_end` args (mutually exclusive with `byte_start`/`byte_end`). Line index recomputed per request (no sidecar in v1; see §7.2). Backend byte-range semantics unchanged. **Amended by D61:** the MCP `read` result carries the same headers' `etag` and totals as `structuredContent`. |
| D46 | Partial writes (PATCH) | **First-class partial-write capability via HTTP PATCH.** New route `PATCH /api/v1/knowledgebases/{kb_slug}/{path}`. Modes: `Content-Range: bytes <first>-<last>/*` OR `Content-Range: lines <first>-<last>/*` (Insert form `lines <N>-<N-1>/*`) OR `NT-Patch-Mode: append` (mutually exclusive with `Content-Range`). `If-Match` REQUIRED for bytes/lines modes; OPTIONAL for append (server uses head_etag internally — single round-trip). `If-Match: *` and multi-value `If-Match` REJECTED with 400 (v1 clarity). Server-side splice: HEAD → caller-precondition-check → GET (with `If-Match: head_etag`) → splice → PUT (with `If-Match: head_etag` — NOT caller's If-Match). Bounded 2× retry on GET or PUT PreconditionFailed. Post-splice size cap `NOTEDTHAT_MAX_PATCHABLE_SIZE` (default 100 MiB). `IndexEvent::Upsert` unchanged. MCP gains `edit` + `append` tools. WebDAV surface unchanged. PATCH correctness note: requires backend to enforce `If-Match` atomically on PUT — see §8.1. |
| D47 | String-based edits | **Content-based edit endpoint.** MCP `replace(old_string, new_string, if_match, replace_all?)` + `POST /api/v1/knowledgebases/{kb_slug}/replace/{*path}`. Server-side exact-byte UTF-8 substring search; splices under `If-Match` with the same two-ETag CAS pattern as PATCH (D46). Zero matches → 422 `no_match`; multiple matches with `replace_all=false` → 422 `ambiguous_match { match_count }`; both leave storage byte-identical. `replace_all=true` replaces every non-overlapping occurrence left-to-right in one splice. Post-splice size cap reuses `NOTEDTHAT_MAX_PATCHABLE_SIZE`. WebDAV unchanged. Complements offset-based PATCH (D46); does not replace it. |
| D48 | Manifest-controlled public reads | **Superseded by D51.** Optional manifest `public_read` granted `discover`, `browse`, `content` and `search` knowledge-base-wide to anonymous callers only. D50 replaces it with path-scoped rules that bind the credential holder too; the field is removed and an old manifest carrying it silently loses its public grants. |
| D49 | Storage backend selection | **Two first-class backends, chosen by `NOTEDTHAT_STORAGE_BACKEND` (`s3` default, `fs`).** The `fs` backend stores each object as a real file at its key path under `NOTEDTHAT_FS_ROOT`, so the store is browsable, greppable and backed up with ordinary file tools — a single-node deployment needs no object store. It implements RFC 7232 itself and makes conditional writes atomic with an in-process lock, which is why it supports **exactly one process per root** (enforced by a lock file at startup) and excludes network filesystems. The selector and both backends' variables are validated strictly: an unrecognised value, or a variable belonging to the unselected backend, refuses startup rather than being ignored (§6.5, §8.1). Derived per-KB directory names reuse `derive_bucket_name` and keep the 63-byte limit (D20), so one `NOTEDTHAT_KBS` stays valid on either backend. Bucket-per-KB (D2) and everything above the `Storage` trait are unchanged. |
| D50 | Filesystem change detection `fs` | **The `fs` backend keeps the search index in step with its own tree.** With `NOTEDTHAT_FS_WATCH` on (the default), each declared KB's directory is watched via `notify` (inotify on Linux, FSEvents on macOS, kqueue on the BSDs) and every knowledge base is compared against the index once at startup. **The watcher never enqueues a tombstone**: a deletion is an `IndexEvent::Refresh` whose re-read reports the object missing, which the worker already converts — so a debounced report can never outrace a re-create and delete points that are live again, the one failure here nothing would repair. `Refresh` is **skipped when the indexed chunks already carry the object's current `ETag`**, and that skip is what makes the startup pass, a rescan and a self-write echo all cheap; `IndexEvent::Upsert` is never skipped, because re-writing an object is v1's only reindex mechanism (D42). Directory operations are resolved by comparing a **prefix** rather than replaying events, since the kernel reports a directory and not its contents — that is what makes a folder rename correct on both sides, and what lets a deletion during downtime be found at all, as an indexed key with no file under it. Watcher work **coalesces by containment and blocks rather than dropping**, deliberately unlike D38's `try_send`: D38 is sound only because a 503 makes the client the retry mechanism, and a filesystem change has no client. Read events are discarded (`notify`'s inotify mask always includes `IN_OPEN`, so serving a `GET` would otherwise re-index the corpus), as are `.git`/`.svn`/`.hg` paths, our own temp prefix, symlinks and anything `.notedthat` (D48). A watch that cannot be established **refuses startup** per D39, naming `fs.inotify.max_user_watches` and the off switch; a watch lost at runtime logs `FS_WATCH_LOST` and asks for a rescan rather than failing `/readyz`. Depends on D49's one-process-per-root guarantee. The `s3` backend gained its equivalent in D67: nothing watched, a comparison pass at startup and on demand. |
| D51 | Manifest access rules | **Two principals, five verbs, allow-only, path-scoped.** Manifest `access` is an array of rules, each naming `who` (`anyone` — no credential — or `signed-in` — the Bearer/Basic holder), the verbs it `may` use (`list`, `read`, `write`, `delete`, `search`), and the glob patterns it applies `under` (§6.7). Private by default; the answer for a `(principal, verb, key)` triple is the union of matching rules, so **order never changes a decision**. There is no `discover` verb: a knowledge base is visible in a listing when the principal holds any grant in it. Rules bind **both** principals, so a manifest can restrict the credential holder — reversing D48's "valid credentials retain full access" — which makes `403` reachable per D43. Two invariants live in the evaluator rather than only in validation: anonymous callers never reach `.notedthat`, and the credential holder always does, so a manifest that revokes everything is repairable through the API rather than only through the bucket. Anonymous `write`/`delete` grants refuse startup and are inert if they reach the evaluator anyway. Absent `access` means the credential holder may do everything and anonymous callers nothing, so upgrading leaves credentialed reach untouched. Policies load once at startup and need a restart to change. Because filtering is per key, a listing page may be shorter than `limit` while still reporting `truncated`: clients page off `next_cursor`, never off page length. MCP inherits the credential holder's rules by way of the API. |
| D52 | Browse surface | **Server-rendered read-only HTML at `/browse`, over the same rules as every other surface.** `GET /browse/` lists knowledge bases the caller can see; `/browse/{kb}/{prefix}/` renders one directory level, synthesised from keys (D40). `list` gates a page and `read` gates each row's link, decided per key because a glob-scoped grant can make a directory listable but only partly readable. Object links point at the existing `/api/v1` representation — no second download path, no Markdown rendering. Anonymous denials answer `404` so the status cannot be used to enumerate private prefixes; a credentialed denial answers `403`. `.notedthat` is rendered for nobody. At a 10 000-key cap the page renders what it read with a visible notice rather than failing, unlike WebDAV's `507`, whose consumer would mistake a partial listing for a complete one. No JavaScript, no accounts, no editing, no search. |
| D53 | OIDC identities, group and user subjects, deny rules | **Identity is delegated to an OIDC issuer; the manifest names its groups; a rule can deny.** With `NOTEDTHAT_OIDC_ISSUER` + `NOTEDTHAT_OIDC_AUDIENCE` set, any bearer that is not the service token is verified as a signed JWT (`RS*`/`ES*`, never `HS*`) against the issuer's JWKS — discovered at startup and refused on failure per D39, cached by `kid`, refetched at most once per 30 s and refreshed after an hour — and becomes a *user* principal: subject from the configurable username claim (`preferred_username`, falling back to `sub`), groups from the configurable groups claim (`groups`; array, string, or Zitadel's role-keyed object). NotedThat mints nothing and holds no session: an opaque access token is refused, so Authelia and Zitadel are configured to issue JWTs. A rule's `who` is now `anyone`, `signed-in` (the service token *and* every user), `group:<name>` or `user:<name>`, and a rule carries `may` **or** `may_not`: a verb is allowed on a key when some matching `may` rule covers it and no matching `may_not` rule does — both unions, so D51's order-independence holds. `.notedthat` is reachable only by the service token, never by a user however broad their grants, because the manifest carries the policy, group names included; scoping any subject to it fails validation. One `Authenticator` serves every surface; `Bearer` is accepted everywhere, `Basic` on WebDAV only. MCP acts as its caller by forwarding the presented bearer on its loopback call (D59 extends this to forwarding nothing for an anonymous caller), and with `NOTEDTHAT_OIDC_RESOURCE` set the server publishes RFC 9728 metadata and names it in `WWW-Authenticate` on every `401`, which is how an MCP client finds the authorization server. Amends D21, D22 and D51; supersedes D27. |
| D54 | Multi-KB MCP search | **MCP `search` takes `kb` as a list and fans out inside the tool; results are grouped per knowledge base and never merged.** The HTTP route stays single-slug: the tool sends one `POST /api/v1/knowledgebases/{kb}/search` per slug, concurrently, as the calling identity (D53), and answers `{results: [{kb, hits}], skipped}` with one group per slug in request order, each keeping its own ranking, every hit naming its knowledge base. `limit` is per knowledge base. **No cross-KB score is published**: a hit's `score` is RRF computed inside one Qdrant collection (D14), a function of the hit's position there, so every knowledge base's top hit scores the same whatever its relevance and a merged sort would be an arbitrary interleave; a ranked cross-KB view needs a comparable component score from the server first (#126) and can be added as a new field without breaking this shape. `kb` is always a list, searched at most 8 at a time — a bare string is rejected, a duplicate is `invalid_request` — and an omitted or empty `kb` means every knowledge base the caller can see, discovered by the same `GET /api/v1/knowledgebases` that `list_knowledgebases` makes. A listing shows a knowledge base the caller holds *any* grant in (D51), not necessarily `search`, which sets the two failure rules: with an **explicit list, any refusal fails the whole call** and the error names the slug (`not_found` for an undeclared slug, `forbidden` for a denied one); with **`kb` omitted, a `forbidden` or concealed `not_found` drops that knowledge base into `skipped`** and the call succeeds, while any other failure still fails it, named. Every other tool stays single-KB. Amends D25. |
| D55 | Object change events | **Every object change is published as an event; subscribers stream them over SSE with durable, cross-replica replay from a configurable log.** `GET /api/v1/knowledgebases/{kb}/events` streams `object.written` / `object.deleted` as `text/event-stream`, each with an adapter-owned id that clients hand back as `Last-Event-ID`; a position the log no longer retains answers `410 gone` rather than silently resuming from now. Publish happens in `notedthat-write` immediately after storage acknowledges and before the D38 enqueue (D65), so every surface is covered by one hook — and a write whose publish is refused is enqueued for indexing all the same, the index never depending on the broker (§5 principle 9), the refusal being the write's answer and its retry re-announcing and re-indexing; on the `fs` backend the indexer worker announces detected changes from the `HEAD` it already performs, with a bounded last-seen stamp per key so the watcher's echo of the server's own write (D50) is not announced twice. Events are filtered per subscriber with the D51 evaluator and `Verb::List`, deletions included; anonymous subscribers are honoured under `anyone` rules. The log is selected by `NOTEDTHAT_EVENTS_BACKEND` under the §6.5 policy: `none` (default, route answers `404`), `memory` (process-local ring, replay across reconnects only — the honest choice for `fs`, one process per root by D49) and `nats` (one JetStream stream, the stream sequence as the id, shared by every replica). A publish that fails after the bytes are stored answers `503 backend_unavailable` with `Retry-After: 5` exactly as D38 does, making the client the retry mechanism and delivery at least once; an unreachable broker refuses startup (D39) and fails `/readyz` at runtime, as the `events` check of D64. MCP writes are attributed by an informational `X-NotedThat-Source: mcp` header the MCP server sets on its API calls. Webhooks and MCP `subscribe` are deferred and can ride the same log (§7.4); the indexer's `object.indexed` / `object.index_failed` ride it since D65, and MCP resource subscriptions consume it since D66. |
| D56 | Search window and ordering | **The searcher fetches the whole fused candidate set and ranks it itself; hits are ordered by score, then object key, then byte offset.** Each prefetch arm's depth is the request's `limit` — `10 × limit` when a key filter is applied client-side, after fusion (the request's `object_key_prefix`, which qdrant-client 1.15 cannot express natively, or the caller's `search` grant) — with a floor of 20, raised to 100 under a Qdrant-native payload filter (§8.6), and a cap of 250. The fused `limit` sent to the backend is twice the arm depth: the fused set is the union of the two arms, so the backend never truncates and never resolves a tie. Fusion can only rank what the arms returned, so a fused `limit` above twice the arm depth is inert — which is how a `10 × limit` over-fetch behind arms of 20 starved every prefix-scoped search (#68); deriving both numbers from one computation is what keeps them from drifting apart again. The caller's `search` grant is handed to the searcher as an opaque key predicate (`notedthat_indexer::KeyPredicate`) and applied together with `object_key_prefix` before the page is cut, so a narrow grant is served from the whole window rather than from an already-truncated page; the HTTP route keeps a second pass as a backstop, so a searcher that ignored the predicate would starve rather than leak. Hits are then ordered by RRF score descending, `object_key` ascending (byte order), `byte_start` ascending — total, since `(object_key, byte_start)` names one chunk — and cut to `limit`, so the same query against an unchanged index returns byte-identical hits, membership at the `limit` boundary included (#128). Residual: a tie exactly at an arm's own edge can still change which candidate enters the set. Amends D14, D41. |
| D57 | Single byte range per read | **A `Range` header names one byte range; a range set is refused with `400 malformed_range`.** `parse_range_header` returns one `ByteRange`, and `Storage::get_object` / `get_object_stream` take `Option<ByteRange>`, so no layer below the HTTP surface can be handed a set it cannot honour. Before this, the parser accepted `bytes=0-4, 10-14`, the route forwarded the vector, and every adapter served the first range as a `206` with a single `Content-Range` — a silent short read (#63), since a `206` for several ranges must be `multipart/byteranges` (RFC 7233 §4.1). Nothing above the adapters can render that: `ObjectRead` carries one `content_range`, and S3 has no multi-range `GET`. Refusing mirrors `lines=`, which already rejects a range set; ignoring the header and serving the whole object with `200` is also RFC-conformant but hands a client that asked for ten bytes the entire object, and makes single- and multi-range requests diverge without an error. `multipart/byteranges` stays out of scope for v1. Amends D6. |
| D58 | Tenant slug alphabet | **Tenant slugs are `[a-z0-9]{1,20}` — no hyphen — so `nt-{tenant_slug}-{kb_slug}` is injective.** With hyphens allowed in both slugs the encoding was ambiguous: `(acme, my-notes)` and `(acme-my, notes)` both derived `nt-acme-my-notes`, and since D49 the same name is the `fs` backend's per-KB directory, so the collision would have been a cross-tenant data leak the moment a second tenant existed — every layer above `derive_bucket_name` assumes the name is unique per pair. Forbidding the hyphen in the tenant slug makes the first hyphen after `nt-` always end the tenant, and was chosen over a new separator, a length prefix or a hash because it changes **no existing name**: the only tenant in the field is `default`, so no bucket and no directory moves. Knowledge-base slugs keep their hyphens (D24). The 63-char budget (§6.6) is unchanged. Amends D20. |
| D59 | Anonymous MCP | **`/mcp` admits a caller with no credential exactly when the manifests would admit one, and forwards nothing for it.** A request with no `Authorization` header is the anonymous caller when at least one declared knowledge base grants `anyone` some verb, decided once at startup like the policies themselves; the loopback API call then carries no credential, so the `anyone` rules bind the tool call exactly as they bind a direct anonymous request — no evaluator in the MCP layer, in keeping with §5 principle 10. `initialize` and `tools/list` succeed and advertise all ten tools (grants are per knowledge base and per path, so a filtered list would be a false signal); `list_knowledgebases` names what anonymous discovery names; a denial is the API's concealed `404` surfaced as `not_found`; a mutating tool is `unauthorized`, since no anonymous caller may write. A supplied credential that does not verify is `401` in every mode and never becomes the anonymous caller. When no knowledge base grants `anyone` anything, a missing credential stays `401` with the bearer challenge, because that challenge is what an OAuth-capable MCP client acts on — which is also why the operator override `NOTEDTHAT_MCP_ANONYMOUS=never` exists: on a deployment with public knowledge bases *and* an identity provider, `auto` admits an OAuth client anonymously and never prompts it to sign in, and `never` restores the prompt at the cost of anonymous MCP. The other surfaces are unaffected by the setting. `/llms.txt` describes the rule beside the API and WebDAV (#127). Amends D31 and D53. |
| D60 | Knowledge base description | **A manifest may say what its knowledge base is for, and listings show it.** `.notedthat/manifest.json` gains an optional `description`: one line, ≤ 500 Unicode code points, not blank, no control characters — an inline label an agent reads before choosing where to search (#98), so it is bounded and single-line by validation rather than by convention. It is written by an operator, never derived from the objects, and loaded with the access rules as one startup snapshot (D51); a manifest outside the limits refuses startup (D39) and is refused with the same message by every write path that can store it — the shared write path (§3.1) checks a body bound for `.notedthat/manifest.json` on `PUT`, `PATCH`, replace, and WebDAV `PUT`/`COPY`/`MOVE`, so a bad description is met now and not by whoever restarts next — and one without the field is unchanged. `NOTEDTHAT_KBS` stays slug-only: words live in the manifest, next to the rules that govern the same knowledge base. `GET /api/v1/knowledgebases` now returns `[{kb_slug, display_name, description?}]` in place of bare slugs, and MCP `list_knowledgebases` returns the same entries — the shape §6.10 promised, less `perms`. Visibility is the listing rule: a knowledge base the caller cannot see contributes no entry, so nothing about it is described. Amends D25. |
| D61 | MCP read metadata and read budget | **A `read` answers with the version and totals of the very response its text came from, beside the text; and no MCP object read fetches more than `NOTEDTHAT_MCP_MAX_READ_BYTES`.** `structuredContent` carries the `text` (the same as `content[0]`, so a host that hands the model only the structured half still hands it the document), `etag` (verbatim, quoted — the `if_match` for a following `edit` or `replace`), `content_type`, `bytes_returned`, `total_bytes`, the slice's `byte_start`/`byte_end` (exclusive, as the argument is; the end is `byte_start + bytes_returned` from the body, since a header's inclusive end cannot spell the empty slice at offset 0) and, on line reads, `total_lines` with the slice's lines — parsed from the same GET's `ETag`, `Content-Range` and `X-Content-Range-Bytes`, a `200` being the whole object by definition; anything the backend did not say is `null`, nothing is invented, and a malformed header costs its fields and never the read. The text stays alone in `content`, so a client never has to tell an object from its own description (the promise of #37, which D45 shipped the arguments for and this completes). The tool declares the shape as its `outputSchema`. The budget is on the bytes fetched from the API, default the API body cap (16 MiB) so anything written through the API reads back whole — only a WebDAV upload or a file placed in an `fs` tree (D49/D50) can be larger; a declared `Content-Length` over it is refused before a byte of body is read, and a body without one is read chunk by chunk and refused the moment it crosses, so the server never holds more than the budget plus one chunk. The refusal, `response_too_large`, states the size and the budget and names the slice arguments; it is not the API's `413`, which is about a request body. A binary resource is base64-encoded on top of the budget, and a text `read` carries its text twice (`content` and `structuredContent`), so its response is up to twice the budget; both expansions are documented rather than a second knob. `resources/read` and the copy inside `move` share the bound. Both transports take the setting; the stdio adapter applies it for its own process. Amends D25, D37, D45. |
| D62 | Per-knowledge-base index health | **Each knowledge base reports one aggregate index state — `healthy`, `indexing`, `backpressured`, `stale` or `failed` — at `GET /api/v1/knowledgebases/{kb}/index` and MCP `index_status(kb)`; job internals stay unexposed.** D38's queue is unchanged: no durable state, no job ids, no retry. What changed is that every producer now stamps a small in-process record (`notedthat_indexer::IndexHealth`) as it goes — the write paths on enqueue and on a queue-full refusal, the worker when an event succeeds or fails, the `fs` bridge when it asks for a rescan and when a pass completes — and one route folds that into a state a caller can act on (#97). `pending` is the difference of two monotonic per-base counters, enqueued and completed, so it covers queued *and* in-flight work (an object being embedded is not searchable, so its base is `indexing` until the outcome lands) and a completion the worker records before the writer's enqueue — the two run on different tasks — is a transient zero rather than a count that never drains. The record is one process's: with several replicas the route answers for whichever took the request, and the docs say so. Precedence, most severe first: `failed` (the most recent outcome for this base failed, or the worker loop has ended — `worker: "stopped"`, which the `INDEX_QUEUE_CLOSED` arm also records instead of silently succeeding), `stale` (`fs` only: a rescan was requested by `FS_WATCH_LOST` or an overflow, or the startup pass has not completed, so changes may be unobserved), `backpressured` (a refusal within 30 s, or the shared queue full right now — full is full for every base), `indexing` (events pending), `healthy`. The view carries counts, timestamps (RFC 3339), the queue's depth and fixed capacity, the failure's one-line summary bounded to 200 characters, and on `fs` the last completed `ReconcileReport`; never bytes, credentials or queue contents. Visibility is the listing rule (D51): whoever would see the base listed may ask, `404`/`403` otherwise as everywhere; the failed object's key is included only for a caller who may `list` it, the failure summary — the pipeline's own first line, which names the endpoint it could not reach and as often the key it was on — only for a caller who may `list` the whole base, and the reconciliation pass's counts — which describe every key — only for such a caller too (its time for everyone). A pass over one prefix reports that prefix as its `scope`, so its counts are not read as the base's; `scope` is a key prefix, so the pass — prefix and counts together — is shown only to a caller who may `list` that prefix. on `s3`, `stale` is the window between boot and the first completed pass, and `last_reconcile` is that pass or the operator's latest (D67); a watch lost to `max_user_watches` still reports `stale` only until its rescan completes, since the bridge cannot tell the two rescan causes apart. `/readyz` stays process-level (#81). Amends D4, D38; extends D25's tool surface to 11. |
| D63 | MCP stdio transport removed | **`notedthat-mcp-stdio` is gone; MCP is served over streamable HTTP at `POST /mcp` and nothing else.** The stdio binary existed because the first MCP clients could only spawn a subprocess (D31). Since M8 the server has mounted the HTTP transport itself, every client this project documents speaks it or authenticates through OAuth against it, and the generic `mcp-remote` bridge covers the clients that still only spawn a command — a solved problem outside this codebase. Keeping the binary meant a second transport to document, a second credential pair (`NOTEDTHAT_URL`/`NOTEDTHAT_TOKEN`), a second install story (Makefile, image layer, cargo-dist archive contents) and a `Fallback::ConfiguredToken` path in `notedthat-mcp` that let a call without a request-scoped caller act as the process's own token — exactly the escalation the HTTP transport refuses. The crate, the bin target, the Makefile, the `NOTEDTHAT_URL`/`NOTEDTHAT_TOKEN` settings and the stdio-only e2e suites are removed; the tool-level e2e coverage those suites carried now drives `POST /mcp` on the same in-process server. `docs/CLIENTS.md` shows the `mcp-remote` shape for command-only clients (Claude Desktop without an IdP, Zed). A `notedthat-server mcp-stdio` subcommand bridging to its own `/mcp` was considered as a zero-dependency local path and declined: one transport, one rule. The crate's published versions stay on crates.io as they are — release-plz publishes workspace members only, so the next release simply does not include it; nothing is yanked, and existing installs and lockfiles keep working (RELEASING.md, *Removing a crate*). Supersedes D29; amends D28, D31; the workspace is 11 crates. |
| D64 | Readiness covers every backend | **`/readyz` is `503` while the storage backend, Qdrant, or a configured event broker is unavailable, and a Qdrant collection that cannot be ensured at startup is fatal.** A background task in the server probes storage (`HeadBucket` on the bucket of the knowledge base whose slug sorts first under `s3`; a stat of its directory under `fs`) and Qdrant (`HealthCheck`) every `NOTEDTHAT_READY_PROBE_INTERVAL_MS` (default 5 000), each probe bounded by that same interval, and publishes a snapshot the route reads. The route never probes inline, so a probe storm from an orchestrator costs the backends nothing, and an outage is reported within two intervals; the snapshot starts from "ready" because provisioning has just reached both backends. The body lists each check with its backend and a reason from a closed vocabulary (`timeout`, `unreachable`, `not_found`, `disconnected`); the backend's own error is logged once per transition (`READINESS_LOST` / `READINESS_DEGRADED` / `READINESS_RESTORED`) and never returned, because the route is unauthenticated and a client error can quote an endpoint. Readiness is about the backend, not the data in it: a probe the backend *answered* with "not found" (the probed bucket or directory deleted after startup) leaves the replica ready — it is up, and every other knowledge base keeps serving — and is shown as `degraded` rather than taking the deployment out of the load balancer for a per-knowledge-base problem (#152 makes that a per-KB `404`). Covered: storage reachability, Qdrant reachability, event broker connection. Not covered: the `fs` watcher (D50 — a lost watch is logged, not a readiness failure), the embedder, and index freshness, which is per knowledge base and lives at `GET /api/v1/knowledgebases/{kb}/index` (D62), not on a process-wide probe. Startup no longer warns and continues when `ensure_collection` fails; it exits non-zero, which is what D39 always said. Amends D39, D44 and D55. |
| D65 | Index outcome events | **The indexer publishes what it did with an object, on the same log as the write that caused it.** When an `Upsert` or `Refresh` completes, the worker publishes `object.indexed` — `etag` (the version now in the index: what `HEAD` reported and the staged stream confirmed), `mime` (as `HEAD` reported it), `chunks` (points now representing the object), `source: indexer` — or `object.index_failed`, with the same bounded one-line `summary` the D62 health view records (one `bound_summary`, shared) and `etag`/`mime` only when `HEAD` had succeeded. A subscriber that saw `object.written` for an `ETag` may search for those bytes once `object.indexed` names the same `ETag` — or a later write's: an upsert names a key, not a version, and indexes whatever is current when the worker reaches it, so of two writes in quick succession both upserts index and announce the second version and the first is never announced as indexed; a verdict for a newer version supersedes the older write. That, not polling `/index` or retrying the search, is the completion signal for one write, and `object.index_failed` is the signal that it will not come. Silence is deliberate where the index gained nothing: an explicit tombstone, the pipeline's own tombstone for a non-indexable or empty object or a key gone on re-read, and a `Refresh` skipped because the `ETag` was already indexed (D50) publish nothing — `object.deleted` and `/index` cover them, and there is no `object.unindexed`. Any failure of an `Upsert` or `Refresh`, at whatever stage, publishes `index_failed`; a failed tombstone does not. The health record is stamped before the event is published, so a subscriber who reads the event and then asks `/index` never finds the view behind the stream. Outcome events bypass the worker's last-seen dedup — they are not change announcements and must not be suppressed against the stamp the write already announced — and a refused publish is logged, never failing the indexing. They are filtered like every event (D51, `Verb::List` on the key; `?event=indexed` / `index_failed`, an `object.` prefix accepted; `?mime=` on the content type `HEAD` reported, so an `index_failed` without one never matches a `mime` filter), and `index_failed.summary` is withheld on the stream, per frame, from a subscriber whose `list` grant does not span the whole knowledge base — the D62 rule for the same string. So that both events for one write reach the log in the order a subscriber expects, `notedthat-write` now publishes before it enqueues (`publish`, then the D38 `try_send`, on writes and deletes alike): the write is on the log before its `Upsert` is in the queue, and `object.indexed` for that `ETag` follows it by construction. D38's at-least-once holds either way; what moves is which side a refused write lands on — a write the full queue turns away has already been announced, and its retry announces it again; a write whose publish the log refused is enqueued regardless (the index is what search depends on, the broker is optional), so it is searchable before the retry and the retry's `object.written` may follow an `object.indexed` for the same version — an orphan verdict, tolerated like a duplicate. A deletion whose publish failed is likewise tombstoned at once. Amends D55, D62. |
| D66 | MCP resource subscriptions on stateful sessions | **`/mcp` is a stateful streamable-HTTP endpoint, and a session may subscribe to `notedthat://` resources; the notifications are the D55 events, consumed by the MCP server as its own caller.** `initialize` opens a session whose `Mcp-Session-Id` every later request carries (absent on a `POST` → `422`, on `GET`/`DELETE` → `400`; unknown → `404`); every `POST` answer is SSE-framed; `GET /mcp` is the notification leg and `DELETE /mcp` ends the session, both authenticated exactly as `POST`, auth outermost so a missing credential is `401` before any session answer; at most 256 sessions per process (`503`, `Retry-After: 5`, a constant — a knob is a follow-up); a session idle for five minutes is closed, and one with live subscriptions — a `resources/subscribe`, not merely a `resources/list` — is kept alive by a server `ping` every 60 s for as long as it still holds one and the client answers, the keeper standing down when the last subscription goes so the session idles out normally, since rmcp's idle timer counts messages, not an open `GET`; a client that stops answering has no working notification leg, so its subscriptions are dropped and any later `subscribe` on that session is refused rather than answered `Ok` and left silent. `resources/subscribe` on a URI the caller may `list` — probed as the caller with a prefix listing, the predicate the events route applies per event, so a knowledge base it may not list is `forbidden` (or the concealed `not_found` for an anonymous caller — the API decides) and a key it cannot see, or that does not exist, is `not_found`, matching `resources/read` — registers the key and answers once the knowledge base's stream is open; `notifications/resources/updated` follows every `object.written` / `object.deleted` for it, `resources/unsubscribe` stops them and is idempotent. `object.indexed` / `object.index_failed` (D65) do not notify: the resource's text did not change. `listChanged` is lazy — a session is watched only from its first `resources/list`, and only for the knowledge base each page actually listed, one held-open loopback stream per (session, knowledge base) rather than one per knowledge base the caller could see — and `notifications/resources/list_changed` is coalesced to one notification at once and at most one more per second per session; it fires on every write, since neither the API nor storage tells a create from a modify: a hint to re-list, not a diff. Both capabilities are advertised only when an events backend is configured — the one the server runs on, not the setting — and answer `method_not_found` otherwise. Delivery is a per-session, per-knowledge-base loopback `GET …/events` as the subscriber — bearer or nothing — so D51 filtering, concealment and the `anyone` rules hold with no evaluator in the MCP layer (D59), resuming with `Last-Event-ID` across reconnects and dropping the knowledge base's subscriptions silently when the route refuses the credential. Subscriptions live and die with the session: no replay, no persistence. Not done here, and the reason D68 exists: a session is not bound to the credential that opened it (rmcp binds nothing), so another authenticated caller holding a session id can attach that session's `GET` leg and read its notification stream — the object URIs the session owner subscribed to and when they change, for objects that caller may not read itself; tool calls are unaffected, each acting as the credential presented on its own request. Session ids are rmcp-generated UUIDs, so this was not guessable; it was a confidentiality follow-up, not tidiness. **Amended by D68**, which binds the session and makes the 256 a setting. Amends D31, D37, D55. |
| D67 | S3 index reconciliation | **The `s3` backend compares every knowledge base's bucket against the search index once at startup (`NOTEDTHAT_S3_RECONCILE`, default on) and whenever the service token `POST`s `…/index/reconcile`; nothing is watched.** S3 has no change feed NotedThat can subscribe to portably, so a pass is the only mechanism, unlike `fs` (D50). The comparison itself moved to `notedthat_core::reconcile` and both backends share it: one `ListObjectsV2` per thousand keys — each entry now carries its `ETag`, which the adapter used to discard — against one scroll of the index, then an `IndexEvent::Refresh` for every key the two sides disagree on. Unchanged objects are neither read nor embedded; a key gone from the bucket becomes a tombstone through the worker's missing-object path, never one enqueued from the pass, for the reason D50 gives. Passes send with D50's blocking discipline, so a large pass fills the queue and writers see `503 Retry-After` until it drains, as on `fs`. One pass per knowledge base at a time: a second request answers `409 conflict`, because the running pass is already past keys a later change could touch and does not satisfy it; the startup pass leaves a knowledge base to a requested pass already holding it. The route is the operator's alone — anonymous `401` before any slug is looked at, an identity `403` whatever the manifests grant it, an undeclared slug `404` — and answers `202` with the result in `GET …/index` `last_reconcile`, as a startup pass's is; the `fs` backend answers `404`, its watcher covering the same ground. A pass that cannot read the index (`S3_RECONCILE_SKIPPED`) or list the bucket (`S3_RECONCILE_INCOMPLETE`) enqueues nothing and leaves the knowledge base `stale`. Amends D50 and D62; amends D42's "no reindex endpoint" (§6.12) for this incremental form only — a full rebuild stays deferred (§7.4). |
| D68 | MCP sessions belong to the principal that opened them | **A session id is issued to one principal and answers only to it: a request presenting a session id whose owner is not the request's own principal is answered exactly as an id the server does not know.** rmcp binds nothing to a session and its `create_session` receives no request at all, so the record is NotedThat's: the `bind_session` layer reads `Mcp-Session-Id` off the `initialize` **response** — the first place the id is visible to it — and stores the session against a key derived from the `Principal` the auth layer already resolved. That key is the identity provider's **subject**, never the bearer and never the groups: the ordinary reason a long-lived session's credential changes is OIDC access-token refresh, which a client performs every few minutes underneath one session and which may carry different groups, so binding either would cost a client its own session (the same reasoning D66 records for forwarder keys). The service token, having no subject, is its own owner. The check covers every method — a `POST` on another principal's session, its `GET` notification leg, and a `DELETE`, which rmcp answers `202` for *any* id and which therefore let anyone holding a session id end it. The refusal **is** rmcp's own unknown-session answer rather than a copy of it: the layer substitutes the nil UUID — an id rmcp cannot have issued, since it mints them with `Uuid::new_v4()` — onto the request and lets rmcp answer. Synthesizing the answer instead short-circuited ahead of every precondition rmcp validates *before* its own session lookup (the `Host`/`Origin` allow-list, `Accept`, `Content-Type`, the JSON parse, `MCP-Protocol-Version`), so a malformed request told the two apart and named which session ids exist; falling through makes them identical by construction. That is a deliberate exception to D38's JSON refusal shape, and on `DELETE` a refusal stays indistinguishable from an unknown id, since answering `202` while doing nothing is the concealing answer and lying about `404` would not be — rmcp's `close_session` on an id it does not hold removes nothing, so the owner's session is untouched. Every anonymous caller is **one** owner, because nothing distinguishes two of them: on a deployment admitting them (D59) one anonymous client can still attach another's leg, and since both hold identical read authority — whatever `anyone` grants — what leaks is which public URIs the other subscribed to and nothing either could not already read. rmcp offers no close callback and a session also ends through the idle timeout, an init timeout or a worker error, so the record is reclaimed on `DELETE` directly (rmcp removes the session before answering) and otherwise by reconciling against rmcp's own table before each new session is recorded, which bounds the record by that table for one pass per `initialize`; a stale record can only ever refuse an id rmcp would refuse anyway, session ids being UUIDs that are never reused. The per-process bound is now `NOTEDTHAT_MCP_MAX_SESSIONS` (default 256; zero or a non-number refuses startup under §6.5), still **soft** — the count and the session's creation are not atomic — and still per process: budgeting sessions per principal, for which this record is the prerequisite, remains open (§7.4). Amends D66. |
| D69 | Prometheus metrics listener | **Metrics are exported in the Prometheus text format at `GET /metrics` on a listener of their own, off unless `NOTEDTHAT_METRICS_ENABLED=true`, bound to `127.0.0.1:9090` by default.** The instrumented crates record through the `metrics` facade alone — its macros compile to a no-op until a recorder is installed, so no crate but `notedthat-server` gains an exporter dependency, no feature flag gates instrumentation, and every existing unit test keeps running against no recorder at all; the consequence an operator must know is that nothing accumulates while metrics are off, so enabling them after an incident starts the counters at that restart rather than recovering a history. `notedthat-server` alone depends on `metrics-exporter-prometheus`, with `default-features = false`, because this server opens its own socket and renders from its own `axum` route and neither that crate's HTTP listener nor its push gateway is wanted. The recorder is process-global (`install_recorder` fails if one is already installed) and is therefore installed at most once behind a `OnceLock`, so a test binary that starts several servers shares the first handle rather than failing the second start; histograms are given explicit buckets from one table in `notedthat-core`, because the exporter otherwise renders them as summaries whose per-process quantiles cannot be aggregated across replicas — which is exactly what §7.3's search P95 needs — and a histogram added without buckets fails the build rather than appearing in a shape no `histogram_quantile` can read; no idle timeout is set, since a series expiring between scrapes reads as a counter reset and invents a spike in `rate()`. `NOTEDTHAT_METRICS_ENABLED` is `true` or `false` exactly, under the §6.5 policy — a deployment that wrote `yes` and got `false` believes it is being scraped and is not, which looks like a healthy one until someone needs the graph — and supplying `NOTEDTHAT_METRICS_LISTEN_ADDR` while metrics are off refuses startup naming both settings, in the shape D19's unselected-backend check uses, because an address nothing binds leaves an operator waiting on a port nothing listens to. This is deliberately not a reversal of D39. That decision moved `WebDAV` and MCP onto `NOTEDTHAT_LISTEN_ADDR` and refuses startup for their old listener variables (`REMOVED_LISTENER_ENV_VARS`, untouched here) because they are authenticated **product** surfaces every client has to reach, and a second port was one more thing to tell every client, proxy and firewall about. `/metrics` is the opposite on every axis: no client ever calls it, it carries no authentication — the exporter has no notion of a principal and a scraper has no credential to present — and it must therefore be unreachable from wherever the product listener is reachable. A separate socket bound to loopback makes that a property of the bind rather than of a middleware anyone can misconfigure or a proxy rule anyone can forget; an operator who wants it scraped from another host says so explicitly and pays one port for it, a cost borne by the operator and not by clients. `GET /metrics` on the product listener is `404` with or without a credential, and every path but `/metrics` on the metrics listener is `404` too, so the operator socket is not a second product surface. Series carry only bounded labels — knowledge base slug, surface, route *template*, method, status, operation, outcome, backend, phase — and never an object key, a principal, a credential, a search query, a request id or a subscriber's filters, because an exposition is retained for months by whoever scrapes it and read by people who were granted nothing (D51). Three labels carry that: `route` is axum's matched pattern and never the path, since the object and `WebDAV` routes carry keys in theirs and an unrouted request's URI is attacker-chosen; `method` is an allow-list, since `http::Method` accepts arbitrary extension tokens and the label is recorded before any `405`; and `surface` reads the informational `x-notedthat-source` header only under `/api/v1`, so one MCP tool call is not counted once as `mcp` for the transport hop and again as `api` for the loopback call it makes. `tests/metrics_labels_e2e.rs` proves it by putting named sentinels into a running deployment — a unique object key, a deleted and a failed one, the service token, the `WebDAV` principal and password, the search text and a subscriber's prefix — exercising writes, a deletion, a search, an index failure, an SSE subscriber and a queue-full refusal, then asserting each is absent while the `kb` label is present, so an empty exposition cannot pass, and bounding label length and series count against the leaks nobody named. Request duration is time to response *head*, not to last byte: the events route returns at once and streams for hours, so body-completion timing would put hour-long observations in the histogram and pin the in-flight gauge at the subscriber count; a live stream's cost is `notedthat_events_subscribers` and a large read's is `notedthat_storage_*`. The backends are metered by wrapping them once where they are assembled rather than inside each adapter, so one implementation covers `s3`, `fs` and the in-process doubles, the `op` label set is the trait's method list and therefore closed by the compiler, and the adapter crates keep no observability dependency (D28); `NotFound`, `NotModified`, `PreconditionFailed` and `RangeNotSatisfiable` are counted as outcomes and not errors, since they are how an idempotent delete and every conditional request work and counting them would make the error rate track client caching. Every metered call records from `Drop` rather than after its await, because a caller that gives up — an abandoned search, a connection dropped mid-`GET` — drops the future and nothing after the await runs: the families would otherwise disagree by construction, and disagree worst on slow calls, so a duration histogram would fall silent under the very condition it exists to reveal. The outcome a call keeps when nothing sets another is `cancelled`, which makes the gap a series an operator can read rather than an absence. The listener binds before the product one, so an occupied metrics port refuses startup per D39 rather than leaving the product surface up with no scrape target, and it drains on the server's own `CancellationToken`. Settings are inventoried in §6.5 with every other runtime variable and the catalogue is in §6.15; the route is absent from §6.13's table by construction, because that table is the product listener's surface and this route is not on it. Amends D39; closes the search-latency half of §7.3. |
| D70 | S3 conditional writes are checked at startup | **On the `s3` backend each knowledge base's bucket is asked once at startup, before provisioning touches anything else, whether it enforces the preconditions NotedThat forwards on a `PUT`; a bucket that does not refuses startup unless `NOTEDTHAT_S3_ALLOW_UNENFORCED_CONDITIONAL_WRITES=true` accepts the risk.** NotedThat forwards `If-Match` and `If-None-Match` verbatim and adds no lock of its own (§8), so optimistic concurrency on every surface — `PUT`, `PATCH`, MCP `edit`/`append`/`replace`, WebDAV — is exactly as good as the backend's enforcement, and a backend that parses the headers and stores the object anyway (§8.1: Garage, SeaweedFS < 4.09) loses one of two concurrent writes with a `200` to both. Nothing in a normal request can tell, so the check asks directly: `S3Storage::check_conditional_writes` stores a scratch object at `.notedthat/conditional-write-probe-<uuid>`, overwrites it once with `If-None-Match: *` and once with an `If-Match` naming an `ETag` no object can carry, and deletes it whatever the answers were — private under D48 and skipped by reconciliation (D67) if a delete ever fails. `412` to both is enforced; a `2xx` to either is not; `501 Not Implemented` is reported separately (every conditional write would fail rather than be lost) but refused the same way; any other answer is the storage error it is and refuses startup as provisioning would. It is an inherent method of the S3 adapter rather than a `Storage` method: `Storage::probe` is read-only by contract, and the `fs` backend enforces preconditions itself (D49). The client is called directly, so the check is not counted in `notedthat_storage_operations_total`. A refusal names the knowledge base, the header that was ignored, this section and the opt-out. With the opt-out, the finding is logged as `S3_CONDITIONAL_WRITES_NOT_ENFORCED`, set as `notedthat_storage_conditional_writes_enforced{kb} 0` (`1` for a bucket that enforced them), and reported by `/readyz` as a `conditional_writes` check that is `degraded` (`preconditions_not_enforced`) for the life of the process — never probed again, and never a readiness failure, since every request is still served. One writer cannot provoke a failure that only appears under contention, so a backend that passes (RustFS under lock-timeout contention) can still lose writes under load; §8.1 remains the reference for those. Garage v2.1.0 does not pass: checked against it, it stores both a mismatched `If-Match` and an `If-None-Match: *` overwrite from a single writer, and is refused. Amends §8's "without capability checks", D39 and D64. |

---

## 3. Core Capabilities `[DECIDED]`

- **Persist** content to per-KB S3 buckets (source of truth)
- **Index** content into per-KB Qdrant collections (rebuildable from S3)
- **Search** hybrid (BM25 + dense) per KB with payload filters
- **Multi-tenant-ready scoping**: v1 single tenant/static full access; v2 per-KB + per-prefix ACL
- **WebDAV** — read-write, `Range`-honoring, unified root
- **HTTP API** — read-write, byte-range aware
- **MCP server** — read-write, byte-range aware, streamable HTTP at `POST /mcp`
- **Byte-range reads** everywhere; search hits carry `object_key + byte_start + byte_end` for exact re-fetch
- **Pass-through optimistic concurrency** — conditional PUT headers forwarded to the backend verbatim (D9)

### 3.1 Single write path across three surfaces `[DECIDED]`
API `PUT`, MCP `write`, and WebDAV `PUT/MOVE/COPY/DELETE/MKCOL` all route through one internal `commit(kb, path, bytes, conditional_headers)` primitive. Path normalization, MIME sniffing, size limits, ACL check, S3 put (with client headers forwarded verbatim), and indexing-event emission live there — not per surface.

---

## 4. Architectural Sketch

```
                        ┌─────────────────────────────────────┐
                        │             Clients                 │
                        │  HTTP API │  MCP  │  WebDAV         │  ← all read-write
                        │           │       │                 │
                        │  Bearer   │  ↓    │  Bearer/Basic   │  ← MCP at POST /mcp
                        │  or OIDC  │  HTTP │                 │     wraps HTTP API
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
7. **Thin over the backend.** NotedThat does not simulate, compensate for, or hide backend capabilities. It forwards headers and status codes honestly. The deployer picks the backend; we document what each one does (§8.1).
8. **No local state that isn't derived.** No SQLite arbiters, no token denylist DB, no in-memory ETag mirrors. State lives in S3 (truth) or Qdrant (derived, rebuildable).
9. **Single binary, single process.** `notedthat-server` hosts HTTP API, WebDAV and MCP in one process. The only optional external service beyond storage, Qdrant and the embedder is an event broker (D55), and only when a deployment asks for cross-replica event replay; the default is none.
10. **MCP wraps the HTTP API, always.** The MCP transport goes through the HTTP API for business logic — never bypass to the storage layer directly. One source of truth for auth, ACL, and validation.
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
| `object_key`       | string   | S3 key |
| `chunk_index`      | int      | position of chunk in doc |
| `byte_start`       | int      | byte offset where chunk begins in source |
| `byte_end`         | int      | byte offset where chunk ends |
| `etag`             | string   | S3 ETag captured at index time — used for dedup / re-index detection. Content-derived on non-multipart PUTs (D36), so no separate SHA256 is stored. |
| `mtime`            | int      | last-modified Unix timestamp |
| `mime`             | string   | source MIME |
| `heading_path`     | string[] | markdown headings, e.g. `["Introduction", "Motivation"]` |
| `tags` | string[] | OKF frontmatter tags; empty for ordinary documents. |
| `okf` | object, optional | Concept ID derived from the path, type, optional title/description/resource, and tags. |

Payload indexes (ensured at startup on new and existing collections): `object_key`, `etag`, `mime`, `mtime`, `heading_path`, `tags`, `okf.type`. Existing objects require re-PUT to backfill new payload fields.

Search: `prefetch` on `dense` + `prefetch` on `sparse_bm25` fused via RRF. Both arms are deepened to the window the request needs and the fused `limit` is twice that depth, so the backend hands back the whole fused set and the searcher orders and cuts it (D56); the floor is 100 under a selective payload filter (§8.6).

### 6.3 Chunking pipeline `[DECIDED — D15]`

```
raw markdown bytes
     │
     ▼
[OKF metadata extraction]  ── use body for concepts; otherwise use the full file
     │
     ▼
[pulldown-cmark::into_offset_iter()]  ── (Event, Range<usize>); add body offset for concepts
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

Wiki-links `[[note]]` — `[POST-v1]` (candidate: `turbovault-parser` v1.5).

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

### 6.5 Storage backend config `[DECIDED — D19, D49]`

`NOTEDTHAT_STORAGE_BACKEND` selects the backend: `s3` (default) or `fs`. Parsed strictly — an
unrecognised value refuses startup rather than falling back, because a mis-selected backend
produces a deployment that looks healthy while reading an empty store. Variables belonging to the
unselected backend also refuse startup, naming every conflict, rather than being silently ignored.
`NOTEDTHAT_EVENTS_BACKEND` (§6.14) is selected under the same policy.

Filesystem env vars (`NOTEDTHAT_STORAGE_BACKEND=fs`):
- `NOTEDTHAT_FS_ROOT` — absolute path of the storage root. Required; no default, since a default
  would silently place data somewhere the operator did not choose.
- `NOTEDTHAT_FS_METADATA` — `sidecar` (default and, today, the only accepted value)
- `NOTEDTHAT_FS_FILE_MODE` / `NOTEDTHAT_FS_DIR_MODE` — octal modes for created files and
  directories; default `0644` / `0755`, so the tree stays readable to people and backup jobs
- `NOTEDTHAT_FS_ALLOW_LOSSY_NAMES` — start despite a case-folding or Unicode-normalizing
  filesystem (default `false`; see §8.1)
- `NOTEDTHAT_FS_WATCH` — watch the tree and re-index objects changed outside NotedThat
  (default `true`; D50). A watch that cannot be established refuses startup, naming
  `fs.inotify.max_user_watches` and this variable.
- `NOTEDTHAT_FS_WATCH_DEBOUNCE_MS` — how long a path must go quiet before a change to it is acted
  on (default `500`, accepted range `50`–`60000`)

The root is validated and exclusively locked at startup, immediately after the staging directory
and before any backend client is built.

Standard AWS S3 SDK config. No NotedThat-specific capability flags.

S3 env vars (`NOTEDTHAT_STORAGE_BACKEND=s3`):
- `NOTEDTHAT_S3_ENDPOINT_URL` (optional for AWS; required for MinIO/Ceph/SeaweedFS/Garage/RustFS/R2)
- `NOTEDTHAT_S3_REGION`
- `NOTEDTHAT_S3_ACCESS_KEY_ID`
- `NOTEDTHAT_S3_SECRET_ACCESS_KEY`
- `NOTEDTHAT_S3_FORCE_PATH_STYLE` (`true` or `false` exactly; usually `true` for non-AWS — anything else refuses startup rather than guessing `false` and failing on the first request)
- `NOTEDTHAT_S3_RECONCILE` — compare every knowledge base's bucket against the search index
  once at startup, re-indexing objects changed outside NotedThat (default `true`; D67). The
  on-demand pass, `POST …/index/reconcile` for the service token, is available either way.

Server listen config (not S3-specific — colocated here so all runtime env vars are in one place):
- `NOTEDTHAT_LISTEN_ADDR` — `host:port` the HTTP API binds to. Default `0.0.0.0:8080`. Standard `SocketAddr` parsing (IPv4, IPv6 in brackets, or hostname).

Metrics listener config (a second listener, and deliberately not the product one — D69):
- `NOTEDTHAT_METRICS_ENABLED` — serve the Prometheus exposition (`true` or `false` exactly; default `false`). Off means nothing is bound *and* nothing is recorded, so enabling it later starts the counters at that restart.
- `NOTEDTHAT_METRICS_LISTEN_ADDR` — `host:port` the metrics listener binds to. Default `127.0.0.1:9090`, loopback because the exposition is unauthenticated. Supplying it while metrics are off refuses startup, naming both settings.

At startup we log which endpoint URL we're pointed at. Operators are responsible for choosing a backend that supports the features their clients depend on — see §8.1 for guidance.

### 6.6 Bucket naming `[DECIDED — D20]`

```
nt-{tenant_slug}-{kb_slug}
```
- `tenant_slug`: 1–20 chars, `[a-z0-9]`, **no hyphen** (D58)
- `kb_slug`: 1–40 chars, `[a-z0-9-]`, no leading/trailing hyphen (§6.8)
- Injective: because the tenant slug has no hyphen, the first hyphen after `nt-` always ends the tenant, so two distinct `(tenant_slug, kb_slug)` pairs never share a bucket name — or, under the `fs` backend, a directory (D49, D58).
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
  "description": "Design notes, ADRs and meeting minutes of the platform team.",
  "created_at": 1782993600,
  "embedding": {
    "endpoint_url_hint": "https://api.openai.com",
    "model": "text-embedding-3-small",
    "dimensions": 1536
  },
  "qdrant_collection": "kb_my-notes_v1",
  "access": [
    { "who": "anyone",        "may": ["list", "read"], "under": ["public/**"] },
    { "who": "anyone",        "may": ["search"] },
    { "who": "signed-in",     "may": ["list", "read", "search"] },
    { "who": "group:editors", "may": ["write", "delete"] },
    { "who": "group:interns", "may_not": ["read", "search"], "under": ["hr/**"] }
  ]
}
```

The `(tenant_slug, kb_slug)` pair *is* the identifier — no separate UUID field. Manifest is a sanity-check record, not the source of truth for identity (D20, D24).

`description` is optional (D60): one line of at most 500 Unicode code points, not blank, with no control characters, stating what the knowledge base is for. It is operator-written metadata — never derived from the objects — validated at startup like `access`, and shown by `GET /api/v1/knowledgebases` and MCP `list_knowledgebases` exactly when the knowledge base is listed (D51).

`access` carries the knowledge base's access rules (D51, extended by D53). Each rule names:

| Field | Meaning |
|---|---|
| `who` | `anyone` (no credential), `signed-in` (any verified credential: the service token and every identity-provider user), `group:<name>` or `user:<name>` (an identity-provider user by group or by username claim) |
| `may` *or* `may_not` | any of `list`, `read`, `write`, `delete`, `search` — exactly one of the two |
| `under` | glob patterns over object keys; omit for the whole knowledge base |

A verb is allowed on a key when some matching `may` rule covers the key **and no matching `may_not`
rule does**. Both sides are unions, so the order of the rules never changes a decision. Unknown
verbs, unknown subjects, malformed patterns, a rule naming both `may` and `may_not` or neither, and
a rule scoping any subject to `.notedthat` all fail startup validation, so a typo cannot quietly
become a weaker policy.

Pattern syntax: `*` matches within one segment and never `/`; `**` matches whole segments and must
be an entire segment; `?` matches one non-`/` character; `{a,b}` alternates. `public/**` matches the
key `public` and everything beneath it. There is no escape character, so a key containing a literal
`*`, `?` or `{` cannot be matched.

An **absent** `access` field means the credential holder may do everything and anonymous callers
nothing — what every manifest written before D51 already meant. An **empty** array grants nobody
anything; the knowledge base is inert but still repairable, and startup logs `ACCESS_RULES_EMPTY`.

`.notedthat` is not addressable by a rule in either direction: only the service token reaches it,
always; anonymous callers and identity-provider users never do, whatever the rules say. The
manifest carries the policy — group names included — so that is the right default for users, and
the asymmetry is what keeps a manifest that revokes the operator's own access repairable through
the API with `NOTEDTHAT_API_TOKEN`. The repair takes effect on the next restart, since policies are
a startup snapshot.

The removed `public_read` field is ignored if still present. A knowledge base whose manifest was
never updated therefore comes up private to anonymous callers, with credentialed access unchanged.

Each KB has one bucket, and one bucket is one policy boundary. Verbs are independent: `search` may
expose paths and snippets under its own patterns without `read`. `.notedthat` and descendants are
never exposed to anonymous callers.

Policies are validated and loaded once during startup provisioning, then retained as a process
snapshot. Editing a manifest requires restarting the server; there is no hot reload. The rules bind
every credential, the service token included; a supplied credential that does not verify returns
`401` rather than falling back to anonymous access. Anonymous writes are never honoured. The policy
provides no application-level rate setting; operators enable reverse-proxy rate and burst controls
before exposing anonymous search.

The manifest is read during startup to sanity-check config versus deployment environment. It is not
on the request hot path and is rebuildable if lost.

### 6.8 KB identity `[DECIDED — D24]`

| Name | Shape | Use | Stability |
|---|---|---|---|
| `kb_slug` | `[a-z0-9-]{1,40}` | S3 bucket name (D20), Qdrant collection name, WebDAV URL path, MCP `kb` arg, HTTP API URL | immutable in v1 |
| `display_name` | Unicode string, not blank, no control characters, ≤ 128 code points | UI-friendly name, held in `.notedthat/manifest.json` (§6.7); returned by listings (D60) | mutable |

Slug supplied by the operator in `NOTEDTHAT_KBS` (D32). `display_name` is written as the slug when a KB is first provisioned (§6.12) and validated — not blank, no control characters, ≤ 128 code points — whenever the manifest is loaded; a manifest outside the limit refuses startup (D39). Uniqueness scope: per tenant.

**No separate UUID identifier in v1.** The slug *is* the identifier. This is safe because v1 is single-tenant, slugs are immutable, and slugs are unique per tenant. If multi-tenant KB rename ever lands post-v1, an internal UUID may reappear then — not now.

### 6.9 Auth

#### 6.9.1 The service token `[DECIDED — D21, D22, D32, D53]`

Every deployment has one static credential of its own, configured from the environment. Single
tenant.

Env vars:
- `NOTEDTHAT_API_TOKEN` — the Bearer token every surface accepts. String comparison, constant-time.
- `NOTEDTHAT_WEBDAV_USERNAME` — HTTP Basic username the `WebDAV` surface accepts
- `NOTEDTHAT_WEBDAV_PASSWORD` — HTTP Basic password the `WebDAV` surface accepts
- `NOTEDTHAT_KBS` — comma-separated KB slugs; the server ensures these KBs exist at startup (bucket + Qdrant collection created idempotently)

Both credentials resolve to the same principal: the **service token**, `signed-in` with no subject
and no groups. It is bound by the manifest's rules like everyone else (D51), with one exception that
exists for recovery: it always reaches `.notedthat`, and nobody else ever does. `Bearer` is accepted
on every surface, `WebDAV` included; `Basic` only on `WebDAV`, whose clients prompt for it.

Example:
```
NOTEDTHAT_API_TOKEN=sk_live_9f8c…
NOTEDTHAT_WEBDAV_USERNAME=notedthat
NOTEDTHAT_WEBDAV_PASSWORD=change-me
NOTEDTHAT_KBS=my-notes,work-kb
```

#### 6.9.2 OIDC identities `[DECIDED — D53]`

NotedThat mints no tokens of its own. With `NOTEDTHAT_OIDC_ISSUER` set, any bearer that is not the
service token is verified as a signed JWT against the issuer's published keys, and the caller
becomes a **user** principal with a subject and a set of groups. Supported and documented
providers: Authentik, Authelia, Zitadel — any OIDC issuer that publishes discovery and a JWKS and
can mint JWT access tokens works.

**Verification.** Discovery (`{issuer}/.well-known/openid-configuration`) runs at startup and
refuses to start if the issuer is unreachable or spells its `issuer` differently from the setting
(D39). Keys are cached by `kid`; an unknown `kid` refetches at most once per 30 s, and a set older
than an hour is refreshed before use. Accepted algorithms: `RS256`, `RS384`, `RS512`, `ES256`,
`ES384` — never `HS*`. Required claims: `iss` (exact), `aud` (any configured audience), `exp`;
`nbf` is honoured; 60 s leeway. There is no introspection and no session: an opaque access token
is refused, so Authelia and Zitadel must be configured to issue JWTs (`docs/CONFIGURATION.md`).

**Identity.** The subject is the configurable username claim (`preferred_username`), falling back
to `sub`; `user:<name>` rules match it. Groups come from the configurable groups claim (`groups`),
which may be an array of strings, a single string, or — Zitadel's roles shape — an object keyed by
role name; `group:<name>` rules match them. A user never reaches `.notedthat`, however broad their
grants: the manifest carries the policy, group names included, and the recovery path belongs to the
operator.

**Surfaces.** One `Authenticator` resolves every request's principal; the HTTP API, the browse
pages, `WebDAV` and `/mcp` all accept identity tokens as bearers. MCP acts as its caller: the bearer
presented to `/mcp` is the bearer the loopback API call carries, so a `group:` rule binds a tool
call exactly as it binds a direct request.

**MCP client discovery.** With `NOTEDTHAT_OIDC_RESOURCE` set to the deployment's public URL, the
server publishes RFC 9728 metadata at `/.well-known/oauth-protected-resource` and every `401` from
`/api/v1` and `/mcp` carries `WWW-Authenticate: Bearer resource_metadata="…"`, which is how an MCP
client finds the authorization server. The supported providers offer no dynamic client
registration, so the client id is pre-registered on the provider.

Env vars:
- `NOTEDTHAT_OIDC_ISSUER` — the switch; exactly as the provider spells `iss`
- `NOTEDTHAT_OIDC_AUDIENCE` — comma-separated; required with the issuer
- `NOTEDTHAT_OIDC_USERNAME_CLAIM`, `NOTEDTHAT_OIDC_GROUPS_CLAIM` — defaults above
- `NOTEDTHAT_OIDC_HTTP_TIMEOUT_MS` — discovery and key fetches; default 5000
- `NOTEDTHAT_OIDC_RESOURCE` — optional public URL
- `NOTEDTHAT_OIDC_CA_CERT` — optional PEM bundle to trust for the issuer; the server does not read the OS trust store

Any of the others without the issuer refuses startup rather than being ignored.

### 6.10 MCP tool surface `[DECIDED — D25]`

All tools take `kb` (the slug) where relevant; `search` alone takes `kb` as a list of slugs (D54). `if_match` / `if_none_match` args map directly to HTTP conditional headers (per D9 — forwarded to backend, no capability check). 11 tools total.

| Tool | Purpose |
|---|---|
| `list_knowledgebases()` | Returns `[{kb_slug, display_name, description?}]` for every KB the caller can see (D51), `description` from the manifest (D60); `perms` is deferred |
| `index_status(kb)` | The body of `GET /api/v1/knowledgebases/{kb}/index`, unchanged (D62): `state`, `pending`, `queue`, `worker`, `last_indexed_at`, `last_failure`, `last_reconcile` |
| `search(kb[]?, query, filters?, limit?)` | Hybrid search over one or more KBs, one HTTP search per slug (D54); `filters` is the HTTP body's `filter`, all seven fields, and both bodies refuse unknown keys (#125); returns `{results: [{kb, hits: [{kb, object_key, byte_start, byte_end, heading_path, score, preview}]}], skipped}` grouped per KB in request order, `limit` per KB, no merged ranking. Omitted `kb` = every KB the caller can see. |
| `read(kb, path, byte_start?, byte_end?, line_start?, line_end?)` | Byte-range or line-range read of an object. `byte_*` and `line_*` args are mutually exclusive; provide one pair or omit both for a full read. The text is `content[0]`; `structuredContent` is `{etag, content_type, bytes_returned, total_bytes, byte_start, byte_end (exclusive), total_lines, line_start, line_end}`, read off the same response as the text (D61) — `null` where the backend did not say. An object over `NOTEDTHAT_MCP_MAX_READ_BYTES` is `response_too_large`, naming the slice arguments. |
| `write(kb, path, content, if_match?, if_none_match?)` | Create/update object |
| `list(kb, prefix?, limit?, cursor?)` | List objects under a prefix |
| `delete(kb, path, if_match?)` | Delete object |
| `move(kb, from, to, if_match?)` | Rename/move object |
| `edit(kb, path, line_start?, line_end?, byte_start?, byte_end?, content, if_match)` | Byte-range or line-range PATCH; mandatory If-Match. Provide `line_start`/`line_end` for line mode or `byte_start`/`byte_end` for byte mode (mutually exclusive). Byte mode requires strict `byte_start < byte_end`; byte-mode insert is not supported in v1. |
| `append(kb, path, content, if_match?)` | Append to EOF; if_match optional (server obtains ETag internally — single round-trip) |
| `replace(kb, path, old_string, new_string, if_match, replace_all?)` | Content-based server-side substring replace with mandatory If-Match; 422 `no_match` / `ambiguous_match` on non-unique matches (D47) |

Resources: expose `notedthat://<kb_slug>/<percent-encoded path>` as MCP Resources for browsable clients — **shipped in M8** (D37). Flat listing with opaque base64 cursor across KB boundaries; `subscribe`/`unsubscribe` and `listChanged` since D66 (below). Text objects return `TextResourceContents`; non-UTF-8 bytes return `BlobResourceContents`. `resources/read` is bounded by the same read budget as the `read` tool (D61) and, having no slice arguments of its own, refers an oversized object to that tool.

Transports: **streamable HTTP only** (D31, D63), stateful since D66: `notedthat-server` mounts `/mcp` on its unified listener with Bearer auth (or the anonymous caller, D59); `POST` for requests — `initialize` opens a session, everything after it carries `Mcp-Session-Id` and is answered SSE-framed — `GET` for the notification leg, `DELETE` to end the session, each session bound to the principal that opened it (D68) and at most `NOTEDTHAT_MCP_MAX_SESSIONS` of them per process (default 256); legacy `/sse` paths are refused with 405.

Subscriptions (D66): `resources/subscribe` registers a `notedthat://` URI the caller may `list`, probed as the caller; `notifications/resources/updated` follows every `object.written` / `object.deleted` for the key, never the indexer's verdicts; `listChanged` is watched from a session's first `resources/list` on and coalesced to at most two notifications per second. The MCP server consumes `GET …/events` over loopback as the subscriber, one stream per session and knowledge base, so the API's per-event filter is the only filter. Advertised only with an events backend.

### 6.11 Repository layout `[DECIDED — D28]`

Cargo workspace:

```
notedthat/
├── Cargo.toml                    # workspace root
└── crates/
    ├── notedthat-core/           # domain types, traits, static auth checks; JWT verify post-v1
    ├── notedthat-storage-s3/     # S3 adapter (aws-sdk-s3); implements Storage trait
    ├── notedthat-storage-fs/     # Local filesystem adapter (D49); implements Storage trait
    ├── notedthat-indexer/        # chunker + embedder client + Qdrant integration
    ├── notedthat-api-http/       # HTTP API surface (axum handlers over core)
    ├── notedthat-webdav/         # WebDAV surface (dav-server DavFileSystem impl)
    ├── notedthat-write/          # shared write path (commit, patch, replace) for HTTP API + WebDAV
    ├── notedthat-events/         # object change event log adapters (D55): memory ring, NATS JetStream
    ├── notedthat-mcp/            # MCP tool definitions (rmcp) + HTTP-client-backed impl
    ├── notedthat-server/         # server library — wires all listeners in one process
    └── notedthat/                # distribution crate — owns the published binary
```

Dep graph:
- `notedthat-core` — no deps on other workspace crates
- `notedthat-storage-s3`, `notedthat-storage-fs`, `notedthat-indexer`, `notedthat-events` — depend on core (the `EventPublisher` trait and event types live in core, like `Storage`)
- `notedthat-api-http` — depends on core + storage + indexer
- `notedthat-webdav` — depends on core + an HTTP client to the local API
- `notedthat-mcp` — depends on core (for types) + an HTTP client
- `notedthat-server` — depends on api-http + webdav + **notedthat-mcp** (deliberate M8 extension, plan §W1.3); runs one listener for HTTP API, WebDAV, and MCP HTTP
- `notedthat` — no logic; owns the `notedthat-server` binary target and depends on that library crate. Nothing depends on it.

Per D10, `notedthat-server` is the main artifact. The binary is built from the `notedthat` crate: it ships in the Docker image, in one cargo-dist archive per target, and as a single installable (`cargo install notedthat`). `notedthat-server` is library-only, so exactly one published crate owns the installed binary name.

### 6.12 v1 operational contracts `[DECIDED — D38–D43]`

The concrete HTTP API route surface (D44) lives in §6.13.

#### Startup provisioning
1. Parse `NOTEDTHAT_KBS` as comma-separated slugs.
2. Validate every slug (`[a-z0-9-]{1,40}`, no leading/trailing hyphen; the tenant slug is `[a-z0-9]{1,20}` per D58; reject duplicates). Reject any `(tenant_slug, kb_slug)` whose derived bucket name (§6.6) exceeds 63 chars.
3. Ensure each bucket exists; `BucketAlreadyOwnedByYou` is success. Under the `fs` backend (D49) this creates the per-KB directory, which is idempotent in the same way. Under `s3`, each bucket is then asked whether it enforces `If-Match` and `If-None-Match` on a `PUT`, and one that does not refuses startup unless `NOTEDTHAT_S3_ALLOW_UNENFORCED_CONDITIONAL_WRITES` accepts it (D70).
4. Ensure each `.notedthat/manifest.json` exists and matches the declared slug and embedding dimensions; a fresh manifest takes the slug as its `display_name`. Validate the manifest (schema version, `display_name` per §6.8, `access` rules) and load its `access` rules into the startup snapshot.
5. Ensure each Qdrant collection exists with the expected dense dimension and sparse BM25 vector. Fatal like every other step — an unreachable Qdrant names `NOTEDTHAT_QDRANT_URL` and exits (D64 restored this; the code had warned and continued).
6. If any step fails: log the exact KB + backend error and exit non-zero. No partial startup.

#### Readiness `[DECIDED — D64]`
`/healthz` is process liveness and answers `200` for as long as the process serves. `/readyz` answers `200` only while every backend the server needs is there, and `503` otherwise:

- **What is probed.** The storage backend (`HeadBucket` on the bucket of the knowledge base whose slug sorts first — `NOTEDTHAT_KBS` is held sorted — under `s3`; a stat of its directory under `fs`) and Qdrant (`HealthCheck`), by a background task every `NOTEDTHAT_READY_PROBE_INTERVAL_MS` (default 5 000 ms), each probe bounded by that same interval, so the interval must exceed the backends' round-trip time. A configured event broker contributes its connection state, read inline (D55).
- **Deadline.** An outage is reported within `2 × NOTEDTHAT_READY_PROBE_INTERVAL_MS` — one interval waiting for the next probe, one for it to time out — and recovery within one interval, without a restart. The snapshot starts from "ready", since provisioning has just reached both backends.
- **Body.** `{"status": "ok" | "degraded" | "unavailable", "checks": {"storage": {…}, "search": {…}, "conditional_writes"?: {…}, "events"?: {…}}}` — the top-level word is the worst check's, so `degraded` is visible without reading the checks, and only `unavailable` costs the `200` — each check `{"backend", "status", "reason"?}` with `status` one of `ok`, `degraded`, `unavailable` and `reason` one of `timeout`, `unreachable`, `not_found` (the probed bucket or directory is gone), `preconditions_not_enforced` (`conditional_writes` only), `disconnected` (events only). `conditional_writes` is present on `s3` alone and is not probed: it repeats what the startup check found (D70), `ok`, or `degraded` for the life of the process when the operator accepted a bucket that does not enforce them. `not_found` is `degraded`, not `unavailable`: the backend answered, so the replica stays ready and `/readyz` stays `200`. The backend's own error is logged once per transition and never returned.
- **Not covered.** The `fs` watcher (a lost watch is `FS_WATCH_LOST` and a rescan, D50), the embedder, and index freshness — that is per knowledge base and lives at `GET /api/v1/knowledgebases/{kb}/index` / MCP `index_status` (D62), not on a process-wide probe.

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
- HTTP API accepts normal `Range: bytes=start-end` and forwards equivalent range semantics to the storage backend.
- One range per request: a comma-separated range set returns `400 malformed_range`. No `multipart/byteranges` in v1 (D57).
- Successful partial reads return `206` + `Content-Range`.
- Full reads return `200`.
- Malformed ranges return `400 malformed_range`; unsatisfiable ranges return `416`.
- MCP `read(kb, path, byte_start?, byte_end?)` uses zero-based byte offsets with `byte_end` exclusive, matching internal chunk offsets. The MCP wrapper converts to HTTP's inclusive `Range` header when calling the API.

#### Line ranges

- HTTP `Range: lines=<first>-<last>` returns 206 with `Content-Range: lines <first>-<last>/<total_lines>` and `X-Content-Range-Bytes: <byte_start>-<byte_end>/<total_bytes>` (inclusive byte_end).
- Line numbers are 1-based inclusive. `last` past EOF is clamped; `first` past EOF is 416.
- `Range: lines=<N>-<N-1>` (Insert form) returns 206 with empty body — valid for validating an insert offset.
- 416 response includes both `Content-Range: lines */<total_lines>` and `X-Content-Range-Bytes: */<total_bytes>`.
- Line index is recomputed per request (no persistent sidecar in v1).

#### Partial writes (PATCH)

- `If-Match` REQUIRED for bytes/lines modes; OPTIONAL for `NT-Patch-Mode: append`.
- `If-Match: *` and multi-value `If-Match` REJECTED with 400 in v1.
- Server-side splice: HEAD → caller-precondition-check → GET (with `If-Match: head_etag`) → splice → PUT (with `If-Match: head_etag` as internal CAS anchor — NOT the caller's If-Match). Bounded 2× retry on window 412 from GET or PUT.
- The retry does NOT absorb concurrent PATCH conflicts. Under sustained concurrent PATCH, the loser's step-3 caller-precondition-check surfaces 412 — as the OCC contract requires.
- Post-splice size cap enforced by `NOTEDTHAT_MAX_PATCHABLE_SIZE` (default 100 MiB). Pre-splice and post-splice size both checked before allocation (checked arithmetic, see D46).
- 503 on indexer backpressure: object IS stored, search index NOT updated (ghost-state, same as PUT 503).

#### String replace `[DECIDED — D47]`

- `POST /api/v1/knowledgebases/{kb_slug}/replace/{*target_path}` with JSON body `{ "old_string": "...", "new_string": "...", "replace_all": bool? }` and mandatory `If-Match`. `<target_path>` is the object path to replace within; the surrounding `POST … /replace/` URL prefix identifies the action.
- **URL namespace convention**: POST on paths beginning with `replace/` invokes the replace action targeting the remainder of the path. `GET/HEAD/PUT/PATCH/DELETE` on the same URL still address the literal object at that path (unchanged). To invoke replace on an object whose path is itself `replace/foo.md`, POST to `.../replace/replace/foo.md` — the outer `replace/` is the action prefix, the inner segments are the target path. This is a POST-only namespace reservation; other verbs are unchanged.
- Exact-byte UTF-8 substring matching. Matching is byte-exact: a needle of `\r\n` finds the two-byte CRLF sequence; a needle of `\n` finds every LF byte, INCLUDING the LF byte that follows a CR in a CRLF. The algorithm is byte-substring, not line-aware.
- Zero matches → `422 { "error": "no_match" }`; storage untouched (both `replace_all=false` and `replace_all=true` return no_match on zero matches; the flag does not turn zero matches into a success).
- `replace_all=false` (default) + multiple matches → `422 { "error": "ambiguous_match", "match_count": N }`; storage untouched.
- `replace_all=true` → replace every non-overlapping occurrence left-to-right in ONE splice.
- Empty `new_string` allowed (delete-in-place). Empty `old_string` REJECTED with 400 `invalid_request` (would match every byte position).
- `If-Match` semantics identical to PATCH per D46: `*` and multi-value → 400; stale ETag → 412; two-ETag CAS with bounded 2× retry on window `PreconditionFailed`.
- Post-splice size cap enforced by `NOTEDTHAT_MAX_PATCHABLE_SIZE` (same knob as PATCH per D46).
- 503 on indexer backpressure: object IS stored (PUT committed), search index NOT updated (ghost-state, identical semantics to PATCH per §6.12 Partial writes).
- **503 retry semantics: NOT a safe replay.** Because the write path commits to storage BEFORE enqueueing the indexer event, a 503 from `replace` means the caller's requested mutation has ALREADY landed — the object's `ETag` has advanced, the `old_string` may no longer exist in the object, and blindly resending the same POST with the original `If-Match` will return 412 (stale). The correct retry pattern is: (a) issue a HEAD to observe the current `ETag` and confirm the state matches expectations; (b) if the state is already what the caller wanted, treat the 503 as a success-with-index-lag and poll `/search` for eventual consistency; (c) if reconciliation is needed, GET the object, re-derive `old_string`/`new_string` against the new content, and reissue with the fresh `ETag`. This diverges from typical "retry the request" 503 semantics — clients MUST reconcile, not blindly replay.
- Success response: `200 OK` + `ETag: "<new>"` + `Content-Location: /api/v1/knowledgebases/{kb_slug}/{percent_encode_path(target_path)}` (canonical API URI for the TARGET object, not the `/replace/`-prefixed request URI) + JSON body `{ "etag": "...", "match_count": N, "total_bytes": M }`.
- MCP tool: `replace(kb, path, old_string, new_string, if_match, replace_all?)` — thin wrapper over the HTTP route; If-Match mandatory (mirrors HTTP contract, not `append`'s optional).

#### List/search pagination
- `list`: lexicographic S3 object order. Default limit `100`, max `1000`. Cursor is opaque and maps to the backend continuation token.
- `search`: top-k only in v1. Default limit `10`, max `50`. No cursor/offset/search pagination. Ordered per D56: score descending, then `object_key`, then `byte_start`. The body — `query`, `filter`, `limit`, and `filter`'s seven fields — refuses unknown keys with `400` naming the key, on the HTTP route and on the MCP tool alike: a misspelt key must not be a silently unfiltered page (#125).

#### Indexing queue
- Write path: S3 commit succeeds first, then enqueue `{kb, object_key, etag, mtime}` onto a bounded in-memory channel.
- Queue capacity: fixed implementation constant in v1 (recommended `1024` events); env tuning post-v1.
- If the queue is full, log `INDEX_QUEUE_FULL` with KB/path and return `WriteError::IndexerBackpressureUpsert` for upserts or `WriteError::IndexerBackpressureTombstone` for tombstones, which the HTTP API and WebDAV surfaces map to HTTP 503 `backend_unavailable` with `Retry-After: 5`. The storage mutation IS committed to S3 before the enqueue attempt, and with an events backend the change event is already published (D65); the client should retry to re-enqueue the indexing event.
- Embedder or Qdrant failures are logged as `INDEXING_FAILED` with the knowledge base and key; callers do not receive job IDs. What they can read is the per-knowledge-base aggregate at `GET /api/v1/knowledgebases/{kb}/index` and MCP `index_status` (D62): a state (`healthy`, `indexing`, `backpressured`, `stale`, `failed`), pending events, the queue's depth and capacity, the last success time, the last failure's one-line summary, and the last reconciliation pass (D50 on `fs`, D67 on `s3`). With an events backend, each finished upsert or refresh is also reported on the events stream as `object.indexed` or `object.index_failed` (D65), the per-write signal.
- Search may be stale in v1. A later write to the same object re-enqueues it.
- With an events backend configured (§6.14), the same write path publishes an `object.written` / `object.deleted` event immediately before the enqueue; a publish failure is `WriteError::EventPublishFailed` → HTTP 503 `backend_unavailable` with `Retry-After: 5`, logged as `EVENT_PUBLISH_FAILED`.

#### Reindex
- No full rebuild in v1: no HTTP endpoint, MCP tool, WebDAV action, or CLI re-embeds a knowledge base from scratch; the storage/index design remains rebuildable from S3 and that becomes a v2/admin feature.
- The incremental form exists (D67): `POST /api/v1/knowledgebases/{kb}/index/reconcile`, for the service token only, compares the bucket against the index and refreshes only what differs. On `fs` the watcher and startup pass do the same without asking (D50). Re-writing an object stays the way to re-embed one that has not changed (D42).

#### Error mapping
| Condition | HTTP | MCP | WebDAV |
|---|---:|---|---:|
| Invalid input/path/range syntax | `400` | `invalid_request` | `400` |
| Missing/invalid auth | `401` | `unauthorized` | `401` |
| Forbidden by the access rules | `403` | `forbidden` | `403` |
| Missing KB/object | `404` | `not_found` | `404` |
| Upload too large | `413` | `payload_too_large` | `413` |
| S3 precondition failed (`If-Match`, `If-None-Match`) | `412` | `precondition_failed` | `412` |
| Unsatisfiable range | `416` | `range_not_satisfiable` | `416` |
| Unprocessable — non-unique match | `422` | `no_match` / `ambiguous_match` | n/a (WebDAV unchanged) |
| Backend unavailable / timeout | `503` | `backend_unavailable` | `503` |
| Event log position retained out (`Last-Event-ID`) | `410` | n/a | n/a |
| Unexpected internal error | `500` | `internal_error` | `500` |

HTTP error bodies are JSON: `{ "error": "code", "message": "human readable", "request_id": "..." }`. MCP tool errors use the same `error` code string and include the human message as tool error content.

### 6.13 HTTP API route surface `[DECIDED — D44]`

API routes are prefixed with `/api/v1`. Object paths are percent-encoded into a single URL path segment — see the encoding rules below. WebDAV is mounted at `/webdav`, and MCP is mounted at `/mcp`. `/metrics` is deliberately absent from this table: the Prometheus exposition is served on a second, unauthenticated listener of its own (D69), off by default, and `GET /metrics` on *this* listener is `404` with or without a credential.

| Method | Route | Purpose |
|---|---|---|
| `GET` | `/healthz` | Liveness — unauthenticated, unversioned |
| `GET` | `/readyz` | Readiness — unauthenticated, unversioned. `503` while the storage backend, Qdrant, or a configured event broker is unavailable (D64); the body lists each check. Per-knowledge-base index state is `…/{kb_slug}/index` (D62) |
| `GET` | `/llms.txt` | Plain-text API navigation — unauthenticated, unversioned |
| `GET` | `/.well-known/oauth-protected-resource` | RFC 9728 protected-resource metadata (D53) — unauthenticated; `404` unless `NOTEDTHAT_OIDC_RESOURCE` is set |
| `GET`, `HEAD` | `/browse/`, `/browse/{*path}` | Server-rendered HTML directory listings (D52). Anonymous or Bearer; other methods return `405` |
| `GET` | `/api/v1/knowledgebases` | List declared KBs — matches MCP `list_knowledgebases()` (§6.10) and WebDAV root PROPFIND (D23) |
| `GET` | `/api/v1/knowledgebases/{kb_slug}` | List objects in a KB. Query params: `prefix`, `limit` (default 100, max 1000), `cursor` (opaque continuation token per §6.12) |
| `HEAD` | `/api/v1/knowledgebases/{kb_slug}/{path}` | Object metadata (ETag, `Content-Length`, `Last-Modified`) |
| `GET` | `/api/v1/knowledgebases/{kb_slug}/{path}` | Read object; supports `Range: bytes=` (D6, §6.12) |
| `PUT` | `/api/v1/knowledgebases/{kb_slug}/{path}` | Create/update object; forwards `If-Match` / `If-None-Match` / `If-*-Since` verbatim (D9) |
| `DELETE` | `/api/v1/knowledgebases/{kb_slug}/{path}` | Delete object; forwards `If-Match` (D9) |
| `PATCH` | `/api/v1/knowledgebases/{kb_slug}/{path}` | Yes | Partial write (bytes/lines splice, append). Requires `If-Match` for bytes/lines; optional for append. |
| `POST` | `/api/v1/knowledgebases/{kb_slug}/search` | Hybrid search of one KB. Body: `{ query, filters?, limit? }`. Response `{ hits }`; MCP `search` (§6.10) wraps one of these per KB |
| `POST` | `/api/v1/knowledgebases/{kb_slug}/replace/{*path}` | String replace with mandatory `If-Match`; body `{ old_string, new_string, replace_all? }`; response `{ etag, match_count, total_bytes }` per D47 |
| `GET` | `/api/v1/knowledgebases/{kb_slug}/events` | Object change events as SSE (D55, §6.14). Query params: `prefix`, `event`, `mime`; header `Last-Event-ID`. `404` unless an events backend is configured. Like `search`, a literal sibling of the object catch-all |
| `GET` | `/api/v1/knowledgebases/{kb_slug}/index` | The knowledge base's search-index health (D62). Visible under the listing rule (D51). Like `search` and `events`, a literal sibling of the object catch-all |

#### Path encoding

- The client **percent-encodes the object path as a single URL segment**. Any `/` within a logical object key becomes `%2F`; `?` becomes `%3F`; `#` becomes `%23`; and so on per RFC 3986.
- The server percent-decodes once, then applies path normalization per D40 (reject `.` / `..` segments, reject backslashes and NUL bytes, `/` as the only separator, preserve case and Unicode exactly).
- Example: logical object `docs/rfc/7231.md` in KB `my-notes` is fetched via `GET /api/v1/knowledgebases/my-notes/docs%2Frfc%2F7231.md`.
- `kb_slug` is `[a-z0-9-]{1,40}` (D24) and never contains reserved characters, but compliant clients should percent-encode it defensively.

#### Auth

`/healthz`, `/readyz` and `/llms.txt` are globally unauthenticated. Every other route
authenticates at the boundary and authorizes per key.

**Authentication** establishes a principal: the service token, or a bearer an OIDC issuer vouches
for (D53), is `signed-in` — the former with no identity, the latter with a subject and groups; an
omitted `Authorization` header is `anyone`; and a supplied credential that does not verify is
always `401` — never quietly downgraded to anonymous. More than one `Authorization` header is also
`401`. When the deployment publishes RFC 9728 metadata, every `401` names it in a
`WWW-Authenticate: Bearer resource_metadata="…"` challenge.

**Authorization** is the manifest's access rules (D51, D53), evaluated against the concrete object key.
A route pattern cannot answer this — `read` on `{*object_path}` has no answer until the key is
known — so the check lives beside the key rather than in the middleware. The middleware keeps one
coarse backstop: an anonymous request whose `(method, route)` pair is not on the reachable list is
refused before a handler sees it, so a route added later is closed by default.

Verb per operation:

| Operation | Verb |
|---|---|
| `GET /api/v1/knowledgebases` | none; the response lists what the principal can see |
| `GET /api/v1/knowledgebases/{kb}` | `list` |
| `GET`, `HEAD` on an object | `read` |
| `PUT`, `PATCH`, `POST .../replace/...` | `write` |
| `DELETE` | `delete` |
| `POST .../search` | `search`, and hits are filtered by the `search` rules' own patterns |
| `/browse` pages | `list`; each row's link needs `read` on that key |

Search filters by `search` and never by `read`, which is what keeps the two independently
grantable — and means a broad `search` grant with a narrow `read` grant publishes previews of keys
the caller cannot fetch. Previews are content; an operator choosing that combination is publishing
excerpts.

**Status codes.** A credential that is missing where one is unconditionally required, or that does
not verify, is `401` — an answer from the authentication layer, which knows nothing about any
knowledge base. A valid credential that is not allowed is `403` (D43). An undeclared knowledge base
is `404` for everyone — not an authorization answer — and an anonymous caller the rules do not
grant gets that same `404`, byte for byte, on the API and on `/browse` alike, so the status cannot
be used to enumerate private knowledge bases or prefixes; the cost, accepted, is that an anonymous
client is not told a credential might change the answer.

MCP acts as the caller who presented itself: the loopback API call carries the same bearer, or no
credential for an anonymous caller (D59), so a tool call is bound by exactly the rules a direct
request would be. A request with no credential is admitted only when some knowledge base grants
`anyone` a verb and `NOTEDTHAT_MCP_ANONYMOUS` is not `never`; otherwise it is `401` with the bearer
challenge. Anonymous search rate and burst control — on the search route and on `/mcp` — belongs
at a reverse proxy rather than in application configuration.

#### Browse surface `[DECIDED — D52]`

| Request | Response |
|---|---|
| `GET /browse` | `308` → `/browse/` |
| `GET /browse/` | Index of knowledge bases the principal can see |
| `GET /browse/{kb}` | `307` → `/browse/{kb}/` |
| `GET /browse/{kb}/`, `/browse/{kb}/{prefix}/` | One directory level |
| `GET /browse/{kb}/{key}` | `303` → the object's `/api/v1` URL; or `307` to the slashed form if it is a folder; else `404` |

Directories are synthesised from keys (D40) after per-key filtering, so a folder appears exactly
when at least one visible key sits beneath it. Pages carry `Cache-Control: no-store` and
`Vary: Authorization`, because anonymous and credentialed callers share a URL and see different
content. Object names are HTML-escaped, and control and bidirectional characters are replaced in the
displayed name so it cannot misrepresent the key it links to.

Rationale: single-segment paths avoid multi-segment wildcard routing and eliminate KB-vs-object boundary ambiguity. WebDAV keeps its native multi-segment path semantics per D23 — the two surfaces are logically equivalent but wire-format-distinct.

---

### 6.14 Object change events `[DECIDED — D55, D65]`

Every write made through NotedThat, and on the `fs` backend every change the server detects in
its tree, is published as one event once storage has acknowledged it; once the indexer has
finished with the object, its verdict follows on the same log (D65). Subscribers read them
from `GET /api/v1/knowledgebases/{kb_slug}/events` as `text/event-stream`.

**Shape.** One JSON object per event:

```
id: 4812
event: object.written
data: {"event":"object.written","kb":"notes","object_key":"inbox/memo.mp3","etag":"\"9a3f…\"","size":48213011,"mime":"audio/mpeg","mtime":1757950000,"source":"http","occurred_at":"2026-09-15T14:33:20Z"}

id: 4814
event: object.indexed
data: {"event":"object.indexed","kb":"notes","object_key":"inbox/memo.mp3.md","etag":"\"b71c…\"","mime":"text/markdown","chunks":3,"source":"indexer","occurred_at":"2026-09-15T14:33:21Z"}
```

Four types. Two describe a change: `object.written` (create or modify — neither the API nor
storage distinguish them) and `object.deleted` (no stamp). Two are the indexer's verdict on an
upsert or refresh (D65): `object.indexed` (`etag` — the version now in the index — `mime`,
`chunks`) and `object.index_failed` (`etag` and `mime` when `HEAD` had succeeded; `summary`, the
bounded first line the D62 health view also reports). `source` is one of `http`, `webdav`,
`mcp`, `fs-watch`, `reconcile`, `indexer`; `mcp` is self-declared through
`X-NotedThat-Source: mcp`, which the MCP server sets on its API calls and which is
informational; `indexer` is never a change to the bytes. A WebDAV `MOVE` is two events. The
`data` object repeats the type under `event` so the payload is self-describing in the broker
too.

**Where publish happens.** In `notedthat-write`, on every path (`commit`, `commit_copy`,
`commit_delete`, `patch`, `replace`), immediately after storage acknowledges and before the D38
enqueue, so a subscriber that `GET`s the key on receipt sees the bytes the event describes or
newer, and so the worker's `object.indexed` for that `ETag` always follows the write on the log
(D65). The write functions take `WriteSinks` — the indexing sender, the optional
`EventPublisher`, and the calling surface's source. On both backends, `IndexEvent::Refresh`
carries its origin (the `fs` watch, or either backend's reconciliation pass) and the indexer worker publishes from the `HEAD` it
already performs, before the indexability check, so an mp3 dropped into the tree is announced
though it is never indexed; a missing key on re-read is `object.deleted`. The worker keeps a
bounded last-seen stamp per key, fed by the `Upsert` and `Tombstone` events it receives, and
announces a `Refresh` only when the stamp differs — that is what keeps the watcher's echo of the
server's own write (D50) from being announced twice for objects the index does not hold.

**Outcomes (D65).** Once the worker has finished an `Upsert` or `Refresh` it stamps the D62
health record and then publishes: `object.indexed` when it re-embedded the object, with the
`ETag` it verified on the staged stream and the number of points now representing the object;
`object.index_failed` when the pipeline failed at any stage, with the same bounded summary the
health view keeps and whatever `HEAD` had established. Nothing is published where the index
gained nothing — an explicit tombstone, the pipeline's own tombstone for a non-indexable or
empty object or a key gone on re-read, and a `Refresh` skipped for an unchanged `ETag` are
silent; `object.deleted` and `/index` cover them. Outcome events do not pass through the
last-seen dedup above, which would suppress them against the stamp the write announced. A
refused publish is logged and indexing goes on; `/index` still says what happened.

**Filtering.** After `KbAccess::resolve` and `require_any(Verb::List)` — so an undeclared or
ungranted knowledge base answers exactly as everywhere else, whether or not events are on — each
event is passed through `allows(Verb::List, key)`. Deletions are filtered like writes: a
deletion reveals that the key existed. Then `?prefix=`, `?event=` (`written`, `deleted`,
`indexed` or `index_failed`, with or without the `object.` prefix) and `?mime=` (exact or
`type/*`, against the content type the event carries: a write's, or the one the indexer's
`HEAD` reported; deletions, and an `index_failed` whose failure came before `HEAD`, carry none
and never match a `mime` filter). One field is withheld inside a visible frame: the `summary`
on `object.index_failed` goes only to a subscriber whose `list` grant spans the whole knowledge
base, the D62 rule for the same string; everyone the key gate admits still learns the key, the
version and that indexing it failed.

**Ids and replay.** The adapter owns the id: a process counter for `memory`, the JetStream
stream sequence for `nats` (global across replicas and knowledge bases; gaps per knowledge base
are normal). `Last-Event-ID: n` replays every event with id greater than `n`, in order, then
live; absent, live only; older than retained, or ahead of the log (a `memory` backend that
restarted, a recreated stream — ids that never existed here), `410 gone` naming the oldest
retained id, so the subscriber resyncs by listing rather than silently resuming from now. The stream opens with `retry: 3000`, sends a comment
every 15 s, and carries `Cache-Control: no-cache` and `X-Accel-Buffering: no`.

**Backends** (`NOTEDTHAT_EVENTS_BACKEND`, §6.5 policy): `none` — default; nothing is published
and the route answers `404`. `memory` — a ring of `NOTEDTHAT_EVENTS_MEMORY_CAPACITY` events plus
a broadcast channel; replay across reconnects, not restarts or replicas. Cross-replica replay is
only meaningful on `s3`; `fs` is one process per root (D49), and `memory` is exactly enough
there. `nats` — one JetStream stream (`NOTEDTHAT_NATS_STREAM`, subjects
`notedthat.events.<kb>.<written|deleted|indexed|index_failed>`, retention
`NOTEDTHAT_NATS_MAX_AGE_SECS`), created if
absent, refused if a stream by that name captures other subjects; a subscription is an ordered,
ack-less consumer filtered to the knowledge base's subjects starting one past the requested id.

**Failure.** A publish that fails after the bytes are stored is `WriteError::EventPublishFailed`
→ `503 backend_unavailable` with `Retry-After: 5` on the HTTP API and WebDAV alike, logged as
`EVENT_PUBLISH_FAILED` with kb and key, exactly as D38: the client is the retry mechanism and
delivery is at least once, never zero times; because the publish precedes the enqueue, a write
the full queue then refuses with `503` has already been announced, and its retry announces it
again. A failed publish from the worker (no caller) is logged: the next comparison re-detects a
change, and `/index` reports an outcome. An unreachable broker refuses startup (D39),
and a lost connection surfaces as the `events` check on `/readyz` (D64).

**Known at-least-once cases, documented for subscribers.** A retried write publishes twice, and
so does a write refused by a full indexing queue once it is retried. On the `fs` backend the
watcher's echo of the server's own write is a second pass over the object: silent when the index
already holds the `ETag`, but after a failure it is a second attempt and reports
`object.index_failed` again. The
`fs` startup comparison re-announces objects the index does not track (anything non-indexable)
on every restart. A worker that writes into the prefix it watches sees its own writes; it
filters by `mime` or `prefix`, or compares `etag`. `events` at the root of a knowledge base is
a route, like `search`.

### 6.15 Metrics catalogue `[DECIDED — D69]`

`GET /metrics` on `NOTEDTHAT_METRICS_LISTEN_ADDR` answers the Prometheus text exposition
(`text/plain; version=0.0.4`) when `NOTEDTHAT_METRICS_ENABLED=true`, and every other path on that
listener is `404`. Names, label keys and the histogram bucket table live in one module,
`notedthat_core::metrics`, so every crate that records a measurement spells it the same way.

| Metric | Type | Labels |
|---|---|---|
| `notedthat_http_requests_total` | counter | `surface`, `route`, `method`, `status` |
| `notedthat_http_request_duration_seconds` | histogram | `surface`, `route`, `method` |
| `notedthat_http_requests_in_flight` | gauge | `surface` |
| `notedthat_search_requests_total` | counter | `kb`, `outcome` |
| `notedthat_search_duration_seconds` | histogram | `kb` |
| `notedthat_search_hits` | histogram | `kb` |
| `notedthat_embedding_requests_total` | counter | `phase`, `outcome` |
| `notedthat_embedding_duration_seconds` | histogram | `phase` |
| `notedthat_embedding_errors_total` | counter | `phase`, `error_kind` |
| `notedthat_embedding_texts` | histogram | `phase` |
| `notedthat_index_queue_depth` / `…_capacity` | gauge | — |
| `notedthat_index_events_enqueued_total` | counter | `kb` |
| `notedthat_index_events_refused_total` | counter | `kb` |
| `notedthat_index_events_completed_total` | counter | `kb`, `outcome` |
| `notedthat_index_stale_marks_total` | counter | `kb` |
| `notedthat_index_worker_alive` | gauge | — |
| `notedthat_vector_store_operations_total` | counter | `op`, `outcome` |
| `notedthat_vector_store_operation_duration_seconds` | histogram | `op` |
| `notedthat_vector_store_errors_total` | counter | `op`, `error_kind` |
| `notedthat_events_published_total` | counter | `kb`, `kind` |
| `notedthat_events_publish_failed_total` | counter | `kb`, `kind` |
| `notedthat_events_subscribers` | gauge | `kb` |
| `notedthat_events_replay_gone_total` | counter | `kb` |
| `notedthat_fs_watch_lost_total` | counter | `reason` |
| `notedthat_reconcile_passes_total` | counter | `kb`, `cause`, `outcome` |
| `notedthat_reconcile_duration_seconds` | histogram | `kb` |
| `notedthat_reconcile_objects_total` | counter | `kb`, `class` |
| `notedthat_reconcile_objects_on_disk` | gauge | `kb` |
| `notedthat_storage_operations_total` | counter | `backend`, `op`, `outcome` |
| `notedthat_storage_operation_duration_seconds` | histogram | `backend`, `op` |
| `notedthat_storage_errors_total` | counter | `backend`, `op`, `error_kind` |
| `notedthat_build_info` | gauge (`1`) | `version`, `revision`, `backend`, `events_backend` |

Label values are literals from closed sets, or a knowledge base slug declared at startup, so
cardinality is bounded by configuration and not by traffic. `route` is the matched *pattern*,
never the request's path; `surface` is one of `api`, `webdav`, `mcp`, `browse`, `root`; `outcome`
on a storage call distinguishes `ok`, `not_found`, `precondition`, `unavailable`, `error` and
`cancelled`, so the conditional answers that make an idempotent delete work are not counted as
failures and a caller that gave up is counted rather than lost. No label
ever carries an object key, a principal, a credential, a query or a subscriber's filters (D51,
D69), which `tests/metrics_labels_e2e.rs` asserts against named sentinels.

---

## 7. Open Questions

### 7.1 WebDAV micro-decisions (closed for v1)
- ✅ Basic-auth username: v1 uses static `NOTEDTHAT_WEBDAV_USERNAME` (D22).
- ✅ `FakeLs`: not in v1 (D34); Finder / Office save-workflow limitations documented.
- ✅ Upload buffer: 16 MiB in-memory, 5 GiB max — hardcoded (D35).
- KB deletion + open WebDAV mount — moot for v1 (D32).

### 7.2 MCP micro-decisions (closed for v1)
- ✅ Resources: shipped in M8 (D37). Flat `notedthat://` URIs, opaque base64 cursor; `subscribe`/`listChanged` since D66.
- ✅ HTTP transport: shipped in M8 (D31), stateful since D66. Streamable HTTP at `/mcp` with sessions; Bearer auth reuses `NOTEDTHAT_API_TOKEN`, identity tokens since D53, and the anonymous caller is admitted where the manifests admit one since D59.
- ✅ Session binding: a session id answers only to the principal that opened it, keyed on the subject so a token refresh keeps the session (D68); a mismatch is answered as an unknown session is. The bound on sessions per process is `NOTEDTHAT_MCP_MAX_SESSIONS`.
- ✅ Legacy SSE MCP transport: not implemented — deprecated in the MCP spec; POST/GET /sse return 405. GET/DELETE /mcp are the stateful transport's own since D66.
- ✅ Line-range reads: shipped (D45).

### 7.3 Non-functional targets `[OPEN]`
- Search P50/P95 latency — **measurable since D69**, and to be set from data rather than guessed. With `NOTEDTHAT_METRICS_ENABLED=true` the search route records `notedthat_search_duration_seconds` as a bucketed histogram labelled by knowledge base, so the P95 over five minutes is `histogram_quantile(0.95, sum by (le) (rate(notedthat_search_duration_seconds_bucket[5m])))` — `sum by (le, kb)` for the per-knowledge-base breakdown, `0.50` for the P50. A budget is set once a deployment has a fortnight of this on real content; setting one before the data exists is how the number ends up describing the test fixture.
- Write ack latency budget? — set with data once the storage path ships. The measurement exists (`notedthat_http_request_duration_seconds{route="/api/v1/knowledgebases/{kb_slug}/{*object_path}",method="PUT"}`); the budget does not.
- Availability target? — SLA depends on deployment shape; not a v1 concern.

### 7.4 v2+ deferred items
- **Auth**: token introspection for providers that only mint opaque access tokens; a browser login flow for `/browse` (today: forward-auth at the proxy, or a bearer); HTTP admin endpoints for KB create.
- **WebDAV**: `FakeLs` for save-workflow clients (D34).
- **KB lifecycle**: delete + rename (D32).
- **Content**: additional frontmatter conventions beyond OKF metadata extraction (D33).
- **MCP**: budgeting sessions per principal rather than per process. D68 binds a session to the principal that opened it, but the bound stays process-wide, so one caller — or an anonymous one where a deployment admits them — can still hold every slot and `503` everyone else's `initialize`; the cap is also soft, the count and the session's creation not being atomic. The session-to-principal record D68 adds is what a per-principal budget needs — [#179](https://github.com/NotedThat/NotedThat/issues/179).
- **Events**: webhooks (server-initiated `POST` to a subscriber URL) over the same log — subscriber registry, outbound retries, signing.
- **Tuning**: env-var overrides for upload buffer / multipart thresholds (D35, D36).
- **Storage**: full-rebuild-from-S3 as a first-class operation (the incremental comparison exists, D67).
- **Line ranges**: server-side line-index sidecar object (`.notedthat/idx/<key>.lines`) — deferred; index is recomputed per request in v1 (D45).

---

## 8. Backend Selection Guidance (informational)

Per D9 / D19 / D30: NotedThat runs against any S3-compatible backend without capability checks, with one exception: at startup it checks that each bucket enforces conditional writes, and refuses to start on one that does not unless the operator accepts the risk (D70). Otherwise, what the backend supports is what the deployment gets. This section exists so **operators can make an informed choice**.

Concrete honest problem: if a client sends `If-Match: <etag>` to NotedThat, the client will either:
- get a proper `412 Precondition Failed` on mismatch (AWS, MinIO, R2, Ceph ≥20.2.1, SeaweedFS ≥4.09, RustFS) — correct behavior, or
- get a silent `200 OK` even on mismatch under contention (Garage always; SeaweedFS <4.09; RustFS under high-concurrency lock-timeout — see §8.1).

The client won't know it got the wrong answer until concurrent conflicts show up as silent lost writes. **This is a property of the deployment, not of NotedThat.** We document; the deployer picks. Since D70 the server also asks: a backend that ignores the headers outright refuses startup, so the choice is made knowingly — `NOTEDTHAT_S3_ALLOW_UNENFORCED_CONDITIONAL_WRITES=true` — rather than discovered. A failure that only appears under contention cannot be provoked by one writer at startup, and stays the deployer's to know from the table below.

### 8.1 Compatibility matrix (as of Q3 2026)

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
| **Local filesystem** (D49) | ✅ | ✅ content-derived SHA-256 | ✅ | ✅ | In-process per-key lock + write-temp-then-rename | ❌ | **Not S3.** Exactly one server process per root, enforced by a startup lock; NFS/SMB unsupported. Requires a filesystem that preserves names byte-for-byte — case-folding or Unicode-normalizing filesystems are refused at startup unless explicitly overridden. Cannot hold an object `a/b` alongside `a/b/c`; the second write is refused. |
| **Garage** | ✅ | ✅ | ⚠️ parses but no atomicity | ⚠️ parses but no atomicity | **none — structurally impossible per Garage docs** ("cannot be safely implemented due to the lack of a consensus algorithm") | ❌ | Works fine for single-writer / best-effort deployments. Silent lost writes under contention. Not a bug — a design choice. v2.1.0 ignores both headers even for a single writer, so the startup check (D70) refuses it unless `NOTEDTHAT_S3_ALLOW_UNENFORCED_CONDITIONAL_WRITES=true`. |

**PATCH correctness note**: PATCH's step-10 conditional PUT uses the same `If-Match` semantics documented above for regular PUT. Backends classified as silent-200 backends in the table above (those that return 200 without actually enforcing `If-Match` on write) may silently overwrite concurrent PATCH results without surfacing a 412. Operators running PATCH workloads on those backends must be aware of this risk. The startup check (D70) refuses a backend that ignores `If-Match` outright; it cannot catch one that enforces it for a single writer and loses it under contention. SeaweedFS ≥ 4.09 and AWS S3 correctly enforce `If-Match` atomically and are safe for PATCH concurrency.

### 8.2 Rough recommendations to operators

- **One node, one operator, and you would rather not run an object store at all**: the
  **local filesystem** backend (D49). Full conditional-write correctness, and the store is a
  directory you can read, edit and back up with ordinary tools — edits made outside NotedThat are
  picked up and re-indexed (D50). One process per root, so it does not scale out.
- **You want it easy, want CAS, and are okay self-hosting**: **SeaweedFS ≥ 4.18** — small footprint, single binary, non-versioned buckets by default (matches NotedThat), full RFC 7232 conditional-write correctness, active project. **This is what NotedThat itself uses.**
- **You want CAS + zero egress + no self-hosting the object store**: **Cloudflare R2**.
- **You already run Kubernetes and want Ceph-grade correctness**: **Ceph RGW on Rook** (≥ v20.2.1).
- **You already run Ceph on bare metal**: **Ceph RGW** direct.
- **You need geo-distributed, mostly single-writer, don't need CAS**: **Garage**. Small, elegant. Pick this only if you accept last-writer-wins for concurrent writes.
- **AGPL is fine and you want a super-mature single-cluster S3**: **MinIO**.
- **You want Rust-native and are willing to pilot**: **RustFS** — track #3097 and #2737 before production.
- **You're on AWS anyway**: **AWS S3**. Cost-check bucket-per-KB at scale (§8.4).

### 8.3 What NotedThat does NOT do

These are about the S3-compatible backends this section is guidance for. The filesystem backend
(D49) has no server behind it, so it necessarily implements the conditional-request semantics
itself — that is the adapter *being* the backend, not a compensating layer sitting over one that
falls short.

- No compensating layer for missing backend features (no SQLite ETag mirror, no app-layer CAS arbiter).
- No capability probes at startup for an S3 backend. The filesystem backend does probe its root,
  for properties that would silently lose data rather than merely limit features (§8.1).
- No feature flags to disable header forwarding — headers are always forwarded verbatim to an S3
  backend.
- No object-lock / retention / legal-hold surface; WebDAV LOCK is refused (D17).
- No test-your-backend probe at startup. If you want to verify your backend really does honor `If-Match` under concurrency, use `ceph/s3-tests` — that's a deployer-side gate, not ours.

### 8.4 Bucket-per-KB at scale
Bucket-per-KB is safe on SeaweedFS, Garage, R2, RustFS (no limits, no per-bucket cost). On AWS the default quota is 10,000 buckets per account (raised from 100 in Nov 2024), free up to 2,000, ~$0.10/bucket/month above that. `ListBuckets` above 10k requires pagination.

Plan: ship bucket-per-KB only. Storage adapter is a trait so a prefix-per-KB fallback can be added if AWS deployments approach the quota. Do not build it now.

### 8.5 IDF drift on small BM25 corpora
Qdrant computes IDF at query time from live stats. On corpora < 10k documents, IDF shifts noticeably as documents are added — subtle search-quality regression. Mitigation: monitor eval scores; periodic full reindex (D42 defers this to post-v1); prefer RRF over DBSF (rank-based, more robust to IDF drift).

### 8.6 Filter selectivity vs sparse prefetch
Qdrant applies payload filters *after* sparse top-k. Selective filters starve fusion input. Mitigation: a floor of 100 on each arm's depth when a native payload filter is present (D56).

A second starvation mode is the searcher's own: a key filter applied client-side *after* fusion (`object_key_prefix`, or the caller's `search` grant) discards candidates the arms already spent their depth on. Mitigation: deepen both arms to `10 × limit` (cap 250) whenever such a filter applies, and apply it over the whole fused window before cutting the page (D56). The over-fetch has to be on the arms — a larger fused `limit` alone changes nothing, since fusion only ranks what the arms returned (#68).

---

## 9. Reference implementations to mine

- **`dav-server` v0.11** (github.com/messense/dav-server-rs) — `DavFileSystem` / `DavFile` / `DavMetaData` traits
- **RustFS `WebDavDriver`** (github.com/rustfs/rustfs → `crates/protocols/src/webdav/driver.rs`) — S3-backed `DavFileSystem` with write-buffer PUT pattern
- **`qdrant-client` v1.18** (github.com/qdrant/rust-client) — Query API with `PrefetchQueryBuilder`, `RrfBuilder`, `DocumentBuilder("qdrant/bm25")`
- **`pulldown-cmark` v0.13** — `Parser::new(...).into_offset_iter()` for byte-offset iteration
- **`serde_yaml_ng` v0.10** — OKF YAML frontmatter parsing
- **`aws-sdk-s3`** — with `path_style` for Garage/SeaweedFS/MinIO/Ceph/RustFS
- **`rmcp`** — official Rust MCP SDK
- **`jsonwebtoken`** (github.com/Keats/jsonwebtoken) — HS256 sign/verify
