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
| `notedthat-write` | `crates/notedthat-write` | Shared write path (`commit()`, `commit_delete()`, MIME sniff, 5 GiB limit) — used by HTTP API + WebDAV surfaces |
| `notedthat-api-http` | `crates/notedthat-api-http` | HTTP API surface |
| `notedthat-webdav` | `crates/notedthat-webdav` | WebDAV surface |
| `notedthat-mcp` | `crates/notedthat-mcp` | MCP tool schemas and HTTP-backed implementation |
| `notedthat-server` | `crates/notedthat-server` | Main server binary — HTTP API + WebDAV + remote MCP in one process (release facade). Published to `ghcr.io/notedthat/server` per tagged release. |
| `notedthat-mcp-stdio` | `crates/notedthat-mcp-stdio` | MCP-over-stdio transport adapter |

All 9 crates share a single version via ecosystem-level Semantic Versioning. See [RELEASING.md](RELEASING.md) for the versioning policy.

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

### Docker (prebuilt image)

Once the first tagged release exists, the server image is published to GHCR at `ghcr.io/notedthat/server`. Every published image is cosign-signed (keyless via Sigstore/Fulcio) and carries a SLSA L2 build provenance attestation.

```sh
docker pull ghcr.io/notedthat/server:latest

docker run --rm -p 8080:8080 -p 8081:8081 \
  -e NOTEDTHAT_API_TOKEN=dev-token \
  -e NOTEDTHAT_KBS=notes,scratch \
  -e NOTEDTHAT_S3_ENDPOINT_URL=http://host.docker.internal:8333 \
  -e NOTEDTHAT_S3_REGION=us-east-1 \
  -e NOTEDTHAT_S3_ACCESS_KEY_ID=any \
  -e NOTEDTHAT_S3_SECRET_ACCESS_KEY=any \
  ghcr.io/notedthat/server:latest
```

Or use `docker compose up` with the bundled [docker-compose.yml](docker-compose.yml) for a full local stack (SeaweedFS + Qdrant + server).

Verify the signature + provenance before running in production:

```sh
gh attestation verify oci://ghcr.io/notedthat/server:0.2.0 --owner NotedThat
```

### WebDAV

With the server running (steps 1-2 above), also set the WebDAV credentials:

```sh
export NOTEDTHAT_WEBDAV_USERNAME=webdav-user-please-change
export NOTEDTHAT_WEBDAV_PASSWORD=webdav-pass-please-change
```

```sh
# 7. PROPFIND root (list KBs)
curl -X PROPFIND -u "$NOTEDTHAT_WEBDAV_USERNAME:$NOTEDTHAT_WEBDAV_PASSWORD" \
  -H 'Depth: 1' http://127.0.0.1:8081/

# 8. PUT a markdown file via WebDAV
echo "# Hello WebDAV" | curl -X PUT \
  -u "$NOTEDTHAT_WEBDAV_USERNAME:$NOTEDTHAT_WEBDAV_PASSWORD" \
  --data-binary @- \
  http://127.0.0.1:8081/notes/hello-webdav.md

# 9. GET it back
curl -u "$NOTEDTHAT_WEBDAV_USERNAME:$NOTEDTHAT_WEBDAV_PASSWORD" \
  http://127.0.0.1:8081/notes/hello-webdav.md

# 10. DELETE it
curl -X DELETE -u "$NOTEDTHAT_WEBDAV_USERNAME:$NOTEDTHAT_WEBDAV_PASSWORD" \
  http://127.0.0.1:8081/notes/hello-webdav.md
```

### MCP (Claude Desktop, Cursor, Zed)

With the server running (steps 1-2 above), configure your MCP client to launch `notedthat-mcp-stdio` as a subprocess.

#### Remote MCP hosting

NotedThat also exposes an HTTP MCP endpoint for remote clients that support the MCP HTTP transport:

- **Endpoint**: `POST /mcp` on port 8082 (configurable via `NOTEDTHAT_MCP_HTTP_BIND`)
- **Auth**: `Authorization: Bearer <NOTEDTHAT_API_TOKEN>` (same token as the HTTP API)
- **Note**: public deployments require HTTPS termination at a reverse proxy before exposing this port

See [`docs/API.md`](docs/API.md) for the full MCP transport and Resources protocol docs.

#### Install options

Three ways to get `notedthat-mcp-stdio` onto your `PATH` — all equivalent, pick whichever fits your setup. Installer scripts become available after the first tagged release.

**Shell installer** (macOS / Linux):

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/NotedThat/NotedThat/releases/latest/download/notedthat-mcp-stdio-installer.sh | sh
```

**PowerShell installer** (Windows):

```powershell
powershell -c "irm https://github.com/NotedThat/NotedThat/releases/latest/download/notedthat-mcp-stdio-installer.ps1 | iex"
```

**cargo install** (requires a Rust toolchain):

```sh
cargo install notedthat-mcp-stdio
```

Once installed, wire it into your MCP client:

```json
{
  "mcpServers": {
    "notedthat": {
      "command": "notedthat-mcp-stdio",
      "env": {
        "NOTEDTHAT_URL": "http://localhost:8080",
        "NOTEDTHAT_TOKEN": "your-token-here"
      }
    }
  }
}
```

Every prebuilt binary is cosign-signed with a SLSA L2 build provenance attestation — verify before running:

```sh
cosign verify-blob \
  --bundle notedthat-mcp-stdio.bundle \
  --certificate-identity-regexp 'https://github.com/NotedThat/NotedThat/.+' \
  --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \
  notedthat-mcp-stdio
```

#### Claude Desktop

Edit `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS) or `%APPDATA%\Claude\claude_desktop_config.json` (Windows):

```json
{
  "mcpServers": {
    "notedthat": {
      "command": "notedthat-mcp-stdio",
      "env": {
        "NOTEDTHAT_URL": "http://localhost:8080",
        "NOTEDTHAT_TOKEN": "your-token-here"
      }
    }
  }
}
```

Restart Claude Desktop after saving.

#### Cursor

Add to Cursor's settings (`.cursor/mcp.json` or via Settings → MCP):

```json
{
  "mcpServers": {
    "notedthat": {
      "command": "notedthat-mcp-stdio",
      "env": {
        "NOTEDTHAT_URL": "http://localhost:8080",
        "NOTEDTHAT_TOKEN": "your-token-here"
      }
    }
  }
}
```

#### Zed

Add to Zed settings (`~/.config/zed/settings.json`):

```json
{
  "assistant": {
    "mcp_servers": {
      "notedthat": {
        "command": {
          "path": "notedthat-mcp-stdio",
          "env": {
            "NOTEDTHAT_URL": "http://localhost:8080",
            "NOTEDTHAT_TOKEN": "your-token-here"
          }
        }
      }
    }
  }
}
```

Full environment variable reference: [`docs/CONFIGURATION.md`](docs/CONFIGURATION.md)

Full API documentation: [`docs/API.md`](docs/API.md)

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for the contribution process — PR workflow, commit conventions (Conventional Commits + signed + DCO), testing requirements, and AI-assistance disclosure. Build, test, and run commands live in [DEVELOPMENT.md](DEVELOPMENT.md). The project is pre-v1, so interfaces change frequently — check open issues before starting significant work.

## License

Mozilla Public License 2.0. See [LICENSE](LICENSE) for the full text.
