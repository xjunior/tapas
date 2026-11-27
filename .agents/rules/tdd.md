# Test-driven development

Every change to behaviour starts with a test that fails without it.

## Loop
1. Write the test first. Run it and watch it fail, and check that it fails for the reason you expect (an assertion, not a typo or a compile error elsewhere).
2. Write the least code that makes it pass.
3. Refactor with the tests green, then run the whole suite.

## Rules
- Bug fixes start with a test that reproduces the bug.
- New config keys, CLI flags, SQL shapes and error messages each get a test that pins them down. Error tests assert on the message that users see.
- Unit tests go in the module's `#[cfg(test)] mod tests`. Anything that needs MySQL goes in `tests/*.rs` and runs only when `TAPAS_TEST_DATABASE_URL` is set. Run those tests locally against a MySQL 8.4 before saying a change works. CI does the same.
- Test through the public behaviour (what a plan contains, the SQL produced, the rows that land in the target), not through private details that a refactor would break.
- Refactors that don't change behaviour need no new test, but the existing tests must pass before and after without being edited to fit.
- Never delete or weaken an assertion to make a change pass. If an expectation really changed, say so in the commit message.
- A pure refactor gets its own commit; a commit that changes behaviour also contains its test.
