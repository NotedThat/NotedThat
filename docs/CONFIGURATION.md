# NotedThat Configuration

NotedThat is configured entirely through environment variables. There are no CLI flags, no config
files, and no `.env` file auto-loading. Every setting the server needs must be present in the
process environment before startup.

This keeps the configuration surface explicit and container-friendly: pass vars via `docker run -e`,
a Kubernetes `Secret`, or your shell's `export` statements.

## Required environment variables

These must be set. The server exits with a non-zero status and a descriptive error message if any
are missing or invalid.

| Variable | Type | Description | Example |
|----------|------|-------------|---------|
| `NOTEDTHAT_API_TOKEN` | string (non-empty) | Static Bearer token for API authentication. All `/v1/` requests must present this token in the `Authorization: Bearer` header. | `s3cr3t-token` |
| `NOTEDTHAT_KBS` | comma-separated slugs | One or more knowledge base slugs to declare. Each slug must match `[a-z0-9-]{1,40}`. Duplicates are rejected. At least one slug is required. | `notes,scratch,work` |
| `NOTEDTHAT_S3_REGION` | AWS region string | AWS region for the S3 bucket. Required even when using a custom endpoint. | `us-east-1` |
| `NOTEDTHAT_S3_ACCESS_KEY_ID` | string | AWS access key ID. No credential chain is consulted; this value is used directly. | `AKIAIOSFODNN7EXAMPLE` |
| `NOTEDTHAT_S3_SECRET_ACCESS_KEY` | string | AWS secret access key corresponding to the access key ID above. | `wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY` |

## Optional environment variables

These have defaults and can be omitted.

| Variable | Type | Default | Description |
|----------|------|---------|-------------|
| `NOTEDTHAT_LISTEN_ADDR` | `host:port` (SocketAddr) | `0.0.0.0:8080` | Address and port the HTTP server binds to. Use `127.0.0.1:8080` to restrict to localhost. |
| `NOTEDTHAT_S3_ENDPOINT_URL` | URL | (unset, uses AWS default) | Custom S3-compatible endpoint. Required for SeaweedFS, MinIO, Ceph, Garage, and other S3-compatible stores. |
| `NOTEDTHAT_S3_FORCE_PATH_STYLE` | `true` or `false` | `false` | Use path-style S3 addressing (`endpoint/bucket/key`) instead of virtual-hosted style (`bucket.endpoint/key`). Set to `true` for SeaweedFS, MinIO, and most self-hosted S3-compatible stores. |
| `NOTEDTHAT_LOG_FORMAT` | `pretty` or `json` | `pretty` | Log output format. `pretty` produces human-readable multi-line output. `json` produces one JSON object per log event, suitable for log aggregators. |
| `RUST_LOG` | tracing filter string | `info,notedthat=debug` | Controls log verbosity. Uses the standard `tracing-subscriber` filter syntax. Examples: `debug`, `warn`, `info,notedthat_api_http=trace`. |

## Example: local development with SeaweedFS

Copy this block and export the variables in your shell, or save it as `.env` and load it with
`direnv` or `source .env`.

```sh
NOTEDTHAT_API_TOKEN=dev-token-please-change
NOTEDTHAT_KBS=notes,scratch
NOTEDTHAT_LISTEN_ADDR=127.0.0.1:8080
NOTEDTHAT_S3_ENDPOINT_URL=http://127.0.0.1:8333
NOTEDTHAT_S3_REGION=us-east-1
NOTEDTHAT_S3_ACCESS_KEY_ID=any
NOTEDTHAT_S3_SECRET_ACCESS_KEY=any
NOTEDTHAT_S3_FORCE_PATH_STYLE=true
RUST_LOG=info,notedthat=debug
```

> **Note:** This file is for reference only. The server does **not** auto-load `.env` files. Export
> these variables manually or use a tool like [direnv](https://direnv.net/) to load them
> automatically when you enter the project directory.

## Example: AWS S3

```sh
NOTEDTHAT_API_TOKEN=<your-api-token>
NOTEDTHAT_KBS=notes
NOTEDTHAT_S3_REGION=eu-west-1
NOTEDTHAT_S3_ACCESS_KEY_ID=<your-access-key-id>
NOTEDTHAT_S3_SECRET_ACCESS_KEY=<your-secret-access-key>
```

No endpoint URL or path-style override needed for real AWS S3.

## Startup validation

The server validates all configuration before binding to any port or connecting to S3. If a
required variable is missing, empty, or invalid, the process exits immediately with a non-zero
status code and prints a descriptive error to stderr. For example:

```
Error: NOTEDTHAT_API_TOKEN is required
Error: NOTEDTHAT_KBS must declare at least one knowledge base
Error: NOTEDTHAT_S3_REGION is required
Error: NOTEDTHAT_LISTEN_ADDR is invalid: invalid socket address syntax
Error: invalid KB slug "My Notes": slugs must match [a-z0-9-]{1,40}
Error: duplicate KB slug in NOTEDTHAT_KBS: "notes"
```

This fail-fast behavior means misconfigured deployments fail loudly at startup rather than
silently misbehaving at runtime.

## What's not configurable in M2

- **Tenant slug:** Hardcoded to `"default"`. There is no `NOTEDTHAT_TENANT_SLUG` variable.
- **Upload buffer sizes:** Fixed at 16 MiB. Configurable buffer sizes are planned for a later
  release.
- **Rate limits:** No per-client or global rate limiting in M2.
- **TLS:** The server speaks plain HTTP. Terminate TLS at a reverse proxy (Traefik, nginx, Caddy).
- **Multiple tokens:** Only one API token is supported. Per-KB tokens and scopes are planned for
  a later release.
