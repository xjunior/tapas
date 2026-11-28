# How it works

This page is for people changing tapas.

## Pieces

| Module | Role |
| --- | --- |
| `config` | Loads the YAML files, expands globs, and labels each sampler with where it came from. |
| `since` | Parses `--since` and turns a `limit`'s `since` into a SQL condition. |
| `plan` | Validates the samplers, turns `deps` into samplers, finds cycles, and orders the tables. |
| `sql` | Builds the queries, the `INSERT`s and the file's first and last lines, and writes values as literals, in MySQL's or Postgres's SQL. |
| `sanitize` | Works out what each table's sanitize rules do to its columns, and rewrites rows to match. |
| `fake` | Makes fake values from a seed. |
| `extract` | Runs the plan: schedules samplers, streams rows, and keeps the values children need. |
| `source` | Opens the source's read-only connections, and reads the columns the sanitize rules cover and the foreign keys. |
| `postgres` | Connects to Postgres, over TLS when the server offers it, and pools the source's connections. |
| `output` | Writes the sample: a SQL file on its own thread, or a target database in one transaction. |
| `run` | Picks MySQL or Postgres from the URLs, refuses a target of another kind, and wires the source, the plan, the extraction and the output together. |

## From config to plan

1. `--since` stays in the plan; the configured samplers aren't changed. A sampler whose `limit` has a `since` gets
   the condition when its queries are built, and doesn't count as taking every row when `--since` is a span. A
   sampler with no `limit` and no `of` takes no rows, so it never runs; its `deps` still count.
2. Each `deps` reference, `table.column → parent.column`, becomes one extra sampler on the parent:
   `of: { parent.column: table.column }`. A reference over several columns stays one reference. A parent that already takes every row gets none.
   With `--deps foreign-keys`, `run::foreign_keys` first reads the source's foreign keys in one query. The plan starts
   from the tables the samplers and their `deps` name, adds the keys of each, and follows them to their parents until
   nothing new turns up. A key becomes a reference like a `deps` entry, unless a `deps` entry says the same.
3. Each `of` adds an edge from the parent table to the child. A named sampler stands for its table.
4. Tarjan's algorithm groups the tables into strongly connected components. A component with several tables, or one
   table that follows itself, is a cycle.
5. Every table remembers the columns its children follow (`keep`), and named samplers do the same for their own rows. A composite key is remembered as one tuple of literals, `(1, 2)`, which goes into `(a, b) IN (...)` as it is. A key with a NULL part isn't remembered.

A table is **full** when one of its samplers takes every row. An `of` on a full table needs no values: it becomes
`column IS NOT NULL`.

## Running

- A sampler outside a cycle starts as soon as every table it follows is done. A table is done when all its samplers
  are, and a table with none is done from the start.
- A cycle starts as one unit once every table outside it that it follows is done.
- Within a sampler, long value lists are split into chunks of `--chunk-size`. Several `of` lists become every
  combination of their chunks, so no row comes back from two queries.
- Chunks run in parallel, except for a sampler with a row cap (`limit: 100`), which runs them one after another until it has enough.
- Rows stream into batches. A batch drops rows the table already has, keeps the values children need, replaces
  sanitized values, and goes to the writer as one `INSERT`.

Duplicates are found by hashing each row with 128-bit xxh3. Tables with a single sampler, outside any cycle, skip that.

## Cycles

A cycle runs in two steps.

1. **Seeds.** Samplers that don't follow anything inside the cycle run once.
2. **Rounds.** The other samplers run again and again. A sampler that follows one table of its cycle only receives the
   values found since its previous round; one that follows several receives all of them, since a new row can come from
   any combination. The cycle ends when a round writes no new rows.

Values are kept in the order they were found, so "new since the previous round" is a slice. Every table in a cycle
drops duplicates, since rounds can find the same row twice.

## Named samplers

A named sampler keeps the values of its own rows, separately from its table. They're recorded before duplicates are
dropped, so a row another sampler already wrote still counts as this sampler's.

## Sanitizing

Before the output opens, `run` reads the columns of the tables that rules cover from `information_schema`, in one
query. `Sanitizer::try_new` picks each column's rule and checks it against the column and against
`Plan::references`. Each query then maps those columns to positions once (`Sanitizer::place`). Per row, only the
sanitized values are rewritten, each in the buffer that held its literal.

Duplicates and kept values are worked out before sanitizing, from the original values. A fake's seed is
`xxh3_128(original value, lowercased)`, seeded by the run's seed mixed with the fake's name. The table and column
aren't part of it, which is what makes the same value's fake the same everywhere.

## Writing

The writer receives finished `INSERT` statements over a bounded channel, so a slow disk or target slows the queries
down rather than filling memory. It keeps the sample only if the whole run worked: see
[The output](cli.md#the-output).

From MySQL, values are written from its text protocol, by column type:

| Column type | Written as |
| --- | --- |
| Integers, decimals, floats, `YEAR` | As they are |
| Dates, times, `JSON` | Quoted strings |
| `BIT`, geometry, binary strings and blobs | Hex, as `X'…'` |
| Everything else | Quoted, with `\`, `'`, NUL, newlines and `\Z` escaped |

From Postgres, every value is read as text with the simple query protocol and written as a quoted literal, with `'`
doubled, such as `'42'`, `'t'` or `'\x00ff'`. Postgres turns each literal into its column's type as the row goes in, so
tapas needs no list of types: arrays, enums and `jsonb` work too. Source connections set `DateStyle=ISO`,
`extra_float_digits=1` and `standard_conforming_strings=on`, so the text reads back as exactly the same value.
