# Blitz patch maintenance

The vendored crates are the published **0.3.0-beta.3** archives at revision
`5890fab3afb087d8c0c5b0652b8b635f161f3a13`, plus the ordered patches in
[`series`](series). Archive SHA-256 checksums are in [`upstream.toml`](upstream.toml).
Do not edit the vendored code without regenerating its patch.

```sh
bash scripts/blitz-patches.sh verify
bash scripts/blitz-patches.sh stage NEW_VERSION
```

Verification downloads and checks both archives, checks their source revision,
applies every patch without rejects, and compares every file with the vendored
trees. It also checks the application/probe pins, float feature, desktop/Android
patch wiring, and both resolved lockfiles.

## Beta.3 audit

The beta.2 queue contained eleven patches, including three successive rewrites
of table layout. Beta.3 changes the table collector, lazy layout styles,
out-of-flow layout, font loading, and text metrics. The queue is consolidated
into six patches so it no longer applies and then replaces intermediate table
implementations.

| Previous patch | Beta.3 disposition |
| --- | --- |
| 0001 bounded event queue | Retained as 0001; also bounds the new `fetch_font_face_rules` sender used by CSSOM. |
| 0002 table layout/borders | Consolidated into 0003/0005. Upstream handles invisible border widths, but still uses the first cell's border for the entire grid; per-edge conflict resolution, cell alignment, and anonymous cells remain local. |
| 0003 inline ancestor backgrounds | Retained in 0005; ancestor backgrounds still need the local traversal. |
| 0004 repository integration | Retained in 0006: objc2 dependency-scanner exception, unused paint euclid removal, and bounded JPEG decoder dependency. |
| 0005 reader selection/resources | Retained in 0002/0003. The image clone removal is already upstream and is dropped. Selection range checks, paragraph separators, resource notifications, visibility export, and clipping remain local. |
| 0006 floats-disabled line breaking | Retained in 0004; upstream still creates custom float boxes without a float-aware line breaker. |
| 0007 inline floats | Retained in 0004, adapted to lazy style access: bounded float width, correct margin basis, and non-looping height-limit failure. |
| 0008 image/painting performance | Retained in 0002/0003/0005/0006: decode limits and native JPEG scaling, separate intrinsic/pixel sizes, fixed-image paint invalidation, bounded line cache and conservative culling. |
| 0009 font hinting | Retained in 0005; uses Parley 0.12 font metrics and casts normalized coordinates as required by the new API. |
| 0010 automatic table sizing | Consolidated into 0003; content minimums, proportional authored columns, and wrapping remain necessary. |
| 0011 presentational hints/table fidelity | Consolidated into 0003/0005; retains cached legacy hints, rowspan handling, collapsed edges, and cell alignment. Preserves beta.3 language hints, column sizing, row-group order/backgrounds, height distribution, and positioned descendants. |

Table-cell layout adjustments use temporary computed-style overrides. They do
not mutate the author's primary style or its shared hint blocks. Each call
restores the previous override; table reconstruction refreshes cached styles.

Beta.3 requires Stylo 0.22, cssparser 0.38, html5ever 0.40, Taffy 0.15,
Parley/Fontique 0.12, and AnyRender 0.14. The CPU/GPU backends move to
AnyRender Vello CPU 0.18 and Vello 0.15 respectively (Vello 0.11/WGPU 30).
Slint 1.18.1 supports WGPU 30. Its Fontique 0.11 and Blitz's Fontique 0.12
both enable dynamic Fontconfig loading.

Beta.3 also removes the implicit available-width clamp on replaced elements.
The reader now supplies `img { max-width: 100% }` in its user-agent stylesheet
to preserve fitted images. Sender CSS can override that default. The existing
JPEG/PNG downsampling regression checks intrinsic dimensions, fitted layout,
and background-image scaling together.

Parley 0.12's published `parley_engine` includes the emoji-presentation state
machine previously backported locally. The application regressions for missing
fonts, plain digits, real emoji, and authored families remain the compatibility
gate. Slint still depends on Parley 0.11.1, so its existing backport and verifier
remain in place until Slint adopts a fixed version too.

## Upgrade validation

```sh
cargo test -p rustmail-renderer-probe --test blitz_upgrade --locked
cargo check --workspace --all-targets --locked
cargo check -p rustmail --no-default-features --locked
cargo check -p rustmail --no-default-features --features remote-content --locked
cargo check -p rustmail --features flatpak --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo test -p blitz-dom --lib --locked
cargo metadata --manifest-path platform/android/Cargo.toml --locked --format-version 1 > /dev/null
# With the NDK compiler environment used by scripts/build-android-test-apk.sh:
cargo check --manifest-path platform/android/Cargo.toml --target aarch64-linux-android --locked
# Requires a working WGPU adapter:
cargo test -p rustmail --lib issue_23_gpu --locked -- --ignored
```

`crates/renderer-probe/tests/blitz_upgrade.rs` covers the upstream/local table
overlap and locale-sensitive text. Application renderer tests cover float
wrapping, selection, image invalidation, caching, backgrounds, and emoji.
Refresh Flatpak's `cargo-sources.json` whenever the root lockfile changes.

The beta.3 upgrade passed all commands above on Linux, including the WGPU
rendering test and Android arm64 compilation. The Blitz library suite passed
86 tests; the focused upgrade suite passed eight. Formatting, patch reproduction,
Parley source verification, and Flatpak source/lockfile consistency also passed.
Android reports an unused `Command::key` method in unchanged `src/tabs.rs`;
the desktop Clippy check passed with warnings denied.
