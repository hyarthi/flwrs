# Idea: distributing trellis beyond crates.io

Status: idea, not scheduled (deferred, Nov 2026). Today trellis is `cargo install flwrs-trellis --locked`, or a pinned dev-dependency with a one-line `examples/trellis.rs` for Rust plugin repos.

## Problem

Trellis is for plugin authors in any language, but installing it needs a Rust toolchain. A Python, Go, Java or TypeScript author shouldn't need cargo to check their plugin.

## Options

- **Prebuilt release binaries** per platform on GitHub releases (Linux x86_64 / aarch64 musl, macOS universal, Windows x86_64), built in CI on each `flwrs-trellis-v*` tag. Also enables `cargo binstall flwrs-trellis`.
- **npm** (`npx @flwrs/trellis …`): a thin package with per-platform optional dependencies carrying the binary (the esbuild pattern).
- **Python wheel**: maturin with `bindings = "bin"` (`pipx run flwrs-trellis …`).
- **Go**: a `go tool`-friendly wrapper module that downloads the matching release binary.
- **A CI container image** (`ghcr.io/…/trellis`) with the binary, for pipelines in any language.

## Open questions

- Which channels first: release binaries are the prerequisite for all the wrappers, so they come first; then probably npm and the wheel.
- Signing and checksums for the binaries; who maintains the wrappers' versions (one per trellis release, generated in CI).
- The demo plugins' `build.sh` could then call `trellis package` instead of zipping by hand.
