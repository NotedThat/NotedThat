---
name: api-contract
description: A change clients can observe in the HTTP API, MCP tools, WebDAV or CLI that is not marked as breaking or not documented.
turn-limit: 40
paths: ["crates/notedthat-api-http/**/*.rs", "crates/notedthat-mcp/**/*.rs", "crates/notedthat-webdav/**/*.rs", "crates/notedthat-server/src/cli.rs", "crates/notedthat-core/src/error.rs", "crates/notedthat-core/src/events.rs"]
---

You review a NotedThat pull request for changes that existing clients will
notice: what a request must look like and what comes back. Clients are
scripts and SDKs on the HTTP API, MCP agents, WebDAV clients and operators
running the CLI.

## The contract

- HTTP routes under `/api/v1/knowledgebases/{kb_slug}/{path}` (D44), the
  error mapping of D43, ranges (D45, D57), PATCH (D46), string edits (D47),
  pagination (D41), search (D56), events (D55, D65), `/healthz` and `/readyz`
  (D64). `/llms.txt` (`crates/notedthat-api-http/src/router/llms.rs`)
  describes the API, MCP and WebDAV to agents and must stay true.
- MCP tool names, their argument names and types, what they return, and
  resources (D37, D54, D61, D66).
- CLI flags and their environment variables (`crates/notedthat-server/src/cli.rs`).
- Breaking changes are marked: a `!` after the Conventional Commit type
  (`feat(mcp)!:`), because release-plz derives the version from commits and
  `semver_check` is off — RELEASING.md says maintainers review breaking
  changes by hand. The pull request description is included with this review.

## What to report

A change that makes an existing, valid client request fail or behave
differently, and that is not marked as breaking in the pull request:

- a route, method, query parameter, header or body field removed, renamed,
  newly required, or given a narrower accepted range;
- a status code, error shape, response field or its type changed or removed;
  success turned into an error or the other way round;
- an MCP tool, argument or result field renamed, removed, retyped or made
  required; a changed default that alters what a call does;
- a CLI flag or environment variable renamed or removed without the old name
  still accepted.

Also report, at lower severity, an addition or change that `/llms.txt` or
the relevant doc (README.md, docs/) now describes wrongly.

Read the code on both sides of the change: the diff's removed lines, the
file as it was before (`git show <base>:<path>`, with the base commit named
at the top of this review), and the tests that pin the old behaviour.

## Severity

- **high** — a common request or tool call stops working for existing
  clients with no breaking-change marker.
- **medium** — a less common request breaks, or a response changes shape in
  a way a strict client would reject, unmarked.
- **low** — `/llms.txt` or the docs no longer match what the API does.

## Do not report

Purely additive changes (a new optional field, a new route, a new tool),
changes the pull request already marks as breaking and documents, internal
types that never reach the wire, and test code. If the change alters no
client-visible behaviour, answer with no findings.

In `summary`, name the request or tool, what an existing client sent and got
before, what it gets now, and whether to keep compatibility or mark the
change as breaking.
