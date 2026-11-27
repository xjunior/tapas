# Performance and memory

tapas streams millions of rows through a few hot paths. Be conservative: a change must not make them slower or hungrier without a measured reason.

## Hot paths
Anything that runs per row or per value. That's `extract::run_query`, `write_batch`, `row_hash`, `KeySet::insert`, `sql::literal`, `sql::insert`, and the output writers. Code that runs once per sampler, query or table is cold by comparison.

## Rules
- Stream. Never collect a whole result set or table in memory. Keep channels bounded, so a slow writer slows the readers instead of filling memory.
- In hot paths, add no allocation you can avoid. Borrow (`&str`, `&[u8]`) instead of cloning. Size buffers with `with_capacity`, reuse them across iterations (`clear()` instead of a new `Vec`), and write into an existing buffer instead of building temporary `String`s with `format!`.
- Remember only what children need: kept columns and row hashes, never whole rows. Any new per-row state must be justified in the PR, with its cost per row.
- Don't add round trips to MySQL. Batch them (`IN` chunks, multi-row INSERTs) and keep queries using indexes: no functions on filtered columns, no `OFFSET` paging.
- Hold locks briefly: never across an `.await`, a query or a channel send.
- Prefer plain data and generics over `dyn`, `Arc`, boxing and extra tasks unless they pay for themselves.
- When a change touches a hot path, measure before and after on a realistic sample, and put the numbers (time and peak memory) in the PR. If you can't measure, say so and keep the change minimal.
- Don't trade correctness for speed. Every reference stays intact and every value round-trips exactly.
