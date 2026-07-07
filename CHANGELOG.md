# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

NotedThat uses ecosystem-level Semantic Versioning: all 9 crates share a single
version; any breaking change in any crate increments the ecosystem major version.
See [RELEASING.md](RELEASING.md) for the full versioning policy.

## [Unreleased]

## [0.1.0](https://github.com/NotedThat/NotedThat/releases/tag/v0.1.0) - 2026-07-07

### Added

- *(server)* NOTEDTHAT_MAX_PATCHABLE_SIZE config (default 100 MiB)
- *(server)* host mcp streamable http listener
- *(server)* add mcp http listener configuration
- *(server)* dual axum listeners with coordinated shutdown
- *(server)* add WebDAV config (listen addr + basic auth creds)
- *(server)* wire HybridSearcher construction
- *(server)* wire indexer worker with Qdrant provisioning and drain-then-signal shutdown
- *(api-http)* add AppState.indexer_tx and thread through test helpers
- *(server)* parse Qdrant and Embedding env vars
- *(server)* fail-fast startup + graceful shutdown — closes #8
- *(server)* Config struct with env parsing — partial closes #8

### Fixed

- satisfy clippy 1.96 (map_unwrap_or, duration_suboptimal_units)
- *(server)* update stale M7 tool count to M9 (10 tools) in cross-surface E2E (issue #39)
- *(api-http)* POST to non-replace path returns 404 not 405; cargo fmt (issue #39)
- *(webdav,fmt)* add PROPFIND matrix coverage, fix stale doc comment, apply cargo fmt ([#36](https://github.com/NotedThat/NotedThat/pull/36))
- *(server)* resolve clippy warnings for m8 delivery
- remaining clippy warnings for m8 delivery
- *(mcp)* resolve clippy warnings for m8 delivery
- *(server)* add auth to WebDAV readiness probe in E2E test
- *(server)* resolve clippy warnings in cross-surface E2E test
- *(server)* resolve clippy warnings in run.rs (doc backticks + function length)
- *(server)* add SeaweedFS IAM config to e2e testcontainer (4.18 requires auth)
- *(m5)* address final wave review findings
- *(server)* fix if_not_else and needless_pass_by_value clippy lints
- add missing aws-sdk-s3 runtime features; fix clippy and fmt violations

### Other

- *(server)* give MCP HTTP listener its own port in webdav cross-surface fixture
- Merge pull request #42 from NotedThat/feat/publishing-infrastructure
- cross-ref sweep for M9 completion (issues #39 #40)
- *(server)* E2E concurrent replace race semantics (replace-vs-replace, replace-vs-PATCH, replace-vs-DELETE) (issue #39)
- *(server)* E2E replace no_match + ambiguous_match + 404 + If-Match guards (issue #39)
- *(server)* E2E cross-surface — HTTP write → MCP replace → HTTP GET (issue #39)
- *(server)* E2E replace happy + replace_all (issue #39)
- *(server)* support helpers for replace E2E (issue #39)
- *(server)* E2E PATCH concurrency + append + size cap + ghost-state
- *(server)* E2E line-range GET over SeaweedFS+Qdrant testcontainers
- *(server)* apply cargo fmt to config.rs test
- *(server)* prove mcp http and stdio search identity
- *(server)* cover mcp resources over http transport
- *(server)* cover mcp http auth failures and sse refusal
- *(server)* add mcp http e2e harness
- *(server)* verify three-listener graceful shutdown
- *(server)* keep readyz independent when mcp http disabled
- apply cargo fmt formatting
- *(server)* expose mcp stdio binary to integration tests
- *(server)* cross-surface E2E — WebDAV PUT becomes searchable via HTTP
- *(webdav)* scaffold real crate structure
- cargo fmt provision.rs
- cargo fmt formatting fixes across api-http, core, server
- apply cargo fmt across workspace
- add integration-test job with SeaweedFS testcontainer + integration tests
- fix clippy missing_docs, fmt, gitignore; commit Cargo.lock
- clean crate descriptions (remove phase-specific noise)
- *(server)* scaffold empty binary crate (facade)

### Added

- `replace` MCP tool for content-based string replacement without byte/line coordinates (closes #39)
- `edit` MCP tool extended with optional byte-range args (`byte_start`, `byte_end`) alongside existing line-range args (closes #40)
- M9 milestone complete: MCP surface now exposes 10 tools total
