---
name: use-superpowers
description: Yeet project policy — apply the Superpowers development methodology on EVERY non-trivial coding task (brainstorm → plan → TDD → subagents → code review → git worktrees → verify). Use for any change to the Rust daemon, Luau plugin, or TS extension. Wraps the installed superpowers@superpowers-marketplace plugin.
---

# use-superpowers (Yeet policy)

Superpowers is installed as the `superpowers@superpowers-marketplace` plugin (v6.1.1). This
skill is the standing instruction to USE that methodology in this project instead of jumping
straight to code.

## Always apply

For any non-trivial task, follow the Superpowers flow:

1. **brainstorming** — refine the spec through dialogue before designing.
2. **writing-plans** / **executing-plans** — break work into bite-size, verifiable tasks.
3. **test-driven-development** — RED → GREEN → REFACTOR.
4. **systematic-debugging** — find the root cause, do not patch symptoms.
5. **subagent-driven-development** / **dispatching-parallel-agents** — fan out independent work.
6. **requesting-code-review** / **receiving-code-review** — review against the spec.
7. **using-git-worktrees** / **finishing-a-development-branch** — isolate and land the branch.
8. **verification-before-completion** — prove it works before calling it done.

Entry skill: `using-superpowers`. All skills live under
`~/.claude/plugins/cache/superpowers-marketplace/superpowers/<version>/skills/`.

## Yeet specifics

- Respect the 3-component split: `yeet-daemon/` (Rust), `yeet-plugin/` (Luau), `yeet-extension/` (TS).
- Keep code production-ready and explicitly typed (see project `CLAUDE.md`); this reinforces,
  not replaces, those rules.
- For a trivial one-liner, the lightweight path is fine — but still verify before finishing.
