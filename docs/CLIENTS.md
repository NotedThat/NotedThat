# Connecting clients

NotedThat speaks [MCP](https://modelcontextprotocol.io) — the protocol AI assistants and
automation tools use to call external tools. Anything that can act as an MCP client can list,
search, read, write, edit and delete notes, bound by the same [access rules](CONFIGURATION.md#manifest-access-rules)
as every other surface. Anything that cannot still has the plain [HTTP API](API.md), and
`GET /llms.txt` on the server explains that API to a model in its own words.

This page is setup snippets, one client at a time. The tools themselves are documented under
[MCP tools](API.md#tools).

## Two ways in

| | Remote (streamable HTTP) | Local (stdio) |
| --- | --- | --- |
| What the client talks to | `POST https://notes.example.com/mcp` | `notedthat-mcp-stdio`, a subprocess |
| Install anything? | No | `cargo install notedthat`, the shell installer, or `make mcp-stdio` — see [Install options](#install-options) |
| Credential | `Authorization: Bearer …` header, or OAuth via your identity provider | `NOTEDTHAT_TOKEN` (and `NOTEDTHAT_URL`) in the client's config |
| Use it when | The client runs somewhere else (n8n, Windmill, claude.ai), or supports OAuth | The client runs on your machine and only knows how to spawn a command |

Both present the same ten tools. The bearer can be `NOTEDTHAT_API_TOKEN` or an identity token
from your OIDC provider; the rules that apply are the caller's, so an agent holding a restricted
identity sees a restricted knowledge base. A knowledge base that is public — its manifest grants
`anyone` `read` or `search` — needs no credential over MCP either: leave the header out and the
client is the anonymous caller, with exactly what `anyone` may do and nothing more
([D57](../SPECIFICATIONS.md#2-decisions-log)).

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

Local, spawning the stdio adapter:

```sh
claude mcp add notedthat \
  --env NOTEDTHAT_URL=http://localhost:8080 \
  --env NOTEDTHAT_TOKEN="$NOTEDTHAT_API_TOKEN" \
  -- notedthat-mcp-stdio
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

Claude Desktop can also spawn the stdio adapter for a server on your own machine. Edit
`~/Library/Application Support/Claude/claude_desktop_config.json` (macOS) or
`%APPDATA%\Claude\claude_desktop_config.json` (Windows) and restart the app:

```json
{
  "mcpServers": {
    "notedthat": {
      "command": "notedthat-mcp-stdio",
      "env": {
        "NOTEDTHAT_URL": "http://localhost:8080",
        "NOTEDTHAT_TOKEN": "your-token-here"
      }
    }
  }
}
```

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
and the agent has the same ten tools an assistant would. For a plain script step, the HTTP API is a
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

Local:

```json
{
  "mcpServers": {
    "notedthat": {
      "command": "notedthat-mcp-stdio",
      "env": {
        "NOTEDTHAT_URL": "http://localhost:8080",
        "NOTEDTHAT_TOKEN": "your-token-here"
      }
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

`~/.config/zed/settings.json`:

```json
{
  "assistant": {
    "mcp_servers": {
      "notedthat": {
        "command": {
          "path": "notedthat-mcp-stdio",
          "env": {
            "NOTEDTHAT_URL": "http://localhost:8080",
            "NOTEDTHAT_TOKEN": "your-token-here"
          }
        }
      }
    }
  }
}
```

## Anything else that speaks MCP

The endpoint is a stateless streamable-HTTP MCP server: every `POST /mcp` is a complete JSON-RPC
exchange with a JSON response, no session to keep. Point the client at the URL, send the bearer,
done. The legacy SSE transport is deliberately not offered
([why](API.md#streamable-http-transport)). To see the tool list without any client, the MCP
Inspector works:

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

The stdio adapter, `notedthat-mcp-stdio`, ships from the single `notedthat` crate together with
`notedthat-server`; every route below installs the pair.

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

**From a checkout or the container image:**

```sh
make mcp-stdio                         # build from this checkout
make mcp-stdio-from-image              # extract from notedthat-server:local
# Override PREFIX=/some/dir or IMAGE=ghcr.io/notedthat/server:tag as needed.
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

The adapter takes `--url`/`--token` flags as well as the `NOTEDTHAT_URL`/`NOTEDTHAT_TOKEN`
variables, which helps with client configs that pass arguments more easily than environment. See
[MCP stdio client](CONFIGURATION.md#mcp-stdio-client-notedthat-mcp-stdio).
