# Releasing

## Overview

NotedThat uses [release-plz](https://release-plz.dev/) with an ecosystem-level versioning model: all 9 crates share a single version from `[workspace.package].version`. The `notedthat-server` crate is the release facade — it owns the workspace git tag, the GitHub Release, and the root `CHANGELOG.md`.

## Versioning Policy

All 9 crates share a single ecosystem-level version:

- **Major bump**: any breaking change in any crate
- **Minor bump**: new capabilities added in any crate
- **Patch bump**: bug fixes only

`semver_check = false` in `release-plz.toml` — release-plz does **not** verify SemVer automatically. Maintainers must review breaking changes manually before merging a release PR.

## Release Cycle

release-plz runs automatically on every push to `main`:

1. `release-plz-release` runs first (needs: test, clippy, fmt). If unreleased changes exist, it publishes all crates to crates.io and creates the `vX.Y.Z` git tag + GitHub Release.
2. `release-plz-pr` runs after (needs: release-plz-release). It opens or updates a release PR with the next version bump and aggregated `CHANGELOG.md` entries from all 9 crates.
3. Merging the release PR into `main` triggers the next cycle.

## Prerequisites (One-Time Setup)

### 1. Create the `release` GitHub Environment

Repo Settings → Environments → New environment → name it `release`. No approval gate is required initially; add one later if desired.

### 2. Set the `CARGO_REGISTRY_TOKEN` Secret

Repo Settings → Secrets and variables → Actions → New repository secret → `CARGO_REGISTRY_TOKEN`. This token is only needed for the initial bootstrap publish of each crate. Routine releases use OIDC Trusted Publishing.

### 3. Bootstrap-Publish Each Crate (First Time Only)

Trusted Publishing cannot create new crates on crates.io. Use the bootstrap workflow for each crate's first publish:

1. Actions → **Publish crate (initial)** → Run workflow
2. Enter the crate name (e.g. `notedthat-core`)
3. Leave `register_trusted_publishers` checked (best-effort TP registration)
4. Repeat for all 9 crates: `notedthat-core`, `notedthat-storage-s3`, `notedthat-indexer`, `notedthat-write`, `notedthat-api-http`, `notedthat-webdav`, `notedthat-mcp`, `notedthat-server`, `notedthat-mcp-stdio`

### 4. Verify Trusted Publishing Configs

After bootstrap, verify TP configs at `https://crates.io/crates/<crate>/settings`. Each crate needs two configs:
- `ci.yml` + environment `release`
- `publish-crate-manual.yml` + environment `release`

If auto-registration failed, add them manually via the crates.io web UI.

## Routine Release Flow

1. Merge feature/fix PRs into `main`
2. `release-plz-release` runs automatically — either no-op (nothing to release) or publishes and tags
3. `release-plz-pr` opens/updates a release PR with the next version + aggregated CHANGELOG entries
4. Review the release PR, then merge it into `main`
5. The merge triggers another `release-plz-release` run which publishes the new version

## Emergency Manual Publish

If a crate needs to be re-published manually:

Actions → **Publish crate (manual)** → Run workflow → select crate from dropdown → Run

This uses OIDC Trusted Publishing (no long-lived secret required after bootstrap).

## Supply Chain Notes

- `semver_check = false` — SemVer is not verified automatically; review breaking changes manually
- `dependencies_update = false` — release-plz does not bump dependencies; use Dependabot or manual updates
- No binary release artifacts in M1 — publishing is source-only via crates.io

## Common Failures

**`release-plz-release` blocked waiting for approval**
The `release` environment may have a required reviewer configured. Grant approval or remove the gate in Repo Settings → Environments → release.

**crates.io `HTTP 403` "Trusted Publishing tokens do not support creating new crates"**
Use `publish-crate-initial.yml` for the first publish of each crate. TP only works for subsequent versions.

**TP `HTTP 403` on routine publish**
The Trusted Publishing config on crates.io may not match the workflow filename or environment name. Verify at `https://crates.io/crates/<crate>/settings` that `ci.yml` + `release` is configured.

**`CARGO_REGISTRY_TOKEN` missing or wrong scope**
The bootstrap token needs the `publish-new` scope. Rotate it in your crates.io account settings and update the repo secret.
