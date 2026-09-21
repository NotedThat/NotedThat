# NotedThat

The distribution crate: it owns the published NotedThat binary and contains no
logic of its own.

```sh
cargo install notedthat
```

| Binary | What it does |
| ------ | ------------ |
| `notedthat-server` | The server — HTTP API, WebDAV, and MCP over streamable HTTP as a single process against an S3- or filesystem-backed knowledgebase. |

Every setting can be given as an environment variable or as the flag named after
it, and the flag wins; run it with `--help` to see them all. See
[docs/CONFIGURATION.md](../../docs/CONFIGURATION.md) for the full reference, and
the workspace [README](../../README.md#running-locally) to get a server running.
MCP clients connect to the server's `POST /mcp`; see
[docs/CLIENTS.md](../../docs/CLIENTS.md).

The implementation lives in [`notedthat-server`](../notedthat-server), which is
library-only. Hosting the binary here means exactly one published crate installs
the name.

This is also the workspace's release facade: it owns the `vX.Y.Z` git tag, the
GitHub Release, and the root [CHANGELOG.md](../../CHANGELOG.md). Because it
depends on every other crate it publishes last, so the tag appears only once the
whole workspace is on crates.io.

## Other ways to install

```sh
# Signed, prebuilt binary
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/NotedThat/NotedThat/releases/latest/download/notedthat-installer.sh | sh

# As a container
docker pull ghcr.io/notedthat/server:latest
```

Every archive and image ships with a cosign `.bundle` sidecar and a SLSA L2 build
provenance attestation — see [RELEASING.md](../../RELEASING.md) for verification
commands.
