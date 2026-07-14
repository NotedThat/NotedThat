# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

NotedThat uses ecosystem-level Semantic Versioning: all 9 crates share a single
version; any breaking change in any crate increments the ecosystem major version.
See [RELEASING.md](RELEASING.md) for the full versioning policy.

## [Unreleased]

## [0.1.5](https://github.com/NotedThat/NotedThat/compare/v0.1.4...v0.1.5) - 2026-07-14

### Fixed

- *(release)* rewrite release.yml build job to match cargo-dist v0.32.0 schema

## [0.1.4](https://github.com/NotedThat/NotedThat/compare/v0.1.3...v0.1.4) - 2026-07-14

### Fixed

- *(server)* backtick WebDAV in main.rs docstring for clippy::doc-markdown
- *(release)* expand main.rs docstring to cross release-plz packaged-file filter

## [0.1.3](https://github.com/NotedThat/NotedThat/compare/v0.1.2...v0.1.3) - 2026-07-14

### Other

- *(clippy)* use assert_eq! for rust 1.97 manual_assert_eq lint

## [0.1.2](https://github.com/NotedThat/NotedThat/compare/v0.1.1...v0.1.2) - 2026-07-08

### Fixed

- *(release)* add description to 4 crates required by crates.io
- *(release)* add version specs to internal path deps for cargo publish

## [0.1.1](https://github.com/NotedThat/NotedThat/compare/v0.1.0...v0.1.1) - 2026-07-08

### Fixed

- *(write)* sniff .MD/.MARKDOWN as text/markdown (case-insensitive extension match)

### CI

- align release-plz jobs with upstream quickstart (drop wrong needs + concurrency)
- *(release)* invoke 'dist' instead of 'cargo dist' (cargo-dist executable renamed)

## [0.1.0](https://github.com/NotedThat/NotedThat/releases/tag/v0.1.0) - 2026-07-07

### Added

- *(storage-s3)* expose list continuation tokens
- *(storage-s3)* forward Range header to aws-sdk-s3, extract Content-Range/ETag
- *(storage-s3)* forward conditional headers and map 304/412/416 via SdkError inspection
- *(core)* extend Storage trait signatures for Range and ConditionalHeaders (BREAKING)
- *(storage-s3)* implement Storage trait against aws-sdk-s3 — closes #6
- *(indexer)* implement HybridSearcher::search with RRF fusion and error mapping
- *(indexer)* HybridSearcher struct scaffolding
- *(indexer)* translate SearchFilter to qdrant Filter
- *(indexer)* utf8-safe preview truncation
- *(indexer)* add Searcher trait
- *(indexer)* populate sparse_bm25 vector and extend payload (mime, tags, content_hash, text); add mime payload index
- *(indexer)* IndexerWorker task loop with Upsert/Tombstone handlers and drain-on-shutdown
- *(indexer)* QdrantProvisioner with idempotent ensure_collection and manifest cross-check
- *(indexer)* OpenAI-compatible embedder client with retry
- *(indexer)* thin QdrantClient wrapper and QdrantConfig
- *(indexer)* heading-aware markdown chunker with byte offsets
- *(indexer)* re-export Embedder and EmbedderError from lib.rs
- *(indexer)* add Embedder trait and EmbedderError enum
- *(indexer)* add Chunk type and stub chunker function
- *(indexer)* add IndexEvent enum for the async indexing queue
- *(write)* add replace() helper — server-side match/splice with two-ETag CAS (issue #39)
- *(write)* add ReplaceNoMatch + ReplaceAmbiguous WriteError variants + ReplaceOutcome (issue #39)
- *(write)* patch() bounded 2× retry on HEAD→GET / GET→PUT window 412
- *(write)* patch() primitive for line/byte/append splice with head_etag CAS anchor
- *(write)* WriteError PATCH variants + ApiError mapping
- *(api-http)* POST /replace/{*path} route + handler (issue #39)
- *(api-http)* add ReplaceAmbiguousBody for ambiguous_match count (issue #39)
- *(api-http)* PATCH error mapping + 416 line-mode headers
- *(api-http)* PATCH /v1/knowledgebases/{kb}/{path} route
- *(api-http)* 400/416 line-mode errors + X-Content-Range-Bytes on 416
- *(api-http)* emit Content-Range: lines + X-Content-Range-Bytes on line-mode 206
- *(api-http)* GET slices object with Range: lines=…
- *(server)* NOTEDTHAT_MAX_PATCHABLE_SIZE config (default 100 MiB)
- *(api)* accept opaque list cursor and return next_cursor
- *(api-http)* wire POST /search route with per-route body limit; promote lookup_kb to pub(crate)
- *(api-http)* search_kb handler with envelope-consistent errors
- *(api-http)* add searcher field to AppState
- *(api-http)* enqueue IndexEvent::Tombstone in DELETE handler
- *(api-http)* enqueue IndexEvent::Upsert in commit() after successful put
- *(api-http)* add AppState.indexer_tx and thread through test helpers
- *(api-http)* HEAD handler emits ETag and forwards conditionals
- *(api-http)* DELETE handler forwards If-Match and maps 412
- *(api-http)* extend InMemoryStorage mock with ETag, range slicing, precondition evaluation
- *(api-http)* extend commit() signature with ConditionalHeaders (BREAKING)
- *(api-http)* implement axum router with static Bearer auth — closes #7
- *(api-http)* add axum router skeleton, Bearer middleware, InMemoryStorage mock
- *(mcp-stdio)* fail-fast env var validation with specific error messages
- *(mcp-stdio)* stdio binary wiring with tracing→stderr
- *(server)* host mcp streamable http listener
- *(server)* add mcp http listener configuration
- *(server)* dual axum listeners with coordinated shutdown
- *(server)* add WebDAV config (listen addr + basic auth creds)
- *(server)* wire HybridSearcher construction
- *(server)* wire indexer worker with Qdrant provisioning and drain-then-signal shutdown
- *(server)* parse Qdrant and Embedding env vars
- *(server)* fail-fast startup + graceful shutdown — closes #8
- *(server)* Config struct with env parsing — partial closes #8
- `replace` MCP tool for content-based string replacement without byte/line coordinates (closes #39)
- `edit` MCP tool extended with optional byte-range args (`byte_start`, `byte_end`) alongside existing line-range args (closes #40)
- M9 milestone complete: MCP surface now exposes 10 tools total

### Fixed

- *(webdav)* map operation-specific indexer backpressure to 503
- *(storage-s3)* handle BucketAlreadyExists in ensure_bucket (SeaweedFS 4.18 with IAM)
- *(storage-s3)* add SeaweedFS IAM config to integration testcontainer (4.18 requires auth)
- add missing aws-sdk-s3 runtime features; fix clippy and fmt violations
- *(write)* surface indexer queue overflow with operation-specific WriteError variants
- *(m5)* address final wave review findings
- *(lint)* resolve clippy warnings across workspace
- *(indexer)* resolve clippy and rustfmt issues from F2 review
- satisfy clippy 1.96 (map_unwrap_or, duration_suboptimal_units)
- *(api-http)* POST to non-replace path returns 404 not 405; cargo fmt (issue #39)
- *(api-http)* map operation-specific indexer backpressure to 503
- *(api-http)* fix clippy lints in handler tests
- *(api-http)* fix cast_possible_wrap clippy lint in current_unix_seconds
- *(api-http)* fix clippy and fmt issues in M3 handler tests
- *(webdav,fmt)* add PROPFIND matrix coverage, fix stale doc comment, apply cargo fmt ([#36](https://github.com/NotedThat/NotedThat/pull/36))
- *(mcp-stdio-tests)* add m8 config fields to e2e test fixtures
- remaining clippy warnings for m8 delivery
- *(mcp-stdio)* clippy fixes in binary and test files
- *(server)* update stale M7 tool count to M9 (10 tools) in cross-surface E2E (issue #39)
- *(server)* resolve clippy warnings for m8 delivery
- *(mcp)* resolve clippy warnings for m8 delivery
- *(server)* add auth to WebDAV readiness probe in E2E test
- *(server)* resolve clippy warnings in cross-surface E2E test
- *(server)* resolve clippy warnings in run.rs (doc backticks + function length)
- *(server)* add SeaweedFS IAM config to e2e testcontainer (4.18 requires auth)
- *(server)* fix if_not_else and needless_pass_by_value clippy lints

### Other

- *(test)* update WebDAV test comments to use nt: namespace
- apply cargo fmt to storage-s3 integration tests
- *(storage-s3)* SeaweedFS integration tests for M3 scenarios
- apply cargo fmt across workspace
- add integration-test job with SeaweedFS testcontainer + integration tests
- fix clippy missing_docs, fmt, gitignore; commit Cargo.lock
- clean crate descriptions (remove phase-specific noise)
- *(storage-s3)* scaffold empty crate
- *(api)* paginate in-memory list storage with opaque cursors
- *(indexer)* searcher integration test with testcontainer
- *(indexer)* Qdrant testcontainer integration tests for provisioner + upsert + tombstone
- *(indexer)* worker E2E integration tests with chaos, backpressure, and drain-on-shutdown
- cargo fmt formatting adjustments
- *(indexer)* add crate deps and module skeleton
- *(indexer)* scaffold empty crate
- *(write)* RED replace() CAS + size cap + backpressure scenarios (issue #39)
- *(write)* RED replace() match/splice scenarios (issue #39)
- *(server)* E2E PATCH concurrency + append + size cap + ghost-state
- *(write)* extract shared commit() into notedthat-write crate
- *(api-http)* RED replace route 200/412/413/422/503 handler cases (issue #39)
- *(api-http)* RED replace route 400 handler cases (issue #39)
- *(api)* lock list cursor contract with 1500 object pagination
- *(api-http)* end-to-end search integration test
- *(api-http)* search handler unit tests
- cargo fmt formatting fixes across api-http, core, server
- *(api-http)* mock-based integration tests for M3 scenarios
- *(api-http)* scaffold empty crate
- *(mcp-stdio)* use multi-threaded runtime for fixture-backed e2e tests
- Merge pull request #42 from NotedThat/feat/publishing-infrastructure
- *(mcp-stdio)* fix Qdrant readiness and mock-embedder lifetime in e2e fixtures
- *(mcp-stdio)* expect exactly 10 tools including replace (issue #39)
- *(mcp-stdio)* expect edit and append tools
- *(server)* expose mcp stdio binary to integration tests
- *(mcp)* cross-surface indexer round-trip (MCP write → MCP search)
- *(mcp)* per-tool happy-path + error-path e2e
- *(mcp)* clean shutdown + stdin EOF exit
- *(mcp)* initialize + tools/list e2e assertions
- *(mcp)* integration test infrastructure (subprocess harness + fixtures in mcp-stdio crate)
- *(mcp-stdio)* verify stdout purity + clean EOF exit
- move notedthat-mcp-stdio from bin/ into crates/
- *(server)* give MCP HTTP listener its own port in webdav cross-surface fixture
- cross-ref sweep for M9 completion (issues #39 #40)
- *(server)* E2E concurrent replace race semantics (replace-vs-replace, replace-vs-PATCH, replace-vs-DELETE) (issue #39)
- *(server)* E2E replace no_match + ambiguous_match + 404 + If-Match guards (issue #39)
- *(server)* E2E cross-surface — HTTP write → MCP replace → HTTP GET (issue #39)
- *(server)* E2E replace happy + replace_all (issue #39)
- *(server)* support helpers for replace E2E (issue #39)
- *(server)* E2E line-range GET over SeaweedFS+Qdrant testcontainers
- *(server)* apply cargo fmt to config.rs test
- *(server)* prove mcp http and stdio search identity
- *(server)* cover mcp resources over http transport
- *(server)* cover mcp http auth failures and sse refusal
- *(server)* add mcp http e2e harness
- *(server)* verify three-listener graceful shutdown
- *(server)* keep readyz independent when mcp http disabled
- apply cargo fmt formatting
- *(server)* cross-surface E2E — WebDAV PUT becomes searchable via HTTP
- *(webdav)* scaffold real crate structure
- cargo fmt provision.rs
- *(server)* scaffold empty binary crate (facade)
