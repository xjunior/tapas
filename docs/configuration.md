# Configuration

A config is a YAML file with a list of samplers. Each sampler picks rows from one table.

```yaml
samplers:
  - table: users
    where: activated_at IS NOT NULL
```

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
| `table` | The table to read. A sampler with nothing else takes every row. |
| `where` | A plain SQL condition, such as `created_at >= NOW() - INTERVAL 6 MONTH`. |
| `order_by` | Plain SQL, such as `id DESC`. Mostly useful with `limit`. |
| `limit` | The most rows this sampler takes. |
| `time_column` | A column, or a list of them, that `--since` filters on. |
| `of` | `my_column: table.column`. Only rows that belong to rows already in the sample. |
| `deps` | `my_column: table.column`. The rows this table points at come along. |
| `name` | Lets an `of` follow just this sampler's rows. |

A table gets every row any of its samplers picks, each row once. Add as many samplers per table as you need:

```yaml
- table: users
  order_by: id DESC
  limit: 100                      # the 100 newest users...
- table: users
  where: admin = 1                # ...and every admin
```

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
  time_column: created_at
  deps:
    user_id: users.id
```

`deps` belongs to the table, not to the sampler it's written on. Every row of `orders`, however it got into the
sample, brings its user; those users bring whatever their own `deps` point at; and so on. List each reference once.

A few things follow from that:

- A table reached only through `deps` needs no sampler of its own.
- A table that already takes every row isn't pulled again.
- A sampler with only `deps` still takes every row of its table, since `deps` never narrows.

| You want | Write |
| --- | --- |
| The rows that belong to what's in the sample | `of` on the child |
| The rows that what's in the sample points at | `deps` on the child |

## Time windows

`time_column` marks the samplers that `--since` sizes:

```yaml
- table: projects
  time_column: [created_at, updated_at]   # created or updated within --since
- table: project_tasks
  of:
    project_id: projects.id               # follows its projects, no window of its own
```

With a list, a row matches when any of the columns is recent enough. Put `time_column` on the tables the sample
starts from, and let `of` and `deps` carry the window to the rest.

As soon as any sampler has a `time_column`, every run needs `--since`. See [Command line](cli.md#--since).

## Following one sampler

Sometimes a table is copied whole but only some of its rows should lead anywhere. Name the sampler that picks them and
follow it with `of`:

```yaml
- table: written_off_projects             # every write-off
- table: written_off_projects
  name: recent_write_offs
  time_column: [created_at, updated_at]
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
  deps:
    manager_id: employees.id

# The projects of recent tasks, and every task of those projects.
- table: project_tasks
  time_column: updated_at
  deps:
    project_id: projects.id
- table: project_tasks
  of:
    project_id: projects.id
```

tapas extracts a cycle in rounds until a round finds nothing new. A sampler that runs in rounds can't have a `limit`.
`--plan` shows each cycle and marks the samplers that run in rounds.

## Writing good samplers

- Start from the tables your sample is about, give them a `time_column`, and follow references from there.
- Prefer `of` and `deps` to subqueries in `where`. They keep every reference intact, and tapas can run them in
  parallel.
- Keep `where` for conditions on the table's own columns, such as `state NOT IN ('draft')`.
- Run `--plan` after every change.
