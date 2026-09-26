#!/usr/bin/env bash
# Render a Goose provider template from .github/goose/providers/ for one
# egress-proxy route, e.g. https://proxy.example/albert.
#
# Goose takes only the origin from `base_url` and drops any path, so the
# route's path goes in front of `base_path` instead. The templates carry
# example.invalid placeholders so the proxy's address stays out of the
# repository; a render that leaves one behind is an error.
#
# Usage: goose-render-provider.sh <template.json> <route-url> <custom_providers dir>
set -euo pipefail

template=$1
route=${2%/}
out_dir=$3

origin=$(grep -oE '^https?://[^/]+' <<<"$route") || {
  echo "::error::route is not an http(s) URL" >&2
  exit 1
}
path=${route#"$origin"}

mkdir -p "$out_dir"
out="$out_dir/$(basename "$template")"
jq --arg origin "$origin" --arg path "$path" \
  '.base_url = $origin | .base_path = ($path + "/chat/completions")' \
  "$template" >"$out"

if grep -q 'example\.invalid' "$out"; then
  echo "::error::$out still contains an example.invalid placeholder" >&2
  exit 1
fi
