# Development

## Build

```sh
cargo build --release
```

The binary lands in `target/release/tapas`. tapas needs Rust 1.89 or newer.

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
| `tests/target.rs` | Writing straight into a target database, and refusing to write into the source. |
| `tests/cycles.rs` | A table that follows itself, two tables that follow each other, and named samplers. |

Each one creates its own databases and drops them when it passes.

## Lint

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Both must be clean; CI runs the same.

## Rules

Code, tests and docs follow the rules in [`.agents/rules/`](../.agents/rules): the style guide, tests first,
cleanup, performance, design and documentation. `.claude` links to `.agents`, so Claude Code reads the same rules.

## Benchmark

Changes to the per-row paths need before and after numbers. Seed a sample big enough to measure:

```sh
docker run -d --name tapas-mysql -e MYSQL_ROOT_PASSWORD=tapas -p 3399:3306 mysql:8.4
docker exec -i tapas-mysql mysql -uroot -ptapas <<'SQL'
SET SESSION cte_max_recursion_depth = 1000000;
CREATE DATABASE bench; USE bench;
CREATE TABLE users (id BIGINT PRIMARY KEY, name VARCHAR(40), token VARBINARY(16), bio TEXT, created_at DATETIME);
CREATE TABLE orders (id BIGINT PRIMARY KEY, user_id BIGINT NOT NULL, total DECIMAL(10,2), KEY (user_id));
INSERT INTO users WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1000000)
  SELECT i, CONCAT('user ', i), UNHEX(MD5(i)), REPEAT(CONCAT('bio ', i, ' '), 6), NOW() FROM n;
INSERT INTO orders SELECT id * 2 + k, id, k FROM users, (SELECT 0 k UNION SELECT 1) ks;
SQL
```

Then time a release build of each version with `/usr/bin/time -l` (macOS) or `/usr/bin/time -v` (Linux), and report
the wall time and the maximum resident size:

```yaml
samplers:
  - table: users
    where: id % 2 = 0
  - table: users
    where: id % 3 = 0
  - table: orders
    of:
      user_id: users.id
```

```sh
cargo build --release
/usr/bin/time -l target/release/tapas bench.yml --source mysql://root:tapas@127.0.0.1:3399/bench -o bench.sql
```

## Commits

Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/). They decide the next version
and what goes in the changelog:

| Prefix | Means | Version |
| --- | --- | --- |
| `feat:` | Something new for people using tapas | Minor |
| `fix:` | A bug fixed | Patch |
| `feat!:`, or a `BREAKING CHANGE:` footer | Configs or flags that worked before no longer do | Major |
| `docs:`, `ci:`, `test:`, `refactor:`, `chore:` | Nothing users notice | None |

While tapas is below 1.0, breaking changes bump the minor version instead.

## Releasing

Releases are cut by [release-please](https://github.com/googleapis/release-please):

1. Every push to `main` updates an open release PR with the next version and the changelog entries since the last
   release.
2. Merging that PR bumps `Cargo.toml`, tags `vX.Y.Z`, and publishes a GitHub release.
3. The same workflow builds tapas for Linux (x86_64 and arm64, static musl) and macOS (Apple silicon and Intel), and
   attaches each build to the release with a SHA-256 checksum.
4. It then points the formula in [xjunior/homebrew-tap](https://github.com/xjunior/homebrew-tap) at the new builds.

To build and publish an existing tag again, run the **Release** workflow by hand with that tag.

### Setup

- **Release PRs:** in the repository's Actions settings, allow GitHub Actions to create pull requests.
- **Homebrew:** add a `HOMEBREW_TAP_TOKEN` secret holding a fine-grained token with read and write access to the
  contents of `xjunior/homebrew-tap`. Without it, releases still build, and the workflow warns that the formula wasn't
  updated.
