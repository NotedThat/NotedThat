# Development Guide

## Prerequisites

- Rust stable — develop on it: `rustup default stable`, installed via [rustup](https://rustup.rs/).
- The workspace declares `rust-version = "1.91.1"` (`Cargo.toml`, `[workspace.package]`). That is the
  floor the published crates promise, derived from the dependency graph rather than chosen: 1.90.0 is
  refused by Cargo naming the `aws-sdk-s3` / `aws-smithy-*` family. Edition 2024's own 1.85 floor stopped
  being the binding constraint some time ago.
- No `rust-toolchain.toml` is present, deliberately — pinning developers to the floor is not the point.
  CI's `test` job runs the suite on `[stable, 1.91.1]` instead, so the declared MSRV is a checked fact
  rather than a claim. To reproduce the MSRV leg locally:
  `rustup toolchain install 1.91.1 && cargo +1.91.1 test --workspace --locked`.

## Daily Commands

```sh
# Fast type check (no codegen)
cargo check --workspace

# Run all tests
cargo test --workspace --locked

# Lint (mirrors CI)
cargo clippy --workspace --all-targets --locked -- -D warnings

# Format check (mirrors CI)
cargo fmt --all -- --check

# Dependency advisories (mirrors CI)
cargo deny --locked check advisories

# Format in place
cargo fmt --all
```

## Local service setup

The supported development paths are documented in the root [README](README.md):

- **Compose** builds the server and runs SeaweedFS and Qdrant, while you supply an
  OpenAI-compatible embedding provider in an ignored local `.env` file.
- **Native server** uses Compose-managed SeaweedFS and Qdrant with host-facing
  endpoints, then runs `cargo run --bin notedthat-server`.

Do not commit provider credentials. Copy `.env.example` to `.env`, replace its
embedding endpoint, model, key, and dimensions, then follow the chosen README flow.

## Running the Server

```sh
cargo run --bin notedthat-server
```

The binary is owned by the `notedthat` crate, so select it with `--bin`;
`-p notedthat-server` names a library crate and has nothing to run.

## Wiring an MCP client to a local build

MCP is served by the running server at `POST /mcp`; there is no client-side
binary to install. Point the client at `http://localhost:8080/mcp` with the
`Authorization: Bearer` header, or bridge a command-only client with
`mcp-remote` — see [docs/CLIENTS.md](docs/CLIENTS.md).

## Test Conventions

- **Unit tests**: inline `#[cfg(test)] mod tests { ... }` in the same source file.
- **Integration tests**: `tests/*.rs` in the crate directory (e.g. `crates/notedthat-core/tests/`).
- **External-service tests**: annotate with `#[ignore]` and a comment explaining what service is needed:
  ```rust
  #[test]
  #[ignore = "requires SeaweedFS + Qdrant running locally"]
  fn test_full_index_round_trip() { ... }
  ```
- **Run ignored tests**: `cargo test --workspace -- --ignored`
- **Stress scenarios**: prefix the test name with `stress_` on top of `#[ignore]`.
  These generate and hash multi-gibibyte bodies and are meant to be run by hand:

  ```rust
  #[test]
  #[ignore = "generated 5 GiB staging and bounded replay stress scenario"]
  fn stress_stages_and_replays_five_gib_without_retaining_outputs() { ... }
  ```

  CI's integration job runs `--include-ignored --skip stress_`, so the prefix is
  what keeps them out of the release path. Without it a stress scenario runs on
  a GitHub runner, overruns the job timeout, and the cancelled job silently skips
  release-plz — no release, no failure report. Run them locally with
  `cargo test --workspace -- --ignored stress_`.

### Filesystem watching

`notedthat-storage-fs` watches the storage tree, which means part of its behaviour is the
*kernel's*, not ours. Two things follow.

Most of the logic is pure functions over synthesized `notify` events — which paths count,
which event kinds count, how reports are coalesced — so it can be tested with no kernel and
no filesystem, on any platform. Put new rules there. `crates/notedthat-storage-fs/tests/watch.rs`
covers only what the kernel itself does, and is the smaller half on purpose.

**CI is Linux-only**, so inotify is the only backend exercised here. FSEvents and kqueue are
compile-checked by the release build matrix and nothing more. Writing tests against the
crate's own signal type rather than against `notify::Event` is what keeps them meaningful on
a developer's Mac, which is the only place that path runs at all.

Asserting that something produces *no* report needs care: waiting a fixed period to "prove"
silence is both slow and a lie. Touch a second file afterwards and wait for that instead —
its arrival proves the watcher worked through everything queued before it. The suite has a
helper for this; read its comment before changing it.

```sh
cargo test -p notedthat-storage-fs --test watch
cargo test -p notedthat-server --test fs_backend_e2e
```

### Backends in tests

Almost everything runs on in-process substitutes. `notedthat_indexer::testing`
carries `InMemoryVectorStore` and `StubEmbedder`, `notedthat_api_http::testing`
carries `InMemoryStorage`; each is behind that crate's `test-support` feature.
Reach for those first — they need no Docker, and the suites that use them run in
the normal `cargo test` pass rather than behind `#[ignore]`.

To bring up a whole server over them, build a `notedthat_server::run::Backends`
and hand it to `run_with` (enable `notedthat-server`'s `test-support` feature).
Startup, provisioning, the indexer worker, every listener and the shutdown
sequence are the real code path; only S3, Qdrant and the embedding endpoint are
substituted. See `crates/notedthat-server/tests/support/patch_backends.rs`.

Two things the substitutes do not cover, so do not read a green run as covering
them either:

- `QdrantClient`'s `VectorStore` impl — the filter, selector and collection
  translation in `crates/notedthat-indexer/src/qdrant.rs`. That is pinned by the
  conformance suite below, not by the E2E suites.
- `S3Storage` against a real S3 implementation, which is what the S3 half of the
  storage integration suite is for (see below).

### Container-backed tests

These suites still need real infrastructure, all `#[ignore]` so they run only
under `--include-ignored`:

- `crates/notedthat-storage-fs/tests/storage_integration_s3.rs` — the storage
  integration suite against SeaweedFS. See "One suite, every backend" below.
- `crates/notedthat-indexer/tests/vector_store_conformance.rs` — runs one
  scenario set through both `QdrantClient` and `InMemoryVectorStore` and asserts
  they agree, so the in-memory substitute cannot drift away from the backend it
  stands in for. Run it after any change to `qdrant.rs` or `testing.rs`.
- `crates/notedthat-storage-fs/tests/storage_conformance_s3.rs` — the same idea for
  `Storage`: one scenario set through both `S3Storage` and `FsStorage`, so an operator
  flipping `NOTEDTHAT_STORAGE_BACKEND` gets the same behaviour. One container for the
  whole suite, not one per test. Run it after any change to either adapter's
  `storage.rs`, or to `notedthat-core`'s `preconditions.rs`.

- `crates/notedthat-server/tests/oidc_authelia_e2e.rs` — the real server against a real
  Authelia (`docker/authelia/`), discovered over TLS through an internal CA, with tokens
  Authelia mints. Its containerless sibling `oidc_e2e.rs` does the same against a wiremock
  issuer and self-minted tokens in the ordinary pass; this one proves the provider
  configuration the Compose overlay and `docs/manual-qa/oidc-mcp.sh` rely on. Authelia
  derives the issuer from the request host, so the container binds host port `9091`
  — stop the Compose overlay before running it.

- `crates/notedthat-server/tests/events_nats_e2e.rs` — two real servers sharing one NATS
  JetStream container (`nats:2.12-alpine -js`): ids strictly increase across replicas, a
  reconnect with `Last-Event-ID` to either replica replays exactly the missed events, a purged
  position is `410`, and losing the broker fails `/readyz` and turns writes into `503` with
  `Retry-After`. Its containerless siblings, `events_e2e.rs` and the events cases in
  `fs_backend_e2e.rs`, run every write surface and the watcher over the `memory` log in the
  ordinary pass.

- `crates/notedthat-events/tests/events_integration_nats.rs` — the event log integration
  suite (`tests/support/event_log_scenarios.rs`, see "One suite, every backend" below) run
  over a real JetStream stream: replay after a position, `Gone` for a retained-out or
  ahead-of-log position, isolation per knowledge base, delivery once and in order. One
  container for the run; the scenarios take turns on one stream that each recreates, because
  JetStream refuses two streams capturing the adapter's subject root. Its containerless
  sibling, `events_integration_memory.rs`, runs the same bodies over the ring in the
  ordinary pass.

Its containerless sibling, `crates/notedthat-storage-fs/tests/storage_conformance_local.rs`,
runs the same scenarios through `FsStorage` and `InMemoryStorage` in the ordinary
`cargo test` pass. That makes `FsStorage` a pivot: agreement in both directions means the
substitute matches real S3 semantics, and the cheap half of that check runs on every push
without Docker.

### One suite, every backend

`crates/notedthat-storage-fs/tests/support/integration_scenarios.rs` holds the storage
integration suite: absolute assertions about one `Storage` implementation — "a wrong
`If-Match` is a `PreconditionFailed`", "a range read reports an inclusive
`Content-Range`". Each scenario takes `&dyn Storage`, so it is written once and run
against every implementation there is:

- `storage_integration_local.rs` — `FsStorage` over a temporary directory. No Docker, so
  it runs in the ordinary `cargo test` pass on every push.
- `storage_integration_s3.rs` — `S3Storage` over SeaweedFS, `#[ignore]`, in CI's
  integration job.
- `storage_integration_memory.rs` — `InMemoryStorage`. Not a real backend, but it is what
  almost every E2E suite in the workspace runs on, and a green E2E run is only worth
  something if the substitute under it behaves correctly rather than merely consistently.

None of these files contains a test body. Each supplies a fixture and calls
`storage_integration_scenarios!`, the macro that expands the scenario list into one
`#[tokio::test]` per scenario. **To add a case, write the `async fn` and add its name to
that list** — every backend picks it up, which is what stops one backend's coverage
from drifting ahead of the others'. The scenario name becomes the knowledge base slug, so it
must be a valid `KbSlug`: at most 40 characters of `[a-z0-9_]`.

The event log has the same arrangement in `crates/notedthat-events/tests/`:
`support/event_log_scenarios.rs` holds the bodies and the `event_log_scenarios!` list,
`events_integration_memory.rs` expands it over `MemoryPublisher` in the ordinary pass and
`events_integration_nats.rs` over `NatsPublisher` under `--include-ignored`. Scenarios take an
`EventLogFixture` rather than the bare `EventPublisher` for one reason: "make the oldest
events unretrievable" is the single operation the contract needs that the adapters do
differently (the ring evicts past its capacity, the stream is purged), so it is the
fixture's `retain_out`, and everything else is asserted through the trait alone.

One scenario is one S3 bucket, and the S3 fixture's `-volume.max=200` is headroom for
that count rather than a bound on it. Past 200 scenarios SeaweedFS runs out of volume
collections again, and the symptom looks nothing like the cause: the master logs "Not
enough data nodes found!" and PUTs fail with an opaque "service error". Raise the flag in
`storage_integration_s3.rs` when the list gets there.

This is a different question from `storage_conformance_*.rs`, which asserts only that two
backends *agree* — they can agree on the wrong answer, and the integration suite is what
says they do not. The two are complementary: conformance compares behaviours no single
backend can be right or wrong about on its own (`ETag` values, `last_modified`, cursors),
and the integration suite pins the ones it can.

The S3 half shares one container between the tests running *concurrently* rather than
booting one per test, and holds it through a `Weak` rather than a `static`:
`testcontainers` removes a container on `Drop` and has no reaper process, so a container
parked in a `static` is never dropped and outlives the run. The cost of that choice is
that `--test-threads=1` gets no sharing at all — nothing overlaps, so every scenario
starts and removes a container of its own. Do not reach for it on this suite.

It also raises `-volume.max`, because SeaweedFS gives every bucket a volume collection of
its own and one bucket per scenario exhausts the default allowance — the master logs "Not
enough data nodes found!" and PUTs fail with an opaque "service error".

Every one of them starts its container with
[testcontainers](https://docs.rs/testcontainers).
**Wait on a readiness signal, never on a fixed duration.** `WaitFor::seconds(5)`
was used throughout and was a reliable source of flakes: on a loaded machine the
container is not ready when the sleep expires, the first RPC lands early, and it
fails against the client timeout with a misleading error — for Qdrant, collection
calls would succeed while the first `upsert_points` returned
`Cancelled: Timeout expired`.

Use the log line each service prints once its listener is bound:

```rust
// Qdrant — stdout
.with_wait_for(WaitFor::message_on_stdout("Qdrant gRPC listening on 6334"))
// SeaweedFS — stderr (it logs to stderr, and takes ~3.5s to reach this line)
.with_wait_for(WaitFor::message_on_stderr("Start Seaweed S3 API Server"))
```

A bound port is not a serving port. The conformance suite polls `health_check`
after the log line before handing the client to a test, and builds its client
through `QdrantClient::new` rather than `Qdrant::from_url(..).build()` — the
latter inherits qdrant-client's 5-second per-RPC default, which a `wait(true)`
upsert can exceed under load.

## The container image

`docker.yml` builds `linux/amd64` and `linux/arm64` on native runners and merges them into one manifest
list. Locally you normally only want your own architecture:

```sh
docker build -t notedthat-server:local .
```

CI also scans the built image, and both scans are reproducible here — a scanner nobody can run locally is
a scanner people argue with. Same flags as the workflow:

```sh
# Vulnerabilities. `--ignore-unfixed` matches CI: a failure means a fix exists
# in the distribution and the image has not taken it, which is always actionable.
trivy image --scanners vuln --severity HIGH,CRITICAL --ignore-unfixed \
  --ignorefile .trivyignore.yaml notedthat-server:local

# Secrets. Every hit is a hard fail; there is no "unfixed" for a leaked key.
trivy image --scanners secret --ignorefile .trivyignore.yaml notedthat-server:local
```

Both base images are pinned by digest. To bump them, follow
[Base image digests](RELEASING.md#base-image-digests) — the digest must be the multi-platform **index**,
or the arm64 build breaks in a way amd64 never reveals.

## Dependency advisories

The `advisories` job in `ci.yml` checks the committed `Cargo.lock` against the
[RustSec advisory database](https://rustsec.org/) and fails the build on any **vulnerability**,
**unmaintained** or **yanked** crate anywhere in the dependency graph — and on **unsound**
advisories against workspace crates only, because `deny.toml` sets `unsound = "workspace"`.

That last exception is worth knowing before you read a green run as "no advisories in the graph",
because it is load-bearing right now: `Cargo.lock` carries `lru` 0.16.4 via `aws-sdk-s3`, which has
RUSTSEC-2026-0253 (a use-after-free in `LruCache::pop()`), and the check still prints `advisories
ok`. `deny.toml` explains the choice — widening it would make every pull request red over a bump
only the AWS SDK can make. Unlike an `ignore` entry, it has no `unused-ignored-advisory` tripwire,
so nothing will prompt a revisit when the SDK moves off `lru` 0.16; this paragraph is the only
place that exception is recorded.

It runs on pull requests and on pushes to `main`, and `advisories.yml` runs the same check on a
weekday schedule so an advisory filed while the repository is quiet does not wait for the next
push. None of them gates a release: `release-plz-release` does not `needs:` any of them, so a red
advisory run blocks no tag. The job's comment in `ci.yml` explains the split.

Same command CI runs, same config, no flags to remember:

```sh
# The version CI pins. cargo-deny is not a workspace dependency, so this is a
# one-off install — and the version moves by hand in two places, here and
# ci.yml, because Renovate cannot see the `tool:` input the workflow uses.
cargo install --locked cargo-deny@0.20.2

# Everything that shapes the check lives in deny.toml, which is why there is
# nothing else on this line. `--locked` is the same assertion every other cargo
# call in CI makes: judge the lockfile that ships.
cargo deny --locked check advisories
```

The first run clones the advisory database into `$CARGO_HOME/advisory-dbs` (about 40 MB); later runs
update it. To judge against the copy you already have, run

```sh
cargo deny --locked --offline check advisories
```

— `--offline` is a global flag and has to come **before** `check`; appended after `advisories` it is
rejected with `unexpected argument '--offline' found`. In that mode `deny.toml` refuses a database
more than seven days old, so an offline pass is never a quietly stale one. A CI run prints the
advisory-db revision it judged against, including when it fails, so a red run stays reproducible
after the fact.

A failure is nearly always fixed by moving the lockfile, not by suppressing the finding:

```sh
# The advisory names its own fix. Take it:
cargo update -p <crate>

# When a plain update stops below the advisory's floor — something else in the
# graph is holding the crate back — name the version and let Cargo move what it
# has to:
cargo update -p <crate> --precise <version>
```

Suppressing is the exception. It belongs in `deny.toml`'s `ignore` list, never as a flag on the
command line where nobody reads it, and every entry needs a reason and, where the fix is upstream, a
link — so the next person can tell a considered deferral from an unreviewed one. An entry also fails
the check the day the advisory stops matching the tree, so a suppression rots loudly: delete it, do
not extend it. `deny.toml` likewise records why the `licenses`, `bans` and `sources` checks are off,
and why `unsound` stays at its narrower scope; those are decisions, not defaults.

## Dependency Ownership Rules

- S3/Qdrant/WebDAV deps live **only** in their respective crates (`notedthat-storage-s3`, `notedthat-indexer`, `notedthat-webdav`).
- Shared deps go in `[workspace.dependencies]` in the root `Cargo.toml`, consumed via `foo = { workspace = true }` in member `Cargo.toml` files.
- No inter-crate `path` dependencies in M1 — each crate is standalone until M2 wires them together.

## Adding a New Crate

1. Create the directory: `crates/<name>/`. It must live under `crates/` —
   `.github/workflows/publish-crate-initial.yml` looks nowhere else.
2. Add `Cargo.toml` inheriting workspace fields (`version.workspace = true`, etc.) and `[lints] workspace = true`.
3. Add the path to `members` in the root `Cargo.toml`.
4. Add the crate name to `changelog_include` in `release-plz.toml` (under the `notedthat` facade package).
5. Add the crate name to the `options` list in `.github/workflows/publish-crate-manual.yml`.
6. Add `tests/it_compiles.rs` containing a single empty `fn it_compiles()`, so
   `cargo test --workspace` proves cargo picked the crate up.
7. Regenerate `Cargo.lock` in the same commit. Every CI job runs `--locked`, so a stale
   lockfile is green locally and red in CI.
8. If the crate sets `readme = "README.md"`, write that file, and add a row to the Crate
   Map in the root [README](README.md).
9. Bump the crate count, which is spelled out in prose in five places: `README.md`,
   `CHANGELOG.md`, `RELEASING.md` (four times, one of which also lists every crate by
   name), and `release-plz.toml` (twice).
10. Binaries belong in the `notedthat` distribution crate, not in a new one — it is the
    only package with `[package.metadata.dist] dist = true`, and one cargo-dist app per
    workspace keeps the release to one archive and one installer per target.
11. Bootstrap it on crates.io **before** merging: Trusted Publishing cannot create a new
    crate, and `cargo publish` verifies against the registry, so a crate cannot be
    bootstrapped until its dependencies are published at the same version. See
    [RELEASING.md](RELEASING.md#3-bootstrap-publish-first-time-only).

The `Wait for crates.io to index` step in `ci.yml` needs no edit — it derives the crate
list from `cargo metadata`.

## Running the Goose review (LLM review) locally

CI reviews every PR with [Goose](https://github.com/block/goose)
(`.github/workflows/goose-review.yml`) in two lanes, DeepSeek V4 Flash and
MiniMax M3. Each lane runs the checks in `.agents/checks/` with its own
model, the other lane's model re-checks every finding against the code, and
only confirmed findings are posted, as the lane's GitHub App. A check runs
only when the PR changes a file matching its `paths:` globs, and sees only
those files' diff; findings several checks raise on the same lines are
posted as one comment. The same
script runs locally, so a finding can be reproduced before anyone argues
with it.

Every model call goes through our self-hosted LLM egress proxy, which holds
the provider keys, paces requests per model, and fixes tool calling on the
Albert route. Locally you need the proxy's token and its two routes — ask a
maintainer; none of them are in the repository on purpose. Goose 1.52 or
later and Python 3.9+ are required. The prompts point the model at
`ripgrep`, `fd` and `ast-grep` and at dependency sources in
`~/.cargo/registry/src` (run `cargo fetch` first); CI installs them, and
locally the review works without them, only less thoroughly.

```sh
export NOTEDTHAT_PROXY_TOKEN=<proxy token>

# Install the two providers into your Goose config, pointed at the proxy routes
dir="${XDG_CONFIG_HOME:-$HOME/.config}/goose/custom_providers"
.github/scripts/goose-render-provider.sh .github/goose/providers/notedthat_albert.json <Albert route> "$dir"
.github/scripts/goose-render-provider.sh .github/goose/providers/notedthat_minimax.json <MiniMax route> "$dir"

# Review everything changed since origin/main, as the DeepSeek lane would
python3 .github/scripts/goose_review.py review --base origin/main \
  --provider notedthat_albert --model deepseek-v4-flash-0731
python3 .github/scripts/goose_review.py verify --base origin/main \
  --provider notedthat_minimax --model MiniMax-M3

# Print the review the lane would post on a PR, without posting it
GH_TOKEN=$(gh auth token) python3 .github/scripts/goose_review.py post --dry-run \
  --repo NotedThat/NotedThat --pr <number> --head-sha "$(git rev-parse HEAD)" \
  --lane deepseek --model deepseek-v4-flash-0731
```

Set `GOOSE_REVIEW_LOG_DIR=<dir>` to keep every run's prompt and full
transcript, tool calls included. `goose review --checks-only` reads the same
checks, but gives them no tools: they see only the diff.

Two provider settings are load-bearing. Each Albert model sets
`request_params.max_tokens`, because Goose otherwise asks for up to 384,000
output tokens and Albert rejects any request whose prompt plus `max_tokens`
exceeds the model's length (131,072 for DeepSeek). `context_limit` is that
length minus `max_tokens`: Goose compacts a long review conversation at 80%
of it, which must be a size Albert still accepts. And both entries
must go through the proxy: Albert's gateway turns a missing `tool_choice`
into `"none"`, and the proxy is what puts `"auto"` back.

`findings.jsonl`, `verified.jsonl` and `review-status.json` are git-ignored.
