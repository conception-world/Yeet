---
name: use-graphify
description: Yeet project policy — for any question about the codebase's architecture, file relationships, or how something works, consult the Graphify knowledge graph FIRST. Graphify is installed (graphifyy 0.9.13, CLI `graphify`, skill /graphify). Build graphify-out/ if missing, otherwise query it.
---

# use-graphify (Yeet policy)

Graphify is installed (`graphifyy` 0.9.13; CLI `graphify`; global skill `/graphify`). The CLI is at
`C:/Users/ihyhe/AppData/Roaming/Python/Python314/Scripts/graphify.exe` (its Scripts dir was added
to the user PATH).

## Rule

- A **codebase / architecture question** ("how does X work?", "what calls Y?", "trace Z") is a graph
  query first:
  - If `graphify-out/graph.json` exists → `graphify query "<question>"`.
  - If not → build once with `/graphify .`, then query.
- After large changes, refresh incrementally: `graphify <path> --update`.
- Honesty rules: never invent edges; always show token cost; warn before HTML viz on >5000 nodes.

Full runbook: the installed skill at `~/.claude/skills/graphify/SKILL.md`.
