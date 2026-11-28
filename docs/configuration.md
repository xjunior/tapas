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

A config can also say how to replace sensitive values, under `sanitize:`. See [Sanitization](sanitization.md).

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

## Writing good samplers

- Start from the tables your sample is about, give them a `limit` with `since`, and follow references from there.
- Prefer `of` and `deps` to subqueries in `where`. They keep every reference intact, and tapas can run them in
  parallel.
- Keep `where` for conditions on the table's own columns, such as `state NOT IN ('draft')`.
- Run `--plan` after every change.
