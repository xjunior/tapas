# Command line

```sh
tapas [OPTIONS] <CONFIGS>...
```

`<CONFIGS>` are YAML files or quoted glob patterns, such as `'apps/**/tapas.yml'`.

| Option | Default | Meaning |
| --- | --- | --- |
| `--source <URL>` | `$TAPAS_SOURCE_URL` | The database to read. Required unless `--plan`. |
| `-o`, `--output <FILE>` | | The SQL file to write. Required unless `--plan`. |
| `--since <SPAN>` | | How far back samplers with a `time_column` reach. |
| `--plan` | | Print the plan and stop, without connecting. |
| `--connections <N>` | `8` | The most connections open to the source at once. |
| `--chunk-size <N>` | `5000` | Values per `IN (...)` list. |
| `--batch-rows <N>` | `500` | Rows per `INSERT` statement. |

## `--source`

A MySQL URL:

```sh
export TAPAS_SOURCE_URL='mysql://user:password@host:3306/database'
```

Prefer the environment variable. A password on the command line is visible to anyone running `ps`, and it ends up in
shell history and CI logs.

- Percent-encode special characters in the user or password, such as `@` as `%40`.
- Turn on TLS with query parameters, such as `?require_ssl=true&verify_ca=true&verify_identity=true`.

Every connection is set to `READ ONLY`, so tapas can't change the source.

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

## `--plan`

Prints every table in the order tapas would run it, with the queries its samplers send:

```
$ tapas tapas.example.yml --since '2 weeks' --plan
5 tables, 7 samplers, 1 with a time_column
--since 2 weeks: limits 1 samplers

orders  (keeps id, user_id)
  tapas.example.yml sampler #4 (orders)
    SELECT * FROM `orders` WHERE `created_at` >= CURDATE() - INTERVAL 2 WEEK

users  (after orders; keeps id)
  tapas.example.yml sampler #2 (users)
    SELECT * FROM `users` ORDER BY id DESC LIMIT 100
  tapas.example.yml sampler #3 (users)
    SELECT * FROM `users` WHERE (admin = 1)
  pulled in by orders.user_id
    SELECT * FROM `users` WHERE `id` IN (<orders.user_id>)

phones  (after users)
  tapas.example.yml sampler #5 (phones)
    SELECT * FROM `phones` WHERE `user_id` IN (<users.id>)
…
```

- `<orders.user_id>` stands for the values found when the sample runs.
- Samplers that `deps` adds show up as "pulled in by".
- Tables in a cycle say so, and the samplers that run in rounds are marked "(in rounds)".

## The output

tapas writes to `<FILE>.partial` and renames it to `<FILE>` only once the whole sample worked, so a failed run never
leaves something that looks complete. The file:

- starts with `SET NAMES utf8mb4` and `SET FOREIGN_KEY_CHECKS=0`, so tables load in any order;
- holds one multi-row `INSERT` per batch, with table names that carry no database;
- ends with `SET FOREIGN_KEY_CHECKS=1`;
- has no schema, `TRUNCATE` or `DROP`. Load it into a database with the schema and empty tables.

Progress goes to stderr. Each run ends with the row count per table, the total and the time taken.

## Exit status

`0` on success. Anything else is an error, and tapas prints its cause, such as a typo in a config, a table that
doesn't exist, or a dropped connection. Errors name the file and sampler they came from, as in
`apps/shop/tapas.yml sampler #2 (orders)`.
