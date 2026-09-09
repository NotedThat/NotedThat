# NotedThat

The distribution crate: it owns every published NotedThat binary and contains no
logic of its own. Installing it gets you both executables at once.

```sh
cargo install notedthat
```

| Binary | What it does |
| ------ | ------------ |
| `notedthat-server` | The server — HTTP API, WebDAV, and remote MCP as a single process against an S3-backed knowledgebase. |
| `notedthat-mcp-stdio` | Bridges an MCP client (Claude Desktop, Cursor, Zed, …) to a running `notedthat-server` over stdio JSON-RPC. |

Every setting on both binaries can be given as an environment variable or as the
flag named after it, and the flag wins; run either with `--help` to see them all.
See [docs/CONFIGURATION.md](../../docs/CONFIGURATION.md) for the full reference,
and the workspace [README](../../README.md#running-locally) to get a server
running.

The implementations live in [`notedthat-server`](../notedthat-server) and
[`notedthat-mcp-stdio`](../notedthat-mcp-stdio), which are library-only. Hosting
both binaries here means exactly one published crate installs each name.

This is also the workspace's release facade: it owns the `vX.Y.Z` git tag, the
GitHub Release, and the root [CHANGELOG.md](../../CHANGELOG.md). Because it
depends on every other crate it publishes last, so the tag appears only once the
whole workspace is on crates.io.

## Other ways to install

```sh
# Signed, prebuilt binaries — one archive carries both executables
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/NotedThat/NotedThat/releases/latest/download/notedthat-installer.sh | sh

# Server only, as a container
docker pull ghcr.io/notedthat/server:latest
```

Every archive and image ships with a cosign `.bundle` sidecar and a SLSA L2 build
provenance attestation — see [RELEASING.md](../../RELEASING.md) for verification
commands.
