@AGENTS.md

## Claude Code notes
- Skills in `.claude/skills/`: `verify`, `run-app`, `update-translations`, `add-backend`.
- Large mechanical work (new backend, mass edits) suits a background fork agent; give it the verify checklist and ask it to report limits and what was not tested.
- Commit messages end with `Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>`; PR bodies end with `🤖 Generated with [Claude Code](https://claude.com/claude-code)`.
- Use they/them for people whose pronouns are unknown.
- Keep scratch files in the session scratchpad, not in the repo. Use full absolute paths when naming them.
- Don't read agent transcript output files through the shell; wait for completion notices.
