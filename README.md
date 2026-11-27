# tapas

Small plates of your production data.

tapas pulls a sample out of a MySQL database and writes it to a SQL file, keeping every reference intact. You describe
what to take in YAML; tapas works out the order, runs everything it can in parallel, and makes sure no row points at
one that was left behind.

- **Samplers.** Pick rows per table with plain SQL, a time window, a limit, or by following other tables.
- **References that hold.** `deps` brings along every row your rows point at. `of` takes the rows that belong to what's
  already in the sample.
- **One config, many sizes.** `--since '2 weeks'` for a laptop, `--since '3 months'` for a review environment.
- **Fast.** Independent tables, samplers and chunks all run at once, over a pool of read-only connections.
- **Look before you leap.** `--plan` prints every query without touching the database.

## Install

```sh
cargo install --path .
```

That puts `tapas` in `~/.cargo/bin`. It needs Rust 1.88 or newer.

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
export TAPAS_SOURCE_URL='mysql://user:password@host:3306/shop'

tapas tapas.yml --since '2 weeks' --plan
tapas tapas.yml --since '2 weeks' -o sample.sql
mysql shop_dev < sample.sql
```

The file holds `INSERT` statements only, so load it into a database that already has the schema. There's a fuller
example in [tapas.example.yml](tapas.example.yml).

## Documentation

- [Configuration](docs/configuration.md): samplers, `of`, `deps`, names, time windows and cycles.
- [Command line](docs/cli.md): every flag, and what the output looks like.
- [How it works](docs/how-it-works.md): planning, scheduling and cycles, for people changing tapas.
- [Limitations](docs/limitations.md): what tapas doesn't do yet, sanitization included.
- [Development](docs/development.md): building and testing.

See [CHANGELOG.md](CHANGELOG.md) for what changed in each version.

## License

MIT. See [LICENSE](LICENSE).
