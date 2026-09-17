# Yeet — project instructions

## Standing skill policy (apply on EVERY prompt)

This project installs and always applies 4 tools. On every task, read and apply the matching
skill in `.claude/skills/`:

| Tool | Skill | What to always do |
|------|-------|-------------------|
| Superpowers | `use-superpowers` | Follow the methodology on non-trivial coding tasks: brainstorm → plan → TDD → subagents → code review → git worktrees → verify. |
| RTK | `use-rtk` | Prefix every shell command with `rtk` (60-90% fewer output tokens). |
| Caveman | `use-caveman` | Compress output prose (~65% fewer tokens); keep PT-BR language and code/commits/errors verbatim; drop caveman for warnings / irreversible / multi-step. |
| Graphify | `use-graphify` | For codebase/architecture questions, query the knowledge graph first; build `graphify-out/` if missing. |

A `UserPromptSubmit` hook (`.claude/settings.local.json` → `.claude/hooks/apply-skills.mjs`)
re-injects this reminder on every prompt. The reminder + skills take effect in each new session.

## Project shape

- `yeet-daemon/` — Rust sync daemon.
- `yeet-plugin/` — Luau (`--!strict`) Roblox Studio plugin.
- `yeet-extension/` — TypeScript (strict) editor extension (VS Code / Antigravity).

Bidirectional full-tree sync; Rojo-compatible on disk but not a Rojo wrapper.

## Conventions

- Explanations in **Portuguese**; code and code comments in **English**.
- Production-ready code: explicit types, no `any`, no Luau globals unless the Studio API demands.
  Comments only when non-obvious.
