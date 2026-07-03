#!/usr/bin/env bash
# docs/manual-qa/webdav-gvfs.sh
# Non-interactive WebDAV walkthrough using cadaver (preferred) or curl.
# Requires: cadaver OR curl. Set DAV_URL, DAV_KB, DAV_USER, DAV_PASS.
set -euo pipefail

DAV_URL="${DAV_URL:-http://127.0.0.1:8081}"
DAV_KB="${DAV_KB:-notes}"
DAV_USER="${DAV_USER:-webdav-user-please-change}"
DAV_PASS="${DAV_PASS:-webdav-pass-please-change}"

assert() {
    local desc="$1" expected="$2" actual="$3"
    if [ "$actual" != "$expected" ]; then
        echo "FAIL: $desc — expected '$expected', got '$actual'" >&2
        exit 1
    fi
    echo "PASS: $desc"
}

# Probe for cadaver or curl
if command -v cadaver >/dev/null 2>&1; then
    CLIENT=cadaver
elif command -v curl >/dev/null 2>&1; then
    CLIENT=curl
else
    echo "ERROR: neither cadaver nor curl found. Install one and retry." >&2
    exit 1
fi

echo "Using client: $CLIENT"
echo "Server: $DAV_URL"

# 1. OPTIONS — check DAV: 1
STATUS=$(curl -s -o /dev/null -w '%{http_code}' -X OPTIONS \
    -u "$DAV_USER:$DAV_PASS" "$DAV_URL/")
assert "OPTIONS returns 204" "204" "$STATUS"

DAV_HEADER=$(curl -s -D - -o /dev/null -X OPTIONS \
    -u "$DAV_USER:$DAV_PASS" "$DAV_URL/" | grep -i '^DAV:' | tr -d '\r\n')
echo "DAV header: $DAV_HEADER"

# 2. PROPFIND root
STATUS=$(curl -s -o /dev/null -w '%{http_code}' -X PROPFIND \
    -u "$DAV_USER:$DAV_PASS" -H 'Depth: 1' "$DAV_URL/")
assert "PROPFIND / returns 207" "207" "$STATUS"

# 3. PUT a markdown file
TEST_FILE="manual-qa-test-$(date +%s).md"
STATUS=$(curl -s -o /dev/null -w '%{http_code}' -X PUT \
    -u "$DAV_USER:$DAV_PASS" \
    -H 'Content-Type: application/octet-stream' \
    --data-binary "# Manual QA Test\n\nThis file was created by webdav-gvfs.sh." \
    "$DAV_URL/$DAV_KB/$TEST_FILE")
assert "PUT returns 201" "201" "$STATUS"

# 4. GET the file back
STATUS=$(curl -s -o /dev/null -w '%{http_code}' \
    -u "$DAV_USER:$DAV_PASS" "$DAV_URL/$DAV_KB/$TEST_FILE")
assert "GET returns 200" "200" "$STATUS"

# 5. DELETE the file
STATUS=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE \
    -u "$DAV_USER:$DAV_PASS" "$DAV_URL/$DAV_KB/$TEST_FILE")
assert "DELETE returns 204" "204" "$STATUS"

# 6. LOCK returns 405
STATUS=$(curl -s -o /dev/null -w '%{http_code}' -X LOCK \
    -u "$DAV_USER:$DAV_PASS" "$DAV_URL/$DAV_KB/test.md")
assert "LOCK returns 405" "405" "$STATUS"

# 7. PROPPATCH returns 405
STATUS=$(curl -s -o /dev/null -w '%{http_code}' -X PROPPATCH \
    -u "$DAV_USER:$DAV_PASS" "$DAV_URL/$DAV_KB/test.md")
assert "PROPPATCH returns 405" "405" "$STATUS"

# 8. Missing auth returns 401
STATUS=$(curl -s -o /dev/null -w '%{http_code}' "$DAV_URL/")
assert "No auth returns 401" "401" "$STATUS"

echo ""
echo "All manual QA checks passed."
