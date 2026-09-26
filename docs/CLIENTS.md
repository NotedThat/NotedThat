# Connecting clients

NotedThat speaks [MCP](https://modelcontextprotocol.io) — the protocol AI assistants and
automation tools use to call external tools. Anything that can act as an MCP client can list,
search, read, write, edit and delete notes, bound by the same [access rules](CONFIGURATION.md#manifest-access-rules)
as every other surface. Anything that cannot still has the plain [HTTP API](API.md), and
`GET /llms.txt` on the server explains that API to a model in its own words.

This page is setup snippets, one client at a time. The tools themselves are documented under
[MCP tools](API.md#tools).

## One way in

Every client talks to the same endpoint: `POST https://notes.example.com/mcp`, the streamable
HTTP transport, with the credential as an `Authorization: Bearer …` header or obtained through
OAuth from your identity provider. There is nothing to install on the client side. The bearer can
be `NOTEDTHAT_API_TOKEN` or an identity token from your OIDC provider; the rules that apply are
the caller's, so an agent holding a restricted identity sees a restricted knowledge base.

A knowledge base that is public — its manifest grants `anyone` `read` or `search` — needs no
credential over MCP either: leave the header out and the client is the anonymous caller, with
exactly what `anyone` may do and nothing more ([D59](../SPECIFICATIONS.md#2-decisions-log)).

A client that can only spawn a command — no HTTP transport of its own — is bridged with
[`mcp-remote`](https://www.npmjs.com/package/mcp-remote), a stdio-to-HTTP adapter that ships
with the MCP ecosystem rather than with NotedThat; see
[Clients that only spawn a command](#clients-that-only-spawn-a-command). NotedThat itself no
longer ships a stdio binary.

**Reaching `/mcp` from another host.** The MCP endpoint answers only to `Host` values it is told
to expect — the default is loopback. Any client that is not on the same machine needs the public
hostname in `NOTEDTHAT_MCP_HTTP_ALLOWED_HOSTS`, and a public deployment needs TLS terminated at a
reverse proxy in front of the listener. See [MCP HTTP listener](CONFIGURATION.md#mcp-http-listener).

```sh
NOTEDTHAT_MCP_HTTP_ALLOWED_HOSTS=notes.example.com
```

## Claude Code

Remote, with the service token — one command, then the tools are available in the session:

```sh
claude mcp add --transport http notedthat https://notes.example.com/mcp \
  --header "Authorization: Bearer $NOTEDTHAT_API_TOKEN"
```

Or commit it to the project so every checkout gets it, in `.mcp.json`:

```json
{
  "mcpServers": {
    "notedthat": {
      "type": "http",
      "url": "https://notes.example.com/mcp",
      "headers": { "Authorization": "Bearer ${NOTEDTHAT_TOKEN}" }
    }
  }
}
```

With an [OIDC provider](CONFIGURATION.md#oidc-authentication) configured and
`NOTEDTHAT_OIDC_RESOURCE` set, drop the header: Claude Code reads the `401` challenge, finds the
authorization server and signs you in through the browser, and then acts as *you* — a
`group:editors` rule that lets you write lets Claude Code write, and one that does not, does not.
The provider has to know the client first; [MCP clients](CONFIGURATION.md#mcp-clients) says what to
register. One caveat: on a deployment where some knowledge base is public, there is no `401` —
a client with no token is simply admitted as the anonymous caller and sees the public knowledge
bases. Sign in explicitly there (`/mcp` → *Authenticate*), or have the operator set
`NOTEDTHAT_MCP_ANONYMOUS=never`.

## Claude.ai and Claude Desktop

Claude on the web and in the desktop app connects to remote MCP servers as **custom connectors**
(Settings → Connectors → *Add custom connector*). Give it `https://notes.example.com/mcp`.

Custom connectors authenticate with OAuth, not with a static header, so this route needs an
[OIDC provider](CONFIGURATION.md#oidc-authentication) with `NOTEDTHAT_OIDC_RESOURCE` set to the
URL above. Register a confidential client for Claude at the provider — the connector dialog shows
the callback URL to allow — and paste its client id and secret into the connector's advanced
settings. Once connected, Claude searches and edits notes as the signed-in user, and the server's
access rules decide what that user may do. The server must be reachable from the internet over
HTTPS. The same caveat as for Claude Code applies: a deployment with public knowledge bases does
not answer `401`, so a connector that has not completed OAuth is served as the anonymous caller;
`NOTEDTHAT_MCP_ANONYMOUS=never` makes the sign-in mandatory again.

For a server on your own machine without an identity provider, Claude Desktop's config file only
spawns commands; bridge it with `mcp-remote` as under
[Clients that only spawn a command](#clients-that-only-spawn-a-command). Edit
`~/Library/Application Support/Claude/claude_desktop_config.json` (macOS) or
`%APPDATA%\Claude\claude_desktop_config.json` (Windows) and restart the app:

```json
{
  "mcpServers": {
    "notedthat": {
      "command": "npx",
      "args": [
        "-y", "mcp-remote@0.14.3", "http://localhost:8080/mcp",
        "--header", "Authorization:${AUTH_HEADER}"
      ],
      "env": { "AUTH_HEADER": "Bearer your-token-here" }
    }
  }
}
```

The header argument has no space in it on purpose, and the `Bearer` prefix lives in the
variable: Claude Desktop on Windows (and Cursor) does not escape spaces inside `args` when it
spawns `npx`, so `"Authorization: Bearer …"` would arrive split and the header never sent. This
shape works on every platform; see [Clients that only spawn a command](#clients-that-only-spawn-a-command)
for the rest of the reasoning.

## n8n

n8n's **MCP Client Tool** node attaches to an AI Agent node and turns every NotedThat tool into
something the agent can call: search the knowledge base for context, append findings to a note,
file the day's output under `inbox/`.

- **Endpoint**: `https://notes.example.com/mcp`
- **Server Transport**: HTTP Streamable
- **Authentication**: Bearer Auth, with a credential holding the token

Workflows that do not involve an agent — "when a file lands here, put it there" — need no MCP at
all. The **HTTP Request** node with a `PUT` to
`/api/v1/knowledgebases/{kb}/{path}` and a `Content-Type` header writes a note in one step, and the
[events stream](API.md#get-apiv1knowledgebaseskb_slugevents) is a trigger for the other direction:
a workflow that reacts to every note someone else creates.

## Windmill

Windmill's AI agent step takes MCP servers as tools: give it the `/mcp` URL and a bearer header
and the agent has the same eleven tools an assistant would. For a plain script step, the HTTP API is a
few lines in any language Windmill runs:

```ts
// TypeScript (Bun) script step: append today's summary to a note
export async function main(summary: string) {
  const res = await fetch(
    `${process.env.NOTEDTHAT_URL}/api/v1/knowledgebases/notes/journal.md`,
    {
      method: "PATCH",
      headers: {
        Authorization: `Bearer ${process.env.NOTEDTHAT_TOKEN}`,
        "Content-Type": "text/markdown",
        "NT-Patch-Mode": "append",
      },
      body: `\n## ${new Date().toISOString().slice(0, 10)}\n\n${summary}\n`,
    },
  );
  if (!res.ok) throw new Error(`${res.status} ${await res.text()}`);
}
```

`NT-Patch-Mode: append` is the append form of [`PATCH`](API.md#patch-apiv1knowledgebaseskb_slugpath):
one round-trip, no prior read, and a note that does not exist yet is a `404` rather than a
surprise — create it with `PUT` first. Keep the URL and token in Windmill's resources or variables rather
than in the script.

## Cursor

`.cursor/mcp.json`, or Settings → MCP. Remote:

```json
{
  "mcpServers": {
    "notedthat": {
      "url": "https://notes.example.com/mcp",
      "headers": { "Authorization": "Bearer your-token-here" }
    }
  }
}
```

## VS Code

`.vscode/mcp.json` in the workspace, or the user-level equivalent:

```json
{
  "servers": {
    "notedthat": {
      "type": "http",
      "url": "https://notes.example.com/mcp",
      "headers": { "Authorization": "Bearer your-token-here" }
    }
  }
}
```

VS Code also supports OAuth for HTTP servers; the notes under [Claude Code](#claude-code) apply.

## Zed

`~/.config/zed/settings.json`. Zed launches context servers as commands, so it goes through the
`mcp-remote` bridge:

```json
{
  "context_servers": {
    "notedthat": {
      "command": {
        "path": "npx",
        "args": [
          "-y", "mcp-remote@0.14.3", "https://notes.example.com/mcp",
          "--header", "Authorization:${AUTH_HEADER}"
        ],
        "env": { "AUTH_HEADER": "Bearer your-token-here" }
      }
    }
  }
}
```

## Clients that only spawn a command

Some clients have no HTTP transport and can only start a subprocess that speaks MCP over stdio.
NotedThat does not ship such a binary; the endpoint is HTTP, and
[`mcp-remote`](https://www.npmjs.com/package/mcp-remote) is the general-purpose bridge — it
speaks stdio to the client and streamable HTTP to the server, forwards the header you give it,
and can run the OAuth flow for a deployment with an identity provider. The shape is the same in
every client:

```json
{
  "command": "npx",
  "args": [
    "-y", "mcp-remote@0.14.3", "https://notes.example.com/mcp",
    "--header", "Authorization:${AUTH_HEADER}"
  ],
  "env": { "AUTH_HEADER": "Bearer your-token-here" }
}
```

Three things about that shape, each from `mcp-remote`'s own README:

- **The header argument has no space, and `Bearer` is in the variable.** Cursor and Claude
  Desktop on Windows do not escape spaces inside `args` when they spawn `npx`, so
  `"Authorization: Bearer …"` arrives split and the header is never sent. `Authorization:${AUTH_HEADER}`
  with the whole value in `env` works in every client, so it is the one shape shown. The `${VAR}`
  expansion is `mcp-remote`'s own, which is also why the token is in `env` rather than in the
  argument list, where the process list would show it. To keep it out of the arguments entirely,
  `mcp-remote` also reads headers from a file: `--header-file /path/to/headers` (one
  `Name: value` per line).
- **The version is pinned.** `npx -y mcp-remote` would resolve whatever is latest at every launch,
  and this is the process that carries the bearer to the server; pin it and move it on purpose.
  The snippets were written against `0.14.3`.
- **Plain `http://` needs `--allow-http` unless the host is `localhost`.** Against a local server
  the URL is `http://localhost:8080/mcp` and nothing more is needed — not the flag, and nothing in
  `NOTEDTHAT_MCP_HTTP_ALLOWED_HOSTS`. A server elsewhere on a LAN without TLS
  (`http://notes.lan:8080/mcp`) needs `"--allow-http"` added to `args` *and* its hostname in
  `NOTEDTHAT_MCP_HTTP_ALLOWED_HOSTS`; `mcp-remote` refuses the URL otherwise, and rightly — that is
  a bearer on the wire in clear.

## Anything else that speaks MCP

The endpoint is a streamable-HTTP MCP server with sessions, the shape the MCP specification
describes: `initialize` returns an `Mcp-Session-Id`, every later request carries it, answers are
SSE-framed, and `GET /mcp` is where server-to-client notifications arrive. The session belongs to
the credential that opened it, so an id is not shared between principals — refreshing an access
token keeps the session, presenting it as someone else does not. A `POST`, or the `GET`
notification leg, whose session is gone for its caller — idled out, ended by a restart, or opened
by another principal — is answered `404`; a `DELETE` is answered `202` either way, since rmcp
answers any id that way, so it is no signal that the id was yours. After a `404` the client starts
a new session with `initialize`, subscribes again, and calls `resources/list` again if it relies on
`list_changed`, since both the subscriptions and the list watch end with their session; a client
that switches to a different identity does the same rather than carrying the old id over. Every
client listed above keeps the session id on its own; a hand-rolled one must also handle the `404`.
Point the client at the URL, send the bearer, done. The legacy SSE transport is deliberately not
offered ([why](API.md#streamable-http-transport)); a hand-rolled client needs the three-step dance
in [the transport notes](API.md#streamable-http-transport).

With an events backend, the server also pushes resource notifications
([Subscriptions](API.md#subscriptions)): Claude Desktop, Claude Code and the MCP Inspector
subscribe to resources they display and refresh on `notifications/resources/updated`;
`mcp-remote` forwards the notifications to whatever it bridges; a client that never calls
`resources/subscribe` is unaffected. To see the tool list without any client, the MCP Inspector
works:

```sh
npx @modelcontextprotocol/inspector --transport http --server-url https://notes.example.com/mcp \
  --header "Authorization: Bearer $NOTEDTHAT_API_TOKEN"
```

## Anything that does not

The MCP layer is a thin proxy over the HTTP API; there is nothing a tool call can do that a
`curl` cannot. `GET /llms.txt` returns a document written for a model that has been handed only
the base URL and a token — it explains the routes, when a credential is needed, and how search
works — so an agent framework with a generic HTTP tool and no MCP support can still be pointed at
a deployment and told to read that first.

## Install options

Nothing on the client side. These are the ways to install `notedthat-server` itself, which
serves `/mcp`; the [README](../README.md) covers running it with Compose.

**Shell installer** (macOS / Linux):

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/NotedThat/NotedThat/releases/latest/download/notedthat-installer.sh | sh
```

**cargo install** (any platform with a Rust toolchain; the only route for Windows at the moment,
since prebuilt Windows binaries are not published yet):

```sh
cargo install notedthat
```

**From a checkout:**

```sh
cargo install --path crates/notedthat --locked
```

Every prebuilt binary is cosign-signed with a SLSA L2 build provenance attestation — verify before
running:

```sh
cosign verify-blob \
  --bundle notedthat-x86_64-unknown-linux-gnu.tar.xz.bundle \
  --certificate-identity-regexp 'https://github.com/NotedThat/NotedThat/.+' \
  --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \
  notedthat-x86_64-unknown-linux-gnu.tar.xz
```
