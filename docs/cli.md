# Command line

```sh
tapas [OPTIONS] <CONFIGS>...
```

`<CONFIGS>` are YAML files or quoted glob patterns, such as `'apps/**/tapas.yml'`.

| Option | Default | Meaning |
| --- | --- | --- |
| `--source <URL>` | | The database to read. Required unless `--plan`. |
| `-o`, `--output <FILE>` | | The SQL file to write, gzipped if `<FILE>` ends in `.gz`. This or `--target`, unless `--plan`. |
| `--target <URL>` | | The database to write into. This or `--output`, unless `--plan`. |
| `--since <SPAN>` | | How far back samplers with a `time_column` reach. |
| `--plan` | | Print the plan and stop, without connecting. |
| `--connections <N>` | `8` | The most connections open to the source at once. |
| `--chunk-size <N>` | `5000` | Values per `IN (...)` list. |
| `--batch-rows <N>` | `500` | Rows per `INSERT` statement. |
| `--sanitize-seed <TEXT>` | | Makes fake values the same from run to run. |
| `--debug` | | Shows the values in errors. |

## `--source`

The database to read, as a MySQL or Postgres URL:

```sh
tapas tapas.yml --source mysql://user:password@host:3306/shop -o sample.sql
tapas tapas.yml --source postgres://user:password@host:5432/shop -o sample.sql
```

Every connection is read-only, so tapas can't change the source. The sample is written for the same kind of database:
MySQL SQL from MySQL, Postgres SQL from Postgres.

In Postgres, tapas reads the tables of the connection's current schema, the first one in its `search_path`. To pick
another, set it in the URL: `postgres://user:password@host:5432/shop?options=-c%20search_path%3Dsales`.

## `--target`

The database to write into, instead of a file. It must be the same kind as the source:

```sh
tapas tapas.yml --source mysql://user:password@host:3306/shop --target mysql://root@localhost:3306/shop_dev
```

- Its tables must already exist. tapas only inserts; it never creates, empties or drops anything.
- Everything goes in one transaction, committed only once the whole sample worked. A failed run leaves the target as
  it was.
- Foreign-key checks are off for tapas's connection, so rows land in any order.
- tapas refuses a target that is the source, or a target of another kind.

Postgres has no switch for foreign-key checks alone, so tapas sets `session_replication_role = replica`, which also
skips the target's ordinary triggers while it loads. That takes a superuser, or on Postgres 15 and later:

```sql
GRANT SET ON PARAMETER session_replication_role TO tapas_writer;
```

Before committing, tapas moves the sequence of each `serial` and identity column to the column's highest value, so
rows the application inserts next don't clash with copied ones. MySQL's `AUTO_INCREMENT` does this by itself.

## Connection URLs

`--source` and `--target` take the same kind of URL:

- Percent-encode special characters in the user or password, such as `@` as `%40`.
- MySQL: turn on TLS with query parameters, such as `?require_ssl=true&verify_ca=true&verify_identity=true`.
- Postgres: tapas uses TLS whenever the server offers it, and checks the server's certificate against the public
  certificate authorities. `?sslmode=require` refuses a server without TLS; `?sslmode=disable` turns TLS off. See
  [Limitations](limitations.md#connections).

A password on the command line is visible to anyone running `ps`, and ends up in shell history and CI logs. To keep it
off, set the URL in an environment variable and leave the flag out:

| Flag | Variable |
| --- | --- |
| `--source` | `TAPAS_SOURCE_URL` |
| `--target` | `TAPAS_TARGET_URL` |
| `--sanitize-seed` | `TAPAS_SANITIZE_SEED` |

```sh
export TAPAS_SOURCE_URL='mysql://user:password@host:3306/shop'
tapas tapas.yml -o sample.sql
```

The flag wins when both are set.

## `--since`

A span of days, weeks, months or years, counted back from midnight today:

```sh
--since '10 days'
--since '2 weeks'
--since 3months
--since '1 year'
--since ever          # keep every row
```

As soon as any sampler has a `time_column`, `--since` is required, so a forgotten flag never copies whole tables. Use
`--since ever` when you do want everything.

## `--sanitize-seed`

Any text, which [fake values](sanitization.md#consistent-fakes) start from. With the same seed, the same original
value gets the same fake one in every run, so samples taken on different days line up:

```sh
export TAPAS_SANITIZE_SEED="$(cat /run/secrets/tapas-seed)"
tapas tapas.yml --since '2 weeks' -o sample.sql
```

Without it, each run picks its own seed. Within one run, fakes always line up.

Treat the seed like a password, and keep it off the command line the same way: see
[why](sanitization.md#consistent-fakes).

## `--debug`

Shows the values in errors. Without it, tapas keeps the sample's values out of its errors, so they're safe in CI logs:

- A query the source fails shows each list of values as the column they come from, as `--plan` does:
  ``running SELECT * FROM `phones` WHERE `user_id` IN (<users.id>)``.
- A statement the target refuses shows only its table and columns:

```
Error: writing to the target

Caused by:
    0: running INSERT INTO `users` (`id`, `name`) VALUES …
    1: Server error: `ERROR 1406 (22001): Data too long for column 'name' at row 3'
```

With `--debug`, a failed query shows its values. When the target refuses a statement, tapas runs its rows again one
at a time, and shows the first one the target refuses, with the target's reason for it:

```
Caused by:
    0: running INSERT INTO `users` (`id`, `name`) VALUES
       (3, 'Maximiliana');
    1: Server error: `ERROR 1406 (22001): Data too long for column 'name' at row 1'
```

If every row goes in on its own, as when two rows of the statement clash with each other, the error shows the whole
statement instead. On Postgres, running the rows again takes a savepoint before each statement.

The rows are as tapas writes them, so [sanitized](sanitization.md) columns hold their replacements.

The database's own messages can quote a value with or without `--debug`, such as MySQL's
`Duplicate entry '5' for key 'users.PRIMARY'` or Postgres's `Failing row contains (…)`.

## `--plan`

Prints every table in the order tapas would run it, with the queries its samplers send. It doesn't connect, but it
writes the queries for the kind of database `--source` names, or for MySQL without `--source`:

```
$ tapas tapas.example.yml --since '2 weeks' --plan
5 tables, 7 samplers, 1 with a time_column
--since 2 weeks: limits 1 samplers
sanitize: 1 rules for every table, from tapas.example.yml

orders  (keeps id, user_id)
  tapas.example.yml sampler #4 (orders)
    SELECT * FROM `orders` WHERE `created_at` >= CURDATE() - INTERVAL 2 WEEK

users  (after orders; keeps id; sanitizes name: fake name)
  tapas.example.yml sampler #2 (users)
    SELECT * FROM `users` ORDER BY id DESC LIMIT 100
  tapas.example.yml sampler #3 (users)
    SELECT * FROM `users` WHERE (admin = 1)
  pulled in by orders.user_id
    SELECT * FROM `users` WHERE `id` IN (<orders.user_id>)

phones  (after users; sanitizes number: fake phone)
  tapas.example.yml sampler #5 (phones)
    SELECT * FROM `phones` WHERE `user_id` IN (<users.id>)
…
```

- `<orders.user_id>` stands for the values found when the sample runs.
- Samplers that `deps` adds show up as "pulled in by".
- Tables in a cycle say so, and the samplers that run in rounds are marked "(in rounds)".
- With [`sanitize:`](sanitization.md), a line counts the rules for every table, and each table lists its own rules,
  as in `users  (every row; sanitizes ssn: fake ssn, login: template user_{id})`.

## The output

With `--target`, rows go straight into the database, as above. With `--output`, tapas writes to `<FILE>.partial` and renames it to `<FILE>` only once the whole sample worked, so a failed run never
leaves something that looks complete. The file:

- starts with `SET NAMES utf8mb4` and `SET FOREIGN_KEY_CHECKS=0`, so tables load in any order. For Postgres, it sets
  `session_replication_role = replica` instead, which needs the privilege [`--target`](#--target) describes;
- holds one multi-row `INSERT` per batch, with table names that carry no database or schema;
- ends with `SET FOREIGN_KEY_CHECKS=1`. For Postgres, it moves the tables' sequences past the copied rows, then resets
  `session_replication_role`;
- has no schema, `TRUNCATE` or `DROP`. Load it into a database with the schema and empty tables.

When `<FILE>` ends in `.gz`, tapas gzips the SQL as it writes it, so the uncompressed SQL never touches the disk.
Neither `mysql` nor `psql` reads gzip itself; decompress on the way in:

```sh
tapas tapas.yml -o sample.sql.gz
gunzip -c sample.sql.gz | mysql -h host -u user -p shop
gunzip -c sample.sql.gz | psql -h host -U user shop
```

tapas uses gzip's fastest level, so compressing doesn't slow the run. The file is about a quarter of the plain SQL's size.

Progress goes to stderr. Each run ends with the row count per table, the total and the time taken.

## Exit status

`0` on success. Anything else is an error, and tapas prints its cause, such as a typo in a config, a table that
doesn't exist, or a dropped connection. Errors name the file and sampler they came from, as in
`apps/shop/tapas.yml sampler #2 (orders)`.
