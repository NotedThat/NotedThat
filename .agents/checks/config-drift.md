---
name: config-drift
description: A setting the change adds, renames or re-defaults that the configuration reference does not match.
turn-limit: 30
paths: ["crates/**/*.rs", "docs/CONFIGURATION.md", ".env.example", "docker-compose*.yml"]
---

You check one thing in a NotedThat pull request: that every operator-facing
setting the change touches is documented the way the code now behaves.

## The contract

- Every `NOTEDTHAT_*` environment variable and its `--flag` has a row in
  `docs/CONFIGURATION.md` — under "Required environment variables" or
  "Optional environment variables", or in the section of the feature it
  belongs to (storage backend, events, metrics, filesystem, S3) — giving the
  flag, the accepted values or type, the default, and what it does.
- `.env.example` is the Docker Compose starting point, not a full reference:
  it holds what a Compose user must set and the settings Compose deployments
  commonly change. A new *required* setting belongs there; an optional one
  only if Compose users will need it.
- `docker-compose*.yml` files pass settings to the server; a renamed or removed
  variable must not linger there.

## What to do

Find what the diff changes about settings: environment variables and flags
added, renamed or removed (look for `NOTEDTHAT_`, clap `env =` / `#[arg`
attributes, and the config structs in `crates/*/src/config.rs`), a changed
default, a changed accepted range or format, a setting that became required
or optional, and new validation that now refuses a value that used to start.
Then read the matching rows of `docs/CONFIGURATION.md`, `.env.example` and
the Compose files, and compare.

Report a mismatch only when you can name both sides: what the code does after
this change (file and line) and what the documentation says, or that it says
nothing.

## Severity

- **medium** — a new required setting, or a removed or renamed one, that the
  reference or `.env.example` still gets wrong: an operator following the
  docs gets a server that refuses to start or silently ignores them.
- **low** — an optional setting missing from the reference, or a default,
  range or description that no longer matches the code.

## Do not report

Variables only used in tests, internal constants that are not read from the
environment, `RUST_LOG`, and wording preferences in documentation that is
otherwise correct. If the change touches no setting, answer with no findings.

In `summary`, name the setting, what the code does now, what the docs say,
and the line to add or fix.
