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
| `notedthat-mcp-stdio` | `crates/notedthat-mcp-stdio` | MCP-over-stdio transport adapter |

All 8 crates share a single version via ecosystem-level Semantic Versioning. See [RELEASING.md](RELEASING.md) for the versioning policy.

## Running locally

NotedThat requires an S3-compatible object store. For local development, we use
[SeaweedFS](https://github.com/seaweedfs/seaweedfs) >= 4.18 in Docker.

```sh
# 1. Start SeaweedFS on the S3 gateway port (8333)
docker run -d --name nt-seaweedfs \
  -p 8333:8333 \
  chrislusf/seaweedfs:4.18 server -s3 -filer

# 2. Start the NotedThat server
NOTEDTHAT_API_TOKEN=dev-token \
NOTEDTHAT_KBS=notes,scratch \
NOTEDTHAT_S3_ENDPOINT_URL=http://127.0.0.1:8333 \
NOTEDTHAT_S3_REGION=us-east-1 \
NOTEDTHAT_S3_ACCESS_KEY_ID=any \
NOTEDTHAT_S3_SECRET_ACCESS_KEY=any \
NOTEDTHAT_S3_FORCE_PATH_STYLE=true \
NOTEDTHAT_LISTEN_ADDR=127.0.0.1:8080 \
RUST_LOG=info,notedthat=debug \
cargo run -p notedthat-server

# 3. Verify the server is running
curl http://127.0.0.1:8080/healthz

# 4. Try the API
curl -H "Authorization: Bearer dev-token" \
     http://127.0.0.1:8080/v1/knowledgebases

# 5. Upload a file
echo "# Hello World" | curl -X PUT \
  -H "Authorization: Bearer dev-token" \
  -H "Content-Type: text/markdown" \
  --data-binary @- \
  http://127.0.0.1:8080/v1/knowledgebases/notes/hello.md

# 6. Read it back
curl -H "Authorization: Bearer dev-token" \
     http://127.0.0.1:8080/v1/knowledgebases/notes/hello.md
```

Full environment variable reference: [`docs/CONFIGURATION.md`](docs/CONFIGURATION.md)

Full API documentation: [`docs/API.md`](docs/API.md)

## Contributing

See [DEVELOPMENT.md](DEVELOPMENT.md) for how to build, test, and run the project locally. The project is pre-v1, so interfaces change frequently. Check open issues before starting significant work.

## License

Mozilla Public License 2.0. See [LICENSE](LICENSE) for the full text.
