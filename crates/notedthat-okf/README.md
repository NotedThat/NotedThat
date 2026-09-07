# notedthat-okf

Open Knowledge Format (OKF) v0.2 support for [NotedThat](https://github.com/NotedThat/NotedThat).

OKF describes a knowledge bundle as a directory of Markdown files with YAML frontmatter, where every
non-reserved `.md` file is a *concept* carrying a required `type`. This crate is the pure,
I/O-free half of NotedThat's support for it:

- splitting frontmatter off a Markdown document, preserving absolute byte offsets;
- parsing that frontmatter into an `OkfConcept`, tolerantly, per OKF §11;
- deriving trust tier and staleness;
- conformance checking;
- parsing and surgically editing the reserved `index.md` and `log.md` files;
- resolving cross-links safely, inside the knowledge base.

## Non-execution invariant

`type: Attested Computation` concepts are **catalogued, never executed**. `computation`,
`executor.resource` and `attester.resource` are opaque strings here. This crate has no storage
client, no HTTP client and no filesystem access in scope — the invariant is enforced by
construction, not by convention.

See `SPECIFICATIONS.md` decision D48.

## License

MPL-2.0
