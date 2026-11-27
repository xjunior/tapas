# Clean up around every change

Each change looks at the code it touches and the code right around it, and leaves it better.

## Look for
- Code made dead or redundant by the change: unused functions, fields, variants, imports, config keys, and branches that can't happen any more.
- Duplication that the change adds or exposes. Pull it into one place once there are two real copies, not before.
- Names, docs, comments, error messages, `--help` text, `README.md` and `docs/` that no longer describe what the code does.
- Clones, allocations and conversions on the changed path that now have no reason to be there (see `performance.md`).
- Existing code near the change that breaks `style-guide.md`.

## Limits
- Stay related: clean up what the change touches, calls or makes stale. Leave unrelated modules alone. Note problems you find there in the PR instead of fixing them.
- Cleanup must not change behaviour. Put it in its own commit, before the feature commit when the feature depends on it, so each diff is easy to review.
- If a cleanup turns out large or risky, stop and propose it as its own change instead of growing the current one.
- List the cleanups in the PR description.
