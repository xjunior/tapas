# Sanitization

tapas can replace sensitive values as it writes them, so a sample doesn't carry personal data out of production.
Rules match column names, and each one says what the values become: a made-up value, a fixed one, `NULL`, or text
built from the row's other columns.

```yaml
samplers:
  - table: orders
    limit: { since: created_at }
    deps:
      user_id: users.id

sanitize:
  columns:                              # every table
    "*_id": keep
    "*email*": { fake: email }
    "*notes*": { value: Redacted }
    "*encrypted*": null
  tables:                               # one table; wins over `columns`
    users:
      first_name: { fake: first_name }
      last_name: { fake: last_name }
      ssn: { fake: ssn }
      login: { template: "user_{id}" }
      recovery_email: keep
```

## Replacements

| Replacement | The values become |
| --- | --- |
| `{ fake: email }` | A made-up value of that kind. See [Fake values](#fake-values). |
| `{ value: Redacted }` | That text or number. |
| `{ template: "user_{id}@example.com" }` | The text, with each `{column}` replaced by that column's value in the same row. |
| `null` | `NULL`. |
| `keep` | Themselves. Use it for exceptions to a broader rule. |

`NULL` and empty values stay as they are, whatever the replacement, so a sanitized sample keeps the same gaps as the
source.

## Fake values

A fake looks like the real thing, so forms, validations and views behave as they would in production. None of them
can belong to a real person:

| `fake:` | Example | Why it's safe |
| --- | --- | --- |
| `first_name` | `Maria` | |
| `last_name` | `Lopez` | |
| `name` | `Maria Lopez` | |
| `email` | `maria.lopez.3j9x0a2b1c4d@example.com` | `example.com`, `.net` and `.org` are reserved domains. |
| `username` | `maria_3j9x0a2b1c4d` | |
| `phone` | `415-555-0137` | 555-0100 to 555-0199 is set aside for fiction. |
| `ssn` | `912345678` | Areas 900–999 are never issued. Digits only, so it fits a numeric column. |
| `credit_card` | `4000001234567899` | Passes the Luhn check, in the `400000` test range. Digits only. |
| `street_address` | `1234 Maple Ave` | |
| `secondary_address` | `Apt. 12` | |
| `city` | `Springfield` | |
| `state` | `OR` | |
| `zip` | `97201` | |
| `company` | `Lopez Group` | |
| `date_of_birth` | `1978-04-23` | Between 1945 and 2005. |
| `ip` | `192.0.2.17` | From the documentation networks. |
| `lorem` | `Lorem dolor sit amet tempor magna.` | |

A fake longer than its column is cut to fit.

## Consistent fakes

A fake comes from the original value, so **the same value gets the same fake everywhere it appears**: in every row,
table and column that uses the same `fake:`. Data that matched in the source still matches in the sample:

```yaml
sanitize:
  columns:
    "*email*": { fake: email }
```

| Source | Sample |
| --- | --- |
| `users.email = 'ana@corp.com'` | `users.email = 'chen.okafor.1x8k2m0q9z3ab@example.org'` |
| `orders.customer_email = 'ANA@corp.com'` | `orders.customer_email = 'chen.okafor.1x8k2m0q9z3ab@example.org'` |
| `users.email = 'bo@corp.com'` | `users.email = 'lena.ito.0w2kq7v9d1r5x@example.com'` |

- **Case doesn't count.** `Ana@corp.com` and `ana@corp.com` get the same fake, as MySQL's usual collations treat them
  as equal. Postgres values get the same fakes as MySQL ones.
- **Different values get different fakes.** `email` and `username` end with a part of the original value's hash, so
  two different emails practically never collide in a unique index.
- **Only the same kind of fake lines up.** `first_name` of `Ana` and `name` of `Ana Lee` are unrelated.

Within a run, fakes always agree. Across runs, they agree only with the same [`--sanitize-seed`](cli.md#--sanitize-seed).
Without one, each run picks its own seed, so samples taken on different days can't be linked to each other.

Keep a fixed seed secret. With it, anyone can fake every candidate value, every possible SSN for example, and match
the results against a sample.

## Which rule wins

For each column, the first of these that matches:

1. The table's rule naming the column.
2. The table's first glob that matches.
3. A `columns` rule naming the column.
4. The first `columns` glob that matches.

Globs match the whole column name and ignore case: `*email*` matches `email` and `recipient_email`, while `email`
matches only `email`. List `keep` exceptions before the globs they're exceptions to, as `"*_id": keep` is above.

## Many files

Each file can have its own `sanitize:`, so a component's `tapas.yml` sanitizes the tables it samples:

```yaml
# components/payroll/tapas.yml
samplers:
  - table: payroll_runs
    limit: all
    deps:
      employee_id: employees.id
sanitize:
  tables:
    employees:                          # reached only through deps, and still covered
      ssn: { fake: ssn }
```

```yaml
# tapas/shared.yml
sanitize:
  columns:
    "*email*": { fake: email }
```

So that no rule depends on the order files load in:

- Only one file can have `sanitize.columns`.
- Several files can have rules for the same table, but not for the same column.
- Two files' globs can't both match the same column. Name the column in one of them.

## Checks

Before extracting anything, tapas reads the columns of the tables the rules cover, in one query, and refuses to start
when:

- A table's rule names a column the table doesn't have, so a typo can't let a column through.
- A rule matches a column that a reference follows, from either side of an `of` or `deps`. Changing it would break
  the reference:

  ```
  shared.yml sanitize.columns.*email*: orders.customer_email can't be sanitized, because a reference follows it.
  Add `customer_email: keep` under sanitize.tables.orders
  ```

- A text fake, a template, or a value that isn't a number goes into a numeric column.
- A value is longer than its column.
- A template uses a column that's missing, or sanitized too, since that would put its original value back.

`--plan` lists each table's own rules. It doesn't connect, so it can't say which columns the globs match.

## How values are written

Fixed and fake values are written as quoted strings. A template becomes SQL that the database runs as it loads the
row, cut to the column's size. A `NULL` column makes the whole value `NULL`:

```sql
-- MySQL
INSERT INTO `users` (`id`, `email`, `login`) VALUES
(42, 'chen.okafor.1x8k2m0q9z3ab@example.org', LEFT(CONCAT('user_', 42), 40));
-- Postgres
INSERT INTO "users" ("id", "email", "login") OVERRIDING SYSTEM VALUE VALUES
('42', 'chen.okafor.1x8k2m0q9z3ab@example.org', LEFT(CAST('user_' || '42' AS text), 40));
```

Duplicate rows and the values children follow are worked out from the original values, before sanitizing. So
sanitizing never changes which rows are in the sample.
