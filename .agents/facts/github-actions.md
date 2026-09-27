---
name: github-actions
description: GitHub Actions and GitHub API behaviour that review models got wrong on this repository.
paths: [".github/**"]
---

Each of these was the premise of a finding that a model raised and a second
model confirmed on this repository, and that turned out to be false. They
are GitHub's documented behaviour; trust them over what you remember.

- **Status check functions in `if:`.** A job or step whose `if:` contains
  none of `success()`, `failure()`, `always()` or `cancelled()` gets an
  implicit `success()`, so it is skipped when a job it `needs` failed.
  `cancelled()` is one of the four: `if: ${{ !cancelled() }}` replaces the
  implicit check and runs after a needed job failed or timed out, and is
  skipped only when the run was cancelled. `always()` runs even then.
- **A matrix job in `needs`.** `needs.<job>.result` is one string for the
  whole matrix, never an array: `success` only if every leg succeeded,
  `failure` if any failed, otherwise `cancelled` or `skipped`.
- **`github.head_ref`** is set for every `pull_request` and
  `pull_request_target` event, from a fork or the same repository alike. It
  is empty for other events.
- **What a `run:` step's environment holds.** The `env:` the workflow, job
  and step set, and the runner's defaults (`GITHUB_*` paths and names,
  `RUNNER_*`, `CI`). No `GITHUB_TOKEN` or `GH_TOKEN` unless an `env:` sets
  it. `ACTIONS_RUNTIME_TOKEN`, `ACTIONS_RESULTS_URL` and
  `ACTIONS_ID_TOKEN_REQUEST_*` are handed to JavaScript and container
  actions, not to `run:` steps. `actions/checkout` with
  `persist-credentials: false` leaves no token in `.git/config`.
- **`GET /repos/{owner}/{repo}/compare/{base}...{head}`.** `per_page` and
  `page` paginate the comparison's commits, not its files. The changed
  files (up to 300 for the whole comparison, each with its `patch`) come
  on the first page, whatever `per_page` is.
- **Minimizing.** `PullRequestReview` implements `Minimizable`, like
  `IssueComment` and `PullRequestReviewComment`. GraphQL's
  `minimizeComment` collapses a whole review as well as a single comment.
- **Concurrency groups.** With `cancel-in-progress: false`, a group holds
  one running and at most one pending run. A newer run replaces the
  pending one, which ends as cancelled without starting. The running one
  is never touched.
