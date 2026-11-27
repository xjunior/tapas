# Development

## Build

```sh
cargo build --release
```

The binary lands in `target/release/tapas`. tapas needs Rust 1.88 or newer.

## Test

```sh
cargo test
```

The unit tests need nothing else. The end-to-end tests need a MySQL server they can create databases on, and skip
themselves without one:

```sh
docker run -d --name tapas-mysql -e MYSQL_ROOT_PASSWORD=tapas -p 3399:3306 mysql:8.4
TAPAS_TEST_DATABASE_URL=mysql://root:tapas@127.0.0.1:3399 cargo test
docker rm -f tapas-mysql
```

| Test | Covers |
| --- | --- |
| `tests/end_to_end.rs` | Several samplers per table, limits across chunks, `--since`, and values that make the trip unchanged. |
| `tests/references.rs` | `deps` pulling in the rows it points at, whatever the parent's own filters. |
| `tests/cycles.rs` | A table that follows itself, two tables that follow each other, and named samplers. |

Each one creates its own databases and drops them when it passes.

## Lint

```sh
cargo fmt --check
cargo clippy --all-targets
```

Both should be clean.

## Before a release

1. Move the entries under `Unreleased` in [CHANGELOG.md](../CHANGELOG.md) to a new version, dated.
2. Bump `version` in `Cargo.toml`.
3. Run the full test suite against MySQL.
