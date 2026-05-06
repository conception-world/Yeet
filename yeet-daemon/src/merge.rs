//! Three-way merge between `Tree_Studio`, `Tree_FS`, and `Tree_Base`.
//!
//! Entry point is [`merge_file`]: given the three entries for a single path,
//! it returns a [`MergeOutcome`] describing what the orchestrator should do
//! (apply to one side, adopt base, raise a conflict, etc.).
//!
//! Hunk extraction uses the `similar` crate's line-level Myers diff. Two
//! passes are run — base-vs-studio and base-vs-fs — and the resulting change
//! spans are correlated by their base line ranges. Spans that overlap the
//! same base region become conflict hunks; disjoint spans are pre-resolved
//! to the side that changed (`auto_hunks`) and never surface in the UI.

use serde::{Deserialize, Serialize};
use similar::{DiffOp, TextDiff};

use crate::protocol::ScriptKind;
use crate::tree::TreeEntry;

/// Which side of the sync a value belongs to.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Studio,
    Fs,
}

/// A single hunk the user must resolve. Ranges are line-based and half-open
/// `[start, end)`. `studio_text` / `fs_text` are the replacement lines with
/// trailing newlines preserved, exactly as the `similar` crate yields them.
#[derive(Debug, Clone, Serialize)]
pub struct ConflictHunk {
    pub id: String,
    pub base_range: [usize; 2],
    pub studio_range: [usize; 2],
    pub fs_range: [usize; 2],
    pub studio_text: String,
    pub fs_text: String,
    pub context_before: String,
    pub context_after: String,
}

/// A hunk that only one side touched. These are applied automatically when
/// we rebuild the final content during `ConflictResolved` — they don't
/// appear in the UI but we need to remember them so the final file contains
/// every change that wasn't actually contested.
#[derive(Debug, Clone)]
pub struct AutoHunk {
    pub base_range: [usize; 2],
    pub replacement: Vec<String>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConflictKind {
    /// Both sides modified the existing file with overlapping line changes.
    Edit,
    /// One side deleted the file, the other modified it.
    DeleteVsEdit { deleted: Side },
    /// Base had no file; both sides created one with different content.
    CreateVsCreate,
}

/// Full conflict record carried between the daemon's detection phase and
/// the eventual `ConflictResolved` applied by the plugin.
#[derive(Debug, Clone)]
pub struct FileConflict {
    pub path: String,
    pub conflict_kind: ConflictKind,
    pub script_kind: ScriptKind,
    pub base_content: Option<String>,
    pub studio_content: Option<String>,
    pub fs_content: Option<String>,
    pub conflict_hunks: Vec<ConflictHunk>,
    /// Hunks that aren't contested — applied during rebuild without prompting.
    /// Kept out of the wire protocol; the plugin doesn't need to know.
    pub auto_hunks: Vec<AutoHunk>,
}

/// Result of `merge_file`. The orchestrator decides how to act on it.
#[derive(Debug, Clone)]
pub enum MergeOutcome {
    /// Nothing to do for this path.
    Noop,
    /// Apply a content change (or deletion) to one side.
    Apply {
        side: Side,
        content: Option<String>,
        kind: ScriptKind,
    },
    /// Both sides moved but ended up identical — we just need to adopt the
    /// new sha into the base tree.
    AdoptBase,
    /// Both sides moved, changes don't overlap — rebuild the merged content
    /// and apply it to *both* sides.
    AutoMerge {
        content: String,
        kind: ScriptKind,
    },
    /// Human intervention required. The orchestrator stores the conflict in
    /// `pending_conflicts` and forwards it to the plugin.
    Conflict(FileConflict),
}

/// Three-way merge decision for a single path.
pub fn merge_file(
    path: &str,
    base: Option<&TreeEntry>,
    studio: Option<&TreeEntry>,
    fs: Option<&TreeEntry>,
) -> MergeOutcome {
    match (base, studio, fs) {
        (None, None, None) => MergeOutcome::Noop,
        // Creation on a single side.
        (None, Some(s), None) => MergeOutcome::Apply {
            side: Side::Fs,
            content: Some(s.content.clone()),
            kind: s.kind,
        },
        (None, None, Some(f)) => MergeOutcome::Apply {
            side: Side::Studio,
            content: Some(f.content.clone()),
            kind: f.kind,
        },
        // Both sides created it.
        (None, Some(s), Some(f)) => {
            if s.sha256 == f.sha256 {
                MergeOutcome::AdoptBase
            } else {
                MergeOutcome::Conflict(FileConflict {
                    path: path.to_owned(),
                    conflict_kind: ConflictKind::CreateVsCreate,
                    script_kind: s.kind,
                    base_content: None,
                    studio_content: Some(s.content.clone()),
                    fs_content: Some(f.content.clone()),
                    conflict_hunks: whole_file_conflict(&s.content, &f.content),
                    auto_hunks: vec![],
                })
            }
        }
        // Both sides deleted — accept and drop the base entry.
        (Some(_), None, None) => MergeOutcome::AdoptBase,
        // One side deleted, the other kept or modified.
        (Some(b), None, Some(f)) => {
            if b.sha256 == f.sha256 {
                MergeOutcome::Apply {
                    side: Side::Fs,
                    content: None,
                    kind: f.kind,
                }
            } else {
                MergeOutcome::Conflict(FileConflict {
                    path: path.to_owned(),
                    conflict_kind: ConflictKind::DeleteVsEdit {
                        deleted: Side::Studio,
                    },
                    script_kind: f.kind,
                    base_content: Some(b.content.clone()),
                    studio_content: None,
                    fs_content: Some(f.content.clone()),
                    conflict_hunks: delete_vs_edit_hunk(&b.content, "", &f.content),
                    auto_hunks: vec![],
                })
            }
        }
        (Some(b), Some(s), None) => {
            if b.sha256 == s.sha256 {
                MergeOutcome::Apply {
                    side: Side::Studio,
                    content: None,
                    kind: s.kind,
                }
            } else {
                MergeOutcome::Conflict(FileConflict {
                    path: path.to_owned(),
                    conflict_kind: ConflictKind::DeleteVsEdit { deleted: Side::Fs },
                    script_kind: s.kind,
                    base_content: Some(b.content.clone()),
                    studio_content: Some(s.content.clone()),
                    fs_content: None,
                    conflict_hunks: delete_vs_edit_hunk(&b.content, &s.content, ""),
                    auto_hunks: vec![],
                })
            }
        }
        // The core case: all three entries exist.
        (Some(b), Some(s), Some(f)) => {
            let studio_changed = b.sha256 != s.sha256;
            let fs_changed = b.sha256 != f.sha256;
            match (studio_changed, fs_changed) {
                (false, false) => MergeOutcome::Noop,
                (true, false) => MergeOutcome::Apply {
                    side: Side::Fs,
                    content: Some(s.content.clone()),
                    kind: s.kind,
                },
                (false, true) => MergeOutcome::Apply {
                    side: Side::Studio,
                    content: Some(f.content.clone()),
                    kind: f.kind,
                },
                (true, true) => {
                    if s.sha256 == f.sha256 {
                        MergeOutcome::AdoptBase
                    } else {
                        reconcile_edits(path, b, s, f)
                    }
                }
            }
        }
    }
}

fn reconcile_edits(
    path: &str,
    base: &TreeEntry,
    studio: &TreeEntry,
    fs: &TreeEntry,
) -> MergeOutcome {
    let studio_spans = spans_against_base(&base.content, &studio.content);
    let fs_spans = spans_against_base(&base.content, &fs.content);

    let base_lines = split_lines(&base.content);
    let (conflict_hunks, auto_hunks) =
        correlate_spans(&base_lines, &studio_spans, &fs_spans);

    if conflict_hunks.is_empty() {
        // All changes were on disjoint base regions — fold into a clean
        // auto-merge and apply on both sides.
        let merged = apply_hunks(&base_lines, &auto_hunks, &[], &[]);
        MergeOutcome::AutoMerge {
            content: merged,
            kind: studio.kind,
        }
    } else {
        MergeOutcome::Conflict(FileConflict {
            path: path.to_owned(),
            conflict_kind: ConflictKind::Edit,
            script_kind: studio.kind,
            base_content: Some(base.content.clone()),
            studio_content: Some(studio.content.clone()),
            fs_content: Some(fs.content.clone()),
            conflict_hunks,
            auto_hunks,
        })
    }
}

/// A contiguous change span relative to `base`: base lines `[base_start,
/// base_end)` were replaced with `replacement`. Both inserts (`base_end` ==
/// `base_start`) and deletes (empty `replacement`) fit the same shape.
#[derive(Debug, Clone)]
struct ChangeSpan {
    base_start: usize,
    base_end: usize,
    other_start: usize,
    other_end: usize,
    replacement: Vec<String>,
}

fn spans_against_base(base: &str, other: &str) -> Vec<ChangeSpan> {
    let diff = TextDiff::from_lines(base, other);
    let new_lines = split_lines(other);
    let mut spans: Vec<ChangeSpan> = Vec::new();
    for op in diff.ops() {
        match *op {
            DiffOp::Equal { .. } => {}
            DiffOp::Delete {
                old_index,
                old_len,
                new_index,
            } => spans.push(ChangeSpan {
                base_start: old_index,
                base_end: old_index + old_len,
                other_start: new_index,
                other_end: new_index,
                replacement: Vec::new(),
            }),
            DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => spans.push(ChangeSpan {
                base_start: old_index,
                base_end: old_index,
                other_start: new_index,
                other_end: new_index + new_len,
                replacement: new_lines[new_index..new_index + new_len].to_vec(),
            }),
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => spans.push(ChangeSpan {
                base_start: old_index,
                base_end: old_index + old_len,
                other_start: new_index,
                other_end: new_index + new_len,
                replacement: new_lines[new_index..new_index + new_len].to_vec(),
            }),
        }
    }
    spans
}

/// Splits `content` into lines preserving trailing newlines, matching how
/// `similar::TextDiff::from_lines` indexes.
fn split_lines(content: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut start = 0;
    let bytes = content.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            out.push(content[start..=i].to_owned());
            start = i + 1;
        }
    }
    if start < content.len() {
        out.push(content[start..].to_owned());
    }
    out
}

/// Walk both change-span lists in order of base position. When a studio
/// span and an fs span overlap on the base axis (inclusive of touching at
/// a single boundary when both are zero-width inserts at the same point),
/// merge them into a conflict hunk covering the union. Otherwise each span
/// becomes an auto-hunk belonging to the side that produced it.
fn correlate_spans(
    base_lines: &[String],
    studio_spans: &[ChangeSpan],
    fs_spans: &[ChangeSpan],
) -> (Vec<ConflictHunk>, Vec<AutoHunk>) {
    let mut i = 0;
    let mut j = 0;
    let mut conflict_hunks: Vec<ConflictHunk> = Vec::new();
    let mut auto_hunks: Vec<AutoHunk> = Vec::new();

    while i < studio_spans.len() && j < fs_spans.len() {
        let s = &studio_spans[i];
        let f = &fs_spans[j];
        if spans_overlap_on_base(s, f) {
            // Grow the group as long as subsequent spans on either side
            // keep touching the union on the base axis.
            let mut group_studio: Vec<&ChangeSpan> = vec![s];
            let mut group_fs: Vec<&ChangeSpan> = vec![f];
            i += 1;
            j += 1;
            loop {
                let base_end = group_studio
                    .iter()
                    .map(|x| x.base_end)
                    .chain(group_fs.iter().map(|x| x.base_end))
                    .max()
                    .unwrap_or(0);
                let mut grew = false;
                if i < studio_spans.len() && studio_spans[i].base_start <= base_end {
                    group_studio.push(&studio_spans[i]);
                    i += 1;
                    grew = true;
                }
                if j < fs_spans.len() && fs_spans[j].base_start <= base_end {
                    group_fs.push(&fs_spans[j]);
                    j += 1;
                    grew = true;
                }
                if !grew {
                    break;
                }
            }
            conflict_hunks.push(build_conflict_hunk(
                conflict_hunks.len(),
                base_lines,
                &group_studio,
                &group_fs,
            ));
        } else if s.base_start < f.base_start {
            auto_hunks.push(AutoHunk {
                base_range: [s.base_start, s.base_end],
                replacement: s.replacement.clone(),
            });
            i += 1;
        } else {
            auto_hunks.push(AutoHunk {
                base_range: [f.base_start, f.base_end],
                replacement: f.replacement.clone(),
            });
            j += 1;
        }
    }
    while i < studio_spans.len() {
        let s = &studio_spans[i];
        auto_hunks.push(AutoHunk {
            base_range: [s.base_start, s.base_end],
            replacement: s.replacement.clone(),
        });
        i += 1;
    }
    while j < fs_spans.len() {
        let f = &fs_spans[j];
        auto_hunks.push(AutoHunk {
            base_range: [f.base_start, f.base_end],
            replacement: f.replacement.clone(),
        });
        j += 1;
    }

    (conflict_hunks, auto_hunks)
}

/// Two spans overlap if their half-open base ranges intersect. We also treat
/// two zero-width inserts at the same base position as overlapping, so
/// simultaneous additions at the same line become one conflict hunk.
fn spans_overlap_on_base(a: &ChangeSpan, b: &ChangeSpan) -> bool {
    if a.base_start == a.base_end && b.base_start == b.base_end {
        return a.base_start == b.base_start;
    }
    a.base_start < b.base_end && b.base_start < a.base_end
}

const CONTEXT_LINES: usize = 3;

fn build_conflict_hunk(
    idx: usize,
    base_lines: &[String],
    studio_group: &[&ChangeSpan],
    fs_group: &[&ChangeSpan],
) -> ConflictHunk {
    let base_start = studio_group
        .iter()
        .map(|s| s.base_start)
        .chain(fs_group.iter().map(|s| s.base_start))
        .min()
        .unwrap_or(0);
    let base_end = studio_group
        .iter()
        .map(|s| s.base_end)
        .chain(fs_group.iter().map(|s| s.base_end))
        .max()
        .unwrap_or(base_start);
    let studio_start = studio_group
        .first()
        .map_or(0, |s| s.other_start);
    let studio_end = studio_group.last().map_or(0, |s| s.other_end);
    let fs_start = fs_group.first().map_or(0, |s| s.other_start);
    let fs_end = fs_group.last().map_or(0, |s| s.other_end);

    let ctx_before_start = base_start.saturating_sub(CONTEXT_LINES);
    let ctx_after_end = (base_end + CONTEXT_LINES).min(base_lines.len());

    let context_before = base_lines[ctx_before_start..base_start].concat();
    let context_after = base_lines[base_end..ctx_after_end].concat();

    let studio_text = studio_group
        .iter()
        .flat_map(|s| s.replacement.iter().cloned())
        .collect::<Vec<_>>()
        .concat();
    let fs_text = fs_group
        .iter()
        .flat_map(|s| s.replacement.iter().cloned())
        .collect::<Vec<_>>()
        .concat();

    ConflictHunk {
        id: format!("hunk_{idx}"),
        base_range: [base_start, base_end],
        studio_range: [studio_start, studio_end],
        fs_range: [fs_start, fs_end],
        studio_text,
        fs_text,
        context_before,
        context_after,
    }
}

/// Used when both sides created a file at once — the entire contents of each
/// side becomes a single conflict hunk against an empty base.
fn whole_file_conflict(studio: &str, fs: &str) -> Vec<ConflictHunk> {
    let studio_lines = split_lines(studio);
    let fs_lines = split_lines(fs);
    vec![ConflictHunk {
        id: "hunk_0".to_owned(),
        base_range: [0, 0],
        studio_range: [0, studio_lines.len()],
        fs_range: [0, fs_lines.len()],
        studio_text: studio.to_owned(),
        fs_text: fs.to_owned(),
        context_before: String::new(),
        context_after: String::new(),
    }]
}

/// `DeleteVsEdit`: one side is empty (deleted), the other carries the text.
/// We synthesize a single whole-file hunk so the resolver UI has something
/// to key its file-level "keep deletion / restore" buttons to.
fn delete_vs_edit_hunk(base: &str, studio: &str, fs: &str) -> Vec<ConflictHunk> {
    let base_lines = split_lines(base);
    let studio_lines = split_lines(studio);
    let fs_lines = split_lines(fs);
    vec![ConflictHunk {
        id: "hunk_0".to_owned(),
        base_range: [0, base_lines.len()],
        studio_range: [0, studio_lines.len()],
        fs_range: [0, fs_lines.len()],
        studio_text: studio.to_owned(),
        fs_text: fs.to_owned(),
        context_before: String::new(),
        context_after: String::new(),
    }]
}

/// Resolution choice for a single conflict hunk, as sent back by the plugin.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "choice", rename_all = "snake_case")]
pub enum HunkResolution {
    KeepStudio,
    KeepFs,
    /// Concatenate studio text then fs text.
    KeepBoth,
    /// Replace the hunk's base region with arbitrary text.
    Manual { text: String },
}

/// The concrete thing the orchestrator does for a resolved conflict:
/// either write the merged content to both sides, or propagate a deletion.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ResolvedAction {
    Write(String),
    Delete,
}

/// High-level resolver: picks between write/delete based on the conflict's
/// kind, then delegates content reconstruction to `rebuild_resolved`.
pub fn resolve_to_action(
    conflict: &FileConflict,
    resolutions: &std::collections::HashMap<String, HunkResolution>,
) -> ResolvedAction {
    if let ConflictKind::DeleteVsEdit { deleted } = conflict.conflict_kind {
        let Some(hunk) = conflict.conflict_hunks.first() else {
            // Defensive: a delete-vs-edit conflict without a hunk means we
            // don't know what the user chose. Keep the extant side.
            return match deleted {
                Side::Studio => ResolvedAction::Delete,
                Side::Fs => conflict
                    .studio_content
                    .clone()
                    .map_or(ResolvedAction::Delete, ResolvedAction::Write),
            };
        };
        let choice = resolutions
            .get(&hunk.id)
            .cloned()
            .unwrap_or(HunkResolution::KeepStudio);
        return match choice {
            HunkResolution::KeepStudio => match deleted {
                Side::Studio => ResolvedAction::Delete,
                Side::Fs => conflict
                    .studio_content
                    .clone()
                    .map_or(ResolvedAction::Delete, ResolvedAction::Write),
            },
            HunkResolution::KeepFs => match deleted {
                Side::Fs => ResolvedAction::Delete,
                Side::Studio => conflict
                    .fs_content
                    .clone()
                    .map_or(ResolvedAction::Delete, ResolvedAction::Write),
            },
            HunkResolution::KeepBoth => {
                // Keep-both on a delete-vs-edit means "don't delete, keep the
                // modified side". Whichever side survived the deletion wins.
                let surviving = conflict
                    .studio_content
                    .clone()
                    .or_else(|| conflict.fs_content.clone())
                    .unwrap_or_default();
                ResolvedAction::Write(surviving)
            }
            HunkResolution::Manual { text } => {
                if text.is_empty() {
                    ResolvedAction::Delete
                } else {
                    ResolvedAction::Write(text)
                }
            }
        };
    }
    let content = rebuild_resolved(conflict, resolutions);
    ResolvedAction::Write(content)
}

/// Rebuilds the final content for a conflict file by splicing hunk
/// resolutions back into the base line array. Auto-hunks are always
/// applied; conflict hunks look up their resolution by id and fall back
/// to `KeepStudio` if the plugin omitted one (defensive — should never
/// happen in practice).
pub fn rebuild_resolved(
    conflict: &FileConflict,
    resolutions: &std::collections::HashMap<String, HunkResolution>,
) -> String {
    let base = conflict.base_content.as_deref().unwrap_or("");
    let base_lines = split_lines(base);
    let conflict_replacements: Vec<(usize, usize, Vec<String>)> = conflict
        .conflict_hunks
        .iter()
        .map(|h| {
            let choice = resolutions
                .get(&h.id)
                .cloned()
                .unwrap_or(HunkResolution::KeepStudio);
            let replacement = match choice {
                HunkResolution::KeepStudio => split_lines(&h.studio_text),
                HunkResolution::KeepFs => split_lines(&h.fs_text),
                HunkResolution::KeepBoth => {
                    let mut out = split_lines(&h.studio_text);
                    out.extend(split_lines(&h.fs_text));
                    out
                }
                HunkResolution::Manual { text } => split_lines(&text),
            };
            (h.base_range[0], h.base_range[1], replacement)
        })
        .collect();
    apply_hunks(
        &base_lines,
        &conflict.auto_hunks,
        &conflict.conflict_hunks,
        &conflict_replacements,
    )
}

/// Splices all hunks into the base line array. `conflict_replacements` is a
/// parallel list against `conflict_hunks` carrying the user's chosen text.
/// All hunks are sorted by base position and applied in descending order so
/// earlier hunks' base indices stay valid.
fn apply_hunks(
    base_lines: &[String],
    auto_hunks: &[AutoHunk],
    conflict_hunks: &[ConflictHunk],
    conflict_replacements: &[(usize, usize, Vec<String>)],
) -> String {
    let mut splices: Vec<(usize, usize, Vec<String>)> = Vec::new();
    for a in auto_hunks {
        splices.push((a.base_range[0], a.base_range[1], a.replacement.clone()));
    }
    for (i, _h) in conflict_hunks.iter().enumerate() {
        if let Some(repl) = conflict_replacements.get(i) {
            splices.push(repl.clone());
        }
    }
    splices.sort_by_key(|(start, _, _)| *start);
    let mut out: Vec<String> = base_lines.to_vec();
    // Apply in reverse so splicing indices stay valid.
    for (start, end, repl) in splices.into_iter().rev() {
        let end = end.min(out.len());
        let start = start.min(end);
        out.splice(start..end, repl);
    }
    out.concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(content: &str) -> TreeEntry {
        TreeEntry {
            kind: ScriptKind::ModuleScript,
            content: content.to_owned(),
            sha256: crate::state::sha256_hex(content.as_bytes()),
        }
    }

    #[test]
    fn noop_when_all_equal() {
        let b = entry("a\nb\nc\n");
        let s = b.clone();
        let f = b.clone();
        assert!(matches!(
            merge_file("p", Some(&b), Some(&s), Some(&f)),
            MergeOutcome::Noop
        ));
    }

    #[test]
    fn clean_fs_only_change() {
        let b = entry("a\nb\nc\n");
        let s = b.clone();
        let f = entry("a\nX\nc\n");
        let out = merge_file("p", Some(&b), Some(&s), Some(&f));
        match out {
            MergeOutcome::Apply {
                side: Side::Studio,
                content: Some(c),
                ..
            } => assert_eq!(c, "a\nX\nc\n"),
            other => panic!("expected Apply to studio, got {other:?}"),
        }
    }

    #[test]
    fn clean_studio_only_change() {
        let b = entry("a\nb\nc\n");
        let s = entry("a\nY\nc\n");
        let f = b.clone();
        let out = merge_file("p", Some(&b), Some(&s), Some(&f));
        match out {
            MergeOutcome::Apply {
                side: Side::Fs,
                content: Some(c),
                ..
            } => assert_eq!(c, "a\nY\nc\n"),
            other => panic!("expected Apply to fs, got {other:?}"),
        }
    }

    #[test]
    fn disjoint_edits_auto_merge() {
        let b = entry("a\nb\nc\nd\ne\nf\n");
        let s = entry("a\nSTUDIO\nc\nd\ne\nf\n");
        let f = entry("a\nb\nc\nd\nFS\nf\n");
        let out = merge_file("p", Some(&b), Some(&s), Some(&f));
        match out {
            MergeOutcome::AutoMerge { content, .. } => {
                assert_eq!(content, "a\nSTUDIO\nc\nd\nFS\nf\n");
            }
            other => panic!("expected AutoMerge, got {other:?}"),
        }
    }

    #[test]
    fn overlapping_edits_conflict() {
        let base = entry("a\nb\nc\nd\ne\n");
        let studio = entry("a\nSTUDIO\nc\nd\ne\n");
        let fs = entry("a\nFS\nc\nd\ne\n");
        let out = merge_file("p", Some(&base), Some(&studio), Some(&fs));
        match out {
            MergeOutcome::Conflict(conflict) => {
                assert!(matches!(conflict.conflict_kind, ConflictKind::Edit));
                assert_eq!(conflict.conflict_hunks.len(), 1);
                let hunk = &conflict.conflict_hunks[0];
                assert_eq!(hunk.studio_text, "STUDIO\n");
                assert_eq!(hunk.fs_text, "FS\n");
                assert_eq!(hunk.base_range, [1, 2]);
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    #[test]
    fn both_create_different_content() {
        let s = entry("line_a\n");
        let f = entry("line_b\n");
        let out = merge_file("p", None, Some(&s), Some(&f));
        match out {
            MergeOutcome::Conflict(c) => {
                assert!(matches!(c.conflict_kind, ConflictKind::CreateVsCreate));
                assert_eq!(c.conflict_hunks.len(), 1);
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    #[test]
    fn both_create_same_content_adopts_base() {
        let s = entry("same\n");
        let f = entry("same\n");
        assert!(matches!(
            merge_file("p", None, Some(&s), Some(&f)),
            MergeOutcome::AdoptBase
        ));
    }

    #[test]
    fn delete_on_unmodified_side_is_clean() {
        let b = entry("a\n");
        let s = b.clone();
        let out = merge_file("p", Some(&b), Some(&s), None);
        assert!(matches!(
            out,
            MergeOutcome::Apply {
                side: Side::Studio,
                content: None,
                ..
            }
        ));
    }

    #[test]
    fn delete_vs_modify_is_conflict() {
        let b = entry("original\n");
        let s = entry("studio_edit\n");
        let out = merge_file("p", Some(&b), Some(&s), None);
        match out {
            MergeOutcome::Conflict(c) => {
                assert!(matches!(
                    c.conflict_kind,
                    ConflictKind::DeleteVsEdit { deleted: Side::Fs }
                ));
                assert!(c.fs_content.is_none());
                assert_eq!(c.studio_content.as_deref(), Some("studio_edit\n"));
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    #[test]
    fn rebuild_keep_studio() {
        let b = entry("a\nb\nc\n");
        let s = entry("a\nSTUDIO\nc\n");
        let f = entry("a\nFS\nc\n");
        let conflict = match merge_file("p", Some(&b), Some(&s), Some(&f)) {
            MergeOutcome::Conflict(c) => c,
            other => panic!("expected Conflict, got {other:?}"),
        };
        let mut res = std::collections::HashMap::new();
        res.insert(conflict.conflict_hunks[0].id.clone(), HunkResolution::KeepStudio);
        let rebuilt = rebuild_resolved(&conflict, &res);
        assert_eq!(rebuilt, "a\nSTUDIO\nc\n");
    }

    #[test]
    fn rebuild_keep_both() {
        let b = entry("a\nb\nc\n");
        let s = entry("a\nSTUDIO\nc\n");
        let f = entry("a\nFS\nc\n");
        let conflict = match merge_file("p", Some(&b), Some(&s), Some(&f)) {
            MergeOutcome::Conflict(c) => c,
            other => panic!("expected Conflict, got {other:?}"),
        };
        let mut res = std::collections::HashMap::new();
        res.insert(conflict.conflict_hunks[0].id.clone(), HunkResolution::KeepBoth);
        let rebuilt = rebuild_resolved(&conflict, &res);
        assert_eq!(rebuilt, "a\nSTUDIO\nFS\nc\n");
    }

    #[test]
    fn rebuild_manual_text() {
        let b = entry("a\nb\nc\n");
        let s = entry("a\nSTUDIO\nc\n");
        let f = entry("a\nFS\nc\n");
        let conflict = match merge_file("p", Some(&b), Some(&s), Some(&f)) {
            MergeOutcome::Conflict(c) => c,
            other => panic!("expected Conflict, got {other:?}"),
        };
        let mut res = std::collections::HashMap::new();
        res.insert(
            conflict.conflict_hunks[0].id.clone(),
            HunkResolution::Manual {
                text: "MANUAL\n".to_owned(),
            },
        );
        let rebuilt = rebuild_resolved(&conflict, &res);
        assert_eq!(rebuilt, "a\nMANUAL\nc\n");
    }
}
