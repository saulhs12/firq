# Releasing Firq

This document defines the release flow for crates.io and GitHub.

## Preconditions

Run release gates from repository root:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test -p firq-core
cargo test -p firq-async
cargo test -p firq-tower --test integration
```

## Publish order

Publish crates in dependency order:

1. `firq-core`
2. `firq-async`
3. `firq-tower`

`firq-examples` and `firq-bench` are marked `publish = false`.

## Dry runs

```bash
cargo publish --dry-run -p firq-core
cargo publish --dry-run -p firq-async
cargo publish --dry-run -p firq-tower
```

Note: `firq-async` dry-run requires `firq-core` available on crates.io,
and `firq-tower` dry-run requires both `firq-core` and `firq-async`.

## Publish commands

```bash
cargo publish -p firq-core
cargo publish -p firq-async
cargo publish -p firq-tower
```

## Post-release

1. Verify crates are visible on crates.io.
2. Verify docs are built on docs.rs.
3. Create GitHub release notes with highlights and migration notes.
