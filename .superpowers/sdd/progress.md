# Yeet audit-fixes — progress ledger

Branch: `audit-fixes` (from `main`). Plan: `AUDITORIA-YEET.md`. Started 2026-07-12.
Baseline: daemon `cargo check --tests` clean; extension `tsc` available; plugin lint = `selene` (no unit runner).

## Track ownership (file-disjoint → parallel-safe)
- **DAEMON** (Rust, TDD): main.rs, state.rs, tree.rs, watcher.rs, merge.rs, protocol.rs, auth.rs
- **DSYNC** (Rust): syncback.rs, project.rs — folded into DAEMON track (same crate, sequential) to keep the build green
- **EXT** (TS): yeet-extension/src/*
- **PSC** (plugin sync-core): Applier, SourceWatcher, TreeBuilder, Widget, Reconnect, Connection
- **PSB** (plugin syncback): Walker, PropertyTable, Base64, ChunkedSender
- **PUI** (plugin ui): DiffView, diffLines
- **WAVE2** cross-cutting after wave1: A4 apply-ACK (protocol+main+Applier+Widget), A10 nested project.json (project.rs+TreeBuilder)

## Tasks
- [ ] DT1: A1 + M17 (fs_removed_pending by path; deferred reconcile by own path; rename-echo after hash check) — main.rs, state.rs
- [ ] DT2: A7 + M6 + M18 + M19 (folder rename on disk; watcher backend errors→rescan; RenameMode Both/Any) — main.rs, watcher.rs, state.rs
- [ ] DT3: A2 (pending_conflicts authoritative in reconcile) — main.rs
- [ ] DT4: M10 + M11 + M12 (dir-rename storm/guard; move .meta.json; Promote rollback) — main.rs
- [ ] DT5: A16 + A17 + M24 + B13 (auth reject+no-leak; syncback target confine; origin exact; bind guard) — main.rs, auth.rs
- [ ] DT6: M15 + B5 (save_base_tree fsync; meta_attributes cleanup) — tree.rs, main.rs
- [ ] DS1: C1 (case-insensitive collision) — syncback.rs
- [ ] DS2: M2 + M7 (merge-overwrite orphan cleanup; exclude Packages from syncback) — syncback.rs
- [ ] DS3: M21d + M22 + M23 + A5d (init reserved; reserved+ext; MAX_PATH \\?\; warn on missing Source) — syncback.rs
- [ ] EXT: A9, M4, M25, B1, B2, B3, B7, B12, B14 — yeet-extension/src/*
- [ ] PSC: A8, A11, A12, A3, M8, M9, M13, M20p, M21p, M26, B11, A6, B10 — plugin sync-core
- [ ] PSB: A5p, A14, M16, B4, large-6 — plugin syncback
- [ ] PUI: A15 — DiffView, diffLines
- [ ] W2-A4: apply-ACK end-to-end
- [ ] W2-A10: nested Wally project.json (daemon + plugin)
- [ ] FINAL: whole-branch review + full build/test

## Reviewed clean — pending merge into audit-fixes (merge at final integration; main tree busy with daemon)
- EXT: branch `worktree-agent-abfe48752e69bf442`, commits d53f07b..be30e57 (A9,M4,M25,B1,B3,B7,B12,B14). Review: PASS. tsc+esbuild clean; CLIENT_VERSION resolves 0.4.1.
- PUI: branch `worktree-agent-a9131377949703111`, commit 4b8e047 (A15). Review: PASS (manual — see note). Cap 2000 lines before LCS matrix; insert-at-1 → append+reverse; sentinel by identity; DiffView O(1) fallback. Normal path unchanged.
- PSB: branch `worktree-agent-ac1bab208c9b7092e`, commits 9d2e430,4b21f9a,217ab8e,2b487ba (A5,A14,M16,B4). Review: PASS. A5 injects Source as {type,value} matching daemon SerializedProperty (encodeValue confirmed public member, PropertyTable.luau:156); daemon drop_source avoids .meta.json dup. A14 byte-budget 8MiB + oversized-skip warning (not silent). M16/B4 per agent (byte-identical / memoized) + selene clean w/ roblox config.
- DT1(A1): commit f5b009f ON audit-fixes (main tree, already integrated). Review: PASS. 134 cargo tests pass.
- C1(path-1, CRÍTICO): branch `worktree-agent-a6061e771d2341417`, commit af119ab. Salvaged (agent failed on session limit mid-test). Orchestrator fixed a Windows-hostile test assert (join("config").exists() resolves case-insensitively) → read_dir-based. `cargo test syncback: 9 passed`. Review: PASS.

## Session-limit recovery note
Sonnet-track agents hit "session limit resets 6am". daemon-sec + psc-perf failed with ZERO committed changes (main tree verified clean at f5b009f). Re-dispatched on OPUS: security (main tree, agent abaac22f768499a66) + psc-perf (worktree, agent a8e6226fe8ab43cc0). Empty stale worktrees to clean at integration: agent-abbb05119de70b433 (old psc-perf, empty).

## TODO at final integration
- Add `yeet-plugin/selene.toml` (std="roblox") so plugin verification works repo-wide (PSB found bare selene can't parse --!strict without it). Then re-run selene on all changed plugin files.

## VERIFICATION GAP (plugin)
selene 0.30.0 panics on typed Luau signatures in this env; no luau-lsp/luau-analyze. Plugin tracks have NO automated verification — review is the only gate. Recommend a real luau-lsp/Studio typecheck of yeet-plugin before shipping. MergePicker shows a placeholder row for oversized files (known limitation, not a regression).

## Minor findings (defer to final cleanup wave)
- EXT B7+B12: dropped bulk-sync shows TWO warning toasts (send() warns for every caller + runBulkSync warns on false). Collapse to one — keep send() log + caller-decided toast, or drop runBulkSync's redundant warn.

## INTEGRATED into audit-fixes (merged, build green)
- A1 (f5b009f), security A16/A17/M24/B13 (51e4cf6..780bf41), selene.toml (c199fab), AuthRejected loop-fix (7e8d765), + merges of EXT/PUI/PSB/C1 branches.
- AuthRejected loop-fix (orchestrator-authored): A16 rejects stale tokens; daemon regenerates token each boot → without a signal the plugin looped. Daemon now sends AuthRejected{reason} (no token) before close; plugin Widget onAuthRejected clears yeet.authToken → re-pairs tokenless. Full `cargo test`: 153 passed (concurrency.rs flakes under parallel load; passes isolated with --test-threads=1).
- selene NOW WORKS repo-wide via yeet-plugin/selene.toml (std=roblox). Verify plugin edits with `rtk selene <file>`.

## Running (opus)
- D-watcher (A7/M17/M18/M19) MAIN tree (agent a97fd490e1d68c39e); DS-path (M2/M7/M21/M22/M23/A5-guard) worktree (agent a6208597c4184e6c4).

## A2 INTEGRATED (main tree, dd218c8) — Review PASS
Approach (a): reconcile_path checks pending_conflicts; if open, refresh_pending_conflict re-syncs the snapshot to current trees (Conflict adopted; clean/AutoMerge → 0-hunk conflict with base=resolved; AdoptBase uses fs/studio; delete/Noop frozen). Prevents silent revert/AutoMerge-discard. cargo test 160 passed. Minor concern (plugin): rendering a 0-hunk ConflictDetected not exercised — check in PSC-correct.
clippy note: pre-existing -D errors in audit.rs (untouched, not ours).

## Reviewed clean, PENDING MERGE (main tree busy with D-A2)
- psc-perf: branch `worktree-agent-a8e6226fe8ab43cc0`, commits 1cdac3b(A12),7c798e3(A11),d8db379(M8),7b23ce1(M9). Review PASS: A12 isEcho(path,content) byte-length-then-hash — verified all callers (SourceWatcher 2 sites updated, Widget:898 forwards positionally). M9 create-echo suppression SAFE (daemon sets tree_studio in push_to_studio before broadcast → echo redundant). Minor: Widget:898 param still named `hash` (cosmetic).
- MERGE ORDER at integration: psc-perf must merge BEFORE dispatching PSC-correct (both touch Applier/SourceWatcher).

## Still pending
- DT2 watcher (A7/M17/M18/M19), M10/M11/M12 rename-io, M15/B5 (all main.rs/watcher/tree — after A2 frees main tree)
- PSC-correct (A8/A3/M13/M26/B11/A6/B10) — after psc-perf frees Applier/SourceWatcher
- Wave 2: A4 apply-ACK, A10 nested Wally project.json
- Minor cleanups: EXT B7/B12 double-toast.

## ==== M1 sourcemap (was missed) — DONE 2026-07-12 ====
setup-1/wally-4 (sourcemap.json never generated) was NOT covered by the original fix tracks — user reported it during testing. Now implemented: new yeet-daemon/src/sourcemap.rs (pure build_sourcemap mirroring TreeBuilder + A10 collapse, deterministic BTreeMap output); regenerated at bootstrap(after rescan_fs) + syncback materialize + structural changes (FileCreated/Deleted/Renamed via broadcast_server_msg + write_to_fs/delete_from_fs), debounced 300ms w/ structure_signature guard (content edits don't rewrite). Extension create.ts writes an initial sourcemap on Yeet: Create. Commits 75e59f6(daemon)+9995c41(ext). cargo test 205 passed (+6). FUNCTIONALLY VERIFIED: ran release daemon on temp project → generated correct Rojo sourcemap.json at bootstrap (DataModel→ServerScriptService→ModuleScript Foo w/ filePaths). Artifacts rebuilt (0.5.0) with M1. Version strings finished (plugin load print + dock title + Cargo.lock).

## ==== COMPLETE ====
ALL ~50 audit findings fixed + integrated on `audit-fixes` (HEAD 8cc3f34). 45 fix/perf commits, 54 total, 24 files, +6005/-541 vs main. FINAL VERIFY (all 3 components green): daemon `cargo test --test-threads=1` 199 passed/1 ignored; extension tsc+esbuild clean; plugin selene 0 NEW errors (3 pre-existing in untouched BulkSyncPreview/ConflictResolver/MergePicker), 0 parse errors. Nothing pushed. A10 was the last finding.
Remaining MINOR (optional, non-blocking): EXT B7/B12 double-toast; A8 TreeBuilder helper dedup (~25 dup lines); A2 0-hunk ConflictDetected plugin render (edge); pre-existing clippy -D in audit.rs; wally-2(M5 inflation)/wally-3(M6 churn) partially overlap A10 but are separate findings already tracked as done or deferred. Plugin has NO unit-test runner — recommend a real luau-lsp/Studio typecheck + manual Studio smoke test before merging to main.

## ==== WAVE 2 ====
A4+M14 (apply-ACK) INTEGRATED: f80a3c7 (daemon protocol+main.rs+state.rs: pending_applies gate — tree_base/tree_studio advance only on FileApplied ACK w/ sha match; FileApplyFailed/8s-timeout sweep for old plugins, no silent advance) + e2f7efb (plugin Applier sends file_applied/file_apply_failed). cargo test 198 passed. selene clean. Review PASS.
A10 (nested Wally project.json) RUNNING main tree (agent ac05482f76bac26d3) — LAST finding.
After A10: final whole-branch review + verify + `git worktree remove` all worktrees + `git stash drop` the dead D-watcher partial.

## ==== ROUND 3 (integration complete) ====
INTEGRATED & VERIFIED on audit-fixes: D-renameio (M10/M11/M12/M15/B5, 8ddebd3+a679b87), PSC-correct (A3/A6/A8/M13/M26/B10/B11, merge eaa44c1 — SourceWatcher/Widget conflict resolved), DS-M2 (M2, 9cc99f9 merged after discarding a duplicate uncommitted copy that had leaked into main tree). Full daemon `cargo test -- --test-threads=1`: 190 passed, 1 ignored. Plugin selene: 0 NEW errors (3 pre-existing in BulkSyncPreview/ConflictResolver/MergePicker — untouched). Working tree clean.
ONLY WAVE 2 LEFT: A4 apply-ACK (dispatching now, main tree), A10 nested Wally project.json.
Findings done so far: ~43 of ~50. Remaining: A4, A10, + minor cleanups (EXT B7/B12 double-toast; A8 TreeBuilder helper dedup; A2 0-hunk render check).

## ==== RESUME (limit raised) — round 2 ====
DONE round 2: D-watcher (A7/M17/M18/M19, a06b15c, 182 tests, INTEGRATED on audit-fixes); PSC-correct (A3/A6/A8/M13/M26/B10/B11, branch worktree-agent-a472bd04b6fa7e73c, selene clean) — CONFLICT-RESOLVED vs audit-fixes (merge-tree now exit 0, ready to merge); DS-M2 (9cc99f9, branch worktree-agent-a07d8758dadc795bc, merge-tree clean, ready to merge).
CAUGHT A MERGE BUG in PSC-correct: worktrees branch from 1412a18 (NOT current audit-fixes), so PSC-correct's SourceWatcher (A3, base had per-keystroke syncHash) auto-merged with audit-fixes A12 (removed hot-path hash) leaving `syncHash` undefined → A3 recovery silently broken. Resolved: store pendingContent only, hash lazily in peekPending + debounce recovery (preserves A12 no-per-keystroke-hash AND A3). SourceWatcher+Widget resolved, selene 0 errors.
RUNNING: D-renameio (M10/M11/M12/M15/B5) main tree (agent ac631cb61cd2d5694).
NEXT after D-renameio: merge DS-M2 + PSC-correct into audit-fixes; full test; then Wave 2 (A4 apply-ACK, A10 nested Wally). NOTE for A10/any future worktree agent: it will branch from 1412a18 — tell it to `git reset --hard audit-fixes` first (like DS-M2 did) OR expect a resolve step.

## ==== CHECKPOINT (monthly spend limit hit) ====
Branch `audit-fixes`. Full daemon suite: **169 passed, 1 ignored** (`cargo test -- --test-threads=1`). Plugin: 0 NEW selene errors (3 pre-existing in untouched BulkSyncPreview/ConflictResolver/MergePicker). Ext: tsc+esbuild clean. ALL work below is committed + merged into audit-fixes.

### DONE & INTEGRATED (~31 findings)
C1(Crítico); A1,A2,A5,A9,A11,A12,A14,A15,A16,A17(+auth-loop-fix); M4,M7,M8,M9,M16,M21,M22,M23,M24,M25; B1,B3,B4,B7,B12,B13,B14; selene.toml.

### REMAINING (~19 findings — need spend limit raised to resume)
- STASHED partial (git stash: "WIP D-watcher M18+partial"): A7,M17,M18,M19 — main.rs/state.rs/watcher.rs did NOT compile; re-do fresh (don't pop the broken stash blindly — M18 was working, A7/M17 incomplete).
- NOT STARTED daemon: M10,M11,M12 (rename-io perform_rename_io), M15(fsync save_base_tree)+B5(meta_attributes cleanup), M2 (merge-overwrite cleanup — DS-path deferred it, partial was discarded).
- NOT STARTED plugin PSC-correct (data-loss!): A3(race-2 lost update), A6(resil-1 output buffer), A8(reparent), M13(preview reset instanceToPath), M26(project_root validation), B10(log after send), B11(log disconnected). Files: Applier/SourceWatcher/Widget/Reconnect/Connection. psc-perf ALREADY MERGED so build on current Applier/SourceWatcher.
- NOT STARTED Wave 2: A4(apply-ACK, cross daemon+plugin), A10(nested Wally project.json, daemon project.rs + plugin TreeBuilder).
- Minor: EXT B7/B12 double-toast collapse; A2 plugin 0-hunk ConflictDetected render check; Widget:898 param name `hash`→`content` (cosmetic).

### Worktrees to clean at end: agent-abfe48752e69bf442, a9131377949703111, ac1bab208c9b7092e, a6061e771d2341417 (all MERGED); a8e6226fe8ab43cc0, a6208597c4184e6c4 (MERGED); agent-abbb05119de70b433 (empty). `git worktree remove` each after confirming merged.
### Resume: raise limit at claude.ai/settings/usage, then continue PSC-correct + daemon watcher/rename-io + Wave 2 per this ledger.
