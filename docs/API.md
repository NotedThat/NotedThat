# NotedThat HTTP API

NotedThat exposes a REST-style HTTP API for reading and writing objects stored in S3-compatible
object storage. The API surface is intentionally small: health probes, a knowledge-base list, and
four operations on objects (list, head, get, put, delete). Every route returns JSON for structured
responses and plain bytes for object bodies.

This document covers the M2 milestone. Features deferred to M3 (range requests, ETags, conditional
headers, cursor pagination) are called out explicitly in the "Not in M2" sections so you know what
to expect in future releases.

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
token rotation, no scopes, and no per-KB access control in M2. Either you have the token or you
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
| 400 | `invalid_request` | Malformed path, invalid KB slug, or other bad input |
| 401 | `unauthorized` | Missing or invalid `Authorization` header |
| 404 | `not_found` | KB slug not declared, or object does not exist |
| 413 | `payload_too_large` | PUT body exceeds 16 MiB |
| 500 | `internal_error` | Unexpected server error |
| 503 | `backend_unavailable` | S3 backend unreachable or returned an error |

## Limits

- **Body size:** PUT requests are rejected if the body exceeds **16 MiB** (16,777,216 bytes). The
  check happens on `Content-Length` before reading the body, and again after buffering. Requests
  without `Content-Length` are still capped at 16 MiB during body collection.
- **List default:** 100 objects per call.
- **List maximum:** 1,000 objects per call (pass `?limit=1000`).
- **Pagination:** M2 has no cursor. If `truncated` is `true`, there is no way to fetch the next
  page in M2. Cursor pagination is planned for M3.

## Not in M2

The following features are planned but not yet implemented:

- `Range:` header for partial content downloads
- `ETag` response header
- `If-Match` / `If-None-Match` conditional request headers
- Cursor-based pagination for object listing
- Per-KB access tokens or scopes

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

Readiness probe. Returns `200 OK` in M2 (static response; no backend connectivity check yet).

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

`truncated` is `true` when there are more objects beyond the returned set. M2 has no cursor, so
you cannot fetch the next page.

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

**Response:**

| Status | Meaning |
|--------|---------|
| 200 OK | Object exists; metadata in headers, no body |
| 404 Not Found | Object or KB does not exist |

**Example:**

```sh
curl -I -H "Authorization: Bearer $TOKEN" \
     http://localhost:8080/v1/knowledgebases/notes/hello.md
```

---

### GET /v1/knowledgebases/{kb_slug}/{path}

Download an object. Returns the raw bytes with appropriate `Content-Type` and `Content-Length`
headers.

**Authentication:** Required.

**Path parameters:**

| Parameter | Description |
|-----------|-------------|
| `kb_slug` | Knowledge base slug |
| `path` | Object path, may contain multiple segments (e.g. `notes/2024/jan.md`) |

**Response headers (on 200):**

| Header | Description |
|--------|-------------|
| `content-type` | MIME type (falls back to `application/octet-stream` if not stored) |
| `content-length` | Object size in bytes |

**Response:**

| Status | Body |
|--------|------|
| 200 OK | Raw object bytes |
| 404 Not Found | `{"error": "not_found", ...}` |

**Not in M2:** `Range:` header, `ETag`, `If-None-Match` conditional requests.

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

---

### PUT /v1/knowledgebases/{kb_slug}/{path}

Upload or replace an object. The operation is idempotent: uploading to an existing path overwrites
it.

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

**Body:** Raw object bytes. Maximum 16 MiB.

**Response:**

| Status | Meaning |
|--------|---------|
| 201 Created | Object stored successfully |
| 400 Bad Request | Invalid path or KB slug |
| 401 Unauthorized | Missing or invalid token |
| 404 Not Found | KB slug not declared |
| 413 Payload Too Large | Body exceeds 16 MiB |

**Response headers (on 201):**

| Header | Description |
|--------|-------------|
| `location` | Path to the created object, e.g. `/v1/knowledgebases/notes/hello.md` |

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

---

### DELETE /v1/knowledgebases/{kb_slug}/{path}

Delete an object. The operation is idempotent: deleting a non-existent object returns `204` just
like deleting one that exists.

**Authentication:** Required.

**Path parameters:**

| Parameter | Description |
|-----------|-------------|
| `kb_slug` | Knowledge base slug |
| `path` | Object path, may contain multiple segments |

**Response:**

| Status | Meaning |
|--------|---------|
| 204 No Content | Object deleted (or did not exist) |
| 400 Bad Request | Invalid path or KB slug |
| 401 Unauthorized | Missing or invalid token |
| 404 Not Found | KB slug not declared |

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

---

## Full route summary

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| GET | `/healthz` | No | Liveness probe |
| GET | `/readyz` | No | Readiness probe |
| GET | `/v1/knowledgebases` | Yes | List declared KBs |
| GET | `/v1/knowledgebases/{kb_slug}` | Yes | List objects in a KB |
| HEAD | `/v1/knowledgebases/{kb_slug}/{path}` | Yes | Object metadata, no body |
| GET | `/v1/knowledgebases/{kb_slug}/{path}` | Yes | Download object |
| PUT | `/v1/knowledgebases/{kb_slug}/{path}` | Yes | Upload or replace object |
| DELETE | `/v1/knowledgebases/{kb_slug}/{path}` | Yes | Delete object (idempotent) |
