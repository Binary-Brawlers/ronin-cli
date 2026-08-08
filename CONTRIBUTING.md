# Contributing to Ronin CLI

Thanks for helping improve Ronin. Bug reports, focused feature proposals, documentation fixes, and code contributions are welcome.

## Before opening a change

- Search existing issues and pull requests first.
- Open an issue before a large behavioral or architectural change.
- Keep server implementation details and credentials out of issues, fixtures, logs, and commits.
- Use a descriptive pull-request title such as `feat(cli): add ...` or `fix(update): handle ...`. GitHub uses merged pull-request titles and authors to generate release notes.

## Development

Install the Rust toolchain declared in `rust-toolchain.toml`, then run:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
```

Add or update tests for behavior changes. Do not depend on a live Ronin account in the automated test suite.

## Pull requests

Explain the user-visible outcome, call out security or compatibility implications, and list the checks you ran. Keep unrelated changes in separate pull requests so release notes remain useful.

By contributing, you agree that your contribution is licensed under this repository's MIT License.
