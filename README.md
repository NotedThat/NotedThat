# NotedThat

A markdown-first knowledgebase system exposed as an HTTP API, MCP server, and WebDAV endpoint. NotedThat stores notes as plain Markdown files, indexes them for semantic search via Qdrant, and surfaces them through multiple access protocols so editors, AI agents, and WebDAV clients can all work with the same content.

> **Status: pre-v1** — under active development. APIs and crate interfaces are unstable.

## Documentation

- [SPECIFICATIONS.md](SPECIFICATIONS.md) — full product and architecture specification
- [DEVELOPMENT.md](DEVELOPMENT.md) — developer commands and test conventions
- [RELEASING.md](RELEASING.md) — release runbook and Trusted Publishing setup
- [LICENSE](LICENSE) — Mozilla Public License 2.0

## Crate Map

| Crate | Path | Role |
| ----- | ---- | ---- |
| `notedthat-core` | `crates/notedthat-core` | Shared domain types, path/range/error/auth primitives, config |
| `notedthat-storage-s3` | `crates/notedthat-storage-s3` | S3 storage adapter |
| `notedthat-indexer` | `crates/notedthat-indexer` | Chunking, embedder client, Qdrant integration |
| `notedthat-api-http` | `crates/notedthat-api-http` | HTTP API surface |
| `notedthat-webdav` | `crates/notedthat-webdav` | WebDAV surface |
| `notedthat-mcp` | `crates/notedthat-mcp` | MCP tool schemas and HTTP-backed implementation |
| `notedthat-server` | `crates/notedthat-server` | Main server binary — HTTP API + WebDAV in one process (release facade) |
| `notedthat-mcp-stdio` | `bin/notedthat-mcp-stdio` | MCP-over-stdio transport adapter |

All 8 crates share a single version via ecosystem-level Semantic Versioning. See [RELEASING.md](RELEASING.md) for the versioning policy.
