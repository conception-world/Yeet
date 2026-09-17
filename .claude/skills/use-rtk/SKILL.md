---
name: use-rtk
description: Yeet project policy — prefix EVERY shell command with `rtk` to cut command-output tokens 60-90%. RTK (Rust Token Killer) is installed at ~/.cargo/bin/rtk. Applies to git, cargo, npm/pnpm, tsc, tests, and ls/read/grep/find.
---

# use-rtk (Yeet policy)

RTK (Rust Token Killer) v0.37.x is installed (`~/.cargo/bin/rtk`) and documented in the global
`~/.claude/CLAUDE.md`. This skill makes RTK the default for shell work in this project.

## Rule

Prefix shell commands with `rtk`. If RTK has a filter it compresses the output; otherwise it
passes through unchanged — so it is always safe to use.

```
rtk git status      rtk git diff        rtk git add .       rtk git commit -m "..."
rtk cargo build     rtk cargo test      rtk cargo clippy
rtk npm run <s>     rtk pnpm install    rtk tsc             rtk lint
rtk ls <path>       rtk read <file>     rtk grep <pat>      rtk find <pat>
```

- Even inside `&&` chains, prefix EACH command with `rtk`.
- Debug the raw, unfiltered output with `rtk proxy <cmd>`.
- See savings with `rtk gain`.
