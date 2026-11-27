# SOLID design

Keep tapas easy to change: each piece has one job, and pieces depend on small, stable interfaces. Apply SOLID the Rust way, within `style-guide.md`. Don't add a trait or a layer just to tick a principle.

## Single responsibility
Each module has one reason to change. Keep this split:
- `config` reads and validates YAML.
- `plan` decides what to extract and in what order. It does no I/O.
- `sql` builds SQL text and does no I/O.
- `extract` runs queries and tracks state.
- `output` writes the sample.
- `run` wires them together.
- `main` only parses the CLI.

Functions follow the same rule: if a description needs "and", split it.

## Open/closed
Add a kind of output, column or span by adding a variant and handling it in the `match`es that the compiler points to. Don't use flags or `if`s scattered across modules. Exhaustive matches (no `_` catch-alls) are what make this safe.

## Liskov substitution
Every implementation of a trait, and every variant of an enum used through one API (like `Sink`), must keep the same promises: the same ordering, the same error and cleanup behaviour, and no surprise panics. A new `Sink` variant must also keep "nothing kept unless the outcome is `Keep`".

## Interface segregation
Functions take only what they use: `&Sampler` or `&[Dep]`, not the whole `Plan`; `&str` or `&[u8]`, not owners. Keep public APIs small, and keep helpers private until a second module needs them.

## Dependency inversion
Pure logic (`config`, `plan`, `sql`) never depends on I/O (`extract`, `output`, MySQL). Data flows in as plain values. If a module needs a seam, for a second implementation or to test without MySQL, add a small trait used through generics, not `dyn`, and only once that need is real.
