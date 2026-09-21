# NotedThat

**A folder of Markdown files that every AI assistant, automation tool and file manager can
open — and that decides, per caller, what each of them may touch.**

NotedThat stores notes as plain Markdown, indexes them for hybrid semantic + keyword search, and
serves the same files three ways at once: an [HTTP API](docs/API.md), an
[MCP server](docs/CLIENTS.md) for anything that speaks the Model Context Protocol, and a
[WebDAV share](docs/WEBDAV.md) you mount as a folder. One set of
[access rules](#access-rules) per knowledge base governs all three.

> **Status: pre-v1** — under active development. APIs and crate interfaces are unstable.

## Why not Obsidian and a sync folder?

Keep Obsidian. Point it at the mounted folder and nothing about writing notes changes. What a
sync folder cannot do is the rest:

- **Anything that speaks MCP is a client.** Claude Code, claude.ai, Claude Desktop, Cursor,
  VS Code, Zed, n8n, Windmill, the MCP Inspector — ten tools (`search`, `read`, `write`, `edit`,
  `append`, `replace`, `list`, `move`, `delete`, `list_knowledgebases`) over one HTTP endpoint
  or a local stdio adapter. An assistant searches your notes for context and files what it
  learned; a workflow appends the day's output to a journal. [Connecting clients →](docs/CLIENTS.md)
- **Anything that opens a folder is a client.** GNOME Files, Dolphin, davfs2, rclone, WinSCP —
  mount the share and interact with it like a human would. What you save is searchable moments
  later; what an agent wrote is sitting in the folder. [Mounting →](docs/WEBDAV.md)
- **Every caller gets its own rules.** A sync folder has one identity: whoever holds the folder
  holds everything. Here a knowledge base carries rules naming who (`anyone`, `signed-in`,
  `group:editors`, `user:agent-bot`) may do what (`list`, `read`, `write`, `delete`, `search`)
  where (`inbox/**`). So a teammate can be given `research/**` and nothing else; an agent with its own identity
  that you do not fully trust can read the vault and write only to `inbox/`; the public can read
  `public/**` and search nothing. The same evaluator answers for the API, MCP, WebDAV and the browse pages.
  [Access rules →](#access-rules)
- **Search is a property of the store, not of an app.** Every write from every surface is
  indexed; `search` is hybrid semantic + keyword across one or many knowledge bases, with
  filters, from any client.
- **Changes are a stream.** `GET …/events` streams every object change with replay, so a workflow
  can react to a note a colleague or an agent just created.
  [Events →](docs/API.md#get-apiv1knowledgebaseskb_slugevents)

Identity comes from your own OpenID Connect provider — Authentik, Authelia and Zitadel are
documented — and MCP clients that support OAuth sign in through it, so an assistant acts as the
person using it, not as a shared super-user. NotedThat mints no tokens and keeps no sessions.

## Plug it in

The server is running (see [Running locally](#running-locally)). Then:

**Claude Code** — one command:

```sh
claude mcp add --transport http notedthat https://notes.example.com/mcp \
  --header "Authorization: Bearer $NOTEDTHAT_API_TOKEN"
```

**n8n** — an *MCP Client Tool* node on an AI Agent: endpoint `https://notes.example.com/mcp`,
transport *HTTP Streamable*, *Bearer Auth* with the token.

**claude.ai** — Settings → Connectors → *Add custom connector* with the same URL; it signs in
through your OIDC provider.

**A folder** — `davs://notes.example.com/webdav/` in GNOME Files, or

```sh
rclone config create notedthat webdav url=https://notes.example.com/webdav/ vendor=other \
  user=webdav-user pass=webdav-pass --obscure
rclone mount notedthat:notes ~/Notes --vfs-cache-mode writes
```

**Anything else** — `curl` against the [HTTP API](docs/API.md), or hand a model
`https://notes.example.com/llms.txt` and let it work the API out itself.

Every client that is not on the server's own machine needs the public hostname in
`NOTEDTHAT_MCP_HTTP_ALLOWED_HOSTS`, and TLS at a reverse proxy in front. Full setup for
Claude Desktop, Cursor, VS Code, Zed, Windmill and the stdio adapter: [docs/CLIENTS.md](docs/CLIENTS.md).
Every mount option, including Windows and the `LOCK` caveat: [docs/WEBDAV.md](docs/WEBDAV.md).

## Documentation

- [Connecting clients](docs/CLIENTS.md) — Claude Code, claude.ai, Claude Desktop, Cursor, VS Code, Zed, n8n, Windmill, and any other MCP client
- [Mounting a knowledge base](docs/WEBDAV.md) — WebDAV on Linux, macOS, Windows; rclone; Obsidian
- [API reference](docs/API.md) — HTTP, WebDAV, MCP, events, and anonymous-read behavior
- [Configuration](docs/CONFIGURATION.md) — environment, manifest access rules, OIDC, storage and events backends
- [SPECIFICATIONS.md](SPECIFICATIONS.md) — full product and architecture specification
- [Open Knowledge Format](docs/OKF.md) — concept metadata, search filters, and example bundle
- [DEVELOPMENT.md](DEVELOPMENT.md) — developer commands and test conventions
- [RELEASING.md](RELEASING.md) — release runbook and Trusted Publishing setup
- [LICENSE](LICENSE) — Mozilla Public License 2.0

## Access rules

Each knowledge base has its own S3 bucket, and that bucket is the policy boundary. Its
`.notedthat/manifest.json` carries an `access` array of rules, each naming a subject (`anyone` for
callers with no credential, `signed-in` for any verified credential, `group:<name>` or
`user:<name>` for callers an OIDC provider vouched for), the verbs it grants with `may` or revokes
with `may_not` (`list`, `read`, `write`, `delete`, `search`), and optionally the object-key globs it
applies `under`:

```json
"access": [
  { "who": "anyone",        "may": ["list", "read"], "under": ["public/**"] },
  { "who": "signed-in",     "may": ["list", "read", "search"] },
  { "who": "group:editors", "may": ["write", "delete"] },
  { "who": "group:interns", "may_not": ["read", "search"], "under": ["hr/**"] }
]
```

Private by default; deny overrides allow, and rule order never matters. The rules govern the HTTP
API, WebDAV, MCP and the browse pages from one evaluator, and they bind the token holder as well as
anonymous callers — so a read-only deployment is expressible, and so is a mistake that locks you
out. `.notedthat` is the exception that makes that recoverable: reachable only for
`NOTEDTHAT_API_TOKEN`, always, and for nobody else.

Granting `write` or `delete` to `anyone` refuses startup. The server reads policies once at
startup; restart it after editing a manifest.

## Who is signed in

`NOTEDTHAT_API_TOKEN` is the deployment's own credential. Point the server at an OpenID Connect
issuer — Authentik, Authelia and Zitadel are documented — and it also accepts that issuer's JWT
access tokens on every surface, MCP included; the token's groups are what `group:` rules match.
NotedThat mints no tokens and keeps no sessions. See
[OIDC authentication](docs/CONFIGURATION.md#oidc-authentication). There is no application rate limiter — configure
reverse-proxy rate and burst limits before exposing anonymous search. See
[Configuration](docs/CONFIGURATION.md#manifest-access-rules) for the full model and the upgrade
path from the removed `public_read` field.

## Browse surface

`/browse` serves plain server-rendered HTML directory listings of whatever the caller may read —
no JavaScript, no accounts, no editing. File links point at the object's existing `/api/v1` URL
rather than a second download path. See the [API reference](docs/API.md#browse-surface).

## Running locally

NotedThat needs somewhere to keep objects — a local directory or any S3-compatible
object store — plus Qdrant and an OpenAI-compatible embedding provider. It does not
bundle an embedding model or provider credentials. Use one of the supported flows below
and keep provider credentials in ignored local environment files; never commit them.

The default Compose stack stores objects on disk, so it runs no object store at all.
Add the S3 overlay when you want the production-shaped setup — that is what CI's
integration suite exercises, and what the reference deployment runs.

### Compose: objects on disk, search alongside

```sh
# Create an ignored local configuration, then replace all four embedding values.
cp .env.example .env
$EDITOR .env
docker compose up --build -d

# Objects live in the `notedthat-data` volume; nothing else is needed to store them.

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
intentionally want to delete its object and Qdrant data.

`docker-compose.yml` fixes its internal listener and Qdrant address, stores objects
under `/var/lib/notedthat` in the `notedthat-data` volume, and forwards documented
size, Qdrant-timeout, and embedding-tuning settings from `.env`. The bundled Qdrant
service is deliberately unauthenticated, so it does not accept a Qdrant API key. A
custom `NOTEDTHAT_UPLOAD_TMP_DIR` also needs an explicit writable mount at that exact
path in a custom Compose deployment.

### Compose with an identity provider

Adds Authelia with two users (`alice` in `editors`, `ivan` in `interns`) and points the server
at it, so identity tokens, `group:` rules and MCP's OAuth discovery can be tried end to end:

```sh
docker compose -f docker-compose.yml -f docker-compose.auth.yml up --build -d
docs/manual-qa/oidc-mcp.sh          # phase 1, then restart the server and PHASE=2
```

Everything under `docker/authelia/` is a throwaway development value. See
[OIDC authentication](docs/CONFIGURATION.md#oidc-authentication) for Authentik and Zitadel.

### Compose with an event broker

Adds a NATS JetStream server and points the server at it, so every object change is
published and `GET /api/v1/knowledgebases/{kb}/events` streams it with replay that survives
restarts and spans replicas:

```sh
docker compose -f docker-compose.yml -f docker-compose.events.yml up --build -d
examples/events/transcribe-mp3.sh   # subscribe, then upload an mp3 in another shell
```

A single-process deployment does not need the broker: `NOTEDTHAT_EVENTS_BACKEND=memory`
keeps the log in the server. See [Events backend](docs/CONFIGURATION.md#events-backend) and
[the endpoint](docs/API.md#get-apiv1knowledgebaseskb_slugevents), including what a reverse
proxy needs for a long-lived stream.

### Compose with an S3-compatible store

Adds SeaweedFS and points the server at it, which is the shape of the reference
deployment and what CI's integration suite exercises:

```sh
docker compose -f docker-compose.yml -f docker-compose.s3.yml up --build -d
```

Pass both files to every subsequent `docker compose` command in that stack, including
`down`. An overlay rather than a Compose profile because the two backends need different
environment variables on the same service, and setting both at once is a startup error —
see [Configuration](docs/CONFIGURATION.md#storage-backend).

### Native server: objects on disk, no object store

The smallest way to run NotedThat from a checkout. It needs Qdrant and an embedding
provider, and nothing else:

```sh
docker compose up -d qdrant
set -a; . ./.env; set +a
export NOTEDTHAT_LISTEN_ADDR=127.0.0.1:8080
export NOTEDTHAT_STORAGE_BACKEND=fs
export NOTEDTHAT_FS_ROOT="$PWD/.notedthat-data"
mkdir -p "$NOTEDTHAT_FS_ROOT"
export NOTEDTHAT_QDRANT_URL=http://127.0.0.1:6334
cargo run -p notedthat-server
```

Objects are files under `$NOTEDTHAT_FS_ROOT`, at their key paths — `ls -R` it, open it in
an editor, back it up with any file-level tool. Edits made that way are picked up: the tree
is watched, so a note changed outside NotedThat is re-indexed and searchable shortly
afterwards ([configuration](docs/CONFIGURATION.md#filesystem-storage-backend)). `.env` sets
`NOTEDTHAT_S3_*`, which the `fs` backend refuses to start alongside, so unset those three
or comment them out first.

### Native server: Compose-managed dependencies

Start SeaweedFS and Qdrant, then export host-facing settings before running the
server natively. Reuse your local embedding values from `.env` without committing it.

```sh
docker compose -f docker-compose.yml -f docker-compose.s3.yml up -d seaweedfs qdrant
set -a; . ./.env; set +a
export NOTEDTHAT_LISTEN_ADDR=127.0.0.1:8080
export NOTEDTHAT_S3_ENDPOINT_URL=http://127.0.0.1:8333
export NOTEDTHAT_S3_FORCE_PATH_STYLE=true
export NOTEDTHAT_QDRANT_URL=http://127.0.0.1:6334
cargo run --bin notedthat-server
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


### WebDAV and MCP against the local stack

With the server up, the WebDAV share is at `http://127.0.0.1:8080/webdav/` (Basic auth with the
`NOTEDTHAT_WEBDAV_*` values from `.env`) and MCP at `POST http://127.0.0.1:8080/mcp` (Bearer with
`NOTEDTHAT_API_TOKEN`). Smoke-test both without installing anything:

```sh
set -a; . ./.env; set +a
# List knowledge bases over WebDAV, write a note, read it back
curl -X PROPFIND -u "$NOTEDTHAT_WEBDAV_USERNAME:$NOTEDTHAT_WEBDAV_PASSWORD" -H 'Depth: 1' \
  http://127.0.0.1:8080/webdav/
echo "# Hello WebDAV" | curl --fail -X PUT --data-binary @- \
  -u "$NOTEDTHAT_WEBDAV_USERNAME:$NOTEDTHAT_WEBDAV_PASSWORD" \
  http://127.0.0.1:8080/webdav/notes/hello-webdav.md
curl -u "$NOTEDTHAT_WEBDAV_USERNAME:$NOTEDTHAT_WEBDAV_PASSWORD" \
  http://127.0.0.1:8080/webdav/notes/hello-webdav.md

# Ask the MCP endpoint for its tool list
curl --fail -X POST -H "Authorization: Bearer $NOTEDTHAT_API_TOKEN" \
  -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}' http://127.0.0.1:8080/mcp
```

Then wire in a real client — [docs/CLIENTS.md](docs/CLIENTS.md) — or mount the share —
[docs/WEBDAV.md](docs/WEBDAV.md). `docs/manual-qa/webdav-gvfs.sh` runs a fuller WebDAV walkthrough
with cadaver or curl.

Full configuration reference — every setting as an environment variable or as the flag that overrides it: [`docs/CONFIGURATION.md`](docs/CONFIGURATION.md)

Full API documentation: [`docs/API.md`](docs/API.md)

## Crate Map

| Crate | Path | Role |
| ----- | ---- | ---- |
| `notedthat-core` | `crates/notedthat-core` | Shared domain types, path/range/error/auth primitives, config |
| `notedthat-storage-s3` | `crates/notedthat-storage-s3` | S3 storage adapter |
| `notedthat-storage-fs` | `crates/notedthat-storage-fs` | Local filesystem storage adapter |
| `notedthat-indexer` | `crates/notedthat-indexer` | Chunking, embedder client, Qdrant integration |
| `notedthat-write` | `crates/notedthat-write` | Shared write path (`commit()`, `commit_delete()`, MIME sniff, 5 GiB limit) — used by HTTP API + WebDAV surfaces |
| `notedthat-api-http` | `crates/notedthat-api-http` | HTTP API surface |
| `notedthat-webdav` | `crates/notedthat-webdav` | WebDAV surface |
| `notedthat-mcp` | `crates/notedthat-mcp` | MCP tool schemas and HTTP-backed implementation |
| `notedthat-server` | `crates/notedthat-server` | Server library — HTTP API + WebDAV + remote MCP in one process. Published to `ghcr.io/notedthat/server` per tagged release. |
| `notedthat-mcp-stdio` | `crates/notedthat-mcp-stdio` | MCP-over-stdio transport adapter |
| `notedthat` | `crates/notedthat` | Distribution crate and release facade — owns the published `notedthat-server` and `notedthat-mcp-stdio` binaries, the workspace git tag, and the root CHANGELOG. `cargo install notedthat` installs both. |

All 11 crates share a single version via ecosystem-level Semantic Versioning. See [RELEASING.md](RELEASING.md) for the versioning policy.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for the contribution process — PR workflow, commit conventions (Conventional Commits + signed + DCO), testing requirements, and AI-assistance disclosure. Build, test, and run commands live in [DEVELOPMENT.md](DEVELOPMENT.md). The project is pre-v1, so interfaces change frequently — check open issues before starting significant work.

## License

Mozilla Public License 2.0. See [LICENSE](LICENSE) for the full text.
