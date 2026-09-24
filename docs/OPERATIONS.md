# Running NotedThat in production

This guide is for someone deploying an instance they did not build. It answers the questions an
operator asks, in the order they come up, and points at the setting, decision or route that
implements each answer.

It is deliberately **not** a settings reference. [`docs/CONFIGURATION.md`](CONFIGURATION.md) is
that, and it is complete; every variable named here links back to the section there that owns it.
What this document adds is the part that lives in no single place: the reverse proxy written out,
what a `503` means, what to back up, what one instance holds, what a release may break, and what
NotedThat does not do.

**Monitoring is not covered here, and what exists depends on your release.** Through v0.10.0
there is no metrics endpoint, and the operational signals are `/readyz`, the per-knowledge-base
index endpoint and the log codes — all named below where they matter. From the release that
carries [#164](https://github.com/NotedThat/NotedThat/issues/164) there is also `GET /metrics` in
the Prometheus exposition format, on the separate listener described below; its settings and its
metric catalogue are `docs/CONFIGURATION.md`'s, not this guide's. Alerting rules on top of either
are still to be written.

---

## The reference deployment

```
        TLS                      plain HTTP
client ──────▶ reverse proxy ──────────────▶ notedthat-server :8080
                                                   │
                                                   ├──▶ object store   (objects — authoritative)
                                                   ├──▶ Qdrant         (search index — derivable)
                                                   ├──▶ embedding endpoint
                                                   └──▶ NATS           (optional; events)
```

One process, one port to forward. The API is under `/api/v1`, WebDAV under `/webdav`, streamable
MCP at `/mcp`, the read-only HTML listings under `/browse`, and `/healthz`, `/readyz` and
`/llms.txt` at the root. There is no per-surface listener — the three variables that used to
create them are refused at startup by name
([removed variables](CONFIGURATION.md#removed-variables)).

The one exception is the metrics listener, and it is the exception that proves the rule: where
your release has `GET /metrics` it is served by a second listener of its own, off by default and
bound to loopback. It is **scraped, never proxied** — it carries no authentication, so the whole
point of the separate socket is that it is not reachable from where the product listener is. The
`location /` block below points at `127.0.0.1:8080` and so cannot reach it; do not add a
`/metrics` location that can.

**The server speaks plain HTTP only.** Terminate TLS once, at one proxy, and forward the complete
path space to it. Bearer tokens over plaintext are acceptable on loopback or a private trusted
link, nowhere else.

**Choosing the pieces:**

| Piece | What to run | Where it is argued |
|---|---|---|
| Object store | SeaweedFS ≥ 4.18 is what NotedThat itself runs; R2, Ceph RGW ≥ v20.2.1, MinIO and AWS S3 are all full-support. Or no object store at all — the `fs` backend keeps objects as ordinary files | [`SPECIFICATIONS.md` §8.2](../SPECIFICATIONS.md), [storage backend](CONFIGURATION.md#storage-backend) |
| Search index | Qdrant ≥ 1.15.2 (server-side `qdrant/bm25` sparse inference) | [Qdrant](CONFIGURATION.md#qdrant) |
| Embeddings | Any OpenAI-compatible endpoint. **Not bundled, and not covered by `/readyz`** | [Embedding](CONFIGURATION.md#embedding) |
| Events | Leave it `none`, or `memory` for a single process. `nats` only when something outside the process consumes the log | [Events backend](CONFIGURATION.md#events-backend) |
| Identity | The service token alone, or an OIDC issuer beside it | [OIDC authentication](CONFIGURATION.md#oidc-authentication) |

Before choosing an S3-compatible store, read
[`SPECIFICATIONS.md` §8.1](../SPECIFICATIONS.md). Several backends accept conditional-write
headers without enforcing them, which is a property of the deployment rather than of NotedThat —
see [Known limitations](#known-limitations).

### Start order

Every backend is contacted **before any listener binds**, so a dependency that is down is a
non-zero exit rather than a degraded start. Bring things up in this order:

1. Object store and Qdrant. Both are reached during provisioning — buckets or directories are
   created, manifests written, Qdrant collections ensured.
2. NATS, if `NOTEDTHAT_EVENTS_BACKEND=nats`. The broker connection is the one network round-trip
   the server makes before serving.
3. The OIDC issuer, if configured. Discovery happens at startup and an unreachable issuer refuses
   the start ([startup validation](CONFIGURATION.md#startup-validation)).
4. `notedthat-server`.
5. The reverse proxy.

Under an orchestrator, let the restart policy absorb a dependency that is still coming up — the
bundled Compose file relies on `restart: unless-stopped` for exactly this. On the `fs` backend the
storage root is locked before any of the above, so a second process exits immediately and says
which PID holds the root.

---

## The reverse proxy

This is the part that breaks quietly. A proxy that forwards every route and terminates TLS can
still leave MCP and WebDAV broken while the API looks fine, because two surfaces check the `Host`
header and the API does not.

### nginx

```nginx
upstream notedthat {
    server 127.0.0.1:8080;
    keepalive 32;
    # Below the server's NOTEDTHAT_HEADER_READ_TIMEOUT_MS (30 s), which is
    # also how long it keeps an idle connection. nginx's own default is 60 s,
    # long enough to reuse a connection the server has just closed.
    keepalive_timeout 20s;
}

server {
    listen 443 ssl;
    listen [::]:443 ssl;
    http2 on;
    server_name notes.example.com;

    ssl_certificate     /etc/letsencrypt/live/notes.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/notes.example.com/privkey.pem;

    # WebDAV and PATCH bodies are far larger than the API's own cap; let the
    # server enforce its limits rather than answering 413 here.
    client_max_body_size 0;

    location / {
        proxy_pass         http://notedthat;
        proxy_http_version 1.1;

        # The line whose absence breaks MCP and WebDAV. nginx defaults to
        # $proxy_host, which would send `Host: 127.0.0.1:8080`.
        proxy_set_header Host              $host;
        proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;

        # Both event streams are idle between messages and heartbeat every 15 s,
        # so the read timeout only has to sit comfortably above that.
        # Do NOT use `proxy_read_timeout 0` — in nginx that is an immediate 504.
        proxy_buffering    off;
        proxy_read_timeout 1h;
    }
}
```

`Authorization`, `Mcp-Session-Id` and `Last-Event-ID` are ordinary request headers and nginx
forwards them unchanged; nothing above removes them, and nothing should be added that does. The
one header this config rewrites is `Host`, and it rewrites it back to what the client sent.

One `location` covers every route. Splitting per surface is possible but buys nothing — the
buffering and timeout settings that the streams need are harmless on the others.

### Caddy

```caddyfile
notes.example.com {
	reverse_proxy 127.0.0.1:8080 {
		flush_interval -1
		transport http {
			read_timeout 1h
			# Below the server's idle timeout; Caddy's default is 2 minutes.
			keepalive 20s
		}
	}
}
```

Caddy passes the client's `Host` through by default and sets `X-Forwarded-*` itself, so the trap
below does not apply to it. `flush_interval -1` disables response buffering, which the two
streaming routes need.

### What the proxy must get right

| Requirement | Why |
|---|---|
| Preserve `Host` | MCP rejects a `Host` outside its allow-list; WebDAV `MOVE`/`COPY` compares `Destination` against it |
| Preserve `Authorization` | Every surface authenticates per request; the server holds no sessions |
| Preserve `Mcp-Session-Id` | Identifies the MCP session on every request after `initialize` |
| Preserve `Last-Event-ID` | Resumes the events stream at a position instead of replaying from now |
| No response buffering | `GET /mcp` and the events route are long-lived; a buffering proxy delivers nothing until the stream ends, which it never does |
| Read timeout above 15 s | Both streams heartbeat every 15 s. A short timeout cuts them mid-stream |
| Read timeout above the server's own | The server answers a request that runs too long itself: `504` after `NOTEDTHAT_REQUEST_TIMEOUT_MS` (30 s), or `NOTEDTHAT_WEBDAV_REQUEST_TIMEOUT_MS` (120 s) on `/webdav`. A proxy that gives up first turns that into its own bare `504` with no request id. The streams are exempt from the server's timeout, so the proxy's ceiling is the only one they have |
| Upstream keep-alive below 30 s | The server closes a connection that has been idle for `NOTEDTHAT_HEADER_READ_TIMEOUT_MS` (30 s). A proxy that keeps idle upstream connections longer will now and then send a request down one the server has just closed |
| No cache ignoring `Vary: Authorization` | `/browse` serves different pages to anonymous and credentialed callers at one URL |
| Rate limiting | There is no application rate limiter. Configure rate and burst limits here before exposing anonymous `search` |

**Which limit belongs where.** The server bounds what only it can see: how long a connection may
take to send its request head, how long a request may take to produce its response head, and how
many requests it works on at once — with the two streams and the probes exempt, because it knows
which routes those are and a proxy does not. The proxy keeps TLS, rate limiting, request and
response body transfer time, and the per-client limits the server cannot apply before it has
authenticated anyone. See [Request bounds](CONFIGURATION.md#request-bounds).

The API events route sets `X-Accel-Buffering: no`, which nginx honours by itself. **`GET /mcp`
does not** — rmcp sets no buffering hint — so `proxy_buffering off` is required for the MCP
notification leg rather than belt and braces, and a per-location config must carry it there too.

### The `Host` trap

`POST /mcp` validates `Host` against `NOTEDTHAT_MCP_HTTP_ALLOWED_HOSTS` as a DNS-rebinding guard.
A proxy that rewrites `Host` to its upstream — **nginx's default** — makes MCP answer:

```
HTTP/1.1 403 Forbidden

Forbidden: Host header is not allowed
```

That body is plain text, not the JSON error envelope the rest of the API uses, and it does not
name the setting. The same misconfiguration breaks WebDAV renames independently: `MOVE` and
`COPY` compare the `Destination` header's host against `Host`, and a mismatch is
`502` with `<nt:destination-different-server/>`. The API and `/browse` keep working throughout,
which is what makes it hard to place.

Both are fixed by `proxy_set_header Host $host;`.

Two properties of the allow-list are worth knowing, because neither is obvious:

- **An entry without a port matches any port.** `127.0.0.1` matches `Host: 127.0.0.1:8080`. An
  entry *with* a port must match exactly.
- **A request with no `Origin` header is always admitted**, whatever
  `NOTEDTHAT_MCP_HTTP_ALLOWED_ORIGINS` says. The origin allow-list constrains browsers; it does
  nothing to `curl`, `mcp-remote` or a desktop MCP client, which send no `Origin` at all. It is a
  browser guard, not an authorization control — authorization is the bearer and the manifest.

Set both lists to the public hostname when you publish MCP; leaving them unset means loopback
only, not "allow all". See
[origin and host allow-list semantics](CONFIGURATION.md#origin-and-host-allow-list-semantics).

### Verifying the proxy

```sh
B=https://notes.example.com; T=$NOTEDTHAT_API_TOKEN
curl -sS $B/healthz                                     # {"status":"ok"}
curl -sS $B/readyz | jq .                               # every check ok
SID=$(curl -sS -D - -o /dev/null -X POST $B/mcp \
  -H "Authorization: Bearer $T" -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"curl","version":"0"}}}' \
  | awk 'tolower($1)=="mcp-session-id:"{print $2}' | tr -d '\r')
test -n "$SID" || echo 'no session id — check Host and the allow-list'
# The notification leg must stay open and print a keep-alive comment within 15 s.
curl -sS -N --max-time 20 $B/mcp \
  -H "Authorization: Bearer $T" -H 'Accept: text/event-stream' -H "Mcp-Session-Id: $SID"
```

If the last command returns at once with nothing, the proxy is buffering. If it hangs and then
returns empty, the read timeout is too short. If the `initialize` yields no session id, read the
body — a plain-text `403` is the `Host` trap.

---

## `503` is normal

Writes answer `503` with a `Retry-After` hint while the indexing queue is full (D38). **This is the
design, not an incident.** The object is already stored; only the search-index update is delayed,
and the client's retry is what re-enqueues it.

Expect a burst of them:

- **at startup on `fs`** — each knowledge base is compared against the index once at boot (D50),
  and everything the two sides disagree on is enqueued at once;
- **at startup on `s3`** while `NOTEDTHAT_S3_RECONCILE` is on (the default), and whenever the
  service token posts a reconciliation (D67) — the same comparison, since S3 has no change feed
  to subscribe to;
- **after bulk-loading objects** by any route.

A pass over a corpus that has not changed is cheap and enqueues nothing: an object already
indexed from exactly the bytes in the store is recognised and left alone. So a startup burst is
proportional to the drift, not to the corpus — a long one after a quiet night means something
changed the store from outside.

Treat it as a problem when it persists without one of those causes. That means the embedder or
Qdrant is consuming slower than the write rate, and the queue never drains — investigate embedder
throughput and Qdrant ingestion latency, in that order. Queue capacity is not tunable.

The per-verb consequences differ, and matter for clients: a `DELETE` that answers `503` has
already removed the object from storage, and a `MOVE` answers in two distinguishable flavours.
[Indexer backpressure](CONFIGURATION.md#indexer-backpressure) states each one and what the client
should do; a conditional write additionally has a `503` → `412` retry hazard worth reading before
you enable one.

Where a knowledge base actually stands is per knowledge base, not process-wide:
`GET /api/v1/knowledgebases/{kb_slug}/index` reports `healthy`, `indexing`, `backpressured`,
`stale` or `failed` ([the endpoint](API.md#get-apiv1knowledgebaseskb_slugindex)). `/readyz` does
not cover index freshness, and is not the place to look for it.

---

## Backup, restore and disaster recovery

**The object store is authoritative. The search index is derivable.** That is the whole story, and
it makes the restore simple.

### Back up

| What | Where it lives | Back it up? |
|---|---|---|
| Objects | The bucket, or `$NOTEDTHAT_FS_ROOT/nt-*/` | **Yes** — this is the only irreplaceable thing |
| Manifests — access rules and descriptions | `<kb>/.notedthat/manifest.json`, inside the knowledge base | **Yes**, and it comes along free with the objects |
| Per-object metadata (`fs` only) | `$NOTEDTHAT_FS_ROOT/.notedthat-meta/` — *outside* every knowledge base | **Yes** — see below |
| Qdrant | Its own storage | No. It is rebuilt from the objects |
| Configuration | Your environment, wherever secrets live | Yes, by your usual means |

On `fs`, back up **the whole root**, not just the `nt-*` directories. The metadata shadow tree
sits beside them, not inside them, and `rsync`-ing only the knowledge bases silently drops it.
Losing it is survivable — the `fs` ETag is derived from content, and a content type falls back to
a guess from the file extension — but an explicitly set content type that the extension does not
imply is gone.

### Restore

1. Restore the objects (and, on `fs`, the whole root).
2. Start the server with the same `NOTEDTHAT_KBS`. Bucket and directory names are derived from the
   slugs, so the same configuration finds the same data; nothing stores them.
3. The index rebuilds itself. On `fs`, the startup pass does it. On `s3`, the startup pass does it
   when `NOTEDTHAT_S3_RECONCILE` is on (the default), or post
   `/api/v1/knowledgebases/{kb_slug}/index/reconcile` per knowledge base
   ([the route](API.md#post-apiv1knowledgebaseskb_slugindexreconcile)).
4. Watch for `503`s during the rebuild. They are the pass working — see above.

An empty Qdrant is a supported starting state, so a lost index needs no special procedure: bring
it back empty and let the pass repopulate it. Reconciliation is incremental and idempotent, so an
interrupted rebuild is resumed by running it again. What it is **not** is a full reindex: it
re-embeds only what the two sides disagree on. Changing the embedding model is a different
operation with its own procedure
([changing the embedding model](CONFIGURATION.md#changing-the-embedding-model)).

A bucket or directory deleted while the server is running is not re-created — it answers `404` on
every surface until a restart provisions it again.

---

## Capacity: what one instance holds

One process, one node. There is no clustering, no sharding and no leader election; scale is a
bigger node, and on `fs` a second process is refused outright.

| Dimension | Ceiling | Set by |
|---|---|---|
| Knowledge bases | One bucket each. On AWS the account quota is the real limit: 10,000 buckets, of which 2,000 are free and the rest cost about $0.10/month each — roughly $800/month at the ceiling, and `ListBuckets` paginates past 10k. Unlimited in practice on SeaweedFS, Garage, R2 and RustFS | [`SPECIFICATIONS.md` §8.4](../SPECIFICATIONS.md) |
| Requests in flight | `NOTEDTHAT_MAX_REQUESTS_IN_FLIGHT` across every surface; past it, `503` with `Retry-After` at once. The streams and the probes do not count | [Request bounds](CONFIGURATION.md#request-bounds) |
| Time per request | To the response head: `NOTEDTHAT_REQUEST_TIMEOUT_MS`, and `NOTEDTHAT_WEBDAV_REQUEST_TIMEOUT_MS` on `/webdav`; past it, `504`. The body's transfer is not bounded here | [Request bounds](CONFIGURATION.md#request-bounds) |
| Concurrent MCP clients | One session each, capped per process; a client that re-`initialize`s per request burns slots and idles them out minutes later | [MCP endpoint](CONFIGURATION.md#mcp-endpoint) |
| Pending index work | A fixed-size queue; past it, writes answer `503` | [Indexer backpressure](CONFIGURATION.md#indexer-backpressure) |
| API request body | Capped, and the cap is not configurable. WebDAV is exempt and takes far larger bodies | [What's not configurable](CONFIGURATION.md#whats-not-configurable-in-m2) |
| One MCP object read | Bounded by `NOTEDTHAT_MCP_MAX_READ_BYTES`; the response can be about twice that, so size a proxy's response limit against the budget | [MCP HTTP environment variables](CONFIGURATION.md#mcp-http-environment-variables) |
| PATCH working set | Roughly three times `NOTEDTHAT_MAX_PATCHABLE_SIZE` per worker, in RAM | [PATCH memory model](CONFIGURATION.md#patch-memory-model) |
| Reconciliation RAM | Proportional to objects in the largest single knowledge base — passes run one at a time, so the ceiling is the largest, not the sum | [S3 reconciliation](CONFIGURATION.md#s3-reconciliation) |
| Upload staging disk | Sized per concurrent maximum-size upload. Must not be `tmpfs`, which is RAM | [Upload and index staging directory](CONFIGURATION.md#upload-and-index-staging-directory) |
| `fs` inotify watches | One per directory, against `fs.inotify.max_user_watches`. Too few refuses startup and names the knob | [Operating it](CONFIGURATION.md#operating-it) |

The numbers those rows point at are owned by `docs/CONFIGURATION.md` and change there first.

Two things that are *not* limits but shape capacity: WebDAV `PROPFIND` walks the whole collection
server-side before answering, so a large knowledge base wants a generous WebDAV timeout, on the
server and on the proxy alike
([PROPFIND on large knowledge bases](CONFIGURATION.md#webdav-propfind-on-large-knowledge-bases));
and `/browse` caps a page at ten thousand keys and renders a notice rather than failing.

---

## Upgrades

**The project is pre-v1, and what that covers is not yet written down.** Every release so far has
been free to change interfaces, and
[#176](https://github.com/NotedThat/NotedThat/issues/176) — defining the v1 gate and the
stability commitment — is still open. Until it closes, assume any release may change the HTTP API,
the MCP tool surface, the WebDAV surface, the event schema or the `NOTEDTHAT_*` settings, and read
`CHANGELOG.md` before upgrading. All crates share one version, so a version bump moves everything
whether or not the part you use changed.

What protects you in practice is that the server **refuses to start** rather than ignoring a
setting it no longer understands (D39). A removed variable is an immediate non-zero exit whose
message names the replacement — the three listeners that became one route space are the worked
example ([removed variables](CONFIGURATION.md#removed-variables)). An upgrade that starts is an
upgrade whose configuration the new version still agrees with.

To upgrade:

1. Read `CHANGELOG.md` for the versions you are crossing.
2. Stop the server. Shutdown is staged — in-flight requests finish, then the indexer drains within
   a bounded budget — so give the container a grace period that covers your longest request plus
   that drain ([shutdown behaviour](CONFIGURATION.md#shutdown-behaviour)). A kill that beats the
   drain loses queued index work, not data; the next startup pass finds it again.
3. Upgrade Qdrant and the object store on their own schedules, not with the server. Neither
   stores anything NotedThat cannot rebuild or re-provision.
4. Start the new version. If it exits, read the message — startup validation is specific about
   what is wrong.
5. Expect `503`s while the startup pass runs.

Verify the image before running it in production:

```sh
gh attestation verify oci://ghcr.io/notedthat/server:<version> --owner NotedThat
```

Configuration changes follow the same shape as upgrades, because **access rules are a startup
snapshot**: edit a manifest, then restart. There is no reload signal, and no `SIGHUP`.

---

## Security posture

### Which credential

`NOTEDTHAT_API_TOKEN` is the deployment's own credential — the *service token*. It is a static
bearer with **no claims, no expiry and no rotation surface** (D21): rotating it means editing the
configuration and restarting, and every client holding the old value breaks at once.

`NOTEDTHAT_WEBDAV_USERNAME` and `NOTEDTHAT_WEBDAV_PASSWORD` are a **separate** Basic-auth pair
that resolves to that **same** principal. Two distinct secrets therefore grant identical
authority, and rotating one does not rotate the other — treat the WebDAV password as a second
copy of the service token, not as a lesser credential.

Point the server at an OIDC issuer and it additionally accepts that issuer's JWTs on every surface
including MCP, with `group:` and `user:` rules matching the token's claims. NotedThat mints no
tokens and keeps no sessions.

**Use the service token for the deployment itself and for automation; use OIDC for people.** A
deployment with more than one human user and no issuer is sharing one unrotatable credential
between them, and no manifest rule can tell them apart.

What a compromised service token costs:

- every knowledge base, at whatever the manifests grant `signed-in` — and the service token is
  bound by those rules like any other credential;
- `.notedthat` in every knowledge base — the manifests themselves, including the access rules.
  **Only the service token reaches it**, by design, so that a manifest that locks everyone out
  stays repairable through the API. That same property makes the token the one credential that can
  rewrite the access rules;
- `POST …/index/reconcile`, which is the operator's route alone — an identity token gets `403`
  there however broadly the manifests grant it;
- every surface, since it is accepted as a `Bearer` on all of them, WebDAV included.

A leaked WebDAV password reaches only `/webdav` — `Basic` is accepted there and nowhere else —
but it arrives as that same principal, so the data it can read and write is the same. Rotate both
together.

Rotating it is the only remedy, and it is a restart. Keep it out of images and shell history
([passing secrets](CONFIGURATION.md#passing-secrets)).

### What anonymous access grants

Anonymity is not a switch; it is per knowledge base, in the manifest's `anyone` rules. A knowledge
base with no such rule is private. Granting `write` or `delete` to `anyone` refuses startup.

Before granting `anyone` anything, note what comes with it: `read` makes objects fetchable without
a credential at their `/api/v1` URL; `list` makes them enumerable; `search` runs an embedding call
per query against your paid endpoint, from an unauthenticated caller — **configure proxy rate
limits first**, because there is no application limiter. `/browse` exposes exactly what the same
rules allow and nothing more: no JavaScript, no accounts, no editing, no search, and `.notedthat`
rendered for nobody. Anonymous denials answer `404` rather than `403` so the status cannot be used
to enumerate private prefixes.

`NOTEDTHAT_MCP_ANONYMOUS=never` closes `/mcp` to anonymous callers while the other surfaces keep
honouring the same `anyone` rules — useful when you want an OAuth-capable client challenged on
connect.

### MCP session ids

Whether a session id is bound to the principal that opened it depends on your release.

**Through v0.10.0 it is not** (D66), so treat the id itself as a credential: another
authenticated caller who obtains one can attach that session's notification leg and watch which
object URIs it subscribed to and when they change — including objects that caller may not read
itself — and can end the session with a `DELETE`. Ids are UUIDs, so this is not guessable, and
tool calls are unaffected: each acts as the credential presented on its own request.

**From the release that carries [#179](https://github.com/NotedThat/NotedThat/issues/179) it is**
(D68): a request presenting a session id whose owner is not the request's own principal is
answered exactly as an id the server does not know, on all three methods, so the `DELETE` is
closed with the rest.

Two things hold either way, and are the residue #179 leaves behind. Every caller presenting no
credential is **one** owner, since nothing distinguishes two of them — so on a deployment
admitting anonymous callers one anonymous client can still attach another's leg, though both hold
identical read authority. And the session bound is **per process, not per principal**, so one
caller can hold every slot.

Send session ids over TLS and keep them out of logs on any release.

### A read-only root filesystem

The image runs under `--read-only`. Exactly two paths need to be writable, and both are mounts:

| Path | Why | When |
|---|---|---|
| `NOTEDTHAT_FS_ROOT` (no default — the image creates `/var/lib/notedthat` for it, but the variable must be set) | Objects, `.notedthat.lock`, and the `.notedthat-meta/` shadow tree | `fs` backend only — an S3-backed deployment does not need it |
| `NOTEDTHAT_UPLOAD_TMP_DIR` | WebDAV upload spooling and indexer snapshots | Always. Under `--read-only` the default (the platform temp dir) is not writable, so this becomes **mandatory** |

```sh
docker volume create notedthat-data
docker volume create notedthat-staging

docker run --rm --read-only \
  --stop-timeout 45 \
  -p 8080:8080 \
  -v notedthat-data:/var/lib/notedthat \
  -v notedthat-staging:/var/lib/notedthat-staging \
  -e NOTEDTHAT_STORAGE_BACKEND=fs \
  -e NOTEDTHAT_FS_ROOT=/var/lib/notedthat \
  -e NOTEDTHAT_UPLOAD_TMP_DIR=/var/lib/notedthat-staging \
  --env-file .env \
  ghcr.io/notedthat/server:<version>
```

Both mount points exist in the image owned by uid 10001, which is what makes this work: Docker seeds a
fresh named volume with the image's ownership **only when the path already exists in the image**. Mount a
volume at a path the image lacks and it arrives `root:root`, uid 10001 cannot write, and startup refuses.

Nothing else in the image is written at runtime. The server reads no home or config directory — which is
why `--home-dir /nonexistent` has never mattered — and the `HEALTHCHECK`'s `curl` writes nothing.

**Do not reach for `--tmpfs` here.** It is the reflex for a read-only rootfs and it will start the server,
but a `tmpfs` staging directory is host RAM, and
[the sizing rule](CONFIGURATION.md#upload-and-index-staging-directory) wants 5 GiB for every concurrent
maximum-size upload. A read-only root filesystem is a production hardening posture, which is precisely
where turning an upload burst into an OOM kill matters most. Use disk-backed mounts, as above; `--tmpfs`
is defensible for a smoke test and nowhere else.

The failure mode is loud, which is the useful part. Run under `--read-only` without setting
`NOTEDTHAT_UPLOAD_TMP_DIR` and startup refuses before any listener binds:

```
staging configuration error: staging directory is unusable: Read-only file system (os error 30)
```

Nothing degrades silently, so a misconfigured hardened deployment cannot look healthy.

### The rest

Run the published image rather than a build of your own where you can: it runs as a non-root user,
runs under a read-only root filesystem with two writable mounts (above), publishes one port, is built
for `linux/amd64` and `linux/arm64` under one tag, and is cosign-signed with SLSA L2 provenance. Keep Qdrant and the object store
on a private network — Qdrant carries your document vectors, and the bundled one is
unauthenticated. `/healthz` and `/readyz` are unauthenticated by design and deliberately say
nothing about the backends beyond a status and a reason; the backend's own error, which can quote
credential-bearing URLs, goes to the log instead.

---

## Known limitations

These are properties of NotedThat today, not bugs, and an operator should know them before
committing to the deployment.

- **One process per root on `fs`.** Conditional writes are made atomic by an in-process lock, so a
  second server on the same root would reintroduce lost writes. The server takes an exclusive lock
  on `$NOTEDTHAT_FS_ROOT/.notedthat.lock` at startup and refuses to start if another process holds
  it, naming that process. For the same reason **NFS and SMB are unsupported** — their advisory
  locking is unreliable (D49, [`SPECIFICATIONS.md` §8.1](../SPECIFICATIONS.md)). The `fs`
  backend also cannot hold an object `a/b` alongside `a/b/c`, and refuses filesystems that
  case-fold or Unicode-normalize names.
- **Some backends accept `If-Match` without enforcing it.** NotedThat forwards conditional-write
  headers verbatim and adds no compensating layer, so on those backends a mismatched `If-Match`
  can answer `200` instead of `412` and a concurrent write is silently lost. As of
  [`SPECIFICATIONS.md` §8.1](../SPECIFICATIONS.md) that is **Garage** (always — structurally
  impossible without a consensus algorithm, and a documented design choice),
  **SeaweedFS < 4.09**, and **RustFS 1.0.0-beta.8** under lock-timeout contention. `PATCH`
  inherits the same exposure, because its final write is a conditional `PUT`.

  Whether the server checks this depends on your release. **Up to and including v0.11.0 it does
  not**, and verifying a backend is the deployer's gate. **From the release that carries
  [#190](https://github.com/NotedThat/NotedThat/issues/190) it does** (D70) — that release is the
  thing to check for, not a version number, so this does not go stale again: at startup each
  bucket is asked whether it honours a `PUT` whose `If-Match` holds and refuses one whose
  `If-Match` or `If-None-Match` does not, and one that stores it anyway **refuses startup** — so
  upgrading a deployment on such a backend
  stops it until you either move to a backend that enforces them or set
  `NOTEDTHAT_S3_ALLOW_UNENFORCED_CONDITIONAL_WRITES=true` to accept the risk. With that setting
  the finding stays visible: `S3_CONDITIONAL_WRITES_NOT_ENFORCED` in the log, `/readyz`
  `degraded` (`preconditions_not_enforced`), and
  `notedthat_storage_conditional_writes_enforced{kb} 0`, which is the thing to alert on
  ([S3 conditional writes](CONFIGURATION.md#s3-conditional-writes)). A single writer at startup
  cannot provoke a failure that only happens under contention, so RustFS's lock timeouts, and any
  store that refuses a lone mismatch but has no atomicity, still pass the check — §8.1 remains the
  reference for those.
- **MCP sessions.** A process holds a fixed maximum of sessions and one caller can hold all of
  them; the bound is per process, and a budget per principal is a follow-up. Every anonymous
  caller is one owner, so they share each other's sessions by construction. Through v0.10.0 a
  session id is additionally not bound to the principal that opened it at all — see
  [MCP session ids](#mcp-session-ids). Subscriptions live and die with the session — no replay,
  no persistence, and a restart ends every one of them. A client that stops answering the
  keeper's ping has its subscriptions dropped, and a later `subscribe` on that session is refused
  rather than accepted and left silent.
- **No stability commitment yet.** Pre-v1, and
  [#176](https://github.com/NotedThat/NotedThat/issues/176) — which would say what v1 covers and
  what a deprecation looks like — is open. The current
  intent recorded there is that the HTTP API, the MCP surface, the WebDAV surface, the event
  schema and the `NOTEDTHAT_*` settings would be covered, and the crates' Rust APIs would not; it
  is a proposal until that issue closes.
- **Monitoring depends on the release.** Through v0.10.0 there is no metrics endpoint, and
  `/readyz`, the per-knowledge-base index endpoint and the log codes are what exists;
  [#164](https://github.com/NotedThat/NotedThat/issues/164) adds `GET /metrics` on its own
  loopback listener. No alerting rules ship either way.
- **Other things NotedThat does not do:** no application rate limiter; no reload signal, so access
  rules need a restart; no object lock, retention or legal hold, and WebDAV `LOCK` is refused; no
  full reindex, so changing the embedding model has its own procedure; no admin API for creating
  knowledge bases — `NOTEDTHAT_KBS` and a restart; no multi-tenancy, the tenant slug is fixed; and
  `MOVE` is not atomic.

---

Full settings reference: [`docs/CONFIGURATION.md`](CONFIGURATION.md) —
full API, WebDAV and MCP reference: [`docs/API.md`](API.md) —
backend selection and the compatibility matrix: [`SPECIFICATIONS.md`](../SPECIFICATIONS.md) §8.
