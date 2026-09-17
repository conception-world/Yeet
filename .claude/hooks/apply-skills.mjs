// Yeet project — UserPromptSubmit hook.
// Prints a standing reminder to apply all 4 tool skills on every prompt.
// Claude Code injects this stdout into the model's context before it answers.
process.stdout.write(
`[Yeet skill policy — apply on EVERY prompt]
1. superpowers -> use the methodology (brainstorm -> plan -> TDD -> review -> verify) for non-trivial coding work. Skill: use-superpowers.
2. rtk -> prefix ALL shell commands with \`rtk\`. Skill: use-rtk.
3. caveman -> compress OUTPUT prose (terse); keep PT-BR + code/commits/errors verbatim; drop caveman for warnings/irreversible/multi-step. Skill: use-caveman.
4. graphify -> codebase/architecture questions = query the knowledge graph first. Skill: use-graphify.
Details in .claude/skills/. Turn caveman off with "normal mode".
`);
