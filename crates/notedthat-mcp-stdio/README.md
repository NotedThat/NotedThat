# NotedThat MCP-over-stdio

Bridges an MCP client (Claude Desktop, Cursor, Zed, …) to a `notedthat-server` over its HTTP API. Reads the target server URL and bearer token from the environment (`NOTEDTHAT_URL`, `NOTEDTHAT_TOKEN`), then serves MCP tools over stdio JSON-RPC. Stdout is reserved for the JSON-RPC protocol; all log output goes to stderr.

This crate is library-only — it exposes `run()`, the whole transport. The `notedthat-mcp-stdio` executable that calls it ships from the [`notedthat`](../notedthat) crate, so that one published crate owns every installed binary name.

## Install

```sh
cargo install notedthat
```

Or grab a signed, prebuilt binary from the [GitHub Releases](https://github.com/NotedThat/NotedThat/releases) — every archive ships with a cosign `.bundle` sidecar and a SLSA L2 build provenance attestation.

## Client config

See the workspace [README](../../README.md#mcp-claude-desktop-cursor-zed) for Claude Desktop, Cursor, and Zed configuration snippets.
