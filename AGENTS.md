# Repository Guidelines

## Project Structure & Module Organization

This is a Rust Cargo workspace. The main Warp application crate lives in `app/`, with source under `app/src/`, examples under `app/examples/`, and app-specific tests under `app/tests/`. Shared libraries live in `crates/*`; notable crates include `crates/warpui/` and `crates/warpui_core/` for the custom UI framework, `crates/editor/`, `crates/graphql/`, `crates/ipc/`, and `crates/integration/` for end-to-end style coverage. Bundled assets and skills are in `resources/`, platform/build helpers are in `script/`, packaging metadata is in `app/channels/`, and feature specs are stored under `specs/GH<issue-number>/`.

## Build, Test, and Development Commands

- `./script/bootstrap`: install platform-specific build dependencies and common agent skills.
- `./script/run` or `cargo run`: build and run Warp locally.
- `cargo run --features with_local_server`: run the client against a local warp-server.
- `./script/presubmit`: run the full local gate: formatting, clippy, C/C++ formatting checks, nextest, and doc tests.
- `cargo nextest run --no-fail-fast --workspace --exclude command-signatures-v2`: run the main Rust test suite.
- `cargo test --doc`: run Rust doc tests.

## Offline Slim Windows Package

Use this flow for the current offline OSS Windows package. It intentionally disables the default feature set and should not include `remote_server_support`, `cloud_conversations`, `cloud_mode`, `firebase_auth`, telemetry, autoupdate, or crash-reporting features unless the offline packaging goal changes.

Prerequisites:

- Rust target: `rustup target add x86_64-pc-windows-gnu`
- MinGW cross tools providing `x86_64-w64-mingw32-strip`
- `zip`

Feature set used for the slim package:

```sh
--no-default-features --features offline_oss,release_bundle,nld_classifier_v1,nld_heuristic_v1 --target x86_64-pc-windows-gnu
```

Before packaging, run the offline Windows check:

```sh
cargo check -p warp --bin warp-oss --no-default-features --features offline_oss,release_bundle,nld_classifier_v1,nld_heuristic_v1 --target x86_64-pc-windows-gnu
```

Build and package the executable:

```sh
cargo fmt
CARGO_FULL_PROFILE=ross CARGO_BIN_NAME=oss WARP_APP_NAME=WarpOss cargo build -p warp --profile ross --bin warp-oss --no-default-features --features offline_oss,release_bundle,nld_classifier_v1,nld_heuristic_v1 --target x86_64-pc-windows-gnu
mkdir -p dist
rm -f dist/WarpOss-offline-windows-x86_64.exe dist/WarpOss-offline-windows-x86_64.zip
cp target/x86_64-pc-windows-gnu/ross/warp-oss.exe dist/WarpOss-offline-windows-x86_64.exe
x86_64-w64-mingw32-strip dist/WarpOss-offline-windows-x86_64.exe
(cd dist && zip -9 -q WarpOss-offline-windows-x86_64.zip WarpOss-offline-windows-x86_64.exe)
```

Verify the package:

```sh
file dist/WarpOss-offline-windows-x86_64.exe
ls -lh dist/WarpOss-offline-windows-x86_64.exe dist/WarpOss-offline-windows-x86_64.zip
sha256sum dist/WarpOss-offline-windows-x86_64.exe dist/WarpOss-offline-windows-x86_64.zip
x86_64-w64-mingw32-objdump -p dist/WarpOss-offline-windows-x86_64.exe | rg 'DLL Name'
cargo tree -p warp --no-default-features --features offline_oss,release_bundle,nld_classifier_v1,nld_heuristic_v1 --target x86_64-pc-windows-gnu -i remote_server
```

The `cargo tree ... -i remote_server` command should report that `remote_server` does not match any packages; that confirms the external remote server crate is not linked into the offline package. The offline build still needs the no-op `RemoteServerManager` and `RemoteCodebaseIndexModel` singletons registered in `app/src/lib.rs`; otherwise startup can panic when `workspace/view.rs` subscribes to `RemoteServerManager`.

## Coding Style & Naming Conventions

Use `cargo fmt`; `.rustfmt.toml` sets Rust 2018 formatting. Clippy must pass with warnings denied. Prefer concise imports over long path qualifiers, inline format args such as `format!("{name}")`, and exhaustive `match` arms instead of `_` where practical. Context parameters named `ctx` should usually be last. Remove unused parameters instead of prefixing them with `_`. Unit test files commonly use `*_tests.rs` or `mod_test.rs`.

## Testing Guidelines

Add regression tests for bug fixes and unit tests for non-trivial logic. Place Rust unit tests beside the module and include separate test files with `#[cfg(test)]` and `#[path = "file_tests.rs"] mod tests;`. Use `crates/integration/` for user-facing flows that can be exercised end to end. Before submitting, run `./script/presubmit` and include proof of manual testing for UI or interactive changes.

## Commit & Pull Request Guidelines

The local git history is minimal, so follow the documented repository convention: branch from `master`, prefix branch names with your handle, for example `alice/fix-parser`, and write commit messages that explain what changed and why. PRs should be focused, linked to a ready issue when applicable, describe testing performed, include screenshots or recordings for visual changes, and add a changelog entry unless the change is docs-only or refactoring-only.

## Security & Configuration Tips

Do not open public issues for security vulnerabilities; follow `SECURITY.md`. Avoid committing local secrets or machine-specific configuration. For local server development, set `SERVER_ROOT_URL` and `WS_SERVER_URL` only in your shell environment.
