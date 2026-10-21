# Candle ZIP compatibility patch

The optional CPU embedding runtime uses the published `candle-core` 0.11.0
sources, with the ZIP 9 filename adaptation proposed in
[Candle PR #4041](https://github.com/huggingface/candle/pull/4041).
ZIP is raised to 9.0.4 because upstream withdrew all ZIP 8 releases for
[security issues](https://github.com/zip-rs/zip2/issues/1075).
The remaining Candle dependencies stay on the published 0.11.0 release.

`upstream.json` pins the original crates.io archive checksum.
`0001-zip9.patch` contains the complete delta, including upstream regression
tests for UTF-8 and CP437 filenames and malformed NPZ/PTH archives. A valid PTH
fixture test is also included; NPY/NPZ fixtures are embedded so these tests can
run from the application workspace. The unused `clap` development dependency is
removed so the repository dependency check also covers this source.

The release's `Cargo.toml.orig` is retained for provenance. Its standalone
lockfile is omitted: application builds and archive tests resolve the patched
manifest through the root `Cargo.lock`. Metadata tools may generate an ignored
standalone lockfile.

Run `python3 scripts/verify-candle.py` to reconstruct and compare every file.
Run the archive regression tests with:

```sh
cargo test -p rustmail-core --locked --features local-embeddings --test embedding_archives
```

Android does not enable local embeddings and has no Candle or ZIP dependency.
Remove this patch, its verification step, and the archive test target when a
published Candle release supports ZIP 9. Retain the regular embedding tests.

The vendored sources and adaptation retain Candle's Apache-2.0 license.
