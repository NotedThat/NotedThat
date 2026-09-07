# Open Knowledge Format

NotedThat can store and search [Open Knowledge Format (OKF)](https://github.com/GoogleCloudPlatform/knowledge-catalog/blob/main/okf/SPEC.md) concept documents. Each knowledge base is a bundle root: object paths preserve the bundle's directory layout, and a concept's ID is its path without `.md`.

## Writing concepts

Upload UTF-8 Markdown using the existing HTTP, MCP, or WebDAV write operations. A concept has YAML frontmatter at the start of a `.md` file, delimited by lines containing `---`, with a non-empty string `type`:

```markdown
---
type: Metric
title: Revenue
description: Revenue from completed sales.
tags: [finance, sales]
---
# Definition

Revenue is the sum of completed sales. See [orders](/tables/orders.md).
```

Any type name is supported. Optional string fields `title`, `description`, and `resource`, and string entries in `tags`, are exposed in search results. Other fields, including OKF v0.2 provenance, trust, lifecycle, and computation metadata, remain intact in the source document. NotedThat does not evaluate trust, freshness, or execute attestations.

Source bytes are preserved, including unknown YAML fields and relative or bundle-root links. Broken links are accepted. Clients resolve bundle-root links within the same knowledge base; the HTTP object API requires `/` inside an object key to be encoded as `%2F`.

`index.md` and `log.md` at any directory level remain ordinary searchable documents, not concepts. Index files are optional and are not generated automatically. Plain Markdown, non-OKF frontmatter, invalid YAML, and missing or invalid `type` fields retain ordinary Markdown indexing. Invalid optional metadata is ignored when extracting search fields. These tolerant reads do not certify that a bundle is conformant with OKF.

## Searching

For recognized concepts, only the Markdown body is embedded and chunked. Search byte offsets still address the original stored file, including its frontmatter. A concept with no body is stored and readable but has no search chunks. Replacing a document removes obsolete chunks after successful indexing, including when its body becomes empty or every new chunk exceeds the configured embedding input limit.

```sh
curl -sSf -X PUT \
  -H "Authorization: Bearer $NOTEDTHAT_API_TOKEN" \
  -H 'Content-Type: text/markdown' \
  --data-binary @examples/okf/metrics/revenue.md \
  http://127.0.0.1:8080/v1/knowledgebases/notes/metrics%2Frevenue.md

curl -sSf -X POST \
  -H "Authorization: Bearer $NOTEDTHAT_API_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"query":"revenue","filter":{"concept_type":"Metric","tags":["finance"]}}' \
  http://127.0.0.1:8080/v1/knowledgebases/notes/search
```

`concept_type` is an exact, case-sensitive match. `tags` matches at least one supplied tag. Different filter fields are AND-composed. MCP's `search` tool uses the same fields inside its `filters` argument.

Concept hits include an optional `okf` object:

```json
{
  "concept_id": "metrics/revenue",
  "type": "Metric",
  "title": "Revenue",
  "description": "Revenue from completed sales.",
  "tags": ["finance", "sales"]
}
```

Legacy and ordinary Markdown hits omit `okf`. Retrieve the full source through the existing read APIs to access all metadata. The portable [example bundle](../examples/okf/index.md) includes an index and linked concepts; upload each file at its corresponding relative path to retain the bundle layout.

## Existing deployments

Restart the upgraded server to provision the `tags` and `okf.type` payload indexes, including on existing Qdrant collections. Re-PUT existing concept files without changing their bytes to enqueue indexing with metadata and body-only chunks. Until then, their old search entries retain the previous indexing behavior. Storage needs no migration. Indexing remains asynchronous; allow it to finish before querying new metadata filters.
