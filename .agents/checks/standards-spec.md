---
name: standards-spec
description: Does the change follow the repository's documented standards, and does it do what its spec asked?
turn-limit: 40
paths: ["crates/**/*.rs", "docs/**/*.md", "SPECIFICATIONS.md"]
---

<!--
Adapted from mattpocock/skills, skills/engineering/code-review/SKILL.md
@ 3cca18b368ae (MIT, © Matt Pocock): the two-axis review and the smell
baseline. Rewritten as a single-pass Goose check for this repository.
-->

You review a NotedThat pull request along two separate axes. Report each
finding under exactly one of them, and start its `summary` with
`Standards:` or `Spec:`.

## Standards

Sources, in order of authority: DEVELOPMENT.md, docs/CONFIGURATION.md and
SPECIFICATIONS.md §5 (Design Principles). Read the parts that apply to the
changed code. Report a place where the change breaks a documented rule and
cite the rule (file and the rule itself). Skip anything `cargo fmt`, clippy
or CI already enforces.

On top of the documented rules, flag these code smells as judgement calls
(always `low`, and a documented rule that endorses the pattern wins):
duplicated logic across the change, a name that hides what it holds, a
function that mostly works on another type's data, the same few parameters
travelling together, a primitive standing in for a domain concept, the same
`match` on the same type repeated, abstraction or options no current caller
needs, and wrappers that only delegate.

## Spec

Sources: SPECIFICATIONS.md §2 (Decisions Log) — the numbered decisions such as
D18 — and the pull request description included with this review. Report:

- a requirement of a referenced decision or issue that is missing or only
  partly done;
- behaviour the change adds that no decision or the description asks for;
- an implementation that contradicts the decision it claims to implement.

Quote the decision or the description line for each finding. If the change
references no decision and the description states no requirement, skip this
axis.

## Severity

`medium` for a clear breach of a documented rule or a decision; `low` for
smells and minor drift. This check never reports `high`.

Few, concrete findings beat many vague ones. When unsure, leave it out.
