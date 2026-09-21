# Releasing

## Overview

NotedThat uses [release-plz](https://release-plz.dev/) with an ecosystem-level versioning model: all 12 crates share a single version from `[workspace.package].version`. The `notedthat` crate is the release facade — it owns the workspace git tag, the GitHub Release, and the root `CHANGELOG.md`. It is the distribution crate users install, and because it depends on every other crate it publishes last, so the tag appears only once the whole workspace is on crates.io.

## Versioning Policy

All 12 crates share a single ecosystem-level version:

- **Major bump**: any breaking change in any crate
- **Minor bump**: new capabilities added in any crate
- **Patch bump**: bug fixes only

`semver_check = false` in `release-plz.toml` — release-plz does **not** verify SemVer automatically. Maintainers must review breaking changes manually before merging a release PR.

## Release Cycle

release-plz runs automatically on every push to `main`:

1. `release-plz-release` runs first (needs: test, clippy, fmt, docker-build, integration-test). If the workspace version is ahead of crates.io, it publishes all crates to crates.io and creates the `vX.Y.Z` git tag + GitHub Release. Because `release_always = true`, a release missed by a red or cancelled CI run is retried on the next push to `main` rather than lost.
2. `release-plz-pr` runs after `release-plz-release` has finished, whether it succeeded or failed. It opens or updates a release PR with the next version bump and aggregated `CHANGELOG.md` entries from all 12 crates. Running after a failure matters: when only a version bump can make the release succeed (see "Adding a crate"), the release PR is the way out, and a job that waited for a green release would never open it.
3. Merging the release PR into `main` triggers the next cycle.

## Prerequisites (One-Time Setup)

### 1. Create the `release` GitHub Environment

Repo Settings → Environments → New environment → name it `release`. No approval gate is required initially; add one later if desired.

### 2. Set the `CARGO_REGISTRY_TOKEN` Secret

Repo Settings → Secrets and variables → Actions → New repository secret → `CARGO_REGISTRY_TOKEN`. This token is only needed for the initial bootstrap publish of each crate. Routine releases use OIDC Trusted Publishing.

The secret is **not** set by default and is not required for routine releases, so it is easy to
find it missing the day a new crate needs bootstrapping (that is what happened with
`notedthat-events`, 2026-09-21). A crate owner can bootstrap from a workstation instead — see
*Adding a Crate* below.

### 3. Bootstrap-Publish Each Crate (First Time Only)

Trusted Publishing cannot create new crates on crates.io. Use the bootstrap workflow for each crate's first publish:

1. Actions → **Publish crate (initial)** → Run workflow
2. Enter the crate name (e.g. `notedthat-core`)
3. Leave `register_trusted_publishers` checked (best-effort TP registration)
4. Repeat for all 12 crates: `notedthat-core`, `notedthat-storage-s3`, `notedthat-storage-fs`, `notedthat-indexer`, `notedthat-write`, `notedthat-events`, `notedthat-api-http`, `notedthat-webdav`, `notedthat-mcp`, `notedthat-server`, `notedthat-mcp-stdio`, `notedthat`

Bootstrap in dependency order, `notedthat` last: `cargo publish` strips the `path` from a
`path` + `version` dependency and verifies the build against crates.io, so a crate cannot
be bootstrapped until every crate it depends on is published at the same version.

**Dev-dependencies on workspace crates carry `path` only, never `version`** (and never
`workspace = true`, which inherits the version). Cargo drops a path-only dev-dependency from the
published manifest; one with a version stays in and is resolved against crates.io during
`cargo publish`. release-plz orders publishes by normal dependencies alone, so a versioned
dev-dependency on a crate that is not on crates.io yet fails the *dependent* before release-plz
reaches the new crate at all. CI's `package` job enforces the rule on the manifests
`cargo package` actually writes, so whatever spelling put a version there is caught.

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

## Adding a Crate to the Workspace

A new publishable crate takes the shared workspace version, which is already on crates.io for every other crate. Expect the following on the merge that adds it, and do not treat it as a broken release:

1. `release-plz-release` fails on the merge push. `release_always = true` sees the new crate's version missing from crates.io and tries to publish it; `cargo publish` verifies the tarball against crates.io, where its sibling dependencies are still the *previous* publish and lack whatever the new crate imports from them. This repeats on every push to `main` until the version is bumped.
2. `release-plz-pr` opens the release PR regardless. Merge it: the bump publishes the siblings first, at the new version, and the new crate builds against them.
3. That release still stops at the new crate with `HTTP 403` — Trusted Publishing cannot create crates. By then its dependencies are on crates.io at the new version, so run **Publish crate (initial)** for the new crate (see Prerequisites, step 3), then push to `main` again (an empty commit will do, and so does *Re-run failed jobs* on the failed run) — `release_always` publishes the remaining crates, tags, and creates the GitHub Release.

You do not have to wait for step 3 to fail: the bootstrap can run as soon as the new crate's
*own* dependencies are on crates.io at the workspace version, which `release-plz-release`
reports (`<crate> <version>: already published`) as it works down the dependency order. The
release job also names any never-published crate at the top of its log, as a **warning** and
not a failure — on purpose: release-plz has to run, because publishing the new crate's
dependencies at the new version is what the bootstrap needs. A pre-flight that failed the job
would leave them unpublished and the bootstrap with nothing to build against.

**Bootstrapping without the secret.** A crate owner with `cargo login` done can publish from a
workstation, from a clean checkout of the commit `main` is at (a worktree, not a working tree
with local changes — `cargo publish` refuses a dirty tree, and `--allow-dirty` would publish
whatever is lying around):

```sh
git worktree add /tmp/notedthat-publish origin/main
cd /tmp/notedthat-publish
cargo publish -p <crate> --dry-run
cargo publish -p <crate>
```

Then register Trusted Publishing for the crate by hand at
`https://crates.io/crates/<crate>/settings` — two configs, `ci.yml` + environment `release` and
`publish-crate-manual.yml` + environment `release`, both for `NotedThat/NotedThat` — or the
*next* release fails on that crate with `HTTP 403`. The API form, if scripting it, wraps the
fields in a `github_config` object (an unwrapped body is a `422 missing field github_config`;
field names as of 2026-09, check against the crates.io API if it rejects them):
`{"github_config":{"crate":"<crate>","repository_owner":"NotedThat","repository_name":"NotedThat","workflow_filename":"ci.yml","environment":"release"}}`.

## Emergency Manual Publish

If a crate needs to be re-published manually:

Actions → **Publish crate (manual)** → Run workflow → select crate from dropdown → Run

This uses OIDC Trusted Publishing (no long-lived secret required after bootstrap).

## Release Artifacts

Every `vX.Y.Z` tag produces:

1. **crates.io publish** for all 12 crates (via OIDC Trusted Publishing in `ci.yml` release-plz-release).
2. **GitHub Release** created by release-plz with the aggregated CHANGELOG body.
3. **Container image** at `ghcr.io/notedthat/server:X.Y.Z` (+ `latest` for non-prerelease), signed with cosign keyless and carrying a SLSA L2 build provenance attestation. Built by `.github/workflows/release.yml` via the reusable `docker.yml`.
4. **Static binary archives** — one `notedthat-<target>.tar.xz` per target across 6 targets (linux glibc/musl x86_64 + aarch64, macOS x86_64 + aarch64), each carrying **both** `notedthat-server` and `notedthat-mcp-stdio`, with a `.bundle` cosign signature and a per-artifact SLSA provenance attestation. Uploaded as GitHub Release assets by `.github/workflows/release.yml`. `notedthat` is the workspace's only cargo-dist app, so there is one archive per target rather than one per binary.
5. **Shell installer** (`notedthat-installer.sh`) generated by cargo-dist, published as a release asset alongside the archives. It installs both binaries.

Windows targets are temporarily disabled — see the comment on `targets` in the root `Cargo.toml`. Windows users install with `cargo install notedthat`.

**Verification** (any user, any binary):

```sh
# Verify an archive signature
cosign verify-blob \
  --bundle notedthat-x86_64-unknown-linux-gnu.tar.xz.bundle \
  --certificate-identity-regexp 'https://github.com/NotedThat/NotedThat/.+' \
  --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \
  notedthat-x86_64-unknown-linux-gnu.tar.xz

# Verify the container image signature + provenance
gh attestation verify oci://ghcr.io/notedthat/server:X.Y.Z --owner NotedThat
```

## Supply Chain Notes

- `semver_check = false` — SemVer is not verified automatically; review breaking changes manually.
- `dependencies_update = false` — release-plz does not bump dependencies; use Dependabot or manual updates.
- The GitHub Release body is owned by release-plz. `release.yml` uses `append_body: false` when uploading binary/installer assets so the CHANGELOG-generated body is never overwritten.

## Common Failures

**`release-plz-release` blocked waiting for approval**
The `release` environment may have a required reviewer configured. Grant approval or remove the gate in Repo Settings → Environments → release.

**crates.io `HTTP 403` "Trusted Publishing tokens do not support creating new crates"**
Use `publish-crate-initial.yml` for the first publish of each crate. TP only works for subsequent versions.

**`release-plz-release` fails on `<sibling>` with `no matching package named <crate> found`**
`<sibling>` names `<crate>` as a dev-dependency *with a version*, and `<crate>` has never been
published. Cargo keeps a versioned dev-dependency in the published manifest and resolves it at
publish time, but release-plz does not order publishes by dev-dependencies, so it reaches the
sibling first. Bootstrap `<crate>` (see *Adding a Crate*), then drop `version` from that
dev-dependency — CI's `package` job refuses the versioned form since 2026-09-21.

**`release-plz-release` warns at its first step that a crate has never been published**
Expected on the merge that adds a crate; the job goes on to publish that crate's dependencies and
then stops at the crate with `HTTP 403`. Bootstrap it (see *Adding a Crate*) and re-run the
failed jobs.

**TP `HTTP 403` on routine publish**
The Trusted Publishing config on crates.io may not match the workflow filename or environment name. Verify at `https://crates.io/crates/<crate>/settings` that `ci.yml` + `release` is configured.

**`CARGO_REGISTRY_TOKEN` missing or wrong scope**
The bootstrap token needs the `publish-new` scope. Rotate it in your crates.io account settings and update the repo secret.

**`release-plz-pr` fails with `failed to check package equality for <crate> at commit <sha>` … `error while running cargo package`**
To decide whether a crate changed since its last publish, release-plz walks the commits that touched the crate's directory, newest first, and at each one compares the checkout with the published package — running `cargo package --list` in that historical tree whenever the manifests match. It stops at the first commit whose checkout equals the published crate, or that is an ancestor of the commit the crate was published from. A commit in between where Cargo cannot resolve the workspace (typically a branch rebased across a release, carrying intra-workspace pins at the old version until a trailing commit fixes them: #141, commits 4d12462–088dae1) aborts the whole job rather than counting as "different", and it keeps doing so on every push until a publish moves the stopping point past it.

The way out is a hand-carried release PR (#148, with the crate touch following before the affected crates published): bump the workspace version the way release-plz would, and make sure that, before the publish, a commit touches every crate whose last-touching commit sits inside the unresolvable range — after the publish, that last-touching commit is exactly where the walk for the crate stops, so it has to be one Cargo can resolve. `release-plz update` generates the bump and the CHANGELOG entry faithfully when run on a scratch clone whose history has the pins repaired (`git filter-branch --tree-filter` over the range); copy its diff over.

To avoid it: when rebasing a branch across a release, fix the intra-workspace pins in the commit that introduces them, not in a follow-up, so every commit on the branch resolves; `cargo package --workspace --locked` in CI only checks the branch tip.
