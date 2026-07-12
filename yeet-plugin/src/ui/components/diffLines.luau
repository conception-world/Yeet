--!strict

-- Pure-function helpers for the line-level diff that DiffView and
-- (eventually) MergePicker render. Split out so the algorithm has a single
-- place to live and so future tests can exercise it without spinning up
-- the surrounding Roact tree.
--
-- The LCS-based diff is O(N*M) in time and memory. For typical Yeet
-- payloads (Roblox scripts <500 lines) that's well under 1 ms. Above
-- MAX_DIFF_LINES combined lines, `align` skips the matrix allocation
-- entirely and returns a sentinel (see REGRESSION(A15) below) so a
-- 20K-line generated file can't freeze/OOM the caller.

local diffLines = {}

-- Splits `text` on `\n`, preserving all empty lines including a trailing
-- empty if `text` ends with `\n`. Returns at least one entry — an empty
-- string yields `{ "" }`. Matches the daemon's notion of "lines" so the
-- diff ranges line up with the merge engine's hunk ranges.
function diffLines.split(text: string): { string }
	local out: { string } = {}
	local cursor = 1
	while true do
		local nl = text:find("\n", cursor, true)
		if nl == nil then
			table.insert(out, text:sub(cursor))
			return out
		end
		table.insert(out, text:sub(cursor, nl - 1))
		cursor = nl + 1
	end
end

-- One row in the aligned diff. `kind` describes the relationship:
--   * "context" — both sides have the line (left == right).
--   * "removed" — only `left` is present (was in studio, not in ide).
--   * "added"   — only `right` is present (was added in ide).
--   * "modified" — both sides have a line at this aligned position but the
--     text differs. Treated as a paired remove+add for colouring.
-- Indices are 1-based or nil when that side has no line for this row.
export type DiffRow = {
	kind: "context" | "removed" | "added" | "modified",
	left: string?,
	right: string?,
	left_lineno: number?,
	right_lineno: number?,
}

-- Computes the LCS table for `a` and `b`. Standard dynamic-programming
-- formulation — `lcs[i][j]` holds the length of the longest common
-- subsequence of `a[1..i-1]` and `b[1..j-1]`. Allocated once and reused
-- by `align` for the backtrace.
local function lcsTable(a: { string }, b: { string }): { { number } }
	local n = #a
	local m = #b
	local table_: { { number } } = table.create(n + 1)
	for i = 0, n do
		local row: { number } = table.create(m + 1, 0)
		table_[i + 1] = row
	end
	for i = 1, n do
		for j = 1, m do
			if a[i] == b[j] then
				table_[i + 1][j + 1] = table_[i][j] + 1
			else
				local up = table_[i][j + 1]
				local left = table_[i + 1][j]
				table_[i + 1][j + 1] = if up >= left then up else left
			end
		end
	end
	return table_
end

-- REGRESSION(A15): above this many combined lines, `lcsTable` would
-- allocate an (N+1)x(M+1) matrix — a 20K-line file on both sides is
-- ~400M cells, multiple GB — and the nested loop that fills it freezes
-- the main thread first. Short-circuit before that allocation instead
-- of trying to survive it. Typical Yeet payloads (Roblox scripts <500
-- lines/side) never get close to this cap.
local MAX_DIFF_LINES = 2000

-- Sentinel `align` returns instead of a real alignment once `left`/
-- `right` are too large (see MAX_DIFF_LINES above). A single reserved
-- table, checked by identity via `diffLines.isTooLarge` rather than by
-- content, so it can never be confused with a genuine one-row diff.
local TOO_LARGE_RESULT: { DiffRow } = {
	{
		kind = "context",
		left = "<diff skipped: file too large>",
		right = "<diff skipped: file too large>",
		left_lineno = nil,
		right_lineno = nil,
	},
}

-- True when `rows` is the sentinel described above, i.e. `align`
-- skipped the real LCS alignment because the input was too large.
-- Callers that render `rows` as-is (e.g. MergePicker) don't need to
-- check this — they just get one harmless placeholder row instead of
-- a frozen/OOM'd Studio. Callers that want a dedicated large-file
-- fallback (DiffView) should check it before rendering `rows`.
function diffLines.isTooLarge(rows: { DiffRow }): boolean
	return rows == TOO_LARGE_RESULT
end

-- Given `left` and `right` line arrays, produces a sequence of `DiffRow`s
-- aligned for side-by-side display. Walks the LCS table backwards to
-- assign each position to context / removed / added.
function diffLines.align(left: { string }, right: { string }): { DiffRow }
	-- REGRESSION(A15): cap check — must run before `lcsTable` so the
	-- O(N*M) matrix is never allocated for oversized input.
	if #left + #right > MAX_DIFF_LINES then
		return TOO_LARGE_RESULT
	end
	local table_ = lcsTable(left, right)
	local rows: { DiffRow } = {}
	local i = #left
	local j = #right
	while i > 0 or j > 0 do
		if i > 0 and j > 0 and left[i] == right[j] then
			table.insert(rows, {
				kind = "context",
				left = left[i],
				right = right[j],
				left_lineno = i,
				right_lineno = j,
			})
			i -= 1
			j -= 1
		elseif j > 0 and (i == 0 or table_[i + 1][j] >= table_[i][j + 1]) then
			-- Right has a line not in left → "added".
			table.insert(rows, {
				kind = "added",
				left = nil,
				right = right[j],
				left_lineno = nil,
				right_lineno = j,
			})
			j -= 1
		else
			-- Left has a line not in right → "removed".
			table.insert(rows, {
				kind = "removed",
				left = left[i],
				right = nil,
				left_lineno = i,
				right_lineno = nil,
			})
			i -= 1
		end
	end
	-- The walk above goes from the end of both files backward to the
	-- start, so `rows` was just appended in that same back-to-front
	-- order (last line first). Reverse once — O(N) — to get top-to-
	-- bottom order. This replaces the old `table.insert(rows, 1, ...)`
	-- per row, which shifts every existing element on every insert and
	-- was O(N) per call / O(N^2) overall for N rows.
	local rowCount = #rows
	for k = 1, math.floor(rowCount / 2) do
		rows[k], rows[rowCount - k + 1] = rows[rowCount - k + 1], rows[k]
	end
	-- Pair adjacent removed+added rows into a single "modified" row when
	-- they're aligned 1:1 — easier to read than two stacked rows that
	-- represent the same logical edit. Optional and conservative: only
	-- coalesces simple swaps, leaves runs alone.
	local i2 = 1
	while i2 < #rows do
		local cur = rows[i2]
		local nxt = rows[i2 + 1]
		if cur.kind == "removed" and nxt.kind == "added" then
			rows[i2] = {
				kind = "modified",
				left = cur.left,
				right = nxt.right,
				left_lineno = cur.left_lineno,
				right_lineno = nxt.right_lineno,
			}
			table.remove(rows, i2 + 1)
		end
		i2 += 1
	end
	return rows
end

return diffLines
