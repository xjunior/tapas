# tapas

Small plates of your production data.

[![CI](https://github.com/xjunior/tapas/actions/workflows/ci.yml/badge.svg)](https://github.com/xjunior/tapas/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/xjunior/tapas)](https://github.com/xjunior/tapas/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Every team ends up wanting the same thing: a database that feels like production, without being production. A full
dump is too big, `LIMIT 1000` leaves orders pointing at users who aren't there, and the hand-rolled script someone wrote
two years ago stopped working at the third migration.

tapas takes a **sample** of a MySQL or Postgres database and writes it to a SQL file, or straight into another database
of the same kind. You describe what you want in a bit of YAML. tapas works out the order, runs what it can in parallel,
and makes sure no row points at one that got left behind.

```sh
brew install xjunior/tap/tapas
tapas tapas.yml --since '2 weeks' --source mysql://user:password@host/shop -o sample.sql
```

## What you get

- **Samplers that say what to take.** Plain SQL, a time window, a row limit, or "whatever belongs to the rows I
  already have".
- **References that hold.** `deps` pulls in every row your rows point at, and `of` takes the rows that belong to
  what's already in the sample. tapas also reads the source's foreign keys, so you only write the links the schema
  doesn't state.
- **One config, many sizes.** `--since '2 weeks'` for a laptop, `--since '3 months'` for a review environment.
- **No personal data.** Swap emails, names, SSNs and more for fakes that look real. The same email gets the same fake
  everywhere it appears, so data that matched still matches.
- **Fast.** Independent tables, samplers and chunks run at once over a pool of read-only connections, streaming rows
  instead of loading them.
- **No surprises.** `--plan` prints every query without touching the database.

## Install

With Homebrew, on macOS or Linux:

```sh
brew install xjunior/tap/tapas
```

Or grab the build for your platform from the [releases](https://github.com/xjunior/tapas/releases): Linux (x86_64 and
arm64, static) or macOS (Apple silicon and Intel). Each one comes with a SHA-256 checksum.

Or build it yourself, with Rust 1.89 or newer:

```sh
cargo install --git https://github.com/xjunior/tapas
```

## Quick start

Say what you want:

```yaml
# tapas.yml
samplers:
  - table: countries
    limit: all                    # every country

  - table: orders
    limit: { since: created_at }  # orders placed within --since
    deps:
      user_id: users.id           # plus the users who placed them

  - table: users
    where: admin = 1
    limit: all                    # plus every admin
    deps:
      country_id: countries.id    # each user brings their country

  - table: line_items
    of:
      order_id: orders.id         # the items of those orders
```

Have a look at the plan first, then run it and load the result:

```sh
tapas tapas.yml --since '2 weeks' --plan
tapas tapas.yml --since '2 weeks' --source mysql://user:password@host:3306/shop -o sample.sql
mysql shop_dev < sample.sql
```

Or skip the file and pour it straight into another database:

```sh
tapas tapas.yml --since '2 weeks' \
  --source mysql://user:password@host:3306/shop \
  --target mysql://root@localhost:3306/shop_dev
```

Postgres works the same way with `postgres://` URLs:

```sh
tapas tapas.yml --since '2 weeks' --source postgres://user:password@host:5432/shop -o sample.sql
psql shop_dev -f sample.sql
```

A few things worth knowing up front:

- tapas writes `INSERT` statements only, so the target needs its schema already.
- Source and target must be the same kind of database.
- Foreign keys are followed by default. To follow only the `deps` you wrote, add `foreign_keys: false` to the config
  or pass `--disable-foreign-keys`.
- Passwords can stay off the command line: see [Command line](docs/cli.md#connection-urls).

There's a fuller config in [tapas.example.yml](tapas.example.yml).

## Documentation

- [Configuration](docs/configuration.md): samplers, `of`, `deps`, foreign keys, names, time windows and cycles, plus
  [one file with every option](docs/configuration.md#everything-in-one-file).
- [Sanitization](docs/sanitization.md): replacing sensitive values with fakes, fixed values or templates.
- [Command line](docs/cli.md): every flag, connection URLs and their environment variables, and what the output
  looks like.
- [How it works](docs/how-it-works.md): planning, scheduling and cycles, for people changing tapas.
- [Limitations](docs/limitations.md): what tapas doesn't do yet.
- [Development](docs/development.md): building, testing and releasing.

What changed in each version lives in [CHANGELOG.md](CHANGELOG.md).

## Contributing

Issues and pull requests are welcome. [Development](docs/development.md) covers building, running the tests (you'll
need a MySQL and a Postgres, and Docker makes that a one-liner) and how commits become releases. The rules we write
code by are in [`.agents/rules/`](.agents/rules).

## License

MIT. See [LICENSE](LICENSE).
