# NotedThat HTTP API

NotedThat exposes a REST-style HTTP API for reading and writing objects stored in S3-compatible
object storage. The API surface is intentionally small: health probes, a knowledge-base list, and
four operations on objects (list, head, get, put, delete). Every route returns JSON for structured
responses and plain bytes for object bodies.

## Base URL and versioning

All API data-plane routes are prefixed with `/api/v1/`. WebDAV is mounted at `/webdav`, and
streamable MCP is mounted at `/mcp`. Health probes (`/healthz`, `/readyz`) and the LLM
navigation document (`/llms.txt`) sit at the root with no version prefix.
`/browse/...` serves human-facing HTML directory listings — see [Browse surface](#browse-surface).

```
http://HOST:PORT/api/v1/knowledgebases/...
http://HOST:PORT/healthz
http://HOST:PORT/llms.txt
```

The default listen address is `0.0.0.0:8080`. Override it with `NOTEDTHAT_LISTEN_ADDR`.

## LLM navigation document

`GET /llms.txt` returns a public `text/plain; charset=utf-8` document that explains how an LLM can
discover and navigate the deployment: the access rules that govern every surface, the `/api/v1/`
routes, the MCP endpoint with its transport, its auth rule and a copy-pasteable HTTP-transport
client block, and the WebDAV share with its schemes. It contains generic guidance only: it never
includes credentials, hostnames, configured knowledge-base names, or deployment-specific details.

Point an LLM at `http://HOST:PORT/llms.txt` before asking it to work with a NotedThat deployment.

## Browse surface

`/browse` serves server-rendered HTML directory listings for people with a browser. It is a
read-only view over the same storage and the same [access rules](#manifest-access-rules) as every
other surface — not a document manager. There is no JavaScript, no accounts, no editing and no
search UI.

| Request | Response |
| --- | --- |
| `GET /browse` | `308` redirect to `/browse/` |
| `GET /browse/` | Index of knowledge bases you can see |
| `GET /browse/{kb_slug}` | `307` redirect to `/browse/{kb_slug}/` |
| `GET /browse/{kb_slug}/` | The knowledge base's top level |
| `GET /browse/{kb_slug}/{prefix}/` | One directory level |
| `GET /browse/{kb_slug}/{key}` | `303` redirect to the object's `/api/v1` URL, or `307` to the slashed form if it is a folder |
| Any other method | `405` with an `Allow` header |

Directory paths use ordinary multi-segment URLs, unlike the machine API's single percent-encoded
segment. `HEAD` behaves as `GET`.

**Object links point at `/api/v1/knowledgebases/{kb_slug}/{path}`** — the existing representation.
There is no second download path, and Markdown is not rendered to HTML.

**What a row means.** Directories are synthesised from object keys; storage has no directories
(D40). A folder appears exactly when at least one key you can see sits beneath it. A file's name is
a link when you hold `read` on that key, and plain text when you do not — with glob-scoped rules a
single directory can be listable while only part of it is readable. Folders show `—` for size and
date rather than an invented value. `.notedthat` is never rendered, for any caller.

**Authentication.** Anonymous by default. A valid `Authorization: Bearer` header browses as the
credential holder; a supplied credential that does not verify is `401`, never a downgrade to a
public view. Browsers cannot send a Bearer token, so credentialed browsing is currently a `curl`
affair.

**Very large directories.** A page reads at most 10 000 keys. Past that it renders what it read and
says where the listing stops, rather than failing — keys arrive in lexicographic order, so a partial
page is a correct prefix of the truth. (WebDAV `PROPFIND` answers `507` in the same situation,
because a sync client would mistake a partial listing for a complete one and delete the difference.)

**For proxy operators.** Responses carry `Cache-Control: no-store` and `Vary: Authorization`:
anonymous and credentialed callers share a URL and see different pages, so a cached anonymous copy
served to a credentialed caller — or the reverse — would be a disclosure. Pages also carry
`X-Content-Type-Options: nosniff`, a restrictive `Content-Security-Policy`, and
`<meta name="robots" content="noindex, nofollow">` so an accidentally public knowledge base does not
land in a search index by default.

## Authentication

Authenticated callers use a Bearer token, passed in the `Authorization` header:

```
Authorization: Bearer <token>
```

Two kinds of bearer verify. The **service token** is `NOTEDTHAT_API_TOKEN`, compared in constant
time; it is the deployment's own credential and the only one that reaches `.notedthat`. An
**identity token** is a signed JWT access token from the OIDC issuer the deployment is configured
with — Authentik, Authelia or Zitadel, say — and makes the caller a user with a subject and groups.
Either kind is bound by the knowledge base's access rules below; there is no token that bypasses
them. A deployment without an issuer accepts only the service token. See
[OIDC authentication](CONFIGURATION.md#oidc-authentication) for what a token must carry.

When the deployment publishes RFC 9728 metadata (`NOTEDTHAT_OIDC_RESOURCE`), every `401` carries
`WWW-Authenticate: Bearer resource_metadata="<url>/.well-known/oauth-protected-resource"`, and that
document names the authorization server a client can obtain a token from.

Health probes (`/healthz`, `/readyz`) and the LLM navigation document (`/llms.txt`) are globally
public. An installation may additionally grant verbs to `anyone` through a knowledge base's
manifest, and such a grant is honoured on every surface — the HTTP API, WebDAV, the browse pages
and, since D59, MCP. A client must omit `Authorization` only when it knows the requested verb is
granted: a supplied malformed or invalid credential always returns `401 unauthorized` and never
falls back to anonymous access.

### Manifest access rules

Each knowledge base's `.notedthat/manifest.json` carries an `access` array. Each rule names a
subject, the verbs it grants (`may`) or revokes (`may_not`), and the object-key patterns it
applies to:

```json
"access": [
  { "who": "anyone",        "may": ["list", "read"], "under": ["public/**"] },
  { "who": "anyone",        "may": ["search"] },
  { "who": "signed-in",     "may": ["list", "read", "search"] },
  { "who": "group:editors", "may": ["write", "delete"] },
  { "who": "group:interns", "may_not": ["read", "search"], "under": ["hr/**"] }
]
```

| Subject | Who it is |
| --- | --- |
| `anyone` | A caller supplying no `Authorization` header |
| `signed-in` | Any caller whose credential verified: the configured token (or WebDAV Basic credential) and every identity-provider user |
| `group:<name>` | An identity-provider user whose token places them in `<name>` |
| `user:<name>` | An identity-provider user whose username claim is exactly `<name>` |

`group:` and `user:` rules can only match a caller authenticated through OIDC; the configured token
is in no group and has no username, so such rules never match it.

| Verb | What it allows |
| --- | --- |
| `list` | `GET /api/v1/knowledgebases/{kb_slug}`, WebDAV `PROPFIND`, browse pages |
| `read` | `GET` or `HEAD` on an object |
| `write` | `PUT`, `PATCH`, `POST .../replace/...`, WebDAV `PUT` |
| `delete` | `DELETE` |
| `search` | `POST /api/v1/knowledgebases/{kb_slug}/search` |

A rule carries exactly one of `may` and `may_not`. Private by default; a verb is allowed on a key
when some matching `may` rule covers the key and no matching `may_not` rule does. Both sides are
unions, so rule order never changes a decision. Omitting `under` scopes the rule to the whole
knowledge base.

Pattern syntax: `*` matches within one path segment and never `/`; `**` matches whole segments and
must be an entire segment; `?` matches one non-`/` character; `{a,b}` alternates. `public/**`
matches the key `public` and everything beneath it. There is no escape character.

A knowledge base appears in `GET /api/v1/knowledgebases` — and on the browse index — when the caller
holds any grant in it. There is no separate discovery capability.

**Verbs do not imply one another, and `search` is filtered by its own patterns rather than by
`read`.** That is deliberate, and it has a consequence worth stating plainly: a broad `search` grant
combined with a narrow `read` grant returns object paths, heading paths and preview text for keys
the caller cannot fetch. Previews are content. An operator choosing that combination is publishing
excerpts. The `search` patterns are applied inside the searcher, over the whole candidate window and
before the page is cut to `limit`, so a narrow grant does not shorten the page: a caller granted
`search` under `public/**` gets `limit` hits from `public/` when that many match.

**The rules bind the credential holder too.** A manifest can narrow what the configured token may
do, so a read-only deployment is expressible — and a manifest mistake can lock an operator out of
their own knowledge base. One thing is never revocable: `.notedthat` is reachable only for the
configured token, always, and never for `anyone` or for an identity-provider user, whatever the
rules say. So a bad policy can be repaired by `PUT`ting a corrected manifest with that token and
restarting, and the manifest — which carries the policy, group names included — is not readable by
a user with a broad `read` grant. A rule that names `.notedthat` in `under` refuses startup.

Granting or revoking `write` or `delete` for `anyone` refuses startup rather than being honoured.

Policies load once at startup and need a restart after a manifest edit — no hot reload — and there
is no built-in rate limit. Operators enabling anonymous `search` must configure reverse-proxy rate
and burst controls.

If a manifest still carries the removed `public_read` field, it is ignored: the knowledge base comes
up private to anonymous callers, with credentialed access unchanged.

### Authorization failures

| Situation | Status |
| --- | --- |
| No credential supplied, and the rules do not grant it | `404 not_found` — see below |
| A credential supplied that does not verify — wrong service token, expired or foreign identity token | `401 unauthorized` — never downgraded to anonymous |
| A valid credential the rules do not grant — service token or identity token alike | `403 forbidden` |
| The knowledge base is not declared | `404 not_found` — not an authorization answer |

**An anonymous denial is `404`, and deliberately indistinguishable from an undeclared knowledge
base** — same status, and the same error body. Access rules are allow-only and private by default,
so any other status would be an oracle: a `401` for a declared-but-hidden knowledge base and a `404`
for an undeclared one lets anyone willing to guess slugs enumerate a deployment's private knowledge
bases, which is exactly what leaving them out of `GET /api/v1/knowledgebases` is meant to prevent.
The same reasoning already governs `/browse`, and the two surfaces now agree.

The cost is accepted rather than overlooked: an anonymous client is not told that a credential might
change the answer. `401` keeps its narrower meaning — the credential is missing where one is
unconditionally required, such as on any mutating route, or it was supplied and did not verify — and
both of those come from the authentication layer, which knows nothing about any particular knowledge
base. Use the `request_id` in the error body and the server logs to tell the two apart when
diagnosing; that is where the distinction was moved to, not removed.

**401 response when the token is missing or wrong:**

```json
{
  "error": "unauthorized",
  "message": "missing or invalid Authorization header",
  "request_id": "0193f6c5-1234-7890-abcd-1234567890ab"
}
```

**Example:**

```sh
curl -H "Authorization: Bearer $TOKEN" http://localhost:8080/api/v1/knowledgebases
```

## Request ID

Every response, including errors, carries an `x-request-id` header. The value is a UUIDv7 string
generated at the start of each request. Error response bodies also include the same value as
`request_id` so you can correlate logs without inspecting headers.

```
x-request-id: 0193f6c5-1234-7890-abcd-1234567890ab
```

## Error response shape

All error responses use the same JSON envelope:

```json
{
  "error": "error_code",
  "message": "Human-readable description of what went wrong.",
  "request_id": "0193f6c5-1234-7890-abcd-1234567890ab"
}
```

| HTTP status | `error` code | When it occurs |
|-------------|--------------|----------------|
| 400 | `invalid_request` | Malformed path, invalid KB slug, or other bad input |
| 400 | `malformed_range` | Unparseable `Range` header, or a `bytes=` range set naming more than one range |
| 401 | `unauthorized` | Missing `Authorization` header on a route that always requires one, or an invalid one on any route |
| 404 | `not_found` | KB slug not declared, object does not exist, the KB's bucket or directory no longer exists in storage, or an anonymous caller the access rules do not grant |
| 412 | `precondition_failed` | `If-Match` mismatch or `If-None-Match`/`If-Unmodified-Since` condition not met |
| 413 | `payload_too_large` | PUT body exceeds 16 MiB |
| 416 | `range_not_satisfiable` | Requested byte range is out of bounds |
| 410 | `gone` | `Last-Event-ID` on the events stream names a position the log no longer retains; the message names the oldest retained id |
| 500 | `internal_error` | Unexpected server error |
| 503 | `backend_unavailable` | Storage backend unreachable or returned an error; the indexing queue is full (`Retry-After: 5`, object already stored); or the change event could not be published after the write (`Retry-After: 5`, object already stored — retry the idempotent write) |

## Limits

- **Body size:** PUT requests are rejected if the body exceeds **16 MiB** (16,777,216 bytes). The
  check happens on `Content-Length` before reading the body, and again after buffering. Requests
  without `Content-Length` are still capped at 16 MiB during body collection.
- **List default:** 100 objects per call.
- **List maximum:** 1,000 objects per call (pass `?limit=1000`).
- **Pagination:** Use the `next_cursor` field from the response as the `?cursor=` query parameter on the next request to fetch subsequent pages.

---

## Range reads

Partial retrieval of objects using the HTTP `Range:` header.

### Byte ranges (RFC 7233)

Clients can request a partial object body by including a `Range` header:

| Request header | Effect |
|---|---|
| `Range: bytes=0-499` | Returns first 500 bytes |
| `Range: bytes=500-` | Returns from byte 500 to end |
| `Range: bytes=-500` | Returns last 500 bytes |
| `Range: bytes=0-499,1000-1499` | Rejected: **400** `malformed_range` (one range per request) |

**Responses:**
- **206 Partial Content** — successful partial read; includes `Content-Range: bytes start-end/total`
- **416 Range Not Satisfiable** — requested range is out of bounds; response includes `Content-Range: bytes */total`
- **400 Bad Request** `malformed_range` — unparseable `Range` header, or more than one `bytes=` range. A `206` for several ranges would have to be `multipart/byteranges` (RFC 7233 §4.1), which NotedThat does not produce; serving only the first range would be a silent short read, so the request is refused instead.
- **200 OK** — unknown range unit (e.g., `items=0-10`) is silently ignored per RFC 7233 §2.1; full object returned

**curl example:**

```sh
# Request first 100 bytes
curl -H "Authorization: Bearer $TOKEN" \
     -H "Range: bytes=0-99" \
     http://localhost:8080/api/v1/knowledgebases/notes/hello.md

# Response: HTTP/1.1 206 Partial Content
# Content-Range: bytes 0-99/1234
# Content-Length: 100
```

### Line ranges

Request a slice of an object by line number rather than byte offset. Line numbers are 1-based; both ends are inclusive.

**Request grammar:**

| Header value | Effect |
|---|---|
| `Range: lines=<first>-<last>` | Lines `first` through `last` (1-based, inclusive both ends) |
| `Range: lines=<first>-` | From line `first` to end of file |
| `Range: lines=-<length>` | Last `N` lines |
| `Range: lines=<N>-<N-1>` | Zero-width insert point at line `N` (returns empty body; useful to validate an insert offset before PATCH) |

**Semantics:**
- Line 1 is the first line of the object.
- A `last` value past the end of the file is clamped to the total line count.
- `Range: lines=100-200` on a 20-line object returns **416** (out of range).

**Response on 206:**
- `Content-Range: lines <first>-<last>/<total_lines>` — line positions within the object
- `X-Content-Range-Bytes: <byte_start>-<byte_end>/<total_bytes>` — corresponding byte positions (byte_end is inclusive); lets clients convert to byte offsets without an extra HEAD request

**Error responses:**
- **400** `malformed_range` — unparseable `lines=` spec
- **416** `range_not_satisfiable` — out-of-range request; response includes:
  - `Content-Range: lines */<total_lines>`
  - `X-Content-Range-Bytes: */<total_bytes>`
  - Empty body

**curl example:**

```bash
curl -H "Authorization: Bearer $TOKEN" \
     -H "Range: lines=1-5" \
     http://localhost:8080/api/v1/knowledgebases/notes/hello.md
```

**Response snippet:**

```
HTTP/1.1 206 Partial Content
Content-Range: lines 1-5/20
X-Content-Range-Bytes: 0-149/400
Content-Length: 150

[lines 1-5 of hello.md]
```

---

## ETag response header

GET, HEAD, and PUT responses include an `ETag` header when the backend provides one:

- The ETag is opaque and strong (per RFC 7232 §2.3), wrapped in double quotes: `"abc123"`
- The value is provided by the S3 backend and forwarded verbatim — NotedThat does not synthesize ETags
- Use the ETag with conditional request headers to implement optimistic concurrency control

```sh
curl -sI http://localhost:8080/api/v1/knowledgebases/notes/hello.md \
     -H "Authorization: Bearer $TOKEN" | grep -i etag
# ETag: "d41d8cd98f00b204e9800998ecf8427e"
```

---

## Conditional requests (optimistic concurrency)

NotedThat forwards HTTP conditional headers verbatim to the S3 backend. The S3 backend evaluates
preconditions and returns 304 or 412 as appropriate.

**Supported headers and applicable methods:**

| Header | GET | HEAD | PUT | DELETE |
|--------|:---:|:----:|:---:|:------:|
| `If-Match` | ✅ | ✅ | ✅ | ✅ |
| `If-None-Match` | ✅ | ✅ | ✅ | ❌ |
| `If-Modified-Since` | ✅ | ✅ | ❌ | ❌ |
| `If-Unmodified-Since` | ✅ | ✅ | ❌ | ❌ |

Headers marked ❌ are silently ignored (not forwarded) because the S3 API doesn't support them for
that method. This is intentional per the NotedThat pass-through architecture (SPECIFICATIONS.md D9).

**Responses:**
- **304 Not Modified** — GET/HEAD: `If-None-Match` or `If-Modified-Since` conditions met; no body
- **412 Precondition Failed** — `If-Match` mismatch or `If-None-Match`/`If-Unmodified-Since` condition not met

**curl examples:**

```sh
# GET: return 304 if ETag hasn't changed (cache validation)
curl -sI http://localhost:8080/api/v1/knowledgebases/notes/hello.md \
     -H "Authorization: Bearer $TOKEN" \
     -H 'If-None-Match: "abc123"'
# HTTP/1.1 304 Not Modified (if ETag matches)
# HTTP/1.1 200 OK (if ETag has changed)

# PUT: only overwrite if ETag matches (optimistic lock)
curl -sI -X PUT http://localhost:8080/api/v1/knowledgebases/notes/hello.md \
     -H "Authorization: Bearer $TOKEN" \
     -H "Content-Type: text/markdown" \
     -H 'If-Match: "abc123"' \
     --data-binary "updated content"
# HTTP/1.1 201 Created (if ETag matched)
# HTTP/1.1 412 Precondition Failed (if ETag didn't match)

# PUT: only create if object doesn't exist
curl -sI -X PUT http://localhost:8080/api/v1/knowledgebases/notes/new.md \
     -H "Authorization: Bearer $TOKEN" \
     -H "Content-Type: text/markdown" \
     -H 'If-None-Match: *' \
     --data-binary "brand new"
# HTTP/1.1 201 Created (if object didn't exist)
# HTTP/1.1 412 Precondition Failed (if object already exists)

# DELETE: only delete if ETag matches
curl -sI -X DELETE http://localhost:8080/api/v1/knowledgebases/notes/hello.md \
     -H "Authorization: Bearer $TOKEN" \
     -H 'If-Match: "abc123"'
# HTTP/1.1 204 No Content (if ETag matched)
# HTTP/1.1 412 Precondition Failed (if ETag didn't match)
```

---

## Backend compatibility

NotedThat parses the `Range` header itself, accepts one `bytes=` range per request, and forwards
that range and the conditional headers to the storage backend. Actual behavior depends on the
backend's RFC 7232/7233 support.

NotedThat is tested against **SeaweedFS 4.18+** which supports:
- Byte-range reads (`Range: bytes=`)
- ETags on GET/HEAD/PUT
- `If-Match`, `If-None-Match` on GET/HEAD/PUT/DELETE
- `If-Modified-Since`, `If-Unmodified-Since` on GET/HEAD

See `SPECIFICATIONS.md §9.1` for the full compatibility matrix.

---

## Not supported in v1

The following features are intentionally out of scope:

- **`If-Range` header** (RFC 7233 §3.2) — not parsed, not forwarded
- **`multipart/byteranges` response bodies** — a `Range` header naming more than one range is
  rejected with `400 malformed_range`; NotedThat never synthesizes `multipart/byteranges`
- **Conditional DELETE with `If-None-Match` / `If-Modified-Since` / `If-Unmodified-Since`** —
  the S3 API does not support these on DELETE; they are silently ignored
- **Conditional PUT with `If-Modified-Since` / `If-Unmodified-Since`** — same; silently ignored

---

## Routes

### GET /healthz

Liveness probe. Returns `200 OK` immediately with no auth check. Use this to verify the process is
alive.

**Authentication:** Not required.

**Response:**

| Status | Body |
|--------|------|
| 200 OK | `{"status": "ok"}` |

**Example:**

```sh
curl http://localhost:8080/healthz
```

**Response body:**

```json
{"status": "ok"}
```

---

### GET /readyz

Readiness probe. Returns `200 OK` unless a configured object change event backend
(`NOTEDTHAT_EVENTS_BACKEND=nats`) reports that it is not connected, in which case it
returns `503`. Storage and Qdrant are not probed in v1.

**Authentication:** Not required.

**Response:**

| Status | Body |
|--------|------|
| 200 OK | `{"status": "ok"}` |
| 503 Service Unavailable | `{"status": "unavailable", "events": "nats", "reason": "event backend not connected"}` |

**Example:**

```sh
curl http://localhost:8080/readyz
```

**Response body:**

```json
{"status": "ok"}
```

---

### GET /api/v1/knowledgebases

List the knowledge bases the caller can see, in slug order. Each entry carries the slug every
other route takes, the manifest's `display_name`, and — when the manifest says what the knowledge
base is for — its `description`, so an agent can choose where to search before it searches.

**Authentication:** The response lists the knowledge bases the caller can see — those whose access
rules grant them anything at all. A valid Bearer token sees every declared knowledge base. If an
anonymous caller can see none, the request returns `401` rather than an empty array: both disclose
the same nothing, but `401` is the truthful answer to "may I look at this deployment". This is the
one route that still answers `401` to an anonymous caller the rules refuse, and it can: it names no
knowledge base, so there is no slug whose existence the status could disclose. Every route that does
name one answers `404` instead. `HEAD` follows `GET`. A supplied invalid credential returns `401`.

**Response:**

| Status | Body |
|--------|------|
| 200 OK | `{"knowledgebases": [{"kb_slug": "slug1", "display_name": "…", "description": "…"}, …]}` |

| Field | Always present | Meaning |
|-------|----------------|---------|
| `kb_slug` | yes | The slug as declared in `NOTEDTHAT_KBS`; entries are sorted by it |
| `display_name` | yes | The manifest's `display_name`; the slug itself until an operator sets one |
| `description` | no | The manifest's `description`: one line, at most 500 characters, set by an operator (see [Knowledge base description](CONFIGURATION.md#knowledge-base-description)). Omitted, never `null`, when the manifest has none |

A description is shown exactly when its knowledge base is listed: a knowledge base the caller
cannot see contributes no entry, so nothing about it is described.

**Upgrade note (bare slugs → objects).** Before knowledge base descriptions the array held bare
slug strings. A client that indexed into it as strings must read `kb_slug` now. The bundled
`notedthat-mcp-stdio` reads both shapes, so a newer adapter still lists an older server; an older
adapter against a newer server does not, and fails `list_knowledgebases`, `resources/list` and a
`kb`-less `search` with a deserialization error — upgrade the adapter, they ship together.

**Example:**

```sh
curl -H "Authorization: Bearer $TOKEN" \
     http://localhost:8080/api/v1/knowledgebases
```

**Response body:**

```json
{
  "knowledgebases": [
    {
      "kb_slug": "notes",
      "display_name": "Engineering notes",
      "description": "Design notes, ADRs and meeting minutes of the platform team."
    },
    { "kb_slug": "scratch", "display_name": "scratch" }
  ]
}
```

---

### GET /api/v1/knowledgebases/{kb_slug}

List objects in a knowledge base. Supports optional prefix filtering and a result limit.

**Authentication:** Requires the `list` verb. Keys the caller may not `list` are omitted, so a
page can be shorter than `limit` — and can even be empty — while `truncated` is still `true`. Drive
pagination from `next_cursor`, never from the number of objects returned. A supplied invalid
credential returns `401`; a valid credential the rules do not grant returns `403`.

**Path parameters:**

| Parameter | Description |
|-----------|-------------|
| `kb_slug` | Knowledge base slug (must be declared in `NOTEDTHAT_KBS`) |

**Query parameters:**

| Parameter | Type   | Default | Description |
|-----------|--------|---------|-------------|
| `prefix`  | string | —       | Return only objects whose key begins with this string |
| `limit`   | number | 100     | Maximum objects per page (1–1000) |
| `cursor`  | string | —       | Opaque continuation token from a previous response's `next_cursor` field. Clients MUST NOT parse or construct this value. |

**Response:**

| Status | Body |
|--------|------|
| 200 OK | `{"objects": [...], "truncated": bool, "next_cursor": string|null}` |
| 404 Not Found | `{"error": "not_found", ...}` |

**Response body fields:**

| Field         | Type             | Description |
|---------------|------------------|-------------|
| `objects`     | array of objects | Matching objects (key, size, last_modified, content_type, etag) |
| `truncated`   | boolean          | `true` when more objects exist beyond this page |
| `next_cursor` | string or null   | Opaque continuation token for the next page. Pass as `?cursor=` on the next request. `null` on the final page. |

Each object in the array has:

```json
{
  "key": "notes/2024/jan.md",
  "size": 1234,
  "last_modified": "2024-01-15T10:30:00Z",
  "content_type": "text/markdown"
}
```

`last_modified` and `content_type` may be absent if the backend doesn't return them.

`truncated` is `true` when there are more objects beyond the returned set.

**Example:**

```sh
curl -H "Authorization: Bearer $TOKEN" \
     "http://localhost:8080/api/v1/knowledgebases/notes"
```

**With prefix and limit:**

```sh
curl -H "Authorization: Bearer $TOKEN" \
     "http://localhost:8080/api/v1/knowledgebases/notes?prefix=2024/&limit=50"
```

**Response body:**

```json
{
  "objects": [
    {
      "key": "2024/jan.md",
      "size": 512,
      "last_modified": "2024-01-15T10:30:00Z",
      "content_type": "text/markdown"
    }
  ],
  "truncated": false,
  "next_cursor": null
}
```

### Pagination example

Page 1 (first request — no cursor):

```sh
curl -H "Authorization: Bearer <token>" \
  "https://example.com/api/v1/knowledgebases/notes?limit=100"
```

Response body (truncated — more pages exist):

```json
{
  "objects": [...],
  "truncated": true,
  "next_cursor": "CgBkb2MtMDA5OS5tZA=="
}
```

Page 2 (pass `next_cursor` as `cursor`):

```sh
curl -H "Authorization: Bearer <token>" \
  "https://example.com/api/v1/knowledgebases/notes?limit=100&cursor=CgBkb2MtMDA5OS5tZA=="
```

Final page (no more results):

```json
{
  "objects": [...],
  "truncated": false,
  "next_cursor": null
}
```

### Invalid or expired cursor

Passing a cursor that is invalid, expired, or otherwise not recognized by the backend returns:

```
HTTP 503 Service Unavailable
{"error": "backend_unavailable", "message": "...", "request_id": "..."}
```

The cursor format is opaque and owned by the storage backend. NotedThat does not validate cursor strings; it passes them through unchanged. Clients that receive a 503 should re-fetch from the beginning (cursor=None).

### Live listing semantics (non-snapshot)

Cursors are for immediate continuation of a live listing, not a stable snapshot. Writes/deletes between page N and page N+1 may cause the specific object at page-boundary positions to appear on both pages, disappear, or shift ordering. Clients that need snapshot semantics must implement their own snapshot layer.

---

### HEAD /api/v1/knowledgebases/{kb_slug}/{path}

Check whether an object exists and retrieve its metadata without downloading the body.

**Authentication:** Required unless the client omits `Authorization` and this knowledge base's
manifest grants `content`. A supplied invalid credential returns `401`.

**Path parameters:**

| Parameter | Description |
|-----------|-------------|
| `kb_slug` | Knowledge base slug |
| `path` | Object path, may contain multiple segments (e.g. `notes/2024/jan.md`) |

**Response headers (on 200):**

| Header | Description |
|--------|-------------|
| `content-length` | Object size in bytes |
| `content-type` | MIME type, if stored |
| `last-modified` | Last modification time, if available |
| `etag` | Object ETag, if provided by the backend |

**Response:**

| Status | Meaning |
|--------|---------|
| 200 OK | Object exists; metadata in headers, no body |
| 304 Not Modified | Conditional request: `If-None-Match` or `If-Modified-Since` matched |
| 404 Not Found | Object or KB does not exist |
| 412 Precondition Failed | `If-Match` mismatch |

**Example:**

```sh
curl -I -H "Authorization: Bearer $TOKEN" \
     http://localhost:8080/api/v1/knowledgebases/notes/hello.md
```

---

### GET /api/v1/knowledgebases/{kb_slug}/{path}

Download an object. Returns the raw bytes with appropriate `Content-Type` and `Content-Length`
headers. Supports byte-range reads and conditional requests.

**Authentication:** Required unless the client omits `Authorization` and this knowledge base's
manifest grants `content`. A supplied invalid credential returns `401`.

**Path parameters:**

| Parameter | Description |
|-----------|-------------|
| `kb_slug` | Knowledge base slug |
| `path` | Object path, may contain multiple segments (e.g. `notes/2024/jan.md`) |

**Request headers:**

| Header | Description |
|--------|-------------|
| `Range` | Request a byte range or line range (see [Range reads](#range-reads)) |
| `If-Match` | Return 412 if ETag doesn't match |
| `If-None-Match` | Return 304 if ETag matches |
| `If-Modified-Since` | Return 304 if not modified since the given date |
| `If-Unmodified-Since` | Return 412 if modified since the given date |

**Response headers (on 200/206):**

| Header | Description |
|--------|-------------|
| `content-type` | MIME type (falls back to `application/octet-stream` if not stored) |
| `content-length` | Object size in bytes (or partial size on 206) |
| `etag` | Object ETag, if provided by the backend |
| `content-range` | Range returned, present only on 206 responses. Format: `bytes <start>-<end>/<total>` for byte-range requests; `lines <first>-<last>/<total_lines>` for line-range requests |
| `x-content-range-bytes` | Present only on 206 line-mode responses. Byte positions corresponding to the returned line range, in the form `<byte_start>-<byte_end>/<total_bytes>` (byte_end inclusive) |

**Response:**

| Status | Body |
|--------|------|
| 200 OK | Full object bytes |
| 206 Partial Content | Partial object bytes (byte-range or line-range request satisfied) |
| 304 Not Modified | No body (conditional request matched) |
| 400 Bad Request | `{"error": "malformed_range", ...}` — unparseable `Range` header, or more than one `bytes=` range |
| 404 Not Found | `{"error": "not_found", ...}` |
| 412 Precondition Failed | `{"error": "precondition_failed", ...}` |
| 416 Range Not Satisfiable | `{"error": "range_not_satisfiable", ...}` |

**Example:**

```sh
curl -H "Authorization: Bearer $TOKEN" \
     http://localhost:8080/api/v1/knowledgebases/notes/hello.md
```

**Multi-segment path:**

```sh
curl -H "Authorization: Bearer $TOKEN" \
     http://localhost:8080/api/v1/knowledgebases/notes/2024/january/meeting-notes.md
```

**Partial download (first 100 bytes):**

```sh
curl -H "Authorization: Bearer $TOKEN" \
     -H "Range: bytes=0-99" \
     http://localhost:8080/api/v1/knowledgebases/notes/hello.md
# HTTP/1.1 206 Partial Content
# Content-Range: bytes 0-99/1234
```

**Cache validation:**

```sh
curl -H "Authorization: Bearer $TOKEN" \
     -H 'If-None-Match: "abc123"' \
     http://localhost:8080/api/v1/knowledgebases/notes/hello.md
# HTTP/1.1 304 Not Modified (if ETag matches)
```

---

### PUT /api/v1/knowledgebases/{kb_slug}/{path}

Upload or replace an object. The operation is idempotent: uploading to an existing path overwrites
it. Use `If-None-Match: *` to create-only, or `If-Match: <etag>` for optimistic concurrency.

**Authentication:** Required.

**Path parameters:**

| Parameter | Description |
|-----------|-------------|
| `kb_slug` | Knowledge base slug |
| `path` | Object path, may contain multiple segments |

**Request headers:**

| Header | Required | Description |
|--------|----------|-------------|
| `Content-Type` | Recommended | MIME type stored alongside the object |
| `Content-Length` | Recommended | Body size in bytes; used for early 413 rejection |
| `If-Match` | Optional | Only overwrite if ETag matches (optimistic lock) |
| `If-None-Match` | Optional | `*` to create-only (fail if object already exists) |

**Body:** Raw object bytes. Maximum 16 MiB.

**Response:**

| Status | Meaning |
|--------|---------|
| 201 Created | Object stored successfully |
| 400 Bad Request | Invalid path or KB slug |
| 401 Unauthorized | Missing or invalid token |
| 404 Not Found | KB slug not declared |
| 412 Precondition Failed | `If-Match` mismatch or `If-None-Match: *` conflict |
| 413 Payload Too Large | Body exceeds 16 MiB |

**Response headers (on 201):**

| Header | Description |
|--------|-------------|
| `location` | Path to the created object, e.g. `/api/v1/knowledgebases/notes/hello.md` |
| `etag` | Object ETag, if provided by the backend |

The response body is empty on success.

**Example — upload a Markdown file:**

```sh
curl -X PUT \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: text/markdown" \
  --data-binary @hello.md \
  http://localhost:8080/api/v1/knowledgebases/notes/hello.md
```

**Example — upload from stdin:**

```sh
echo "# Hello World" | curl -X PUT \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: text/markdown" \
  --data-binary @- \
  http://localhost:8080/api/v1/knowledgebases/notes/hello.md
```

**Example — upload a nested path:**

```sh
curl -X PUT \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: text/markdown" \
  --data-binary @jan.md \
  http://localhost:8080/api/v1/knowledgebases/notes/2024/january/meeting-notes.md
```

**Example — create-only (fail if exists):**

```sh
curl -X PUT \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: text/markdown" \
  -H 'If-None-Match: *' \
  --data-binary @hello.md \
  http://localhost:8080/api/v1/knowledgebases/notes/hello.md
# HTTP/1.1 201 Created (if object didn't exist)
# HTTP/1.1 412 Precondition Failed (if object already exists)
```

**Example — conditional overwrite (optimistic lock):**

```sh
curl -X PUT \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: text/markdown" \
  -H 'If-Match: "abc123"' \
  --data-binary @hello.md \
  http://localhost:8080/api/v1/knowledgebases/notes/hello.md
# HTTP/1.1 201 Created (if ETag matched)
# HTTP/1.1 412 Precondition Failed (if ETag didn't match)
```

---

### DELETE /api/v1/knowledgebases/{kb_slug}/{path}

Delete an object. The operation is idempotent: deleting a non-existent object returns `204` just
like deleting one that exists. Use `If-Match` to guard against deleting a version you didn't intend.

**Authentication:** Required.

**Path parameters:**

| Parameter | Description |
|-----------|-------------|
| `kb_slug` | Knowledge base slug |
| `path` | Object path, may contain multiple segments |

**Request headers:**

| Header | Required | Description |
|--------|----------|-------------|
| `If-Match` | Optional | Only delete if ETag matches |

**Response:**

| Status | Meaning |
|--------|---------|
| 204 No Content | Object deleted (or did not exist) |
| 400 Bad Request | Invalid path or KB slug |
| 401 Unauthorized | Missing or invalid token |
| 404 Not Found | KB slug not declared |
| 412 Precondition Failed | `If-Match` mismatch |

The response body is always empty on success.

**Example:**

```sh
curl -X DELETE \
  -H "Authorization: Bearer $TOKEN" \
  http://localhost:8080/api/v1/knowledgebases/notes/hello.md
```

**Example — delete a nested path:**

```sh
curl -X DELETE \
  -H "Authorization: Bearer $TOKEN" \
  http://localhost:8080/api/v1/knowledgebases/notes/2024/january/meeting-notes.md
```

**Example — conditional delete (only if ETag matches):**

```sh
curl -X DELETE \
  -H "Authorization: Bearer $TOKEN" \
  -H 'If-Match: "abc123"' \
  http://localhost:8080/api/v1/knowledgebases/notes/hello.md
# HTTP/1.1 204 No Content (if ETag matched)
# HTTP/1.1 412 Precondition Failed (if ETag didn't match)
```

---

### PATCH /api/v1/knowledgebases/{kb_slug}/{path}

Partial write — replaces a byte range, a line range, or appends to the end of an object without uploading the full object.

**NOT idempotent**: the object ETag advances on each successful PATCH. Concurrent PATCHes with the same `If-Match` result in exactly one winner and 412 for the rest (OCC contract).

#### Request headers

| Header | Format | Required? | Notes |
|--------|--------|-----------|-------|
| `Content-Range` | `bytes <first>-<last>/*` or `lines <first>-<last>/*` | Required for bytes/lines mode | Mutually exclusive with `NT-Patch-Mode: append`. |
| `If-Match` | `"<etag>"` | Required for bytes/lines mode; optional for append | Single strong ETag. `*` and comma-separated lists are rejected (400). |
| `NT-Patch-Mode` | `append` | Optional | Mutually exclusive with `Content-Range`. Triggers single-round-trip append (server obtains ETag internally). |
| `Content-Type` | MIME type | Optional | Hint for the resulting object's content type. |

#### Response

| Status | Condition | Response headers |
|--------|-----------|-----------------|
| `200 OK` | Splice succeeded | `ETag`, `Location` |
| `400 invalid_request` | Malformed `Content-Range`, missing `If-Match` in bytes/lines mode, `If-Match: *`, multi-value `If-Match`, or `NT-Patch-Mode: append` combined with `Content-Range` | JSON error body |
| `404 not_found` | Object does not exist | JSON error body |
| `412 precondition_failed` | `If-Match` does not match the current ETag (caller's OCC assertion failed, or retry budget exhausted) | JSON error body |
| `413 payload_too_large` | Request body OR resulting object exceeds `NOTEDTHAT_MAX_PATCHABLE_SIZE` | JSON error body |
| `416 range_not_satisfiable` | Line range beyond EOF | `Content-Range: lines */<total>`, `X-Content-Range-Bytes: */<total_bytes>`, empty body |
| `503 backend_unavailable` | Indexer queue full (object IS stored; retry to re-enqueue index) | `Retry-After: 5`, JSON body |

#### curl examples

```bash
# Replace lines 2-3
ETAG=$(curl -sI -H "Authorization: Bearer $TOKEN" http://localhost:8080/api/v1/knowledgebases/notes/hello.md | grep -i etag | cut -d' ' -f2 | tr -d '\r')
curl -X PATCH \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Range: lines 2-3/*" \
  -H "If-Match: $ETAG" \
  -d "new line 2\nnew line 3\n" \
  http://localhost:8080/api/v1/knowledgebases/notes/hello.md

# Insert before line 5 (Content-Range: lines 5-4/*)
curl -X PATCH \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Range: lines 5-4/*" \
  -H "If-Match: $ETAG" \
  -d "inserted line\n" \
  http://localhost:8080/api/v1/knowledgebases/notes/hello.md

# Append (single round-trip; no prior HEAD needed)
curl -X PATCH \
  -H "Authorization: Bearer $TOKEN" \
  -H "NT-Patch-Mode: append" \
  -d "appended text\n" \
  http://localhost:8080/api/v1/knowledgebases/notes/hello.md
```

#### Backend compatibility

PATCH correctness under concurrent writers requires the backend to enforce `If-Match` atomically on PUT. Backends listed in `SPECIFICATIONS.md §8.1` as **silent-200 backends** (those that return a silent 200 for `If-Match` without enforcing it atomically) may silently overwrite each other's data under concurrent PATCH. Operators should consult §8.1 before enabling PATCH workloads on those backends.

#### Ghost-state on 503

When PATCH returns `503 backend_unavailable`, the splice has been applied to storage but the search index was not updated — the object is stored-but-not-indexed. This is the same ghost-state condition as PUT/DELETE 503: a subsequent PATCH with the same `If-Match` will return `412` (the ETag has advanced), while a GET will return the new content. Operators should retry the failed index update by re-issuing the same PATCH with an updated `If-Match` obtained from a fresh HEAD request. See `docs/CONFIGURATION.md` for the indexer backpressure configuration.

---

### POST /api/v1/knowledgebases/{kb_slug}/replace/{path}

String replace — find and replace an exact UTF-8 substring within an object without uploading the full body.

**NOT idempotent**: the object ETag advances on each successful replace. A 503 response means the mutation already landed and the ETag has advanced; clients MUST reconcile (HEAD or GET the current state) rather than blindly resending.

#### Request headers

| Header | Required | Notes |
|--------|----------|-------|
| `If-Match` | Required | Single strong ETag. `*` and comma-separated multi-value are rejected (400). |
| `Content-Type` | Required | Must be `application/json`. |

#### Request body

```json
{
  "old_string": "exact text to find (required, non-empty)",
  "new_string": "replacement text (required, may be empty string)",
  "replace_all": false
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `old_string` | string | Yes | Exact UTF-8 byte sequence to search for. Must be non-empty. |
| `new_string` | string | Yes | Replacement text. May be an empty string (deletion). |
| `replace_all` | boolean | No | Default `false`. When `false`, exactly one match is required; zero matches → 422 `no_match`, multiple matches → 422 `ambiguous_match`. When `true`, all non-overlapping occurrences are replaced left-to-right in a single splice. |

#### Response

**200 OK** — replace succeeded.

Response headers:

| Header | Description |
|--------|-------------|
| `ETag` | New ETag of the object after the splice |
| `Content-Location` | `/api/v1/knowledgebases/{kb_slug}/{percent_encoded_path}` |

Response body:

```json
{
  "etag": "\"d41d8cd98f00b204e9800998ecf8427e\"",
  "match_count": 1,
  "total_bytes": 4096
}
```

| Field | Description |
|-------|-------------|
| `etag` | New ETag (same value as the `ETag` response header) |
| `match_count` | Number of occurrences replaced (1 when `replace_all=false`; ≥ 0 when `replace_all=true`) |
| `total_bytes` | Size of the object after the splice |

**Error codes:**

| Status | `error` code | When |
|--------|-------------|------|
| 400 | `invalid_request` | Missing or blank `old_string`; `If-Match: *`; multi-value `If-Match`; missing `If-Match`; non-`application/json` Content-Type; malformed JSON body |
| 412 | `precondition_failed` | `If-Match` does not match the current ETag |
| 413 | `payload_too_large` | Resulting object would exceed `NOTEDTHAT_MAX_PATCHABLE_SIZE` |
| 422 | `no_match` | `old_string` not found in the object (storage unchanged) |
| 422 | `ambiguous_match` | `old_string` found more than once and `replace_all=false` (storage unchanged); body includes `"match_count": N` |
| 503 | `backend_unavailable` | S3 backend unreachable, or indexer queue full. **NOT safe to replay** — the mutation may have landed and the ETag has advanced. Reconcile with HEAD/GET before retrying. |

#### URL namespace

The replace action lives at `.../replace/<path>`. To replace content in an object whose own path starts with `replace/`, double the prefix: `POST .../replace/replace/foo.md` targets the object at `replace/foo.md`.

#### curl example

```bash
# Fetch the current ETag
ETAG=$(curl -sI -H "Authorization: Bearer $TOKEN" \
  http://localhost:8080/api/v1/knowledgebases/notes/hello.md \
  | grep -i '^etag:' | awk '{print $2}' | tr -d '\r')

# Replace the first occurrence of "old text" with "new text"
curl -X POST \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -H "If-Match: $ETAG" \
  -d '{"old_string": "old text", "new_string": "new text"}' \
  http://localhost:8080/api/v1/knowledgebases/notes/hello.md
```

---

### POST /api/v1/knowledgebases/{kb_slug}/search

Perform a hybrid semantic search (dense cosine + sparse BM25 with server-side RRF fusion) against a knowledge base.

**Authentication**: Requires the static Bearer token unless this knowledge base has the `search`
anonymous capability and the client omits `Authorization` (see [Authentication](#authentication)).

**Path parameters**:

| Parameter | Format | Description |
|-----------|--------|-------------|
| `kb_slug` | `[a-z0-9-]{1,40}` | Slug of a declared knowledge base |

**Request body** (`application/json`):

```json
{
  "query": "string (required, 1–8192 bytes after trim)",
  "filter": {
    "object_key_prefix": "docs/rfc/",
    "mime": "text/markdown",
    "heading_path_prefix": ["Introduction"],
    "updated_after": 1700000000,
    "updated_before": 1800000000,
    "tags": ["rust"],
    "concept_type": "Reference"
  },
  "limit": 10
}
```

All `filter` fields are optional and AND-composed. `limit` defaults to `10`, is clamped to `[1, 50]`, and a value above 50 is silently clamped (not an error).

An unknown key — at the top level or inside `filter` — is `400 invalid_request`, and the message
names the key and the accepted ones (`invalid request body: unknown field `filters`, expected one
of `query`, `filter`, `limit``). Nothing is ignored: the field set is small and stable, and a
misspelt key answered with the unfiltered result is exactly the confusion #125 was filed over.

**Response body (200 OK)**:

```json
{
  "hits": [
    {
      "object_key": "docs/rfc/7231.md",
      "byte_start": 1024,
      "byte_end": 2048,
      "heading_path": ["Section 1", "Subsection 1.2"],
      "score": 0.0163,
      "preview": "RFC 7231 defines HTTP semantics and content negotiation..."
    }
  ]
}
```

`hits` is empty (`[]`) when no results match — the response is never `{}` or `{"hits": null}`.

**Status codes and errors**:

| Status | `error` code | When |
|--------|-------------|------|
| 200 | — | Success. `hits` may be empty. |
| 400 | `invalid_request` | Missing or blank `query`; malformed JSON; an unknown key at the top level or inside `filter`; missing or non-`application/json` Content-Type; `limit=0`; malformed slug format. |
| 401 | `unauthorized` | Missing or invalid Bearer token. |
| 404 | `not_found` | `kb_slug` not declared in `NOTEDTHAT_KBS`. |
| 413 | `payload_too_large` | Request body exceeds 64 KiB. |
| 500 | `internal_error` | Unexpected server error. |
| 503 | `backend_unavailable` | Qdrant or embedding service unreachable. |

**Example**:

```bash
curl -sSf -X POST \
  -H "Authorization: Bearer $NOTEDTHAT_API_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"query": "install cargo", "limit": 5}' \
  http://127.0.0.1:8080/api/v1/knowledgebases/notes/search
```

**Notes**:

- **Score semantics**: Search returns top-`limit` hits ordered by descending RRF fusion score. Scores are RRF rank values — higher is better. They are **NOT** probabilities or cosine similarities, are **NOT** comparable across queries or knowledge bases, and should **not** be displayed to users as confidence values. A non-empty knowledge base returns up to `limit` hits for any query, however unrelated; an empty `hits` array never means "nothing relevant". `/llms.txt` carries this caveat too, so it reaches agents that never see this file (#126).

- **Ordering is deterministic** (D56): equal scores are the normal case with RRF, so hits are ordered by `score` descending, then `object_key` ascending (byte order), then `byte_start` ascending — a total order, since `(object_key, byte_start)` names one chunk. The server retrieves the whole fused candidate set from the vector backend (both retrieval arms in full, at most 500 points) and ranks it itself, so the backend never decides a tie, at the `limit` boundary or anywhere else. For an unchanged index, the same `query`, `limit`, `filter` and credential return a byte-identical `hits` array on every call. The one residual: each retrieval arm is itself a top-k, and a tie *exactly at an arm's edge* can change which candidate enters the set — a much rarer event than a tie among the fused scores, and one that only reaches the response when the arm is shallower than the corpus of near-equal candidates.

- **Indexing lag**: Indexing is asynchronous best-effort (D38). A document just written may take a few seconds to appear in search results. **Exception**: if the indexing queue is full, the write returns HTTP 503 `backend_unavailable` with `Retry-After: 5` (not ordinary async lag) — the object is stored but not yet searchable, and the client should retry to re-enqueue the indexing event.

- **Preview**: The `preview` field is a UTF-8-safe truncation of the chunk text to at most 500 characters. Use `object_key` with `byte_start`/`byte_end` and a `Range: bytes=<byte_start>-<byte_end - 1>` header on `GET /api/v1/knowledgebases/{kb_slug}/{path}` to fetch the full chunk.

- **OKF metadata and filters**: Concept hits include an optional `okf` object containing `concept_id`, `type`, and available `title`, `description`, `resource`, and `tags`. `concept_type` matches the type exactly; `tags` matches at least one supplied tag. Both are indexed in Qdrant. MCP exposes these through its `filters` argument. See [OKF support and upgrade instructions](OKF.md).

- **`content_hash`**: Stored in the Qdrant payload for idempotent reindex detection but is **not** exposed in `SearchHit`.

- **`object_key_prefix` filter**: Applied **client-side** by the Searcher, together with the caller's `search` grant, after the vector backend has fused its candidates and before the page is cut to `limit`. (qdrant-client 1.15 does not expose a native keyword-index prefix matcher.) When either applies, both retrieval arms are deepened to 10× your `limit` (at most 250 per arm, 500 fused) and only hits inside the prefix and the grant are kept. A response shorter than `limit` means fewer than `limit` matching chunks ranked inside that window; a narrower query or a wider prefix recovers coverage.

#### Upgrade notes (M4 → M5)

> **Reindex recommended after upgrading from M4.** The Qdrant payload schema was extended in M5: `mime`, `tags`, `content_hash`, and `text` fields were added, and a `mime` payload index was created. Documents written by an M4 server will not be returned by `mime` filters and will have empty `preview` fields until they are re-written or the KB is reindexed. Reindex tooling is a post-v1 feature (D42); operators can trigger a rewrite by PUTting existing documents again via `PUT /api/v1/knowledgebases/{kb_slug}/{path}`.
>
> The CHANGELOG for this release is generated automatically by release-plz — do not edit it by hand. This section is the operator-facing source of truth for upgrade guidance.

#### Upgrade notes (strict search body)

> The search body used to ignore keys it did not know, at the top level and inside `filter`. It
> now answers `400 invalid_request` naming the key (#125). A client that sent extra keys — most
> likely `filters` for `filter` — was getting unfiltered results and now gets told; fix the key.
> The MCP `search` tool is strict the same way, and its `filters` argument gained the four fields
> it was missing (`object_key_prefix`, `heading_path_prefix`, `updated_after`, `updated_before`).

---

### GET /api/v1/knowledgebases/{kb_slug}/events

Subscribe to object change events for one knowledge base as
[server-sent events](https://html.spec.whatwg.org/multipage/server-sent-events.html)
(`text/event-stream`). Every write made through NotedThat — the HTTP API, WebDAV and MCP — and,
on the `fs` backend, every change the server detects in its tree, is published as one event once
storage has acknowledged it, so a subscriber that `GET`s the key on receipt sees the bytes the
event describes or something newer.

Requires an events backend: with the default `NOTEDTHAT_EVENTS_BACKEND=none` the route answers
`404 not_found` (after the access checks below, so an undeclared or ungranted knowledge base
answers exactly as it does everywhere else). See
[CONFIGURATION.md](CONFIGURATION.md#events-backend).

**Authentication:** Requires a credential the knowledge base's access rules grant `list`
somewhere, or an `anyone` rule granting `list` for anonymous callers. Each event is then filtered
individually: a subscriber receives an event for a key only if it may `list` that key. Deletions
are filtered the same way as writes, since a deletion reveals that the key existed. A credential
granted nothing answers `403`; an anonymous caller granted nothing is concealed with `404`.

**Path parameters:**

| Parameter | Format | Description |
|-----------|--------|-------------|
| `kb_slug` | `[a-z0-9-]{1,40}` | Slug of a declared knowledge base |

**Query parameters** (all optional, applied after the access filter):

| Parameter | Description |
|-----------|-------------|
| `prefix` | Only keys starting with this string, e.g. `prefix=inbox/` |
| `event` | `written` or `deleted`; anything else is `400` |
| `mime` | Only writes whose stored content type matches — exactly (`audio/mpeg`) or by type (`audio/*`). Deletions carry no content type and are excluded whenever `mime` is set. |

**Request headers:**

| Header | Description |
|--------|-------------|
| `Last-Event-ID` | Resume after this id: every retained event with a greater id is replayed, in order, before live events. Omit it to receive live events only. Not a non-negative integer → `400`. Older than the log retains, or ahead of it → `410`. |
| `Accept` | `text/event-stream` is conventional; the server does not require it |

**Response:**

| Status | When |
|--------|------|
| 200 OK | The stream is open; `Content-Type: text/event-stream`, `Cache-Control: no-cache`, `X-Accel-Buffering: no` |
| 400 Bad Request | Malformed `Last-Event-ID` or `event` |
| 403 Forbidden | Credential holder granted `list` nowhere in this knowledge base |
| 404 Not Found | Undeclared slug, anonymous caller granted nothing, or no events backend configured (the message names `NOTEDTHAT_EVENTS_BACKEND`) |
| 410 Gone | `Last-Event-ID` is older than the log retains, or ahead of it (a log that started again); the message names the oldest retained id. Resync by listing the knowledge base rather than resuming from "now". |
| 503 Service Unavailable | The event backend cannot be read; `Retry-After: 5` |

**Stream format.** The first frame carries a reconnect hint and a comment. Then one frame per
event: `id` is the event's position in the log, `event` is its type, and `data` is one JSON
object. A comment line is sent every 15 seconds while idle so proxies and idle timeouts keep the
connection open.

```
retry: 3000
: subscribed

id: 4812
event: object.written
data: {"event":"object.written","kb":"notes","object_key":"inbox/memo.mp3","etag":"\"9a3f…\"","size":48213011,"mime":"audio/mpeg","mtime":1757950000,"source":"http","occurred_at":"2026-09-15T14:33:20Z"}

id: 4813
event: object.deleted
data: {"event":"object.deleted","kb":"notes","object_key":"inbox/old.md","source":"webdav","occurred_at":"2026-09-15T14:33:21Z"}

: keep-alive
```

**Event schema.** The `data` object repeats the type under `event` so it is self-describing on its
own.

| Field | Present on | Description |
|-------|------------|-------------|
| `event` | both | `object.written` (create or modify — neither the API nor storage distinguish the two) or `object.deleted` |
| `kb` | both | Knowledge base slug |
| `object_key` | both | The key, without a leading slash |
| `etag` | `object.written` | The `ETag` of the bytes the event describes, quoted as in `HEAD` |
| `size` | `object.written` | Size in bytes |
| `mime` | `object.written` | Stored content type |
| `mtime` | `object.written` | Last-modified Unix timestamp, seconds |
| `source` | both | Which surface made or detected the change — see below |
| `occurred_at` | both | When the server published it, RFC 3339 UTC |

**Sources.** Every path that enqueues indexing work also publishes an event:

| `source` | Produced by |
|----------|-------------|
| `http` | `PUT`, `PATCH`, `POST …/replace/…` and `DELETE` on the HTTP API |
| `webdav` | WebDAV `PUT`, `DELETE`, `COPY` (destination) and `MOVE` — a `MOVE` is two events: the destination written, then the source deleted |
| `mcp` | MCP tool calls, which write through the HTTP API. The MCP server marks its requests with `X-NotedThat-Source: mcp`; the header is informational, any client may send it, and nothing is granted or refused on its account |
| `fs-watch` | The `fs` backend's watcher noticing a file written or removed in its tree by something other than NotedThat |
| `reconcile` | The `fs` backend's comparison of its tree against the index: at startup, after a directory-level change (a new or renamed folder's files arrive this way), or on a rescan |

**Ids and replay.** Ids are strictly increasing within a deployment and never reused. With the
`memory` backend they are a process-local counter; with `nats` they are the JetStream stream
sequence, which is global across replicas and across knowledge bases — a subscriber to one
knowledge base sees gaps where other knowledge bases' events sit, which is normal. A client that
reconnects with `Last-Event-ID` receives every event after that id that it may see, once, in
order, from whichever replica it lands on. A `Last-Event-ID` ahead of the log (a `memory`
backend that restarted and began counting again, or a recreated stream) is `410` as well: the
ids it names never existed in this log, and whatever was published since — on the `fs`
backend, the startup comparison's announcements of what changed while the server was down —
is exactly what the subscriber has missed, so it must resync by listing rather than resume
from "now".

**Delivery is at least once.** A write that stored its bytes but could not publish its event
answers `503 backend_unavailable` with `Retry-After: 5`; the write is idempotent, and the retry
publishes. A retry may therefore publish the same change twice. On the `fs` backend the startup
comparison re-announces objects the index does not track — anything non-indexable, such as
audio — on every restart, since nothing records that they were announced before. Subscribers
should be idempotent: compare `etag` with what they last processed, or check for the output they
would produce (the transcription worker below skips an mp3 whose `.md` already exists).

**Self-triggered loops.** A worker that writes into the prefix it watches sees its own writes.
Nothing suppresses that server-side; filter by `mime` (a transcription worker subscribes to
`mime=audio/*` and writes `text/markdown`) or by `prefix`, or compare `etag`.

**Naming.** `events` at the root of a knowledge base is a route, like `search`: an object stored
under exactly that key is not reachable through `GET /api/v1/knowledgebases/{kb_slug}/events`.

**Example** — transcribe every mp3 uploaded under `inbox/` and write the text back as a document
beside it (`examples/events/transcribe-mp3.sh` is the complete script):

```sh
curl -sN -H "Authorization: Bearer $NOTEDTHAT_API_TOKEN" \
  "http://localhost:8080/api/v1/knowledgebases/notes/events?prefix=inbox/&mime=audio/*" |
while IFS= read -r line; do
  case "$line" in
    data:*) key=$(printf '%s' "${line#data:}" | jq -r .object_key)
            # download, transcribe, then PUT "$key.md" as text/markdown
            ;;
  esac
done
```

**Behind nginx**, forward the route without buffering or a read timeout:

```nginx
location /api/v1/ {
    proxy_pass         http://notedthat:8080;
    proxy_http_version 1.1;
    proxy_buffering    off;      # also set by the X-Accel-Buffering: no response header
    proxy_read_timeout 0;        # the stream is idle between events; heartbeats every 15 s
}
```

---

## Full route summary

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| GET | `/healthz` | No | Liveness probe |
| GET | `/readyz` | No | Readiness probe |
| GET | `/llms.txt` | No | Plain-text navigation instructions for LLM clients: access rules, API, MCP, WebDAV |
| GET | `/.well-known/oauth-protected-resource` | No | RFC 9728 protected-resource metadata; `404` unless `NOTEDTHAT_OIDC_RESOURCE` is set |
| GET, HEAD | `/browse/`, `/browse/{path}` | Anonymous or Bearer | Server-rendered HTML directory listings |
| GET | `/api/v1/knowledgebases` | Yes | List declared KBs |
| GET | `/api/v1/knowledgebases/{kb_slug}` | Yes | List objects in a KB |
| HEAD | `/api/v1/knowledgebases/{kb_slug}/{path}` | Yes | Object metadata, no body |
| GET | `/api/v1/knowledgebases/{kb_slug}/{path}` | Yes | Download object; supports `Range`, conditional headers |
| PUT | `/api/v1/knowledgebases/{kb_slug}/{path}` | Yes | Upload or replace object; supports `If-Match`, `If-None-Match` |
| DELETE | `/api/v1/knowledgebases/{kb_slug}/{path}` | Yes | Delete object (idempotent); supports `If-Match` |
| PATCH | `/api/v1/knowledgebases/{kb_slug}/{path}` | Yes | Partial write; supports `Content-Range: bytes|lines` and `NT-Patch-Mode: append` |
| POST | `/api/v1/knowledgebases/{kb_slug}/replace/{path}` | Yes | String replace; exact UTF-8 substring find-and-replace under `If-Match` |
| POST | `/api/v1/knowledgebases/{kb_slug}/search` | Yes | Hybrid semantic search (RRF fusion) |
| GET | `/api/v1/knowledgebases/{kb_slug}/events` | Yes | Object change events as `text/event-stream`; `404` unless an events backend is configured |
| POST | `/mcp` | Bearer, or anonymous where a manifest grants `anyone` something (D59) | Streamable HTTP MCP, stateless JSON-response; `GET`/`DELETE` and `/sse` answer `405` |
| any | `/webdav/`… | Basic or Bearer, or anonymous where granted | WebDAV Class 1 share, one directory per knowledge base |


---

## WebDAV

NotedThat exposes a WebDAV read-write surface at `/webdav` on the same listener as the HTTP API
and MCP. Authentication uses HTTP Basic auth (`NOTEDTHAT_WEBDAV_USERNAME` /
`NOTEDTHAT_WEBDAV_PASSWORD`) or, as on every other surface, `Authorization: Bearer` (D53).

WebDAV is governed by the same [access rules](#manifest-access-rules) as the HTTP API, mapped onto
its own methods:

| Method | Verb |
| --- | --- |
| `PROPFIND` | `list` |
| `GET`, `HEAD` | `read` |
| `PUT`, `MKCOL`, `COPY` | `write` |
| `DELETE` | `delete` |
| `MOVE` | `write` at the destination, `delete` at the source |
| `OPTIONS` | none; it reports what the others allow |

`PROPFIND` maps to `list` at every depth, including `Depth: 0` on a single file, where the HTTP
API's `HEAD` maps to `read`. `PROPFIND` returns properties and never bytes, and a `list` grant
already exposes a child's size, etag and mtime — so requiring `read` at `Depth: 0` would produce a
listing whose own entries refused to describe themselves.

`OPTIONS` reports the methods allowed for that caller at that path, so it now narrows for a
restricted credential as well as for an anonymous one. `.notedthat` stays hidden from anonymous
reads and `PROPFIND` responses. Supplying invalid Basic credentials returns `401` with a Basic
challenge rather than falling back to anonymous access; a valid credential the rules do not grant
returns `403`. An unrecognised method is treated as a write, so an unauthenticated `PROPPATCH`
answers `401` rather than advertising which methods are unimplemented.

### Path normalization and traversal rejection

All WebDAV methods (GET, HEAD, PROPFIND, PUT, DELETE, MOVE, COPY) reject requests
whose URI path contains `.`, `..`, empty segments (from `//`), or percent-encoded
equivalents (`%2e%2e`, `%2f`, `%5c`) with **400 Bad Request**. This applies uniformly
to both read and write methods, closing the traversal path described in D40. A single
trailing `/` on a collection path is permitted (e.g. `PROPFIND /notes/folder/` returns
207). All other traversal or path-injection shapes return 400 with no response body.

| Shape | Example URI | GET | HEAD | PROPFIND | PUT | DELETE | MOVE/COPY |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `..` (dot-dot) | `/notes/../secret.md` | 400 | 400 | 400 | 400 | 400 | 400 |
| `%2e%2e` (encoded `..`) | `/notes/%2e%2e/secret.md` | 400 | 400 | 400 | 400 | 400 | 400 |
| `.` (single dot) | `/notes/./hello.md` | 400 | 400 | 400 | 400 | 400 | 400 |
| empty segment | `/notes//hello.md` | 400 | 400 | 400 | 400 | 400 | 400 |
| `%2f` (encoded `/`) | `/notes/foo%2fbar.md` | 400 | 400 | 400 | 400 | 400 | 400 |
| `%5c` (encoded `\`) | `/notes/foo%5cbar.md` | 400 | 400 | 400 | 400 | 400 | 400 |

See `SPECIFICATIONS.md` D40 for the full normative path validation rules.

### URL layout

| Path | Meaning |
|------|---------|
| `/webdav/` | Virtual root — lists all declared knowledge bases |
| `/webdav/{kb}/` | Knowledge base root — lists objects in the KB |
| `/webdav/{kb}/{path}` | Object — multi-segment paths are native (unlike the HTTP API's percent-encoded single segment) |

### Supported methods

| Method | Status codes | Notes |
|--------|-------------|-------|
| `OPTIONS` | 204 | `DAV: 1` (Class 1 only). `Allow` header lists all supported methods. |
| `HEAD` | 200 | Returns metadata without body. |
| `GET` | 200, 206 | Supports `Range` header for partial content. |
| `PROPFIND` | 207 | Depth 0 and 1 supported. **Depth: infinity returns 501** (v1 limitation — see below). |
| `PUT` | 201 (create), 204 (overwrite) | Returns `ETag`. Supports `If-Match` / `If-None-Match`. MIME sniff applies. |
| `DELETE` | 204 | Idempotent — deleting a non-existent object returns 204. |
| `MKCOL` | 201 | Creates a virtual folder. See v1 quirks below. |
| `MOVE` | 201 (new dest), 204 (overwrite) | Single-object only. Same KB only. |
| `COPY` | 201 (new dest), 204 (overwrite) | Single-object only. Same KB only. |

### Rejected methods

| Method | Status | Reason |
|--------|--------|--------|
| `LOCK` | 405 | No lock system in v1 (D17). Finder and Office require LOCK to save — see Known-broken clients. |
| `UNLOCK` | 405 | Same as LOCK. |
| `PROPPATCH` | 405 | No custom DAV properties in v1. |
| Collection `MOVE` / `COPY` | 403 + `<nt:no-collection-move/>` | S3 has no atomic collection rename. |
| Cross-KB `MOVE` / `COPY` | 403 + `<nt:cannot-modify-source/>` | KBs are isolated storage namespaces. |
| Cross-server `MOVE` / `COPY` | 502 + `<nt:destination-different-server/>` | Per RFC 4918 §9.9.2. |

### WebDAV custom error conditions

NotedThat emits custom WebDAV error conditions under the **custom XML namespace URI** `urn:notedthat:error` (used as an XML namespace identifier only; the `notedthat` NID is not registered as a formal URN namespace with IANA per RFC 8141 — RFC 4918 §16 requires only that the namespace be non-`DAV:`). Condition names are in the `nt:` prefix bound to this namespace.

Current custom conditions:

| Condition | HTTP status | Trigger |
|-----------|-------------|---------|
| `nt:destination-different-server` | 502 | MOVE/COPY Destination header points to a different server |
| `nt:cannot-modify-source` | 403 | MOVE/COPY would modify the source KB, which is read-only |
| `nt:no-collection-move` | 403 | MOVE of a collection (directory) is not supported |
| `nt:propfind-too-large` | 507 | PROPFIND enumeration would exceed the 10 000-object v1 cap |

### v1 quirks

- **MKCOL is a no-op**: `curl -X MKCOL http://127.0.0.1:8080/webdav/notes/newfolder/` returns 201, but the
  empty folder does not persist across PROPFIND until a file is written into it. S3 has no directory
  primitive; folders are virtual prefixes derived from object keys.

### MIME sniff behaviour

The content type stored with an object is determined as follows:

1. If the request provides a `Content-Type` header that is NOT `application/octet-stream`, that
   value is used verbatim.
2. Otherwise (header absent or `application/octet-stream`), the extension is used:
   - `.md` / `.markdown` → `text/markdown`
   - Anything else → `application/octet-stream`

This ensures that WebDAV clients that send `application/octet-stream` for `.md` files (rclone
default, Finder default) still get indexed correctly by the M5 semantic search indexer.

Note: the `getcontenttype` property in PROPFIND responses is generated by dav-server from the file
extension and may differ from the stored content type. This is a v1 characteristic.

### Filename Percent-Decoding

WebDAV write methods (PUT, DELETE, MOVE, COPY) percent-decode each URL path segment individually before object key validation. This ensures that WebDAV clients can use percent-encoding to represent special characters in filenames, and that the stored object key matches the decoded filename.

#### Per-segment decoding semantics

The URL path is split on `/` first (raw path), then each individual segment is percent-decoded using strict UTF-8. This matches `dav-server`'s read behavior (GET, PROPFIND).

**Example**: `PUT /notes/Untitled%201.canvas` decodes the segment `Untitled%201.canvas` to `Untitled 1.canvas` and stores the object with key `Untitled 1.canvas`.

#### Rejected decoded values (400 Bad Request)

The following decoded values are rejected:

- **Empty segment** — e.g., double slash `/notes//file.md`
- **Decoded forward-slash inside a segment** — e.g., `%2F` (the `/` separator must remain unencoded)
- **Segments equal to `.` or `..`** — e.g., `%2E%2E` (path traversal prevention)
- **Decoded backslash `\`** — e.g., `%5C` (platform compatibility)
- **Decoded NUL byte** — e.g., `%00` (filesystem safety)
- **Non-UTF-8 percent sequences** — e.g., `%FF%FE` (strict UTF-8 validation)
- **Double leading slash** — e.g., `//notes/file.md` (path normalization)

#### Allowed decoded values

The following are allowed and will be stored in the object key:

- **Spaces** — `%20` decodes to a literal space in the filename
- **Unicode characters** — any valid UTF-8 sequence (e.g., `%C3%A9` for `é`)
- **Hash symbol** — `%23` decodes to `#`
- **Question mark** — `%3F` decodes to `?`
- **Literal percent sign** — `%25` decodes to `%`
- **Other reserved characters** — `@`, `:`, `!`, `$`, `&`, `'`, `(`, `)`, `*`, `+`, `,`, `;`, `=`

#### Round-trip guarantee

A file named `Untitled 1.canvas` that a WebDAV client encodes as `Untitled%201.canvas` in the URL is stored and listed as `Untitled 1.canvas` (the decoded name), never as `Untitled%201.canvas`. This ensures that the object key in LIST responses matches what the client expects.

#### Query parameters

Characters after `?` in the URL are query parameters and are never part of the object key. Example: `PUT /notes/file%3Fname.md?ignored=1` stores the object with key `file?name.md` (the query string is ignored).

#### Fragment handling

`Destination` headers containing `#` (fragment) are rejected with 400 Bad Request. Fragments are not transmitted in normal HTTP requests and are not supported in WebDAV operations.

### Depth: infinity limitation

`PROPFIND` with `Depth: infinity` returns `501 Not Implemented`. This is a v1 limitation:
dav-server v0.11 hardcodes 501 for infinity depth, and the underlying `Storage::list_objects()`
has no continuation cursor (deferred to post-v1 per D41). Implementing recursive listing without
a cursor would silently truncate at 1000 objects per KB, which is worse than an honest 501.

A follow-up ticket will add proper infinity depth when the D41 cursor ships.

### Known-broken clients

The following clients require `LOCK` support to save files, which NotedThat does not provide in v1:

- **macOS Finder** — requires LOCK for save operations. Read-only mount works.
- **Microsoft Office** — requires LOCK for save operations.
- **Some mobile Files apps** — behaviour varies.

**Clients that work for read-write on Class 1:**
- GVFS / Nautilus on Linux
- WinSCP
- rclone (WebDAV backend)
- cadaver (command-line)
- curl

### Upgrade notes (M5 → M6)

Operators upgrading from M5 must set `NOTEDTHAT_WEBDAV_USERNAME` and `NOTEDTHAT_WEBDAV_PASSWORD`
before restarting. The server exits with a non-zero status and a descriptive error message if either
is missing or empty. See [CONFIGURATION.md](CONFIGURATION.md) for details.

### Upgrade notes (separate listeners → unified listener)

The API, WebDAV, and streamable MCP surfaces now share the one listener bound to
`NOTEDTHAT_LISTEN_ADDR`. This is a breaking change for both clients and operators.

**API routes moved.** Every `/v1/...` route is now `/api/v1/...`. There are no aliases and no
redirects: a client still calling `/v1/knowledgebases` receives `404`. Update clients, scripts, and
reverse-proxy rules before restarting. Health probes (`/healthz`, `/readyz`) and `/llms.txt` are
unchanged at the root.

**Three environment variables were removed, and the server refuses to start while any of them is
still set** (D39 fail-fast; the error names the replacement):

| Removed variable | Replacement |
|---|---|
| `NOTEDTHAT_WEBDAV_LISTEN_ADDR` | WebDAV is always served at `/webdav` on `NOTEDTHAT_LISTEN_ADDR` |
| `NOTEDTHAT_MCP_HTTP_BIND` | MCP HTTP is always served at `/mcp` on `NOTEDTHAT_LISTEN_ADDR` |
| `NOTEDTHAT_MCP_HTTP_ENABLED` | MCP HTTP is always served at `/mcp` on `NOTEDTHAT_LISTEN_ADDR` |

Startup fails rather than ignoring these because ignoring them silently widens network exposure.
An operator who bound WebDAV to `127.0.0.1` while the API listened on `0.0.0.0` would find WebDAV
publicly reachable at `/webdav`, and an operator who set `NOTEDTHAT_MCP_HTTP_ENABLED=false` would
find `/mcp` mounted. Authentication still applies to both surfaces — Basic or Bearer for WebDAV;
Bearer, or the anonymous caller where the manifests admit one, plus Host/Origin validation for MCP
— but the reachable network surface changes, so the decision is returned to the operator instead
of being made silently.

**Reverse proxies and containers.** Forward every route to the single application port; the
container now exposes only `8080`. Terminate TLS once, in front of that port. Per-surface proxy
rules that pointed at the old WebDAV or MCP ports must be removed.

**`/browse` is a live surface.** Forward it like any other route. Its responses carry
`Cache-Control: no-store` and `Vary: Authorization`, because anonymous and credentialed callers
share a URL and see different pages — do not configure a proxy cache that ignores either header.

**`/api/v1/knowledgebases/{kb_slug}/events` is a long-lived stream.** It never finishes on its
own, so a proxy must neither buffer it nor time it out on read. The response carries
`Cache-Control: no-cache` and `X-Accel-Buffering: no` (which nginx honours by itself); for
other proxies, disable response buffering and the read timeout on that route, and forward it
over HTTP/1.1 or HTTP/2 rather than HTTP/1.0. See
[the endpoint](#get-apiv1knowledgebaseskb_slugevents) for the nginx snippet.

---

## MCP

The MCP (Model Context Protocol) surface wraps the HTTP API described above. It is a thin proxy: every MCP tool call translates to one or more HTTP API requests. The MCP layer **never** accesses storage or the index directly (§5 principle 10).

### Transports

NotedThat supports two MCP transports: stdio (M7) and streamable HTTP (M8).

#### stdio transport

MCP is served by the `notedthat-mcp-stdio` binary over **stdio**. Configure your MCP client to launch the binary as a subprocess; see [Connecting clients](CLIENTS.md) for setup snippets.

#### Streamable HTTP transport

MCP is also available over HTTP at `POST /mcp` on the unified listener. It is always mounted with
the API and WebDAV.

**Endpoint:** `POST /mcp`

**Authentication:** Bearer token, same credentials as the HTTP API — the service token or an
identity token:

```
Authorization: Bearer <NOTEDTHAT_API_TOKEN or identity token>
```

MCP acts as its caller: the bearer presented to `/mcp` is the bearer the server's own API calls
carry, so a tool call is bound by exactly the rules a direct request would be, and a refusal
surfaces as a `forbidden` tool error. An unverifiable bearer — or a `Basic` credential, which is
WebDAV's — is `401` with a JSON body and never a quiet downgrade to anonymous access.

**Anonymous callers (D59).** A request with no `Authorization` header at all is admitted as the
anonymous caller when at least one declared knowledge base grants `anyone` some verb, unless the
operator set [`NOTEDTHAT_MCP_ANONYMOUS=never`](CONFIGURATION.md#mcp-http-listener). The loopback
API call then carries no credential either, so the manifests' `anyone` rules decide exactly as they
would for a direct anonymous request: `initialize` and `tools/list` succeed and advertise all ten
tools; `list_knowledgebases` names only the knowledge bases in which `anyone` holds a grant; `list`,
`read` and `search` work where `anyone` holds that verb under the matching patterns; a denial is a
`not_found` tool error — the API's concealed `404`, so the answer cannot enumerate private
knowledge bases; and a mutating tool (`write`, `edit`, `append`, `replace`, `move`, `delete`) is an
`unauthorized` tool error, since no anonymous caller may ever write. `resources/list` needs `list`
in every visible knowledge base and fails whole otherwise, for anonymous and signed-in callers
alike.

When no knowledge base grants `anyone` anything, or under `never`, a missing bearer is `401` with
a JSON body plus a `WWW-Authenticate: Bearer resource_metadata="…"` challenge when the deployment
publishes RFC 9728 metadata — which is how an OAuth-capable MCP client finds the authorization
server and runs the authorization-code flow against it. That is the reason `never` exists: on a
deployment with public knowledge bases *and* an identity provider, an OAuth client that connects
without a token is simply admitted as anonymous and never prompted to sign in; `never` restores
the prompt at the cost of anonymous MCP. The startup log line `MCP_ANONYMOUS` says which case a
deployment is in.

The server operates in **stateless JSON-response mode**: each `POST /mcp` request is a complete, self-contained JSON-RPC exchange. No session state is retained between requests.

**SSE refusal:** The legacy SSE transport is not supported. The following requests return `405 Method Not Allowed`:

- `GET /mcp`
- `DELETE /mcp`
- `POST /sse`
- `GET /sse` and any path under `/sse/`

The 405 response body is always:

```json
{"error":"transport_not_supported","message":"Legacy SSE transport is not supported. Use streamable HTTP at POST /mcp"}
```

**HTTPS requirement:** Bearer tokens must not travel over plaintext HTTP on untrusted networks.
Terminate TLS at one reverse proxy (nginx, Caddy, Traefik) before exposing the unified listener to
the internet, and forward all routes to it. Bearer over plaintext HTTP is only acceptable on
loopback or private trusted links (e.g., `127.0.0.1` in a local dev setup).

### Tools

All 10 tools are exposed:

#### `list_knowledgebases`

List the knowledge bases visible to the caller, with what each one is for.

**Arguments**: none

**Response**: `[{ "kb_slug": string, "display_name": string, "description"?: string }]` — the
entries of [`GET /api/v1/knowledgebases`](#get-apiv1knowledgebases), unchanged. `description` is
present only when the manifest sets one; read it before choosing which knowledge base to `search`.

Note: `perms` (§6.10) is post-v1.

#### `search`

Hybrid semantic + keyword search across one or more knowledge bases in a single call.

**Arguments**: `kb?` (array of slugs), `query` (string), `filters?` (object), `limit?` (u32). `filters` takes exactly the HTTP body's [`filter` fields](#post-apiv1knowledgebaseskb_slugsearch): `object_key_prefix`, `mime`, `concept_type`, `heading_path_prefix`, `updated_after`, `updated_before`, `tags` — AND-composed, each with a description in the tool's input schema. An unknown argument or filter key is refused **before the tool runs**: rmcp fails to deserialize the arguments and answers the way MCP reports a tool failure — a result with `isError: true` whose text is rmcp's message naming the key and the accepted ones (`failed to deserialize parameters: unknown field `filter`, expected one of `kb`, `query`, `filters`, `limit``). It is not a JSON-RPC error and does not carry the tool's `invalid_request:` prefix, so a client matching on that string will not see it. `filter` (singular, the HTTP body's spelling) is the likeliest slip.

- `kb` is always a list, even for one knowledge base: `["whatwg"]`. A bare string is rejected. Slugs come from `list_knowledgebases`; listing a slug twice is `invalid_request`.
- Omit `kb` (or pass `[]`) to search every knowledge base the caller can see — the same set `list_knowledgebases` returns.
- `limit` is **per knowledge base** (default 10, maximum 50), not a cap on the whole request.

The tool fans out to one `POST /api/v1/knowledgebases/{kb_slug}/search` per slug, at most 8 in flight at once, as the calling identity; the HTTP route itself stays single-slug.

**Response**: one group per knowledge base, in request order (or the listing's order when `kb` was omitted), each keeping that knowledge base's own ranking. Every hit names its knowledge base so a flattened list stays unambiguous.

```json
{
  "results": [
    { "kb": "whatwg", "hits": [ { "kb": "whatwg", "object_key": "dom.md", "byte_start": 0, "byte_end": 1024, "score": 0.5, "preview": "…" } ] },
    { "kb": "odf",    "hits": [ { "kb": "odf",    "object_key": "v1.3/part2-packages.md", "byte_start": 0, "byte_end": 900, "score": 0.5, "preview": "…" } ] }
  ],
  "skipped": []
}
```

Each hit otherwise has the HTTP `SearchHit` fields: `object_key`, `byte_start`, `byte_end`, `heading_path`, `score`, `preview`, and optional `okf` concept metadata. See [OKF support](OKF.md) for the metadata shape and upgrade steps.

**No cross-knowledge-base ranking.** The groups are not merged and no request-wide order is implied. `score` is a reciprocal-rank fusion value computed inside one knowledge base's collection (see *Score semantics* above), so every knowledge base's top hit scores the same whatever its relevance; a merged sort would be an arbitrary interleave, not a ranking.

**Failures.** With an explicit `kb` list, any knowledge base that cannot be searched fails the whole call — a slug the server does not declare is `not_found`, one the caller's access rules deny is `forbidden` — and the error message names the slug, e.g. `forbidden (knowledge base "hr")`. With `kb` omitted, a knowledge base that is visible but refuses search (`forbidden`, or the concealed `not_found`) is dropped and its slug listed under `skipped`, so the caller can see coverage; any other failure (`invalid_request`, `backend_unavailable`, a transport error) still fails the whole call and names the knowledge base. `skipped` is always present and always empty for an explicit list.

#### `read`

Read an object, whole or by byte range or line range.

**Arguments**: `kb` (string), `path` (string), and at most one of:

- `byte_start?` (u64, inclusive), `byte_end?` (u64, **exclusive**) — a byte slice
- `line_start?` (u64, 1-based inclusive), `line_end?` (u64, 1-based inclusive) — a line slice; `line_end = line_start - 1` names an insert point and returns an empty slice

**Response**: the text in `content[0].text`, and in `structuredContent` the same text under `text` beside the read's own metadata — from the same HTTP response as the text, never from a separate `list` or `HEAD`, whose answer could describe a newer version. The text is in both halves on purpose: MCP asks that `structuredContent` and `content` be functionally equivalent, and a host that hands the model only the structured half (some do once a tool declares an output schema) must still hand it the document.

```json
{
  "text": "# Title\n\n…",
  "etag": "\"3a2f…\"",
  "content_type": "text/markdown",
  "bytes_returned": 150,
  "total_bytes": 400,
  "byte_start": 0,
  "byte_end": 150,
  "total_lines": 20,
  "line_start": 1,
  "line_end": 5
}
```

| Field | Meaning |
|-------|---------|
| `text` | The text read — identical to `content[0].text`. |
| `etag` | The version the text belongs to, verbatim (quoted). Pass it as `if_match` to `edit` or `replace`; a concurrent change is then `precondition_failed` rather than overwritten. |
| `content_type` | The object's content type. |
| `bytes_returned` | Bytes in `content` — the slice, not the object. |
| `total_bytes` | The whole object's size. |
| `byte_start`, `byte_end` | The returned slice, `byte_end` exclusive. `byte_start` and `total_bytes` come from the header; `byte_end` is `byte_start + bytes_returned`, from the body, because a header's inclusive end cannot spell the empty slice at offset 0 (an insert point before line 1 arrives as `0-0/N`). A full read is `0` and `total_bytes`, set even when the response carried no `Content-Length`. |
| `total_lines`, `line_start`, `line_end` | Line reads only: the object's line count and the returned lines (1-based, inclusive; `line_end = line_start - 1` for an insert point). `null` on byte and full reads. |

Every field but `bytes_returned` is `null` when the backend did not say — nothing is invented. The tool declares this shape as its `outputSchema`.

**Byte range semantics**: MCP uses zero-based exclusive `byte_end`, while the HTTP `Range` header uses inclusive bounds. The MCP layer converts automatically: `byte_end=10` → `Range: bytes=0-9`.

**Constraints**: `byte_end` requires `byte_start`; `byte_start >= byte_end` is rejected with `invalid_request`. `line_end` requires `line_start`; `line_start` is at least 1; `line_end < line_start - 1` is rejected. Byte and line arguments together are rejected.

**Read budget**: an object larger than `NOTEDTHAT_MCP_MAX_READ_BYTES` (default 16 MiB — the API's own body cap, so anything written through the API reads back whole) is refused with `response_too_large`, whose message states the object's size and the budget and names the slice arguments to use instead. A slice within the budget is served. Objects that large arrive over WebDAV or straight into an `fs` tree, never through the API.

#### `write`

Create or update an object. Content is UTF-8 text in v1 (binary not supported).

**Arguments**: `kb` (string), `path` (string), `content` (string), `if_match?` (string ETag), `if_none_match?` (string), `mime_type?` (string)

**Response**: `{ "etag": string|null, "location": string|null }`

#### edit

Edit an object by replacing or inserting a line range.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `kb` | string | Yes | Knowledge base slug |
| `path` | string | Yes | Object path |
| `line_start` | integer | Yes | First line to replace (1-based inclusive) |
| `line_end` | integer | Yes | Last line to replace (1-based). Set to `line_start - 1` for an insert point. |
| `content` | string | Yes | Replacement content |
| `if_match` | string | Yes | ETag from a prior read (required for OCC) |

Returns: `{ "etag": "<new-etag>", "location": "<path>" }`

---

#### append

Append content to the end of an object — single round-trip, no prior `HEAD` needed.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `kb` | string | Yes | Knowledge base slug |
| `path` | string | Yes | Object path |
| `content` | string | Yes | Content to append |
| `if_match` | string | No | Optional ETag; server obtains it internally when omitted |

Returns: `{ "etag": "<new-etag>", "location": "<path>" }`

---

#### `replace`

Replace an exact UTF-8 substring within an object — server-side find-and-replace without uploading the full body.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `kb` | string | Yes | Knowledge base slug |
| `path` | string | Yes | Object path |
| `old_string` | string | Yes | Exact UTF-8 byte sequence to find. Must be non-empty. |
| `new_string` | string | Yes | Replacement text. May be an empty string. |
| `if_match` | string | Yes | ETag from a prior read (required for OCC) |
| `replace_all` | boolean | No | Default `false`. When `false`, exactly one match required; zero → `no_match` error, multiple → `ambiguous_match` error. When `true`, all non-overlapping occurrences replaced left-to-right. |

Returns: `{ "etag": "<new-etag>", "match_count": N, "total_bytes": M }`

---

#### `list`

List objects in a knowledge base under an optional prefix.

**Arguments**: `kb` (string), `prefix?` (string), `limit?` (u32, max 1000), `cursor?` (string)

**Response**: `{ "objects": [ObjectMeta], "truncated": bool, "cursor"?: string }`

#### `delete`

Delete an object. Idempotent — deleting a non-existent object returns success.

**Arguments**: `kb` (string), `path` (string), `if_match?` (string ETag)

**Response**: text confirmation

#### `move`

Move/rename an object within a knowledge base. **Non-atomic** in v1: implemented as GET source → PUT destination → DELETE source. If DELETE fails after PUT succeeds, a descriptive error is returned and the source must be removed manually.

**Arguments**: `kb` (string), `from` (string), `to` (string), `if_match?` (string ETag on source)

**Response**: text confirmation or partial-failure error

The copy runs through the MCP process, so the [read budget](#read) bounds a move as well: an object
over `NOTEDTHAT_MCP_MAX_READ_BYTES` is refused (`invalid params`, message `move: …`) before anything is
written or deleted, and the message says what to do — raise the budget, or move it over WebDAV,
whose `MOVE` is server-side. (Sliced reads are no help here; there is no sliced move.)

### Resources

NotedThat exposes MCP Resources so clients can browse and read objects without calling tools directly. Resources are advertised in the `initialize` response under `capabilities.resources`. There is no `subscribe` or `listChanged` support in v1.

#### URI scheme

```
notedthat://<kb_slug>/<percent-encoded object_key>
```

Examples:

```
notedthat://notes/hello.md
notedthat://notes/2024%2Fjanuary%2Fmeeting-notes.md
notedthat://scratch/Untitled%201.canvas
```

The object key is percent-encoded exactly once. The `/` separator within a key is encoded as `%2F`.

#### Listing resources

`resources/list` returns a flat listing across all declared knowledge bases. Results are paginated using an opaque base64 cursor that tracks position across KB boundaries. Pass the returned `nextCursor` value back as the `cursor` parameter on the next call. The server never silently truncates: if more results exist, `nextCursor` is always present.

`Resource.name` equals the object key. No `title`, `description`, or annotations are set.

#### Reading resources

`resources/read` accepts `{ "uri": "notedthat://..." }` and returns the object contents:

- **Text resources** (valid UTF-8): `TextResourceContents` with the detected MIME type.
- **Binary resources** (non-UTF-8 bytes): `BlobResourceContents` with base64-encoded data and `mimeType: "application/octet-stream"`.

MIME detection by extension:

| Extension | MIME type |
|-----------|-----------|
| `.md`, `.markdown` | `text/markdown` |
| `.txt` | `text/plain` |
| `.png` | `image/png` |
| `.jpg`, `.jpeg` | `image/jpeg` |
| anything else | `application/octet-stream` |

**Byte ranges** are not available through `resources/read` (which takes only `{ uri }`). Use the `read` tool with `byte_start` and `byte_end` arguments to fetch a slice of an object.

**Read budget**: an object larger than `NOTEDTHAT_MCP_MAX_READ_BYTES` (default 16 MiB) is refused with `response_too_large`, which points at the `read` tool's slice arguments. The budget is on the bytes fetched; a binary resource is base64-encoded on top of that, so its `blob` is about four thirds of the budget at most.

### Path Encoding

Object paths are percent-encoded per RFC 3986 before being placed in URLs. The `/` separator within a path is encoded as `%2F`. Example: `docs/rfc/7231.md` → `docs%2Frfc%2F7231.md`.

### Error Codes

All MCP errors carry one of these codes:

| Code | HTTP Status | Meaning |
|------|-------------|---------|
| `invalid_request` | 400 | Bad arguments or validation failure |
| `unauthorized` | 401 | The API refused the call's credential — over HTTP, an anonymous caller invoking a mutating tool, since a bad bearer never reaches a tool |
| `forbidden` | 403 | The credential is valid but the knowledge base's access rules do not grant this operation on this key |
| `not_found` | 404 | Knowledge base or object not found (including a declared knowledge base whose bucket is gone) — or, for an anonymous caller, a knowledge base or key the `anyone` rules do not grant, concealed as the same `404` |
| `precondition_failed` | 412 | ETag mismatch (If-Match / If-None-Match) |
| `payload_too_large` | 413 | Content exceeds server limit (16 MiB) |
| `range_not_satisfiable` | 416 | Byte range beyond object size |
| `response_too_large` | — | The object exceeds `NOTEDTHAT_MCP_MAX_READ_BYTES`; the message names the object's size, the budget and the `read` tool's slice arguments. Raised by the MCP server itself, not by the API. |
| `backend_unavailable` | 503 | S3 or Qdrant unavailable |
| `internal_error` | 500 | Unexpected server error |

### v1 Limitations

- **Non-atomic MOVE**: GET → PUT → DELETE; partial failure is possible
- **No `display_name`/`description`/`perms`** on `list_knowledgebases` responses (HTTP list endpoint v1 limitation)
- **No subscribe or listChanged**: Resources capability is advertised without these fields; clients must poll `resources/list` for updates
- **`tools/list` is static**: an anonymous caller is shown the mutating tools too and learns they are refused only by calling one; grants are per knowledge base and per path, so a filtered list would be a false signal anyway
