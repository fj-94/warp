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

## Coding Style & Naming Conventions

Use `cargo fmt`; `.rustfmt.toml` sets Rust 2018 formatting. Clippy must pass with warnings denied. Prefer concise imports over long path qualifiers, inline format args such as `format!("{name}")`, and exhaustive `match` arms instead of `_` where practical. Context parameters named `ctx` should usually be last. Remove unused parameters instead of prefixing them with `_`. Unit test files commonly use `*_tests.rs` or `mod_test.rs`.

## Testing Guidelines

Add regression tests for bug fixes and unit tests for non-trivial logic. Place Rust unit tests beside the module and include separate test files with `#[cfg(test)]` and `#[path = "file_tests.rs"] mod tests;`. Use `crates/integration/` for user-facing flows that can be exercised end to end. Before submitting, run `./script/presubmit` and include proof of manual testing for UI or interactive changes.

## Commit & Pull Request Guidelines

The local git history is minimal, so follow the documented repository convention: branch from `master`, prefix branch names with your handle, for example `alice/fix-parser`, and write commit messages that explain what changed and why. PRs should be focused, linked to a ready issue when applicable, describe testing performed, include screenshots or recordings for visual changes, and add a changelog entry unless the change is docs-only or refactoring-only.

## Security & Configuration Tips

Do not open public issues for security vulnerabilities; follow `SECURITY.md`. Avoid committing local secrets or machine-specific configuration. For local server development, set `SERVER_ROOT_URL` and `WS_SERVER_URL` only in your shell environment.
