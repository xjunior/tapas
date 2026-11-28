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

The unit tests need nothing else. The integration tests need a MySQL server and a Postgres server they can create
databases and roles on. Each kind skips itself when its URL isn't set, unless `CI` is set: there a missing URL fails
the run, so a broken setup can't pass for a green one.

```sh
docker run -d --name tapas-mysql -e MYSQL_ROOT_PASSWORD=tapas -p 3399:3306 mysql:8.4
docker run -d --name tapas-postgres -e POSTGRES_PASSWORD=tapas -p 5499:5432 postgres:18
TAPAS_TEST_DATABASE_URL=mysql://root:tapas@127.0.0.1:3399 \
  TAPAS_TEST_POSTGRES_URL=postgres://postgres:tapas@127.0.0.1:5499 cargo test
docker rm -f tapas-mysql tapas-postgres
```

The integration tests live in two binaries, `tests/mysql` and `tests/postgres`, so CI can run each against its own
database. `tests/mysql/support.rs` holds what the MySQL tests share: sampling a seeded database into an empty one,
and a few queries.

| Test | Covers |
| --- | --- |
| `mysql/end_to_end.rs` | Several samplers per table, limits across chunks, `--since`, and values that make the trip unchanged. |
| `mysql/references.rs` | `deps` pulling in the rows it points at, whatever the parent's own filters. |
| `mysql/foreign_keys.rs` | Reading foreign keys, following them like the `deps` that say the same, and turning them off with `--disable-foreign-keys` and `foreign_keys: false`. |
| `mysql/composite.rs` | `of` and `deps` over several columns, where a user's id alone doesn't say which user it is. |
| `mysql/reach.rs` | `limit` with `rows` and `since` together, and tables that only `deps` pull in. |
| `mysql/target.rs` | Writing straight into a target database, refusing to write into the source, and what errors show of a refused statement. |
| `mysql/errors.rs` | What errors show of a failed source query, with and without `--debug`. |
| `mysql/cycles.rs` | A table that follows itself, two tables that follow each other, and named samplers. |
| `mysql/sanitize.rs` | Every kind of replacement, fakes that agree across tables and runs, and refusing rules that break references. |
| `postgres/main.rs` | Postgres: values of many types that make the trip unchanged, cycles, sequences, composite references, foreign keys, and what a target user needs. |

Each one creates its own databases and drops them when it passes.

### In CI

One job compiles every test with [cargo-nextest](https://nexte.st) and archives the binaries. Then three jobs run
that archive at the same time: `unit` (no database), `mysql` and `postgres`, each with only the service it needs. To
run one the same way locally:

```sh
cargo nextest archive --archive-file tests.tar.zst
cargo nextest run --archive-file tests.tar.zst --workspace-remap . -E 'binary(mysql)'
```

## Lint

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Both must be clean; CI runs the same.

## Site

`site/` is the project's web page: one static file, published to GitHub Pages by `.github/workflows/pages.yml` when it changes on
`main`. Pages needs **Settings → Pages → Source: GitHub Actions**, once. Open `site/index.html` in a browser to preview it.

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
    limit: all
  - table: users
    where: id % 3 = 0
    limit: all
  - table: orders
    of:
      user_id: users.id
```

```sh
cargo build --release
/usr/bin/time -l target/release/tapas bench.yml --source mysql://root:tapas@127.0.0.1:3399/bench -o bench.sql
```

Postgres reads rows another way, so changes to its path need the same numbers from the same tables in Postgres:

```sh
docker run -d --name tapas-postgres -e POSTGRES_PASSWORD=tapas -p 5499:5432 postgres:18
docker exec -i tapas-postgres psql -U postgres <<'SQL'
CREATE DATABASE bench;
\c bench
CREATE TABLE users (id bigint PRIMARY KEY, name varchar(40), token bytea, bio text, created_at timestamp);
CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint NOT NULL, total numeric(10,2));
CREATE INDEX ON orders (user_id);
INSERT INTO users SELECT i, 'user ' || i, decode(md5(i::text), 'hex'), repeat('bio ' || i || ' ', 6), now()
  FROM generate_series(1, 1000000) i;
INSERT INTO orders SELECT id * 2 + k, id, k FROM users, (VALUES (0), (1)) ks(k);
SQL
/usr/bin/time -l target/release/tapas bench.yml --source postgres://postgres:tapas@127.0.0.1:5499/bench -o bench.sql
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
