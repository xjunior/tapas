# tapas

Small plates of your production data.

tapas pulls a sample out of a MySQL database and writes it to a SQL file or straight into another database, keeping
every reference intact. You describe
what to take in YAML; tapas works out the order, runs everything it can in parallel, and makes sure no row points at
one that was left behind.

- **Samplers.** Pick rows per table with plain SQL, a time window, a limit, or by following other tables.
- **References that hold.** `deps` brings along every row your rows point at. `of` takes the rows that belong to what's
  already in the sample.
- **One config, many sizes.** `--since '2 weeks'` for a laptop, `--since '3 months'` for a review environment.
- **No personal data.** Replace emails, names, SSNs and more with fakes that look real. The same email gets the same
  fake everywhere it appears, so data that matched still matches.
- **Fast.** Independent tables, samplers and chunks all run at once, over a pool of read-only connections.
- **Look before you leap.** `--plan` prints every query without touching the database.

## Install

With Homebrew, on macOS or Linux:

```sh
brew install xjunior/tap/tapas
```

Or download the build for your platform from the [releases](https://github.com/xjunior/tapas/releases): Linux
(x86_64 and arm64, static) or macOS (Apple silicon and Intel). Each comes with a SHA-256 checksum.

Or build it yourself with Rust 1.89 or newer:

```sh
cargo install --git https://github.com/xjunior/tapas
```

## Quick start

Describe the sample:

```yaml
# tapas.yml
samplers:
  - table: countries              # every country

  - table: orders
    time_column: created_at       # orders placed within --since
    deps:
      user_id: users.id           # plus the users who placed them

  - table: users
    where: admin = 1              # plus every admin
    deps:
      country_id: countries.id    # each user brings their country

  - table: line_items
    of:
      order_id: orders.id         # the items of those orders
```

Check it, run it, load it:

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

Either way tapas writes `INSERT` statements only, so the target needs the schema already. Passwords can stay off the
command line too: see [Command line](docs/cli.md#connection-urls). There's a fuller example in
[tapas.example.yml](tapas.example.yml).

## Documentation

- [Configuration](docs/configuration.md): samplers, `of`, `deps`, names, time windows and cycles.
- [Sanitization](docs/sanitization.md): replacing sensitive values with fakes, fixed values or templates.
- [Command line](docs/cli.md): every flag, connection URLs and their environment variables, and what the output
  looks like.
- [How it works](docs/how-it-works.md): planning, scheduling and cycles, for people changing tapas.
- [Limitations](docs/limitations.md): what tapas doesn't do yet.
- [Development](docs/development.md): building and testing.

See [CHANGELOG.md](CHANGELOG.md) for what changed in each version.

## License

MIT. See [LICENSE](LICENSE).
