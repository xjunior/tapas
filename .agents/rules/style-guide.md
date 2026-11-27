# Rust style (Google Rust guide, app-code subset)

Base: Rust Style Guide + API Guidelines (API rules = strong advice here; prefer simple over general). rustfmt, max_width 100. `#[rustfmt::skip]` only for deliberate alignment/generated code. Invisible chars → `\u{...}`.

## Modules/imports
- File/module named after its main item: `Foo` → `foo.rs`.
- Non-leaf modules: only `mod` + re-exports (+ `mod test`).
- No glob imports. OK: `use super::*` in inline modules, `use Enum::*` in enum-heavy code.
- One module per `use`: `use a::b::{C, D};`, no `::` inside braces.
- Trait imports inside fn bodies unless used file-wide.
- Free fns: call via module (`mem::swap(..)`). Types: import unqualified (`Regex`); ambiguous names qualified or `as` (`io::Error`).

## Ownership/signatures
- Avoid `Rc`/`Arc` unless alternative is much worse. Borrow over `.clone()`.
- Params: `&T`, `&[T]`, `&str`, `Option<&T>`; never `&Box<T>`, `&Vec<T>`, `&String`, `&Option<T>`.
- Returning `Vec`/taking `&[T]` is fine; no `impl Into<T>` params.
- Non-obvious bool param → enum, else `/* name */` comment at call site.
- Errors: `Option` = obvious failure; `Result` = caller needs detail; `panic!` = bug/invariant only.

## Types/traits
- Newtype over alias when semantics differ. Named fields over tuples (except 1-field newtype, very local).
- `#[derive]` when possible. Std traits over ad-hoc methods (`FromStr`, not just `from_str`).
- Generics/`impl Trait` over `dyn` (unless code size). Traits only for generics, `impl`/`dyn` use, or extension (`FooExt`); not for grouping code.
- `Clone` on most types (not external resources). `Copy` if it'll stay `Copy`. `Default` if `new()` or natural default. `PartialEq`/`PartialOrd` unless `Eq`/`Ord` needed. Map keys: `Hash + Eq` (+ `Ord`).
- Important `From` impl → also `new`/`from_foo` constructor. Option-bag structs: impl `Default`.

## Naming
- `as_`/`to_`/`into_` per std `self` convention.
- Constructors: `new`, `new_with_foo`, `try_new` (fallible), `from_foo` (+ `From`).
- Accessors: `foo()` (get), `foo_mut()`, `set_foo(T)`, `take_foo()`, `replace_foo(T)`, `with_foo(|x|)`.
- `len()` ⇒ also `is_empty()`.
- Lifetimes descriptive (`'_`, ≤2 single-letter OK). Generics short, hint at bound.
- No obscure abbreviations or dropped letters.

## Docs/comments
- Doc public items; always document panics and unsafe invariants. Full sentences.
- `///` items, `//!` module top, `//` impl notes. Don't restate names or explain the language; nothing to say → no comment.

## Implementation
- `unsafe`: avoid; wrap in safe API, `// SAFETY:` comment, justify. No unstable features.
- Never use a poisoned mutex's contents. No `catch_unwind`. No code before `main` (`ctor`).
- Macros last resort: plain code > `macro_rules!` > codegen > proc macro.
- Operator overloading only for math/logic meaning.
- `#[must_use]` when ignoring the result is a bug (skip if type already is, e.g. `Result`); on error types and lazy computations.
- Types: annotate enough for a reader; prefer `let x: T` over turbofish; annotate `.into()`, `.parse()`, `.collect()`.
- `if let`/`let else`, never `is_some()` then `unwrap()`. `expect("invariant")` over `unwrap()`.
- `match` lists variants explicitly; `_` only for ignored ones.
- `assert!` gets a message explaining why/values, not the condition restated.
- Short iterator chains; no chains in `for` headers (bind first); no `for_each` without reason.
- `format!`/`write!` over `+` for >2 parts or templating.
