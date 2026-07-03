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
- **Pagination:** If `truncated` is `true`, there is no way to fetch the next page in v1.

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

| Parameter | Type | Default | Max | Description |
|-----------|------|---------|-----|-------------|
| `prefix` | string | (none) | | Only return objects whose key starts with this string |
| `limit` | integer | 100 | 1000 | Maximum number of objects to return |

**Response:**

| Status | Body |
|--------|------|
| 200 OK | `{"objects": [...], "truncated": bool}` |
| 404 Not Found | `{"error": "not_found", ...}` |

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
  "truncated": false
}
```

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
