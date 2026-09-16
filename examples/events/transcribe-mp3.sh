#!/usr/bin/env bash
# examples/events/transcribe-mp3.sh
#
# A worker on the object change event stream: every mp3 uploaded under a prefix is
# downloaded, "transcribed", and the text written back beside it as an ordinary
# Markdown object — the motivating case for `GET /api/v1/knowledgebases/{kb}/events`.
#
#   docker compose -f docker-compose.yml -f docker-compose.events.yml up --build -d
#   NOTEDTHAT_API_TOKEN=dev-token-please-change examples/events/transcribe-mp3.sh
#   # in another shell:
#   curl -T memo.mp3 -H 'Authorization: Bearer dev-token-please-change' \
#        -H 'Content-Type: audio/mpeg' http://127.0.0.1:8080/api/v1/knowledgebases/notes/inbox/memo.mp3
#
# Requires: curl, jq. Set NOTEDTHAT_URL, NOTEDTHAT_API_TOKEN, KB, PREFIX, TRANSCRIBE.
#
# Two things keep the worker from chasing its own tail:
#   - it subscribes with `mime=audio/*`, so the `text/markdown` object it writes back is
#     never an event it receives;
#   - it skips an mp3 whose `.md` already exists. The `fs` backend's startup comparison
#     re-announces every object the index does not track (audio included) on each restart,
#     and a retried upload can publish twice — delivery is at least once, so the worker
#     is idempotent on the output it would produce.
#
# The stream is resumed with `Last-Event-ID` after a disconnect; a `410` means the log no
# longer holds that position, and the worker lists the prefix instead.
set -euo pipefail

NOTEDTHAT_URL="${NOTEDTHAT_URL:-http://127.0.0.1:8080}"
NOTEDTHAT_API_TOKEN="${NOTEDTHAT_API_TOKEN:?set NOTEDTHAT_API_TOKEN}"
KB="${KB:-notes}"
PREFIX="${PREFIX:-inbox/}"
# The command that turns an audio file on stdin into text on stdout. The default is a
# stub so the example runs anywhere; point it at whisper, or a hosted API, for real.
TRANSCRIBE="${TRANSCRIBE:-transcribe_stub}"

for tool in curl jq; do
    command -v "$tool" >/dev/null 2>&1 || { echo "ERROR: $tool not found" >&2; exit 1; }
done

auth=(-H "Authorization: Bearer $NOTEDTHAT_API_TOKEN")
kb_url="$NOTEDTHAT_URL/api/v1/knowledgebases/$KB"
object_url() { printf '%s/%s' "$kb_url" "$1"; }

transcribe_stub() {
    local bytes
    bytes=$(wc -c | tr -d ' ')
    printf '# Transcript\n\n(%s bytes of audio; replace TRANSCRIBE with a real transcriber)\n' "$bytes"
}

# Download the object, transcribe it, PUT `<key>.md` beside it — unless that already exists.
handle() {
    local key="$1" etag="$2" out="$1.md"
    if curl -fsS -o /dev/null "${auth[@]}" -I "$(object_url "$out")" 2>/dev/null; then
        echo "skip  $key  ($out exists)"
        return
    fi
    echo "work  $key  etag=$etag"
    local tmp
    tmp=$(mktemp)
    curl -fsS "${auth[@]}" -o "$tmp" "$(object_url "$key")"
    # If-None-Match: * makes two workers racing on the same key harmless.
    "$TRANSCRIBE" < "$tmp" | curl -fsS -o /dev/null "${auth[@]}" \
        -X PUT -H 'Content-Type: text/markdown' -H 'If-None-Match: *' \
        --data-binary @- "$(object_url "$out")" \
        && echo "wrote $out" || echo "lost  $out  (another worker got there first)"
    rm -f "$tmp"
}

# Resync by listing when the stream cannot replay from where we were.
resync() {
    echo "resync: listing $PREFIX"
    curl -fsS "${auth[@]}" "$kb_url?prefix=$PREFIX&limit=1000" \
        | jq -r '.objects[] | select(.key | test("\\.mp3$")) | [.key, .etag] | @tsv' \
        | while IFS=$'\t' read -r key etag; do handle "$key" "$etag"; done
}

last_id=""
while :; do
    headers=(-H 'Accept: text/event-stream')
    [ -n "$last_id" ] && headers+=(-H "Last-Event-ID: $last_id")
    echo "subscribe: $kb_url/events?prefix=$PREFIX&mime=audio/*  (after ${last_id:-now})"

    status_file=$(mktemp)
    # `-N` disables curl's buffering so each frame is seen as it arrives. The stream
    # is read line by line; `id:` is remembered for the reconnect, `data:` is the event.
    curl -sN "${auth[@]}" "${headers[@]}" -w '%{http_code}' -o >(
        while IFS= read -r line; do
            case "$line" in
                id:*)   last_id="${line#id:}"; last_id="${last_id# }"; echo "$last_id" > "$status_file.id" ;;
                data:*) payload="${line#data:}"
                        key=$(printf '%s' "$payload" | jq -r .object_key)
                        etag=$(printf '%s' "$payload" | jq -r '.etag // empty')
                        [ "$(printf '%s' "$payload" | jq -r .event)" = "object.written" ] && handle "$key" "$etag" ;;
            esac
        done
    ) "$kb_url/events?prefix=$PREFIX&mime=audio/*" > "$status_file" || true

    code=$(cat "$status_file")
    [ -f "$status_file.id" ] && last_id=$(cat "$status_file.id")
    rm -f "$status_file" "$status_file.id"
    case "$code" in
        410) echo "gone: events after $last_id are no longer retained"; last_id=""; resync ;;
        404) echo "ERROR: events are not enabled on $NOTEDTHAT_URL (NOTEDTHAT_EVENTS_BACKEND)" >&2; exit 1 ;;
        401|403) echo "ERROR: refused ($code); check NOTEDTHAT_API_TOKEN and the manifest's list grant" >&2; exit 1 ;;
        *) echo "disconnected ($code); reconnecting in 3s" ;;
    esac
    sleep 3
done
