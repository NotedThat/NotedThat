#!/usr/bin/env bash
# Render a Goose provider template from .github/goose/providers/ for one
# egress-proxy route, e.g. https://proxy.example/thirdparty.
#
# Goose takes only the origin from `base_url` and drops any path, so the
# route's path goes in front of `base_path` instead. The templates carry
# example.invalid placeholders so the proxy's address stays out of the
# repository; a render that leaves one behind is an error.
#
# A route is checked against one strict shape rather than repaired case by
# case: http(s)://host[:port] and zero or more path segments of unreserved
# characters. Anything else -- a query, a fragment, userinfo, an empty,
# `.` or `..` segment, whitespace inside it -- is an error. Whitespace around
# it (a secret pasted with a newline) and one trailing slash are the only
# things forgiven. Errors never print the route: it is a secret.
#
# Usage: goose-render-provider.sh <template.json> <route-url> <custom_providers dir>
#        goose-render-provider.sh --check <route-url>
#          checks the route and prints it normalised, for the workflow's
#          other uses of it; nothing is rendered.
set -euo pipefail

label='[A-Za-z0-9]([A-Za-z0-9-]{0,61}[A-Za-z0-9])?'
host="$label(\\.$label)*|\\[[0-9A-Fa-f:.]+\\]"
segment='[A-Za-z0-9._~-]+'
shape="^(https?://($host)(:[0-9]{1,5})?)(/$segment)*$"

# Sets `origin` and `path` from the route in $1, or exits with an error.
parse_route() {
  local route=$1
  route=${route#"${route%%[![:space:]]*}"}
  route=${route%"${route##*[![:space:]]}"}
  route=${route%/}
  if [[ ! $route =~ $shape ]]; then
    echo "::error::route must be http(s)://host[:port] with optional /segments of letters, digits, '.', '_', '~' or '-'; no query, fragment, userinfo or empty segment" >&2
    exit 1
  fi
  origin=${BASH_REMATCH[1]}
  path=${route#"$origin"}
  if [[ $origin =~ :([0-9]+)$ ]] && ((10#${BASH_REMATCH[1]} < 1 || 10#${BASH_REMATCH[1]} > 65535)); then
    echo "::error::route's port is not between 1 and 65535" >&2
    exit 1
  fi
  if [[ /$path/ == */./* || /$path/ == */../* ]]; then
    echo "::error::route has a '.' or '..' path segment" >&2
    exit 1
  fi
}

if [[ ${1:-} == --check ]]; then
  parse_route "${2-}"
  printf '%s\n' "$origin$path"
  exit 0
fi

template=$1
out_dir=$3
parse_route "$2"

mkdir -p "$out_dir"
out="$out_dir/$(basename "$template")"
jq --arg origin "$origin" --arg path "$path" \
  '.base_url = $origin | .base_path = ($path + "/chat/completions")' \
  "$template" >"$out"

if grep -q 'example\.invalid' "$out"; then
  echo "::error::$out still contains an example.invalid placeholder" >&2
  exit 1
fi
