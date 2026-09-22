#!/usr/bin/env bash
# docs/manual-qa/oidc-mcp.sh
# Non-interactive walkthrough of OIDC identity tokens against the Authelia
# overlay: docker compose -f docker-compose.yml -f docker-compose.auth.yml up --build -d
# Requires: curl, python3. Set NT_URL, NT_TOKEN, NT_KB, AUTH_URL, AUTH_CA.
#
# Runs in two phases because policies are a startup snapshot:
#   PHASE=1 (default)  the stock manifest — every signed-in caller may do
#                      everything except reach `.notedthat`; then writes a
#                      group-scoped manifest with the service token.
#   PHASE=2            after `docker compose ... restart notedthat-server`:
#                      editors write, interns are barred from hr/, alice alone
#                      deletes under personal/alice/.
set -euo pipefail

NT_URL="${NT_URL:-http://127.0.0.1:8080}"
NT_TOKEN="${NT_TOKEN:-dev-token-please-change}"
NT_KB="${NT_KB:-notes}"
AUTH_URL="${AUTH_URL:-https://auth.localhost:9091}"
AUTH_CA="${AUTH_CA:-$(dirname "$0")/../../docker/authelia/ca.crt}"
PHASE="${PHASE:-1}"

assert() {
    local desc="$1" expected="$2" actual="$3"
    if [ "$actual" != "$expected" ]; then
        echo "FAIL: $desc — expected '$expected', got '$actual'" >&2
        exit 1
    fi
    echo "PASS: $desc"
}

for tool in curl python3; do
    command -v "$tool" >/dev/null 2>&1 || { echo "ERROR: $tool not found" >&2; exit 1; }
done

# Authelia derives the issuer from the request host, so the host must ask for
# the same name the server was configured with (see docker-compose.auth.yml).
AUTH_HOST=$(python3 -c "import sys,urllib.parse as u; p=u.urlparse(sys.argv[1]); print(f'{p.hostname}:{p.port or 443}')" "$AUTH_URL")
auth_curl() { curl -s --cacert "$AUTH_CA" --resolve "$AUTH_HOST:127.0.0.1" "$@"; }

# Obtain an access token for a user through the authorization-code flow, using
# Authelia's first-factor API in place of the login page.
token_for() {
    local user="$1" pass="$2" jar
    jar=$(mktemp)
    auth_curl -c "$jar" -b "$jar" -o /dev/null -H 'Content-Type: application/json' \
        -d "{\"username\":\"$user\",\"password\":\"$pass\",\"keepMeLoggedIn\":false}" \
        "$AUTH_URL/api/firstfactor"
    local location code
    location=$(auth_curl -c "$jar" -b "$jar" -o /dev/null -w '%{redirect_url}' \
        "$AUTH_URL/api/oidc/authorization?client_id=notedthat&response_type=code&scope=openid%20profile%20groups&redirect_uri=http%3A%2F%2F127.0.0.1%3A1%2Fcallback&state=manual-qa-state")
    code=$(printf '%s' "$location" | sed -n 's/.*[?&]code=\([^&]*\).*/\1/p')
    rm -f "$jar"
    [ -n "$code" ] || { echo "ERROR: no authorization code for $user: $location" >&2; exit 1; }
    auth_curl -d "grant_type=authorization_code&code=$code&redirect_uri=http%3A%2F%2F127.0.0.1%3A1%2Fcallback&client_id=notedthat&client_secret=notedthat-client-secret" \
        "$AUTH_URL/api/oidc/token" | python3 -c 'import json,sys; print(json.load(sys.stdin)["access_token"])'
}

status() { curl -s -o /dev/null -w '%{http_code}' "$@"; }
obj() { printf '%s/api/v1/knowledgebases/%s/%s' "$NT_URL" "$NT_KB" "$1"; }

echo "Server: $NT_URL   Issuer: $AUTH_URL   Phase: $PHASE"

# 1. Discovery on both sides.
ISSUER=$(auth_curl "$AUTH_URL/.well-known/openid-configuration" | python3 -c 'import json,sys; print(json.load(sys.stdin)["issuer"])')
assert "Authelia reports the configured issuer" "$AUTH_URL" "$ISSUER"
AS=$(curl -s "$NT_URL/.well-known/oauth-protected-resource" | python3 -c 'import json,sys; print(json.load(sys.stdin)["authorization_servers"][0])')
assert "protected-resource metadata names the issuer" "$AUTH_URL" "$AS"

# 2. Tokens for both users.
ALICE=$(token_for alice alice-password)
IVAN=$(token_for ivan ivan-password)
CLAIMS=$(printf '%s' "$ALICE" | cut -d. -f2 | python3 -c 'import base64,json,sys; s=sys.stdin.read().strip(); print(json.dumps(json.loads(base64.urlsafe_b64decode(s+"="*(-len(s)%4)))))')
assert "alice's access token is a JWT carrying groups" "editors" "$(printf '%s' "$CLAIMS" | python3 -c 'import json,sys; print(json.load(sys.stdin)["groups"][0])')"

# 3. A refused bearer is 401 with the challenge; anonymous stays concealed.
assert "a bogus bearer is 401" "401" "$(status -H 'Authorization: Bearer not-a-token' "$(obj handbook.md)")"
CHALLENGE=$(curl -s -D - -o /dev/null -H 'Authorization: Bearer not-a-token' "$(obj handbook.md)" | grep -i '^www-authenticate:' | tr -d '\r\n')
case "$CHALLENGE" in *resource_metadata=*) echo "PASS: 401 carries the resource_metadata challenge";; *) echo "FAIL: no challenge on 401: '$CHALLENGE'" >&2; exit 1;; esac
# This holds because no knowledge base here grants `anyone` anything; with a public knowledge
# base, `auto` (the default) admits a bare request as the anonymous caller (D59) and only
# NOTEDTHAT_MCP_ANONYMOUS=never keeps the 401.
MCP_STATUS=$(status -X POST -H 'Content-Type: application/json' -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' "$NT_URL/mcp")
assert "MCP without a bearer is 401" "401" "$MCP_STATUS"

# 4. MCP acts as the caller. The transport is stateful (D66): initialize opens a
# session, the session id goes on every later request, and the answer is the
# data: line of an SSE frame.
mcp_call() { # <bearer> <json-rpc body> -> the JSON-RPC message
    local sid
    sid=$(curl -s -D - -o /dev/null -X POST -H "Authorization: Bearer $1" -H 'Accept: application/json, text/event-stream' -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"manual-qa","version":"0"}}}' "$NT_URL/mcp" \
        | awk 'tolower($1)=="mcp-session-id:"{print $2}' | tr -d '\r')
    curl -s -X POST -H "Authorization: Bearer $1" -H "Mcp-Session-Id: $sid" -H 'Accept: application/json, text/event-stream' -H 'Content-Type: application/json' \
        -d "$2" "$NT_URL/mcp" | sed -n 's/^data: //p' | grep -v '^$' | head -1
}
MCP_RESULT=$(mcp_call "$ALICE" '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_knowledgebases","arguments":{}}}' \
    | python3 -c 'import json,sys; d=json.load(sys.stdin); print("ok" if "result" in d and not d["result"].get("isError") else d)')
assert "MCP list_knowledgebases with alice's token" "ok" "$MCP_RESULT"

if [ "$PHASE" = "1" ]; then
    # 5. The stock manifest: signed-in may do everything, `.notedthat` excepted.
    assert "alice writes handbook.md" "201" "$(status -X PUT -H "Authorization: Bearer $ALICE" -H 'Content-Type: text/markdown' --data-binary '# Handbook' "$(obj handbook.md)")"
    assert "ivan reads handbook.md" "200" "$(status -H "Authorization: Bearer $IVAN" "$(obj handbook.md)")"
    assert "alice cannot read the manifest" "403" "$(status -H "Authorization: Bearer $ALICE" "$(obj .notedthat%2Fmanifest.json)")"
    assert "the service token can" "200" "$(status -H "Authorization: Bearer $NT_TOKEN" "$(obj .notedthat%2Fmanifest.json)")"
    assert "alice over WebDAV with a bearer" "200" "$(status -H "Authorization: Bearer $ALICE" "$NT_URL/webdav/$NT_KB/handbook.md")"

    # 6. Seed the objects and the group-scoped manifest for phase 2.
    for key in hr/salaries.md personal/alice/todo.md; do
        status -X PUT -H "Authorization: Bearer $NT_TOKEN" -H 'Content-Type: text/markdown' --data-binary "# $key" "$(obj "${key//\//%2F}")" >/dev/null
    done
    MANIFEST=$(curl -s -H "Authorization: Bearer $NT_TOKEN" "$(obj .notedthat%2Fmanifest.json)" | python3 -c '
import json,sys
m=json.load(sys.stdin)
m["access"]=[
  {"who":"signed-in","may":["list","read","search"]},
  {"who":"group:editors","may":["write","delete"]},
  {"who":"group:interns","may_not":["read","search"],"under":["hr/**"]},
  {"who":"user:alice","may":["delete"],"under":["personal/alice/**"]},
]
print(json.dumps(m))')
    assert "group-scoped manifest written" "201" "$(status -X PUT -H "Authorization: Bearer $NT_TOKEN" -H 'Content-Type: application/json' --data-binary "$MANIFEST" "$(obj .notedthat%2Fmanifest.json)")"
    echo
    echo "Phase 1 complete. Policies are a startup snapshot, so now:"
    echo "  docker compose -f docker-compose.yml -f docker-compose.auth.yml restart notedthat-server"
    echo "  PHASE=2 $0"
else
    # 5. The group-scoped manifest from phase 1.
    assert "ivan reads handbook.md" "200" "$(status -H "Authorization: Bearer $IVAN" "$(obj handbook.md)")"
    assert "ivan is barred from hr/" "403" "$(status -H "Authorization: Bearer $IVAN" "$(obj hr%2Fsalaries.md)")"
    assert "alice reads hr/" "200" "$(status -H "Authorization: Bearer $ALICE" "$(obj hr%2Fsalaries.md)")"
    assert "alice (editor) writes" "201" "$(status -X PUT -H "Authorization: Bearer $ALICE" -H 'Content-Type: text/markdown' --data-binary '# Handbook v2' "$(obj handbook.md)")"
    assert "ivan (intern) cannot write" "403" "$(status -X PUT -H "Authorization: Bearer $IVAN" -H 'Content-Type: text/markdown' --data-binary '# nope' "$(obj handbook.md)")"
    assert "alice deletes under personal/alice/" "204" "$(status -X DELETE -H "Authorization: Bearer $ALICE" "$(obj personal%2Falice%2Ftodo.md)")"
    assert "ivan's MCP read of hr/ is a forbidden tool error" "forbidden" "$(mcp_call "$IVAN" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"read\",\"arguments\":{\"kb\":\"$NT_KB\",\"path\":\"hr/salaries.md\"}}}" \
        | python3 -c 'import json,sys; print("forbidden" if "forbidden" in json.dumps(json.load(sys.stdin)) else "allowed")')"
    # The denial names read and search, not list: the page renders, and the row
    # the intern may not read is listed without a link (D52).
    assert "the intern's browse of hr/ renders" "200" "$(status -H "Authorization: Bearer $IVAN" "$NT_URL/browse/$NT_KB/hr/")"
    assert "…but salaries.md is listed as restricted, unlinked" "restricted" "$(curl -s -H "Authorization: Bearer $IVAN" "$NT_URL/browse/$NT_KB/hr/" | python3 -c 'import sys,re; h=sys.stdin.read(); print("linked" if re.search(r"<a [^>]*salaries", h) else ("restricted" if "restricted" in h else "missing"))')"
    echo
    echo "Phase 2 complete."
fi
