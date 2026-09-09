# NotedThat Server

The NotedThat server library. Boots the HTTP API, WebDAV, and remote MCP surfaces as a single process against an S3-backed knowledgebase — see the workspace [README](../../README.md#running-locally) for the full env-var set and the [SPECIFICATIONS](../../SPECIFICATIONS.md) for the surface protocols.

The `notedthat-server` executable that drives it ships from the [`notedthat`](../notedthat) crate, so that one published crate owns every installed binary name.

This is the release facade crate for the workspace: it owns the workspace git tag (`vX.Y.Z`), the root [CHANGELOG.md](../../CHANGELOG.md), and the container image at [`ghcr.io/notedthat/server`](https://github.com/NotedThat/NotedThat/pkgs/container/server).

## Install

```sh
cargo install notedthat                      # both binaries
docker pull ghcr.io/notedthat/server:latest  # server only
# or grab a signed binary from https://github.com/NotedThat/NotedThat/releases
```

Every image and binary ships with a cosign `.bundle` sidecar and a SLSA L2 build provenance attestation — see the workspace README for verification commands.
