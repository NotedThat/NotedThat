# NotedThat

A markdown-first knowledgebase system exposed as an HTTP API, MCP server, and WebDAV endpoint. NotedThat stores notes as plain Markdown files, indexes them for semantic search via Qdrant, and surfaces them through multiple access protocols so editors, AI agents, and WebDAV clients can all work with the same content.

> **Status: pre-v1** — under active development. APIs and crate interfaces are unstable.

## Documentation

- [SPECIFICATIONS.md](SPECIFICATIONS.md) — full product and architecture specification
- [API reference](docs/API.md) — HTTP, WebDAV, MCP, and anonymous-read behavior
- [Configuration](docs/CONFIGURATION.md) — environment and manifest operations
- [DEVELOPMENT.md](DEVELOPMENT.md) — developer commands and test conventions
- [Open Knowledge Format](docs/OKF.md) — concept metadata, search filters, and example bundle
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

## Anonymous public reads

Each knowledge base has its own S3 bucket, and that bucket is the policy boundary. Its
`.notedthat/manifest.json` may add a `public_read` array with independent `discover`, `browse`,
`content`, and `search` capabilities. Missing or empty means private. This does not create an
anonymous write mode: every HTTP and WebDAV mutation remains authenticated, valid credentials
retain full access, and supplied invalid credentials receive `401` rather than anonymous access.

The server reads policies once during startup; restart it after changing a manifest. Public-read
policy does not apply to MCP authentication, does not support namespace or path-prefix grants, and
does not add an application rate limiter. Configure reverse-proxy rate and burst limits before
exposing anonymous search. See [Configuration](docs/CONFIGURATION.md#manifest-controlled-anonymous-reads)
for the manifest and operational procedure.

## Running locally

NotedThat needs an S3-compatible object store, Qdrant, and an OpenAI-compatible
embedding provider. It does not bundle an embedding model or provider credentials.
Use one of the supported flows below and keep provider credentials in ignored local
environment files; never commit them.

### Compose: local storage and search with your embedding provider

```sh
# Create an ignored local configuration, then replace all four embedding values.
cp .env.example .env
$EDITOR .env
docker compose up --build -d

# Verify HTTP, upload, read, and semantic search.
TOKEN=dev-token-please-change
curl --fail http://127.0.0.1:8080/healthz
printf '# Hello NotedThat\nsemantic search works\n' | curl --fail -X PUT \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: text/markdown' \
  --data-binary @- http://127.0.0.1:8080/api/v1/knowledgebases/notes/hello.md
curl --fail -H "Authorization: Bearer $TOKEN" \
  http://127.0.0.1:8080/api/v1/knowledgebases/notes/hello.md
curl --fail -X POST -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' -d '{"query":"semantic search"}' \
  http://127.0.0.1:8080/api/v1/knowledgebases/notes/search
```

The search result is asynchronous: retry the final request until the uploaded document
appears. Stop the local stack with `docker compose down`; add `-v` only when you
intentionally want to delete its SeaweedFS and Qdrant data.

`docker-compose.yml` fixes its internal listener, SeaweedFS, and Qdrant addresses.
It forwards documented size, Qdrant-timeout, and embedding-tuning settings from
`.env`. The bundled Qdrant service is deliberately unauthenticated, so it does not
accept a Qdrant API key. A custom `NOTEDTHAT_UPLOAD_TMP_DIR` also needs an explicit
writable mount at that exact path in a custom Compose deployment.

### Native server: Compose-managed dependencies

Start SeaweedFS and Qdrant, then export host-facing settings before running the
server natively. Reuse your local embedding values from `.env` without committing it.

```sh
docker compose up -d seaweedfs qdrant
set -a; . ./.env; set +a
export NOTEDTHAT_LISTEN_ADDR=127.0.0.1:8080
export NOTEDTHAT_S3_ENDPOINT_URL=http://127.0.0.1:8333
export NOTEDTHAT_S3_FORCE_PATH_STYLE=true
export NOTEDTHAT_QDRANT_URL=http://127.0.0.1:6334
cargo run -p notedthat-server
```

Use the upload, read, and search commands from the Compose flow against this server.

### Docker image

Once the first tagged release exists, the server image is published to GHCR at `ghcr.io/notedthat/server`. Every published image is cosign-signed (keyless via Sigstore/Fulcio) and carries a SLSA L2 build provenance attestation.

```sh
docker pull ghcr.io/notedthat/server:latest

# Start the storage/index dependencies first. On Linux, use --add-host below.
docker compose up -d seaweedfs qdrant
set -a; . ./.env; set +a
docker run --rm --stop-timeout 45 --add-host=host.docker.internal:host-gateway \
  -p 8080:8080 \
  -e NOTEDTHAT_API_TOKEN -e NOTEDTHAT_KBS \
  -e NOTEDTHAT_S3_REGION -e NOTEDTHAT_S3_ACCESS_KEY_ID -e NOTEDTHAT_S3_SECRET_ACCESS_KEY \
  -e NOTEDTHAT_WEBDAV_USERNAME -e NOTEDTHAT_WEBDAV_PASSWORD \
  -e EMBEDDING_ENDPOINT_URL -e EMBEDDING_MODEL -e EMBEDDING_API_KEY -e EMBEDDING_DIMENSIONS \
  -e NOTEDTHAT_S3_ENDPOINT_URL=http://host.docker.internal:8333 \
  -e NOTEDTHAT_S3_FORCE_PATH_STYLE=true \
  -e NOTEDTHAT_QDRANT_URL=http://host.docker.internal:6334 \
  ghcr.io/notedthat/server:latest
```

The image publishes one HTTP port, **8080**: the API is under `/api/v1`, WebDAV is under
`/webdav`, and streamable MCP is at `POST /mcp`. Public deployments should terminate TLS once at
a reverse proxy and forward the complete path space to this upstream.

The default temporary directory stages uploads and index snapshots. For production-sized uploads,
ensure its backing filesystem has at least 5 GiB for every concurrent maximum-size upload, plus
index snapshots and backend/build working space; configure
`NOTEDTHAT_UPLOAD_TMP_DIR` when a specific writable location is needed. Do not use `tmpfs`,
which consumes RAM. See [configuration](docs/CONFIGURATION.md#upload-and-index-staging-directory)
for cleanup behavior and sizing details.

Verify the signature + provenance before running in production:

```sh
gh attestation verify oci://ghcr.io/notedthat/server:0.2.0 --owner NotedThat
```

### WebDAV

With the server running, configure a WebDAV client with the credentials from your local `.env`:

```sh
export NOTEDTHAT_WEBDAV_USERNAME=webdav-user-please-change
export NOTEDTHAT_WEBDAV_PASSWORD=webdav-pass-please-change
```

```sh
# 7. PROPFIND root (list KBs)
curl -X PROPFIND -u "$NOTEDTHAT_WEBDAV_USERNAME:$NOTEDTHAT_WEBDAV_PASSWORD" \
  -H 'Depth: 1' http://127.0.0.1:8080/webdav/

# 8. PUT a markdown file via WebDAV
echo "# Hello WebDAV" | curl -X PUT \
  -u "$NOTEDTHAT_WEBDAV_USERNAME:$NOTEDTHAT_WEBDAV_PASSWORD" \
  --data-binary @- \
  http://127.0.0.1:8080/webdav/notes/hello-webdav.md

# 9. GET it back
curl -u "$NOTEDTHAT_WEBDAV_USERNAME:$NOTEDTHAT_WEBDAV_PASSWORD" \
  http://127.0.0.1:8080/webdav/notes/hello-webdav.md

# 10. DELETE it
curl -X DELETE -u "$NOTEDTHAT_WEBDAV_USERNAME:$NOTEDTHAT_WEBDAV_PASSWORD" \
  http://127.0.0.1:8080/webdav/notes/hello-webdav.md
```

### MCP (Claude Desktop, Cursor, Zed)

With the server running, configure your MCP client to launch `notedthat-mcp-stdio` as a subprocess.

#### Remote MCP hosting

NotedThat also exposes an HTTP MCP endpoint for remote clients that support the MCP HTTP transport:

- **Endpoint**: `POST /mcp` on the same listener as the API and WebDAV
- **Auth**: `Authorization: Bearer <NOTEDTHAT_API_TOKEN>` (same token as the HTTP API)
- **Note**: public deployments require HTTPS termination at a reverse proxy before exposing this listener

See [`docs/API.md`](docs/API.md) for the full MCP transport and Resources protocol docs.

#### Install options

Four ways to get `notedthat-mcp-stdio` onto your `PATH` — all equivalent, pick whichever fits your setup. Installer scripts become available after the first tagged release.

**Build or extract locally:**

```sh
make mcp-stdio                         # build from this checkout
make mcp-stdio-from-image              # extract from notedthat-server:local
# Override PREFIX=/some/dir or IMAGE=ghcr.io/notedthat/server:tag as needed.
```

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
