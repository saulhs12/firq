# Contributing to Firq

Thanks for contributing to Firq.

## Development setup

1. Fork and clone the repository.
2. Install stable Rust.
3. Run local checks:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test -p firq-core
cargo test -p firq-async
cargo test -p firq-tower --test integration
```

## Pull request guidelines

- Keep PRs focused and small.
- Include tests for behavior changes.
- Update public docs when API behavior changes.
- Use clear commit messages and PR descriptions.
- Do not introduce breaking API changes without documenting migration impact.

## Project conventions

- Language: English for user-facing docs and code-facing text.
- Formatting/linting: PRs must pass `fmt` and `clippy -D warnings`.
- Public API: prefer additive changes over silent behavior changes.

## Reporting bugs

Use the bug report template and include:

- Environment (`rustc --version`, OS, architecture)
- Reproduction steps
- Expected vs actual behavior
- Minimal code sample if possible

## Security issues

Do not open public issues for sensitive vulnerabilities.
Follow `SECURITY.md`.
