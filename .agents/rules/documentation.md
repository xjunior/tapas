# Documentation

Documentation matters. Keep it concise, direct and clear: say what a reader needs, once, and stop.

## Where
- Code: doc comments, as `style-guide.md` describes.
- Users: `README.md` gives the overview and quick start. `docs/` holds the details (`cli.md`, `configuration.md`, `how-it-works.md`, `limitations.md`).
- Contributors: `docs/development.md`.
- Every change updates the docs it affects in the same PR: new flags, config keys, behaviour, limits and error messages. Docs that are stale count as a bug.

## How
- Lead with the point. Put the answer first, then only the context needed to use it.
- Use plain words and short sentences in the active voice. Cut filler ("simply", "basically", "note that", "in order to").
- Be concrete. Show a real example (a YAML snippet, a command, the SQL it produces) rather than describing one.
- Say each thing in one place, and link to it from elsewhere instead of repeating it.
- Document the why and the limits: what a reader can't get from the code or `--help`. Don't narrate the code.
- Use the same terms as the code and config (`sampler`, `deps`, `of`, `time_column`, `--since`).
- Use a table or list only when it's clearer than prose.

## Commits and PRs
- Commit subject: Conventional Commits (`feat:`, `fix:`, `refactor:`, `docs:`, …), in the imperative, 72 characters or fewer. The body says why, not a list of every file.
- PR description: what changed and why, how it was tested, and anything a reviewer should decide. Leave out what the diff already shows.
