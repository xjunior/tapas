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

- **No schema.** The target, file or database, needs every table already.
- **MySQL only,** for both reading and the SQL it writes.

## Configuration

- **No variables.** `where` is plain SQL; `--since` is the only value that changes per run.
- **A `deps` sampler can't follow a named sampler.** Use `of` for that.
- **A sampler that runs in rounds can't have a `limit`.**
- **Every table needs a primary key, or whole rows that differ.** Duplicates are found by hashing whole rows.
