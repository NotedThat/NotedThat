# Consuming NotedThat events from NATS

With `NOTEDTHAT_EVENTS_BACKEND=nats`, every object change NotedThat publishes on
[`GET …/events`](API.md#get-apiv1knowledgebaseskb_slugevents) is a message on a JetStream stream
first. An application can read that stream directly, with a durable consumer of its own, instead
of holding an SSE connection open. You get what SSE cannot give you: several instances sharing
the work, acknowledgements, redelivery when an instance dies mid-message, and no HTTP connection
to keep alive.

This page is the contract for doing that: what the stream is called, what the subjects,
headers and payload are, and what NotedThat promises not to change. Connection settings are in
[CONFIGURATION.md](CONFIGURATION.md#nats-connection).

> **Access rules do not apply here.** The SSE route filters every event through the caller's
> `list` grant ([access rules](../README.md#access-rules)). A NATS consumer sees every event on the
> subjects it can read, for every knowledge base. Restrict consumers with
> [NATS accounts and subject permissions](#restricting-a-consumer), and treat subscribe
> permission on `notedthat.events.>` as the equivalent of `list` on everything.

## The stream

| | |
|---|---|
| Name | `NOTEDTHAT_NATS_STREAM`, default `notedthat-events` |
| Subjects | `notedthat.events.>` |
| Retention | `limits`: messages age out after `NOTEDTHAT_NATS_MAX_AGE_SECS` (default 7 days) |
| Storage, replicas | `NOTEDTHAT_NATS_STORAGE`, `NOTEDTHAT_NATS_REPLICAS` |

NotedThat creates the stream, owns it, and updates its settings on every start. **Do not edit
it, add subjects to it, or purge it**: a stream whose subjects are not exactly
`notedthat.events.>` refuses the server's startup. Attach consumers to it; consumers are yours.

## Subjects

```
notedthat.events.<kb>.<kind>
```

- `<kb>` is the knowledge base slug, `[a-z0-9-]{1,40}`, so it is always one subject token.
- `<kind>` is `written`, `deleted`, `indexed` or `index_failed`.

Filter with ordinary NATS wildcards: `notedthat.events.notes.written` (writes to `notes`),
`notedthat.events.*.deleted` (deletions anywhere), `notedthat.events.notes.>` (everything in
`notes`).

## Headers

| Header | Value |
|--------|-------|
| `NotedThat-Schema` | `object-event/1`. The payload's schema and major version. |
| `Nats-Msg-Id` | A unique id per publish. Two messages never share one. The broker uses it to drop a retried publish; you can use it as an idempotency key. |

## Payload

The payload is one JSON object: exactly the SSE frame's `data:` object, with the same fields,
meanings and presence rules as the [event schema](API.md#get-apiv1knowledgebaseskb_slugevents).
There is one difference. `summary` on `object.index_failed` is always present when the indexer
produced one, because there is no caller whose grant it could be withheld from.

```json
{"event":"object.written","kb":"notes","object_key":"inbox/memo.mp3","etag":"\"9a3f…\"","size":48213011,"mime":"audio/mpeg","mtime":1757950000,"source":"http","occurred_at":"2026-09-15T14:33:20Z"}
```

The stream sequence is the event id: the same number SSE sends as `id:` and accepts as
`Last-Event-ID`. It is global across knowledge bases and replicas.

## Delivery guarantees

These are the same guarantees SSE subscribers get ([delivery](API.md#get-apiv1knowledgebaseskb_slugevents)):

- **At least once.** A client that retries a write whose event could not be published produces
  a second event for the same change. On the `fs` backend a restart re-announces objects the
  index does not track. **Make your consumer idempotent**: compare `etag` with what you last
  processed, or check for the output you would produce.
- **Ordered per stream.** Messages are stored in publish order. With several instances on one
  durable consumer, processing order across instances is up to you. Anything that must happen
  in key order needs its own serialisation.
- **`object.indexed` follows its `object.written`.** It carries the `etag` that is now
  searchable. See *Indexing outcomes* under [the events route](API.md#get-apiv1knowledgebaseskb_slugevents).
- **Self-triggered loops.** A consumer that writes back into NotedThat sees its own writes.
  Filter by subject, `mime` or `object_key`.

## Attaching a consumer

Create a durable pull consumer with explicit acks. With the [`nats` CLI](https://github.com/nats-io/natscli):

```sh
nats consumer add notedthat-events transcriber \
  --filter 'notedthat.events.notes.written' \
  --pull --ack explicit --deliver all \
  --max-deliver 5 --wait 60s --replay instant --defaults
```

Then run as many instances as you like against it. JetStream hands each message to one of them,
and redelivers any message not acknowledged within `--wait`:

```sh
nats consumer next notedthat-events transcriber --count 10 --ack
```

[`crates/notedthat-events/examples/nats_consumer.rs`](../crates/notedthat-events/examples/nats_consumer.rs) is a complete worker in that shape (`cargo run -p notedthat-events --example nats_consumer`). `--deliver all` starts
from the oldest retained message; `--deliver new` starts from now. Unlike SSE, a durable consumer
remembers its position across disconnects. A consumer that falls behind by more than the
retention window loses the aged-out messages. Watch its pending count, and resync by listing
the knowledge base if it ever has to be recreated.

## Restricting a consumer

Give each application its own NATS user, allowed to read only its subjects and to drive only
its own consumer:

```
authorization {
  users = [
    { user: transcriber, password: "…", permissions: {
        publish:   { allow: ["$JS.API.CONSUMER.MSG.NEXT.notedthat-events.transcriber",
                             "$JS.API.CONSUMER.INFO.notedthat-events.transcriber",
                             "$JS.API.STREAM.INFO.notedthat-events",
                             "$JS.ACK.notedthat-events.transcriber.>"] }
        subscribe: { allow: ["_INBOX.>"] }
    } }
  ]
}
```

Create the consumer as an administrator (the command above), not as the application. That way
the application cannot widen its own filter. In a decentralised-auth deployment, express the
same rules in the user's JWT.

## Versioning

`object-event/1` is stable:

- **Fields may be added** to the payload, and new `event` kinds may appear on new subject
  tokens, within `/1`. Ignore fields you do not know, and ignore kinds you do not handle.
- **Nothing is removed or changes meaning within `/1`.**
- **A breaking change** gets a new `NotedThat-Schema` value (`object-event/2`) and a new subject
  root, and is called out in the changelog.
