# Limitations

What tapas doesn't do yet, or does in a way worth knowing about.

## Data

- **Sanitization only covers the columns you name.** A column that no rule matches is copied as it is. Check new
  columns against your [`sanitize:`](sanitization.md) rules, and keep the target as protected as the source until
  you have.
- **Columns that references follow can't be sanitized.** Changing them would break the reference. Sanitize the
  sensitive columns around them.
- **Free text isn't scanned.** A `notes` column that mentions an email keeps it, unless a rule replaces the whole
  column.
- **No single snapshot.** Each query sees the source as it is when it runs, so a sample taken while the source changes
  can catch a row without one it was just given.
- **References are only as good as the source.** A row that points at one missing from the source points at nothing
  in the sample either.

## Output

- **No schema.** The target, file or database, needs every table already, with every column the source has. A
  target behind on migrations refuses the INSERTs, and tapas stops with the statement and the target's error, such
  as ``running INSERT INTO `users` (`id`, `name`) VALUES …`` and `Unknown column 'name' in 'field list'`.
  [`--debug`](cli.md#--debug) shows the row the target refused.
- **The same kind of database in and out.** A MySQL source writes MySQL SQL and a Postgres source writes Postgres
  SQL. tapas refuses a target of the other kind.
- **One Postgres schema.** tapas reads and writes the tables of the connection's current schema. See
  [`--source`](cli.md#--source) for how to pick it.
- **Postgres targets need `session_replication_role`,** which takes a superuser or a grant. See
  [`--target`](cli.md#--target).

## Connections

- **Postgres TLS only trusts public certificate authorities.** A server whose certificate comes from a private one,
  such as Amazon RDS's or a self-signed one, fails the handshake, even with the default `sslmode=prefer`. tapas can't
  take a CA file yet; only `?sslmode=disable` gets through, without TLS.

## Configuration

- **No variables.** `where` is plain SQL; `--since` is the only value that changes per run.
- **A `deps` sampler can't follow a named sampler.** Use `of` for that.
- **Foreign keys are the ones the database enforces.** A reference that exists only in the application needs a `deps`
  entry. tapas reads the keys of the connection's current schema, and ignores one that points at another schema.
- **A foreign key is followed from every table the sample reaches.** One to a table you'd rather leave out can't be
  skipped yet; use `--deps declared` and write the `deps` you want.
- **A sampler that runs in rounds can't cap its rows.** `limit: all` and `limit: { since: … }` work; `limit: 100` doesn't.
- **Every table needs a primary key, or whole rows that differ.** Duplicates are found by hashing whole rows.
