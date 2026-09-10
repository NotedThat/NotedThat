# notedthat-storage-s3

S3 adapter over `aws-sdk-s3`; implements NotedThat's `Storage` trait.

Use it only from `notedthat-server` — `notedthat-api-http` depends on the `Storage`
trait alone, and no S3 dependency reaches it.

## Tests live in another crate

`cargo test -p notedthat-storage-s3` exercises no backend behaviour. The storage
integration suite is written once against `&dyn Storage` and expanded per backend, so it
sits with the expansions in `notedthat-storage-fs/tests/`:

- `storage_integration_s3.rs` — the shared scenarios over a SeaweedFS container.
- `storage_conformance_s3.rs` — `S3Storage` against `FsStorage`, so an operator flipping
  `NOTEDTHAT_STORAGE_BACKEND` gets the same behaviour.

Both need Docker and are `#[ignore]`. After changing `src/storage.rs`, run:

```sh
cargo test -p notedthat-storage-fs --locked -- --include-ignored
```

See DEVELOPMENT.md, "One suite, every backend", for why the suite is shaped this way.
