# Configuration

A config is a YAML file with a list of samplers. Each sampler picks rows from one table, and its `limit` says how far
it reaches.

```yaml
samplers:
  - table: users
    where: activated_at IS NOT NULL
    limit: all
```

A table with no sampler, or a sampler with no `limit` and no `of`, brings no rows of its own. It is extracted only
when another table's [`deps`](#of-and-deps) point at it.

A config can also say how to replace sensitive values, under `sanitize:` (see [Sanitization](sanitization.md)), and
whether to follow foreign keys, under `foreign_keys:` (see [Foreign keys](#foreign-keys)).

## Files

Pass as many files as you like. Quote a glob and tapas expands it itself, `**` included:

```sh
tapas 'apps/**/tapas.yml' shared.yml -o sample.sql
```

All the files' samplers are merged into one list, so a sampler in one file can follow a table another file extracts.
A file can hold only samplers, only [`sanitize:`](sanitization.md#many-files) rules, or both.
A pattern that matches nothing is an error, and a file matched twice is read once. Unknown keys are errors too, so a
typo like `limt: 100` never goes unnoticed.

## Sampler keys

| Key | Meaning |
| --- | --- |
| `table` | The table to read. |
| `limit` | How far the sampler reaches: `all`, a row count, or `since`. See [Limits](#limits). |
| `where` | A plain SQL condition, such as `state <> 'draft'`, in the source's SQL. Needs a `limit` or `of`. |
| `order_by` | Plain SQL, such as `id DESC`, in the source's SQL. Mostly useful with a row count. Needs a `limit` or `of`. |
| `of` | `my_column: table.column`. Only rows that belong to rows already in the sample. Several columns: [a list of maps](#several-columns). |
| `deps` | `my_column: table.column`. The rows this table points at come along. Several columns: [a list of maps](#several-columns). |
| `name` | Lets an `of` follow just this sampler's rows. |

A table gets every row any of its samplers picks, each row once. Add as many samplers per table as you need:

```yaml
- table: users
  order_by: id DESC
  limit: 100                      # the 100 newest users...
- table: users
  where: admin = 1
  limit: all                      # ...and every admin
```

## Limits

`limit` takes one of these:

| Write | Rows taken |
| --- | --- |
| `limit: all` | Every row that matches `where`. |
| `limit: 100` | At most 100, after `where` and `order_by`. |
| `limit: { since: created_at }` | Rows whose `created_at` is within `--since`. |
| `limit: { rows: 100, since: created_at }` | At most 100 of those, after `order_by`. |

`limit: 100` is short for `limit: { rows: 100 }`, and `limit: all` for `limit: { rows: all }`. In the map, `rows`
defaults to `all`, and `since` takes one column or a list. A row matches when any listed column is recent enough.

```yaml
- table: events
  order_by: id DESC
  limit: { rows: 1000, since: created_at }   # the 1000 newest events inside --since
```

A sampler with no `limit` takes no rows of its own. With `of` it takes the rows that belong to its parents; with only
`deps` it brings no rows, and just says what its rows point at. Each of these is an error, since it can't pick a row:

- a sampler with only `table`: write `limit: all` to copy the whole table;
- a sampler with `where` or `order_by` but no `limit` or `of`.

The old `time_column: created_at` is `limit: { since: created_at }` now.

## `of` and `deps`

Both name a column and the row it refers to. They work on different sides of that reference.

**`of` narrows the sampler it's on.** These are the line items of the orders in the sample, and no others:

```yaml
- table: line_items
  of:
    order_id: orders.id
```

With several entries, a row must match all of them.

**`deps` grows the table it points at.** Every order in the sample brings the user who placed it:

```yaml
- table: orders
  limit: { since: created_at }
  deps:
    user_id: users.id
```

`deps` belongs to the table, not to the sampler it's written on. Every row of `orders`, however it got into the
sample, brings its user; those users bring whatever their own `deps` point at; and so on. List each reference once.

A few things follow from that:

- A table reached only through `deps` needs no sampler of its own.
- A table that already takes every row isn't pulled again.
- A sampler with only `deps` takes no rows. The table's rows come only from its other samplers and from `deps` that
  point at it.

### Several columns

A key made of several columns is one reference. Write it as a list item, with every column pointing at the same
table:

```yaml
- table: memberships
  limit: 100
  deps:
    - { tenant_id: users.tenant_id, user_id: users.id }   # one reference: the user of that tenant
    - { created_by: users.id }                             # another
- table: notes
  of:
    - { tenant_id: users.tenant_id, user_id: users.id }
```

A plain map such as `deps: { user_id: users.id }` is a list of one-column references, so each entry stands alone.
The order you write the columns in doesn't matter, and two references may share a column, as `tenant_id` often is.
A row whose key has a NULL column points at nothing, so it brings nothing along. This is how a foreign key with
`MATCH SIMPLE`, the default, behaves too.

| You want | Write |
| --- | --- |
| The rows that belong to what's in the sample | `of` on the child |
| The rows that what's in the sample points at | `deps` on the child |

## Foreign keys

Most schemas already say what points at what, so tapas reads the source's foreign keys and follows them like `deps`.
This config needs no `deps` at all:

```yaml
samplers:
  - table: orders
    limit: { since: created_at }   # every user, territory and so on the orders point at comes along
```

Every table the sample reaches brings what its foreign keys point at, and so on up the chain. Keys of tables the
sample never reaches are ignored. A key over several columns works too, and so does one that points at its own table.

Declared `deps` add to the foreign keys, for references the schema doesn't state, as many Rails apps don't. One that
says the same as a key is the same reference. One that says a key's columns point at another table is an error:

```
config sampler #1 (orders): orders.user_id points at accounts.id, but foreign key orders_user_fk says it
points at users.id
```

To follow only the `deps` and `of` you declare, write `foreign_keys: false` at the top of a config, or pass
[`--disable-foreign-keys`](cli.md#--disable-foreign-keys), which also overrides `foreign_keys: true`. When several
files set `foreign_keys`, they must agree.

## Time windows

`since` in a `limit` marks the samplers that `--since` sizes:

```yaml
- table: projects
  limit:
    since: [created_at, updated_at]       # created or updated within --since
- table: project_tasks
  of:
    project_id: projects.id               # follows its projects, no window of its own
```

Put `since` on the tables the sample starts from, and let `of` and `deps` carry the window to the rest.

As soon as any sampler has a `since`, every run needs `--since`. See [Command line](cli.md#--since).

## Following one sampler

Sometimes a table is copied whole but only some of its rows should lead anywhere. Name the sampler that picks them and
follow it with `of`:

```yaml
- table: written_off_projects             # every write-off
  limit: all
- table: written_off_projects
  name: recent_write_offs
  limit:
    since: [created_at, updated_at]
- table: projects
  of:
    id: recent_write_offs.project_id      # but only recent ones bring their project
```

A name can't match a table name or another sampler's name. `deps` always names a table; only `of` follows a name.

## Cycles

Tables can lead to each other, or to themselves:

```yaml
# An employee, their manager, their manager's manager, ...
- table: employees
  where: id = 40
  limit: all
  deps:
    manager_id: employees.id

# The projects of recent tasks, and every task of those projects.
- table: project_tasks
  limit: { since: updated_at }
  deps:
    project_id: projects.id
- table: project_tasks
  of:
    project_id: projects.id
```

tapas extracts a cycle in rounds until a round finds nothing new. A sampler that runs in rounds can't cap its rows:
`limit: all` and `since` work, `limit: 100` doesn't.
`--plan` shows each cycle and marks the samplers that run in rounds.

## Everything in one file

Every option, in one config. A real one uses a fraction of these:

```yaml
# tapas tapas.yml --since '2 weeks' --plan

# Follow the source's foreign keys as well as the `deps` below. This is the default; `false` follows only what
# you declare. Several files may set it, but they must agree.
foreign_keys: true

samplers:
  # `limit: all` takes every row that matches `where`, or every row without one.
  - table: countries
    limit: all

  # A row count, with `order_by` choosing which rows. `deps` brings along the rows these point at.
  - table: users
    order_by: id DESC
    limit: 100
    deps:
      country_id: countries.id

  # Another sampler of the same table adds its rows. A row picked twice is written once.
  - table: users
    where: admin = 1
    limit: all

  # A time window: rows whose column is within --since. Several columns match when any one is recent enough.
  - table: orders
    limit: { since: created_at }
    deps:
      user_id: users.id

  # A count and a window together: at most 1000 of the rows within --since, after `order_by`.
  - table: events
    order_by: id DESC
    limit: { rows: 1000, since: [created_at, updated_at] }

  # `of` takes only the rows that belong to rows already in the sample. `deps` on a table nothing samples
  # (products) is enough to bring its rows along.
  - table: line_items
    of:
      order_id: orders.id
    deps:
      product_id: products.id

  # Several columns make one reference: a list item, every column pointing at one table. This works in `of` too.
  - table: invoices
    limit: { since: issued_at }
  - table: invoice_lines
    of:
      - { tenant_id: invoices.tenant_id, invoice_id: invoices.id }
  - table: payments
    limit: { since: paid_at }
    deps:
      - { tenant_id: invoices.tenant_id, invoice_id: invoices.id }   # one reference...
      - { created_by: users.id }                                      # ...and another

  # A named sampler lets `of` follow just its rows: every write-off is copied, but only recent ones bring
  # their project.
  - table: written_off_projects
    limit: all
  - table: written_off_projects
    name: recent_write_offs
    limit: { since: [created_at, updated_at] }
  - table: projects
    of:
      id: recent_write_offs.project_id

  # A table that points at itself: an employee, their manager, their manager's manager, ...
  - table: employees
    where: id = 40
    limit: all
    deps:
      manager_id: employees.id

# Replace sensitive values on the way out. See Sanitization.
sanitize:
  columns:                                # every table; list the `keep` exceptions first
    "*_id": keep
    "*email*": { fake: email }
    "*notes*": { value: Redacted }
    "*encrypted*": null
  tables:                                 # one table; wins over `columns`
    users:
      name: { fake: name }
      login: { template: "user_{id}" }
      recovery_email: keep
    payments:
      card_number: { fake: credit_card }
```

## Writing good samplers

- Start from the tables your sample is about, give them a `limit` with `since`, and follow references from there.
- Prefer `of` and `deps` to subqueries in `where`. They keep every reference intact, and tapas can run them in
  parallel.
- Keep `where` for conditions on the table's own columns, such as `state NOT IN ('draft')`.
- Run `--plan` after every change.
