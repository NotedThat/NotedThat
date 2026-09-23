# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

NotedThat uses ecosystem-level Semantic Versioning: all 11 crates share a single
version; any breaking change in any crate increments the ecosystem major version.
See [RELEASING.md](RELEASING.md) for the full versioning policy.

## [Unreleased]

## [0.10.0](https://github.com/NotedThat/NotedThat/compare/v0.9.0...v0.10.0) - 2026-09-23

### Added

- *(mcp)* resources/subscribe, unsubscribe and listChanged fed by the object change events
- *(mcp)* [**breaking**] stateful streamable HTTP sessions at /mcp
- *(core)* reconcile — the two-cursor comparison and a backend-agnostic ETag walk
- *(storage-s3)* NOTEDTHAT_S3_RECONCILE switches the startup pass
- *(storage-s3)* list_objects reports each object's ETag
- *(api-http)* POST /knowledgebases/{kb}/index/reconcile for the service token
- *(mcp)* incremental SSE frame parser
- *(server)* reconcile every knowledge base against its bucket on the s3 backend

### Fixed

- address review on the s3 reconciliation pass
- *(mcp)* the keeper's stand-down tests and releases under one lock
- *(mcp)* stand the keeper down when the last subscription goes
- *(mcp)* address review on subscriptions and the stateful transport

### Other

- *(mcp)* one MCP session client for every HTTP suite
- record D66 — s3 reconciliation on startup and on demand
- Merge pull request #173 from NotedThat/feat/s3-reconcile
- *(storage-fs)* reconcile is a thin wrapper over core's comparison
- record MCP resource subscriptions on stateful sessions as D66
- *(server)* the s3 pass indexes, forgets and reports, with and without Docker

## [0.9.0](https://github.com/NotedThat/NotedThat/compare/v0.8.0...v0.9.0) - 2026-09-22

### Added

- [**breaking**] /readyz reports storage, search and events from a background prober
- *(core)* add object.indexed and object.index_failed events and the indexer source
- *(core)* [**breaking**] add Storage::probe for readiness checks
- *(indexer)* publish object.indexed and object.index_failed when an upsert or refresh completes
- *(indexer)* [**breaking**] add VectorStore::probe backed by Qdrant health_check
- *(api-http)* select and gate the index outcome events on the events stream
- *(api-http)* /readyz says degraded at the top when a check is

### Fixed

- *(storage)* the readiness probes answer through the same bucket lookup as every operation
- *(core)* a display_name is not blank and carries no control characters; pin the two live read-path branches
- *(server)* shutdown interrupts a readiness probe in flight; a gone bucket is degraded, not unready
- *(write)* a refused publish still enqueues the index work
- *(write)* publish the change event before enqueueing the index work
- *(server)* a readiness probe that hangs is waited for, not restarted every tick
- *(server)* [**breaking**] refuse to start when a Qdrant collection cannot be provisioned

### Other

- renumber the index outcome events decision to D65
- *(core)* one rule for the manifest's two free-text fields
- *(core)* [**breaking**] validate manifest display_name on load; remove the unused Kb struct
- *(events)* a verdict for a newer version supersedes an older write; publish-first leaves a refused write unindexed
- *(events)* round-trip the index outcome events through both logs
- index freshness has an address — GET …/index (D62), not a process-wide probe
- *(webdav)* [**breaking**] port WebDavFile read-path tests to StorageReadFile and delete the dead module
- *(mcp)* tell agents a write is searchable once object.indexed arrives
- *(server)* a write is announced, then indexed, then searchable

## [0.8.0](https://github.com/NotedThat/NotedThat/compare/v0.7.2...v0.8.0) - 2026-09-21

### Added

- [**breaking**] remove the notedthat-mcp-stdio binary; MCP is streamable HTTP only
- *(api-http)* expose index health at GET /knowledgebases/{kb}/index; add MCP index_status
- *(server,mcp-stdio)* NOTEDTHAT_MCP_MAX_READ_BYTES
- *(mcp)* admit the anonymous caller where the manifests admit one
- *(api-http)* a manifest is validated when it is written, not only at the next boot
- *(core)* [**breaking**] carry a description in the knowledge base manifest and list knowledge bases as objects
- *(indexer)* keep a per-knowledge-base index health record
- *(mcp)* return the read's own ETag and totals as structured content

### Fixed

- *(api-http)* reconcile counts go only to a caller who may list the whole knowledge base
- *(search)* [**breaking**] refuse unknown body keys; expose every filter over MCP
- *(core)* the in-memory storage double refuses an unprovisioned knowledge base
- *(core)* forbid hyphens in tenant slugs so bucket names are injective
- *(api-http)* a refused Range header says why
- *(core)* [**breaking**] reject multi-range byte reads and carry one range through Storage
- *(storage-s3)* map NoSuchBucket to BucketNotFound
- *(storage-fs)* an unreadable knowledge-base directory is unavailable, not gone
- *(storage-fs)* answer a missing knowledge-base directory with BucketNotFound
- *(indexer)* a reconciliation pass over one prefix says so
- *(indexer)* pending counts queued and in-flight work, from two monotonic counters
- *(write)* the manifest is validated wherever its bytes are stored
- *(api-http)* the failure summary and a pass's scope follow the list grant
- *(api-http)* a bucket-not-found reached through CoreError names no bucket
- *(api-http)* a missing bucket's 404 names neither the bucket nor the tenant
- *(mcp)* an over-budget move says how to move, not how to read
- *(mcp)* read's slice end comes from the body; structuredContent carries the text
- *(mcp)* bound every object read by the client's read budget
- *(mcp)* a newer adapter still lists a server that sends bare slugs
- *(server,mcp-stdio)* an empty NOTEDTHAT_MCP_MAX_READ_BYTES is the default

### Other

- rebase over #153–#157 — client_for as the Caller match, the read-budget e2e cases on McpSession, eleven tools
- *(notedthat)* McpSession builds every request the same way
- *(mcp)* read metadata end to end; oversized reads are redirected to slices
- *(mcp)* assert the anonymous listing by slug; renumber the decision to D59
- *(search)* match backticked field names so the accepted-key assertions are not vacuous
- renumber the tenant-slug decision to D58
- *(storage-fs)* a refused write is best-effort, not a guarantee
- *(storage)* a missing bucket is BucketNotFound on every backend
- Merge pull request #151 from NotedThat/fix/reject-multi-range-reads
- dev-dependencies on workspace crates carry no version; name an unpublished crate up front
- *(write)* the test-support dev-dependency on core carries no version
- *(api-http)* the missing-bucket fixture from #152 carries index_health
- *(api-http)* the missing-bucket fixture from #152 carries kb_details
- *(api-http,webdav)* a manifest startup would refuse is refused on every surface
- *(api-http)* describe MCP and WebDAV in /llms.txt, not only the HTTP API
- *(mcp)* say what a client actually receives for an unknown search argument
- *(mcp)* the filter parity check round-trips the API's filter through the tool type
- *(server)* cargo install notedthat installs one binary
- *(server)* the env-key inventory counts NOTEDTHAT_MCP_ANONYMOUS and NOTEDTHAT_MCP_MAX_READ_BYTES

## [0.7.2](https://github.com/NotedThat/NotedThat/compare/v0.7.1...v0.7.2) - 2026-09-21

### Other

- Merge pull request #150 from NotedThat/chore/inherit-workspace-metadata
- inherit homepage and authors from the workspace in every crate

## [0.7.1](https://github.com/NotedThat/NotedThat/compare/v0.7.0...v0.7.1) - 2026-09-21

### Added

- *(server)* NOTEDTHAT_EVENTS_BACKEND selector, NATS adapter and wiring
- *(api-http)* stream object change events over SSE per knowledge base
- *(indexer)* announce detected fs changes from the worker, once
- *(write)* publish object change events beside the index enqueue
- *(events)* notedthat-events crate with the memory ring adapter
- *(core)* object change event types and the EventPublisher trait

### Fixed

- *(events)* never skip an event on reconnect; a position ahead of the log is gone

### Other

- Merge pull request #141 from NotedThat/feat/object-change-events
- *(events)* renumber the decision to D55 and pin the crate at the workspace version
- *(events)* one integration suite over the memory ring and a real JetStream stream
- object change events — endpoint, backend selector, D54, overlay, example
- *(server)* two replicas on one NATS stream replay and expire events
- *(server)* events E2E over the memory log and the fs watcher

## [0.7.0](https://github.com/NotedThat/NotedThat/compare/v0.6.0...v0.7.0) - 2026-09-14

### Added

- *(mcp)* [**breaking**] search a list of knowledge bases in one call

### Fixed

- *(mcp)* drop the cap on the number of knowledge bases per search
- *(mcp)* cap and pace the multi-KB search fan-out

### Other

- Merge pull request #138 from NotedThat/feat/mcp-search-multi-kb
- *(mcp)* share the knowledge-base listing on the client

## [0.6.0](https://github.com/NotedThat/NotedThat/compare/v0.5.0...v0.6.0) - 2026-09-14

### Added

- *(server)* OIDC verifier with discovery, a cached JWKS and settings
- *(core)* [**breaking**] Authenticator with a pluggable bearer-token verifier
- *(core)* [**breaking**] deny rules (`may_not`) with deny-overrides evaluation
- *(core)* [**breaking**] identity-bearing principals and group/user rule subjects
- *(mcp)* act as the calling identity on the API
- *(server)* trust an internal CA for the issuer via NOTEDTHAT_OIDC_CA_CERT

### Fixed

- *(mcp)* refuse an HTTP call without a caller token instead of acting as the server

### Other

- *(server)* Authelia-backed OIDC e2e, Compose overlay and manual QA

## [0.5.0](https://github.com/NotedThat/NotedThat/compare/v0.4.0...v0.5.0) - 2026-09-10

### Added

- *(core)* [**breaking**] replace manifest public_read with path-scoped access rules
- *(core)* add the manifest access policy model
- *(core)* add glob key patterns for access rules
- *(api-http)* serve an HTML browse surface at /browse

### Fixed

- *(core)* drop the manual counter from the glob literal matcher
- *(api-http)* [**breaking**] answer an anonymous denial with the undeclared-slug 404
- *(api-http)* render a truncated browse folder instead of denying it
- *(api-http)* scan a browse page from the grant rather than the root
- *(api-http)* compare a listing prefix against the grant on a segment boundary
- *(mcp)* map HTTP 403 to a forbidden tool error

### Other

- renumber this branch's decisions around main's D50
- *(core)* share the directory rollup between surfaces
- *(api-http)* make the route backstop cover the credentialed principal
- *(api-http)* mark the browse page title raw, and sanitise it
- describe access rules and the browse surface
- *(server)* assert the concealed 404 on the cross-surface access suite
- *(server)* browse a knowledge base end to end

## [0.4.0](https://github.com/NotedThat/NotedThat/compare/v0.3.1...v0.4.0) - 2026-09-10

### Added

- *(storage-fs)* make watching configurable
- *(storage-fs)* watch the tree for out-of-band changes
- *(storage-fs)* reconcile a knowledge base against its index
- *(indexer)* [**breaking**] skip re-indexing an object whose content has not changed
- *(indexer)* [**breaking**] report which objects a collection has indexed
- *(server)* index changes made to the filesystem tree directly

### Fixed

- *(storage-fs)* charge pending capacity to the knowledge base that pays for it
- *(storage-fs)* drop a removed directory's descendants from the watch set
- *(storage-fs)* stop reconciliation reporting the manifest every pass
- *(indexer)* tell a missing collection apart from an empty one

### Other

- *(storage-s3)* say where this crate's backend tests went
- *(storage)* cover list_objects pagination, and stop over-promising the rest
- *(storage)* run one integration suite against both real backends
- Merge pull request #119 from NotedThat/feat/file-watch
- say what these four comments' code actually does
- record filesystem watching and its limits
- *(indexer)* pin the order indexed_objects returns

## [0.3.1](https://github.com/NotedThat/NotedThat/compare/v0.3.0...v0.3.1) - 2026-09-09

### Added

- *(server)* accept a command-line flag for every setting
- *(mcp-stdio)* accept --url and --token

### Other

- *(config)* document the command line and its precedence
- *(cli)* cover flag-over-environment precedence end to end
- *(config)* separate environment reading from validation

## [0.3.0](https://github.com/NotedThat/NotedThat/compare/v0.2.0...v0.3.0) - 2026-09-09

### Added

- *(server)* [**breaking**] select the storage backend from the environment

### Fixed

- *(storage-s3)* serve the first byte range and map a missing copy source

### Other

- *(release)* make notedthat the release facade
- drop two leftovers the filesystem review turned up
- *(storage-fs)* pin the SeaweedFS quirks the conformance suite found
- *(storage-fs)* assert the storage backends agree, and fix the substitute
- *(core)* share precondition and ETag logic across backends
- *(server)* run the API over the filesystem backend end to end

## [0.2.0](https://github.com/NotedThat/NotedThat/compare/v0.1.6...v0.2.0) - 2026-09-09

### Added

- *(notedthat)* [**breaking**] ship both binaries from a new distribution crate
- *(server)* [**breaking**] reject removed listener environment variables
- *(server)* [**breaking**] unify HTTP surfaces on one listener

### Fixed

- *(api-http)* restore observability on the unauthenticated root routes

### Other

- *(api-http)* declare each API route once
- *(api-http)* name the route prefixes once
- *(webdav)* cover COPY/MOVE destination confinement to /webdav
- *(mcp)* remove the unreachable SSE refusal predicate

### Added

- *(notedthat)* new distribution crate owning both published binaries;
  `cargo install notedthat` installs `notedthat-server` and `notedthat-mcp-stdio`.
- *(mcp-stdio)* `run()`, the MCP-over-stdio entry point, promoted from the binary
  into the library.

### Changed

- *(release)* `notedthat` is now the workspace's only cargo-dist app. Each target
  ships one `notedthat-<target>` archive carrying both binaries, behind a single
  `notedthat-installer.sh` / `.ps1`, replacing the per-binary archives and installers.

### Removed

- *(server)* **Breaking:** `notedthat-server` no longer has a binary target.
  Install the binary with `cargo install notedthat`.
- *(mcp-stdio)* **Breaking:** `notedthat-mcp-stdio` no longer has a binary target.
  `cargo install notedthat-mcp-stdio` no longer installs anything; use
  `cargo install notedthat`. The binary name is unchanged, so MCP client
  configuration needs no edits.
- *(server)* `build.rs`, which synthesized `CARGO_BIN_EXE_notedthat-mcp-stdio` for a
  cross-package dev-dependency. The suites that drive the binaries now live in the
  package that declares them, which also removes the server ↔ mcp-stdio
  dev-dependency cycle.

## [0.1.6](https://github.com/NotedThat/NotedThat/compare/v0.1.5...v0.1.6) - 2026-07-14

### Added

- *(api)* add LLM navigation document
- *(api)* enforce manifest-controlled public reads
- *(core)* add manifest public-read policy
- *(okf)* index concept metadata and expose search filters

### Fixed

- *(server)* drain indexer during container shutdown
- *(mcp)* prevent self-move deletion
- *(docker)* package MCP stdio and expose tuning
- *(docker)* use default upload staging directory
- *(indexer)* preserve streaming heading boundaries
- *(webdav)* infer MIME from COPY destination
- *(server)* configure shared disk-backed upload staging
- *(indexer)* stream snapshots and remove obsolete chunks after batches
- *(webdav)* protect mutations and reuse PROPFIND listings
- *(storage)* stage large bodies and use conditional native copies
- *(mcp)* return a ready future when listing tools
- *(indexer)* clear stale hits when replacement chunks exceed limits
- *(indexer)* backfill payload indexes on an existing collection
- *(indexer)* set an explicit Qdrant client timeout
- *(indexer)* keep the Qdrant message on write and DDL paths
- *(release)* define [profile.dist] required by cargo-dist
- *(server)* restore staging-first startup and gate the test seam
- *(ci)* restore the auto-release path
- harden public-read authorization boundaries

### Other

- clarify local deployment flows
- *(indexer)* await all expected payload indexes
- *(okf)* document search behavior and upgrade steps
- *(server)* isolate storage for multi-chunk integration tests
- wait on readiness signals in container-backed tests
- split HTTP/WebDAV modules and share storage test helpers
- document manifest-controlled public reads
- *(webdav)* replace the SeaweedFS testcontainer with InMemoryStorage
- *(indexer)* add a VectorStore seam and mock the Qdrant suites
- *(server)* run the E2E suites over injected in-process backends
- *(indexer)* pin InMemoryVectorStore against Qdrant with a conformance suite
- pin the stress skip set and close the release-plz index race
- correct what the converted suites actually run against

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
- *(api-http)* PATCH /api/v1/knowledgebases/{kb}/{path} route
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
