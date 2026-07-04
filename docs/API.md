# NotedThat HTTP API

NotedThat exposes a REST-style HTTP API for reading and writing objects stored in S3-compatible
object storage. The API surface is intentionally small: health probes, a knowledge-base list, and
four operations on objects (list, head, get, put, delete). Every route returns JSON for structured
responses and plain bytes for object bodies.

## Base URL and versioning

All data-plane routes are prefixed with `/v1/`. Health probes (`/healthz`, `/readyz`) sit at the
root with no version prefix.

```
http://HOST:PORT/v1/knowledgebases/...
http://HOST:PORT/healthz
```

The default listen address is `0.0.0.0:8080`. Override it with `NOTEDTHAT_LISTEN_ADDR`.

## Authentication

All `/v1/` routes require a Bearer token. Pass it in the `Authorization` header:

```
Authorization: Bearer <token>
```

The token is compared against `NOTEDTHAT_API_TOKEN` using a constant-time comparison. There is no
token rotation, no scopes, and no per-KB access control in v1. Either you have the token or you
don't.

Health probes (`/healthz`, `/readyz`) do **not** require authentication.

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
curl -H "Authorization: Bearer $TOKEN" http://localhost:8080/v1/knowledgebases
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
| 400 | `invalid_request` | Malformed path, invalid KB slug, malformed `Range` header, or other bad input |
| 401 | `unauthorized` | Missing or invalid `Authorization` header |
| 404 | `not_found` | KB slug not declared, or object does not exist |
| 412 | `precondition_failed` | `If-Match` mismatch or `If-None-Match`/`If-Unmodified-Since` condition not met |
| 413 | `payload_too_large` | PUT body exceeds 16 MiB |
| 416 | `range_not_satisfiable` | Requested byte range is out of bounds |
| 500 | `internal_error` | Unexpected server error |
| 503 | `backend_unavailable` | S3 backend unreachable or returned an error |

## Limits

- **Body size:** PUT requests are rejected if the body exceeds **16 MiB** (16,777,216 bytes). The
  check happens on `Content-Length` before reading the body, and again after buffering. Requests
  without `Content-Length` are still capped at 16 MiB during body collection.
- **List default:** 100 objects per call.
- **List maximum:** 1,000 objects per call (pass `?limit=1000`).
- **Pagination:** Use the `next_cursor` field from the response as the `?cursor=` query parameter on the next request to fetch subsequent pages.

---

## Byte-range requests

Clients can request a partial object body by including a `Range` header:

| Request header | Effect |
|---|---|
| `Range: bytes=0-499` | Returns first 500 bytes |
| `Range: bytes=500-` | Returns from byte 500 to end |
| `Range: bytes=-500` | Returns last 500 bytes |
| `Range: bytes=0-499,1000-1499` | Multi-range (forwarded to S3 backend as-is) |

**Responses:**
- **206 Partial Content** — successful partial read; includes `Content-Range: bytes start-end/total`
- **416 Range Not Satisfiable** — requested range is out of bounds; response includes `Content-Range: bytes */total`
- **400 Bad Request** — malformed `Range` header (unparseable syntax)
- **200 OK** — unknown range unit (e.g., `items=0-10`) is silently ignored per RFC 7233 §2.1; full object returned

**curl example:**

```sh
# Request first 100 bytes
curl -H "Authorization: Bearer $TOKEN" \
     -H "Range: bytes=0-99" \
     http://localhost:8080/v1/knowledgebases/notes/hello.md

# Response: HTTP/1.1 206 Partial Content
# Content-Range: bytes 0-99/1234
# Content-Length: 100
```

---

## ETag response header

GET, HEAD, and PUT responses include an `ETag` header when the backend provides one:

- The ETag is opaque and strong (per RFC 7232 §2.3), wrapped in double quotes: `"abc123"`
- The value is provided by the S3 backend and forwarded verbatim — NotedThat does not synthesize ETags
- Use the ETag with conditional request headers to implement optimistic concurrency control

```sh
curl -sI http://localhost:8080/v1/knowledgebases/notes/hello.md \
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
curl -sI http://localhost:8080/v1/knowledgebases/notes/hello.md \
     -H "Authorization: Bearer $TOKEN" \
     -H 'If-None-Match: "abc123"'
# HTTP/1.1 304 Not Modified (if ETag matches)
# HTTP/1.1 200 OK (if ETag has changed)

# PUT: only overwrite if ETag matches (optimistic lock)
curl -sI -X PUT http://localhost:8080/v1/knowledgebases/notes/hello.md \
     -H "Authorization: Bearer $TOKEN" \
     -H "Content-Type: text/markdown" \
     -H 'If-Match: "abc123"' \
     --data-binary "updated content"
# HTTP/1.1 201 Created (if ETag matched)
# HTTP/1.1 412 Precondition Failed (if ETag didn't match)

# PUT: only create if object doesn't exist
curl -sI -X PUT http://localhost:8080/v1/knowledgebases/notes/new.md \
     -H "Authorization: Bearer $TOKEN" \
     -H "Content-Type: text/markdown" \
     -H 'If-None-Match: *' \
     --data-binary "brand new"
# HTTP/1.1 201 Created (if object didn't exist)
# HTTP/1.1 412 Precondition Failed (if object already exists)

# DELETE: only delete if ETag matches
curl -sI -X DELETE http://localhost:8080/v1/knowledgebases/notes/hello.md \
     -H "Authorization: Bearer $TOKEN" \
     -H 'If-Match: "abc123"'
# HTTP/1.1 204 No Content (if ETag matched)
# HTTP/1.1 412 Precondition Failed (if ETag didn't match)
```

---

## Backend compatibility

NotedThat forwards Range and conditional headers verbatim to the S3 backend. Actual behavior
depends on the backend's RFC 7232/7233 support.

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
- **`multipart/byteranges` response bodies** — multi-range requests are forwarded to S3, but
  NotedThat does not parse or synthesize `multipart/byteranges` responses
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

Readiness probe. Returns `200 OK` (static response; no backend connectivity check in v1).

**Authentication:** Not required.

**Response:**

| Status | Body |
|--------|------|
| 200 OK | `{"status": "ok"}` |

**Example:**

```sh
curl http://localhost:8080/readyz
```

**Response body:**

```json
{"status": "ok"}
```

---

### GET /v1/knowledgebases

List all knowledge bases declared in `NOTEDTHAT_KBS`. Returns their slugs in sorted order.

**Authentication:** Required.

**Response:**

| Status | Body |
|--------|------|
| 200 OK | `{"knowledgebases": ["slug1", "slug2"]}` |

The array contains the slug strings exactly as declared in `NOTEDTHAT_KBS`, sorted
lexicographically.

**Example:**

```sh
curl -H "Authorization: Bearer $TOKEN" \
     http://localhost:8080/v1/knowledgebases
```

**Response body:**

```json
{
  "knowledgebases": ["notes", "scratch"]
}
```

---

### GET /v1/knowledgebases/{kb_slug}

List objects in a knowledge base. Supports optional prefix filtering and a result limit.

**Authentication:** Required.

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
     "http://localhost:8080/v1/knowledgebases/notes"
```

**With prefix and limit:**

```sh
curl -H "Authorization: Bearer $TOKEN" \
     "http://localhost:8080/v1/knowledgebases/notes?prefix=2024/&limit=50"
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
  "https://example.com/v1/knowledgebases/notes?limit=100"
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
  "https://example.com/v1/knowledgebases/notes?limit=100&cursor=CgBkb2MtMDA5OS5tZA=="
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

### HEAD /v1/knowledgebases/{kb_slug}/{path}

Check whether an object exists and retrieve its metadata without downloading the body.

**Authentication:** Required.

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
     http://localhost:8080/v1/knowledgebases/notes/hello.md
```

---

### GET /v1/knowledgebases/{kb_slug}/{path}

Download an object. Returns the raw bytes with appropriate `Content-Type` and `Content-Length`
headers. Supports byte-range reads and conditional requests.

**Authentication:** Required.

**Path parameters:**

| Parameter | Description |
|-----------|-------------|
| `kb_slug` | Knowledge base slug |
| `path` | Object path, may contain multiple segments (e.g. `notes/2024/jan.md`) |

**Request headers:**

| Header | Description |
|--------|-------------|
| `Range` | Request a byte range (see [Byte-range requests](#byte-range-requests)) |
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
| `content-range` | Byte range returned, present only on 206 responses |

**Response:**

| Status | Body |
|--------|------|
| 200 OK | Full object bytes |
| 206 Partial Content | Partial object bytes (Range request satisfied) |
| 304 Not Modified | No body (conditional request matched) |
| 400 Bad Request | `{"error": "invalid_request", ...}` — malformed `Range` header |
| 404 Not Found | `{"error": "not_found", ...}` |
| 412 Precondition Failed | `{"error": "precondition_failed", ...}` |
| 416 Range Not Satisfiable | `{"error": "range_not_satisfiable", ...}` |

**Example:**

```sh
curl -H "Authorization: Bearer $TOKEN" \
     http://localhost:8080/v1/knowledgebases/notes/hello.md
```

**Multi-segment path:**

```sh
curl -H "Authorization: Bearer $TOKEN" \
     http://localhost:8080/v1/knowledgebases/notes/2024/january/meeting-notes.md
```

**Partial download (first 100 bytes):**

```sh
curl -H "Authorization: Bearer $TOKEN" \
     -H "Range: bytes=0-99" \
     http://localhost:8080/v1/knowledgebases/notes/hello.md
# HTTP/1.1 206 Partial Content
# Content-Range: bytes 0-99/1234
```

**Cache validation:**

```sh
curl -H "Authorization: Bearer $TOKEN" \
     -H 'If-None-Match: "abc123"' \
     http://localhost:8080/v1/knowledgebases/notes/hello.md
# HTTP/1.1 304 Not Modified (if ETag matches)
```

---

### PUT /v1/knowledgebases/{kb_slug}/{path}

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
| `location` | Path to the created object, e.g. `/v1/knowledgebases/notes/hello.md` |
| `etag` | Object ETag, if provided by the backend |

The response body is empty on success.

**Example — upload a Markdown file:**

```sh
curl -X PUT \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: text/markdown" \
  --data-binary @hello.md \
  http://localhost:8080/v1/knowledgebases/notes/hello.md
```

**Example — upload from stdin:**

```sh
echo "# Hello World" | curl -X PUT \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: text/markdown" \
  --data-binary @- \
  http://localhost:8080/v1/knowledgebases/notes/hello.md
```

**Example — upload a nested path:**

```sh
curl -X PUT \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: text/markdown" \
  --data-binary @jan.md \
  http://localhost:8080/v1/knowledgebases/notes/2024/january/meeting-notes.md
```

**Example — create-only (fail if exists):**

```sh
curl -X PUT \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: text/markdown" \
  -H 'If-None-Match: *' \
  --data-binary @hello.md \
  http://localhost:8080/v1/knowledgebases/notes/hello.md
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
  http://localhost:8080/v1/knowledgebases/notes/hello.md
# HTTP/1.1 201 Created (if ETag matched)
# HTTP/1.1 412 Precondition Failed (if ETag didn't match)
```

---

### DELETE /v1/knowledgebases/{kb_slug}/{path}

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
  http://localhost:8080/v1/knowledgebases/notes/hello.md
```

**Example — delete a nested path:**

```sh
curl -X DELETE \
  -H "Authorization: Bearer $TOKEN" \
  http://localhost:8080/v1/knowledgebases/notes/2024/january/meeting-notes.md
```

**Example — conditional delete (only if ETag matches):**

```sh
curl -X DELETE \
  -H "Authorization: Bearer $TOKEN" \
  -H 'If-Match: "abc123"' \
  http://localhost:8080/v1/knowledgebases/notes/hello.md
# HTTP/1.1 204 No Content (if ETag matched)
# HTTP/1.1 412 Precondition Failed (if ETag didn't match)
```

---

### POST /v1/knowledgebases/{kb_slug}/search

Perform a hybrid semantic search (dense cosine + sparse BM25 with server-side RRF fusion) against a knowledge base.

**Authentication**: Requires the static Bearer token (see [Authentication](#authentication)).

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
    "tags": ["rust"]
  },
  "limit": 10
}
```

All `filter` fields are optional and AND-composed. `limit` defaults to `10`, is clamped to `[1, 50]`, and a value above 50 is silently clamped (not an error).

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
| 400 | `invalid_request` | Missing or blank `query`; malformed JSON; missing or non-`application/json` Content-Type; `limit=0`; malformed slug format. |
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
  http://127.0.0.1:8080/v1/knowledgebases/notes/search
```

**Notes**:

- **Score semantics**: Search returns top-`limit` hits ordered by descending RRF fusion score. Scores are RRF rank values — higher is better. They are **NOT** probabilities or cosine similarities, are **NOT** comparable across queries or knowledge bases, and should **not** be displayed to users as confidence values.

- **Indexing lag**: Indexing is asynchronous best-effort (D38). A document just written may take a few seconds to appear in search results.

- **Preview**: The `preview` field is a UTF-8-safe truncation of the chunk text to at most 500 characters. Use `object_key` with `byte_start`/`byte_end` and a `Range: bytes=<byte_start>-<byte_end - 1>` header on `GET /v1/knowledgebases/{kb_slug}/{path}` to fetch the full chunk.

- **Tags filter**: The `tags` field in `SearchFilter` is reserved shape. Tags are not populated in v1 (D33 defers frontmatter extraction to post-v1). Tag filter values will match nothing until tag extraction ships.

- **`content_hash`**: Stored in the Qdrant payload for idempotent reindex detection but is **not** exposed in `SearchHit`.

- **`object_key_prefix` filter**: Applied **client-side** by the Searcher after Qdrant returns its top-k fused hits. (qdrant-client 1.15 does not expose a native keyword-index prefix matcher.) When this filter is set, the server over-fetches internally (up to 10x your `limit`, capped at 500 hits) and then retains only hits matching your prefix. In the pathological case where your prefix is highly selective and none of the top-500 fused hits match it, the response may contain fewer than `limit` hits (possibly zero). Widen the query or the prefix to recover coverage.

#### Upgrade notes (M4 → M5)

> **Reindex recommended after upgrading from M4.** The Qdrant payload schema was extended in M5: `mime`, `tags`, `content_hash`, and `text` fields were added, and a `mime` payload index was created. Documents written by an M4 server will not be returned by `mime` filters and will have empty `preview` fields until they are re-written or the KB is reindexed. Reindex tooling is a post-v1 feature (D42); operators can trigger a rewrite by PUTting existing documents again via `PUT /v1/knowledgebases/{kb_slug}/{path}`.
>
> The CHANGELOG for this release is generated automatically by release-plz — do not edit it by hand. This section is the operator-facing source of truth for upgrade guidance.

---

## Full route summary

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| GET | `/healthz` | No | Liveness probe |
| GET | `/readyz` | No | Readiness probe |
| GET | `/v1/knowledgebases` | Yes | List declared KBs |
| GET | `/v1/knowledgebases/{kb_slug}` | Yes | List objects in a KB |
| HEAD | `/v1/knowledgebases/{kb_slug}/{path}` | Yes | Object metadata, no body |
| GET | `/v1/knowledgebases/{kb_slug}/{path}` | Yes | Download object; supports `Range`, conditional headers |
| PUT | `/v1/knowledgebases/{kb_slug}/{path}` | Yes | Upload or replace object; supports `If-Match`, `If-None-Match` |
| DELETE | `/v1/knowledgebases/{kb_slug}/{path}` | Yes | Delete object (idempotent); supports `If-Match` |
| POST | `/v1/knowledgebases/{kb_slug}/search` | Yes | Hybrid semantic search (RRF fusion) |


---

## WebDAV

NotedThat exposes a WebDAV read-write surface on a second listener (default `0.0.0.0:8081`).
Authentication uses HTTP Basic auth (`NOTEDTHAT_WEBDAV_USERNAME` / `NOTEDTHAT_WEBDAV_PASSWORD`).

### URL layout

| Path | Meaning |
|------|---------|
| `/` | Virtual root — lists all declared knowledge bases |
| `/{kb}/` | Knowledge base root — lists objects in the KB |
| `/{kb}/{path}` | Object — multi-segment paths are native (unlike the HTTP API's percent-encoded single segment) |

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

- **MKCOL is a no-op**: `curl -X MKCOL http://127.0.0.1:8081/notes/newfolder/` returns 201, but the
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

---

## MCP

The MCP (Model Context Protocol) surface wraps the HTTP API described above. It is a thin proxy: every MCP tool call translates to one or more HTTP API requests. The MCP layer **never** accesses storage or the index directly (§5 principle 10).

### Transport

MCP is served by the `notedthat-mcp-stdio` binary over **stdio only** in v1 (D31). HTTP transport is post-v1. Configure your MCP client to launch the binary as a subprocess; see [README.md](../README.md) for setup snippets.

### Tools

All 7 tools are exposed:

#### `list_knowledgebases`

List all knowledge bases declared on the server.

**Arguments**: none

**Response**: `[{ "kb_slug": string }]`

Note: `display_name`, `description`, and `perms` are post-v1 (HTTP list endpoint does not return them yet).

#### `search`

Hybrid semantic + keyword search across a knowledge base.

**Arguments**: `kb` (string), `query` (string), `filters?` (object with `mime?`), `limit?` (u32)

**Response**: `{ "hits": [SearchHit] }` where each `SearchHit` has `object_key`, `byte_start`, `byte_end`, `heading_path`, `score`, `preview`

#### `read`

Read an object by optional byte range.

**Arguments**: `kb` (string), `path` (string), `byte_start?` (u64, inclusive), `byte_end?` (u64, **exclusive**)

**Response**: UTF-8 text content of the object (or byte slice)

**Byte range semantics**: MCP uses zero-based exclusive `byte_end`, while the HTTP `Range` header uses inclusive bounds. The MCP layer converts automatically: `byte_end=10` → `Range: bytes=0-9`.

**Constraints**: `byte_end` requires `byte_start`; `byte_start >= byte_end` is rejected with `invalid_request`.

#### `write`

Create or update an object. Content is UTF-8 text in v1 (binary not supported).

**Arguments**: `kb` (string), `path` (string), `content` (string), `if_match?` (string ETag), `if_none_match?` (string), `mime_type?` (string)

**Response**: `{ "etag": string|null, "location": string|null }`

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

### Path Encoding

Object paths are percent-encoded per RFC 3986 before being placed in URLs. The `/` separator within a path is encoded as `%2F`. Example: `docs/rfc/7231.md` → `docs%2Frfc%2F7231.md`.

### Error Codes

All MCP errors carry one of these codes:

| Code | HTTP Status | Meaning |
|------|-------------|---------|
| `invalid_request` | 400 | Bad arguments or validation failure |
| `unauthorized` | 401 | Missing or invalid bearer token |
| `not_found` | 404 | Knowledge base or object not found |
| `precondition_failed` | 412 | ETag mismatch (If-Match / If-None-Match) |
| `payload_too_large` | 413 | Content exceeds server limit (16 MiB) |
| `range_not_satisfiable` | 416 | Byte range beyond object size |
| `backend_unavailable` | 503 | S3 or Qdrant unavailable |
| `internal_error` | 500 | Unexpected server error |

### v1 Limitations

- **stdio only**: HTTP transport is post-v1 (D31)
- **No Resources**: MCP Resources (`notedthat://<kb>/<path>`) are post-v1 (D37)
- **Non-atomic MOVE**: GET → PUT → DELETE; partial failure is possible
- **No `display_name`/`description`/`perms`** on `list_knowledgebases` responses (HTTP list endpoint v1 limitation)
