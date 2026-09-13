import * as fs from "node:fs";
import * as path from "node:path";
import * as vscode from "vscode";

// Path segments inside a `default.project.json` tree. Empty leaves (used for
// `$path` entries) use an empty object so rojo honors the mapping with no
// children; adding `$className` would force a new instance, which is wrong
// for services.
type TreeNode = {
	$className?: string;
	$path?: string;
	[child: string]: TreeNode | string | undefined;
};

interface ProjectJson {
	name: string;
	tree: TreeNode;
}

/// Canonical set of Roblox services Yeet: Create knows how to scaffold under.
/// Everything the user configures in `yeet.createTemplate` is validated
/// against this list — anything else is surfaced as a warning and skipped so
/// a typo doesn't silently produce an orphaned folder.
const KNOWN_SERVICES = new Set<string>([
	"ServerScriptService",
	"ReplicatedStorage",
	"ReplicatedFirst",
	"ServerStorage",
	"Workspace",
	"Lighting",
	"StarterGui",
	"StarterPack",
	"StarterPlayer",
	"Teams",
	"TestService",
	"SoundService",
	"Chat",
	"LocalizationService",
]);

/// Children of StarterPlayer that are themselves "containers" rojo mounts
/// scripts under. They get a `$className` so rojo creates the right child
/// instance even if the place doesn't have it yet.
const STARTER_PLAYER_CHILDREN: Record<string, string> = {
	StarterPlayerScripts: "StarterPlayerScripts",
	StarterCharacterScripts: "StarterCharacterScripts",
};

export async function createProject(output: vscode.OutputChannel): Promise<void> {
	const folder = await pickWorkspaceFolder(output);
	if (folder === undefined) {
		return;
	}

	const projectFile = path.join(folder, "default.project.json");
	const sourcemapFile = path.join(folder, "sourcemap.json");
	// Both files are rewritten from the scaffold below, so both are at risk —
	// the modal has to name whichever ones actually exist, and each gets a
	// `.bak` before being replaced.
	const doomed = [projectFile, sourcemapFile].filter((f) => fs.existsSync(f));
	if (doomed.length > 0) {
		const names = doomed.map((f) => path.basename(f)).join(" and ");
		const choice = await vscode.window.showWarningMessage(
			`${names} already exist${doomed.length === 1 ? "s" : ""} in this folder. ` +
				"Yeet: Create replaces the entire tree with the yeet.createTemplate scaffold — " +
				"any custom mounts (Packages, extra services, hand-edited $path entries, etc.) " +
				"not in that template will be lost. " +
				`The current file${doomed.length === 1 ? "" : "s"} will be backed up as ` +
				`${doomed.map((f) => `${path.basename(f)}.bak`).join(" and ")} first.`,
			{ modal: true },
			"Overwrite",
			"Cancel",
		);
		if (choice !== "Overwrite") {
			output.appendLine("[yeet:create] cancelled (project already exists)");
			return;
		}
		for (const file of doomed) {
			backupExistingFile(file, output);
		}
	}

	const cfg = vscode.workspace.getConfiguration("yeet");
	const templateRaw = cfg.get<string[]>("createTemplate") ?? [];
	const { tree, createdDirs, skipped } = buildTree(templateRaw, folder);

	const projectName = await vscode.window.showInputBox({
		prompt: "Project name (used in default.project.json)",
		value: path.basename(folder),
		validateInput: (v) => (v.trim().length === 0 ? "Required" : null),
	});
	if (projectName === undefined) {
		output.appendLine("[yeet:create] cancelled at project-name prompt");
		return;
	}

	const project: ProjectJson = {
		name: projectName.trim(),
		tree,
	};

	fs.writeFileSync(projectFile, `${JSON.stringify(project, null, 2)}\n`, "utf8");
	output.appendLine(`[yeet:create] wrote ${projectFile}`);

	for (const rel of createdDirs) {
		const abs = path.join(folder, rel);
		fs.mkdirSync(abs, { recursive: true });
		output.appendLine(`[yeet:create] scaffolded ${rel}`);
	}

	writeInitialSourcemap(folder, project, output);
	writeGitignore(folder, output);
	writeLuauLspSettings(folder, output);

	if (skipped.length > 0) {
		output.appendLine(
			`[yeet:create] skipped unknown scopes: ${skipped.join(", ")}`,
		);
		void vscode.window.showWarningMessage(
			`Yeet: Create skipped unrecognized scopes: ${skipped.join(", ")}. ` +
				"Edit yeet.createTemplate in settings to fix.",
		);
	}

	void vscode.window.showInformationMessage(
		`Yeet project scaffolded in ${path.basename(folder)}. Run Yeet: Start to begin sync.`,
	);
}

/// Preserves a file about to be overwritten as a sibling `.bak` so a user who
/// confirmed the modal without fully reading it can still recover custom
/// mounts (Packages, extra services) by hand. Best effort: a failed backup is
/// logged but does not block the overwrite, since the user already confirmed
/// the modal's warning about data loss.
function backupExistingFile(file: string, output: vscode.OutputChannel): void {
	const backupFile = `${file}.bak`;
	try {
		fs.copyFileSync(file, backupFile);
		output.appendLine(`[yeet:create] backed up ${path.basename(file)} to ${backupFile}`);
	} catch (err) {
		const message = err instanceof Error ? err.message : String(err);
		output.appendLine(`[yeet:create] failed to back up ${file}: ${message}`);
	}
}

async function pickWorkspaceFolder(
	output: vscode.OutputChannel,
): Promise<string | undefined> {
	const folders = vscode.workspace.workspaceFolders;
	if (folders === undefined || folders.length === 0) {
		const picked = await vscode.window.showOpenDialog({
			canSelectFolders: true,
			canSelectFiles: false,
			canSelectMany: false,
			openLabel: "Select folder for new Yeet project",
		});
		const fsPath = picked?.[0]?.fsPath;
		if (fsPath === undefined) {
			output.appendLine("[yeet:create] cancelled at folder prompt");
			return undefined;
		}
		return fsPath;
	}
	if (folders.length === 1) {
		const only = folders[0];
		return only === undefined ? undefined : only.uri.fsPath;
	}
	const picked = await vscode.window.showWorkspaceFolderPick({
		placeHolder: "Pick the folder to scaffold Yeet into",
	});
	return picked?.uri.fsPath;
}

/// Turns the flat list of scope strings into a rojo-compatible tree + the set
/// of `src/...` directories we need to create on disk. Validation is permissive:
/// unknown services are dropped (with a warning bubbled back to the caller)
/// rather than failing the whole command, so a partial template still works.
function buildTree(
	scopes: string[],
	_folder: string,
): { tree: TreeNode; createdDirs: string[]; skipped: string[] } {
	const tree: TreeNode = { $className: "DataModel" };
	const createdDirs: string[] = [];
	const skipped: string[] = [];

	for (const scopeRaw of scopes) {
		const scope = scopeRaw.trim().replace(/\\/g, "/");
		if (scope.length === 0) {
			continue;
		}
		const segments = scope.split("/").filter((s) => s.length > 0);
		const service = segments[0];
		if (service === undefined || !KNOWN_SERVICES.has(service)) {
			skipped.push(scope);
			continue;
		}

		if (segments.length === 1) {
			ensureServiceNode(tree, service);
			const relDir = `src/${service}`;
			applyPath(tree, [service], relDir);
			createdDirs.push(relDir);
			continue;
		}

		// Nested under a service (e.g. StarterPlayer/StarterPlayerScripts).
		// Today only StarterPlayer has honest sub-containers; guard other
		// services so we don't invent tree shapes rojo can't honor.
		if (service !== "StarterPlayer") {
			skipped.push(scope);
			continue;
		}
		const child = segments[1];
		if (child === undefined || STARTER_PLAYER_CHILDREN[child] === undefined) {
			skipped.push(scope);
			continue;
		}
		ensureServiceNode(tree, service);
		ensureStarterPlayerChild(tree, child);
		const relDir = `src/${service}/${child}`;
		applyPath(tree, [service, child], relDir);
		createdDirs.push(relDir);
	}

	return { tree, createdDirs, skipped };
}

function ensureServiceNode(tree: TreeNode, service: string): void {
	if (tree[service] === undefined) {
		tree[service] = { $className: service };
	}
}

function ensureStarterPlayerChild(tree: TreeNode, child: string): void {
	const starter = tree["StarterPlayer"];
	if (typeof starter !== "object" || starter === null) {
		return;
	}
	const className = STARTER_PLAYER_CHILDREN[child];
	if (className === undefined) {
		return;
	}
	if (starter[child] === undefined) {
		starter[child] = { $className: className };
	}
}

function applyPath(tree: TreeNode, segments: string[], relDir: string): void {
	let node: TreeNode = tree;
	for (const seg of segments) {
		const next = node[seg];
		if (typeof next !== "object" || next === null) {
			return;
		}
		node = next;
	}
	node["$path"] = relDir;
}

/// One node of a Rojo-format `sourcemap.json`. luau-lsp reads this to resolve
/// `game.ServerScriptService.Foo`, `require(Packages.X)`, and Wally package
/// types; without it a freshly-scaffolded project has no type resolution at all.
interface SourcemapNode {
	name: string;
	className: string;
	filePaths: string[];
	children: SourcemapNode[];
}

/// Ownership marker the daemon stamps on maps it generates, and looks for
/// before overwriting an existing one (`sourcemap::is_foreign`). The scaffold
/// map MUST carry it: without it the daemon treats this file as a
/// hand-maintained one, refuses to manage it, and a freshly created project
/// never gets per-file type resolution at all.
const GENERATED_BY_KEY = "generatedBy";
const GENERATED_BY_VALUE = "yeet";

/// Emits an initial `sourcemap.json` at the project root mirroring the scaffold
/// (the KNOWN_SERVICES → `src/<Service>` mounts). It carries only the empty
/// service nodes so the LSP has a valid DataModel root immediately after
/// `Yeet: Create`; the daemon regenerates it with per-file detail as soon as
/// `Yeet: Start` scans the tree. Deriving it from the same `tree` we just wrote
/// keeps the two files structurally in sync.
function writeInitialSourcemap(
	folder: string,
	project: ProjectJson,
	output: vscode.OutputChannel,
): void {
	const rootClass =
		typeof project.tree.$className === "string" ? project.tree.$className : "DataModel";
	// Mirror the daemon's `build_sourcemap` fallback: an empty/whitespace-only
	// project name emits the literal "game" rather than an empty string, so the
	// scaffold and the first regeneration agree on the root node's name.
	const rootName = project.name.trim() === "" ? "game" : project.name;
	// Key order matches the daemon's output byte-for-byte. The daemon builds the
	// JSON with serde_json's default `Map`, which is a BTreeMap (the crate is
	// built without the `preserve_order` feature), so every object comes out
	// ALPHABETICALLY sorted: children, className, filePaths, generatedBy, name.
	// `JSON.stringify` emits plain string keys in insertion order, so listing
	// them alphabetically here is what keeps the two generators identical.
	// Otherwise the daemon's first regeneration rewrote a structurally
	// equivalent file purely to reorder keys, which shows up as a spurious diff
	// and makes "did the sourcemap update?" impossible to answer by eye.
	const sourcemap: SourcemapNode & Record<string, unknown> = {
		children: treeToSourcemapChildren(project.tree),
		className: rootClass,
		filePaths: [],
		[GENERATED_BY_KEY]: GENERATED_BY_VALUE,
		name: rootName,
	};
	const file = path.join(folder, "sourcemap.json");
	fs.writeFileSync(file, `${JSON.stringify(sourcemap, null, 2)}\n`, "utf8");
	output.appendLine("[yeet:create] wrote sourcemap.json");
}

/// Recursively converts a `default.project.json` tree's instance children into
/// sourcemap nodes. `$`-prefixed keys (`$className`, `$path`, ...) are metadata,
/// not instances, so they're skipped.
///
/// className resolution mirrors the daemon's `ensure_segment_chain` exactly:
/// an explicit `$className` wins; otherwise a direct child of the DataModel
/// keeps its own name (a service's ClassName equals its name) and anything
/// deeper defaults to `Folder`. Using name-as-className at every depth — as
/// this did — disagreed with the daemon, so a hand-written nested node was
/// written one way by `Yeet: Create` and then flipped the moment the daemon
/// regenerated the file.
function treeToSourcemapChildren(node: TreeNode, depth = 0): SourcemapNode[] {
	const children: SourcemapNode[] = [];
	for (const [key, value] of Object.entries(node)) {
		if (key.startsWith("$")) {
			continue;
		}
		if (typeof value !== "object" || value === null) {
			continue;
		}
		const className =
			typeof value.$className === "string" ? value.$className : depth === 0 ? key : "Folder";
		// Alphabetical key order, same reason as the root node — see
		// `writeInitialSourcemap`.
		children.push({
			children: treeToSourcemapChildren(value, depth + 1),
			className,
			filePaths: [],
			name: key,
		});
	}
	// The daemon keys each node's children in a BTreeMap, so they serialize
	// sorted by instance name regardless of visit order. `Object.entries` here
	// follows JS property order instead, which for a hand-edited project file is
	// whatever the user typed. Sorting makes the two agree.
	children.sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
	return children;
}

/// The luau-lsp setting that decides who owns `sourcemap.json`.
///
/// luau-lsp defaults `sourcemap.autogenerate` to `true`, which makes it shell
/// out to `rojo sourcemap --watch` on its own. In a Yeet project that either
/// fails outright (no `rojo` on PATH — Yeet is not a Rojo wrapper and does not
/// require it) or races the daemon for the same file. Since Yeet generates and
/// maintains the map itself, the LSP's own generator has to be off.
///
/// Deliberately NOT touched: `luau-lsp.sourcemap.enabled`, which defaults to
/// `true` and is the switch that makes the LSP *read* sourcemap.json at all.
/// Turning that off would kill cross-instance type resolution entirely — the
/// exact opposite of what this scaffold is for.
const LUAU_LSP_AUTOGENERATE_KEY = "luau-lsp.sourcemap.autogenerate";

/// Writes `.vscode/settings.json` so luau-lsp reads Yeet's sourcemap instead of
/// trying to generate its own. Without this, a freshly scaffolded project has
/// two writers fighting over one file and the user sees "the sourcemap doesn't
/// work" with nothing in any log to explain why.
///
/// Merges rather than overwrites: an existing settings file keeps every key it
/// already has, and an existing `autogenerate` value is left ALONE — a user who
/// deliberately set it is not second-guessed by a scaffold. Only a missing key
/// is added.
///
/// A settings file we cannot parse is left untouched and reported. VS Code
/// accepts JSONC (comments, trailing commas) which `JSON.parse` rejects, and
/// silently rewriting a file we failed to understand would destroy the user's
/// configuration. Telling them the one line to add is strictly better than
/// guessing.
function writeLuauLspSettings(folder: string, output: vscode.OutputChannel): void {
	const dir = path.join(folder, ".vscode");
	const file = path.join(dir, "settings.json");

	if (!fs.existsSync(file)) {
		fs.mkdirSync(dir, { recursive: true });
		const settings = { [LUAU_LSP_AUTOGENERATE_KEY]: false };
		fs.writeFileSync(file, `${JSON.stringify(settings, null, 2)}\n`, "utf8");
		output.appendLine("[yeet:create] wrote .vscode/settings.json");
		return;
	}

	const raw = fs.readFileSync(file, "utf8");
	let parsed: unknown;
	try {
		parsed = JSON.parse(raw);
	} catch {
		output.appendLine(
			`[yeet:create] .vscode/settings.json is not plain JSON (comments or trailing `
				+ `commas?) — leaving it untouched. Add "${LUAU_LSP_AUTOGENERATE_KEY}": false `
				+ "yourself, or luau-lsp will fight the daemon over sourcemap.json.",
		);
		return;
	}
	if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
		output.appendLine(
			"[yeet:create] .vscode/settings.json is not a JSON object — leaving it untouched.",
		);
		return;
	}

	const settings = parsed as Record<string, unknown>;
	if (LUAU_LSP_AUTOGENERATE_KEY in settings) {
		output.appendLine(
			`[yeet:create] .vscode/settings.json already sets ${LUAU_LSP_AUTOGENERATE_KEY} `
				+ `(${JSON.stringify(settings[LUAU_LSP_AUTOGENERATE_KEY])}) — keeping it`,
		);
		return;
	}

	settings[LUAU_LSP_AUTOGENERATE_KEY] = false;
	backupExistingFile(file, output);
	fs.writeFileSync(file, `${JSON.stringify(settings, null, 2)}\n`, "utf8");
	output.appendLine(
		`[yeet:create] added ${LUAU_LSP_AUTOGENERATE_KEY} to .vscode/settings.json`,
	);
}

function writeGitignore(folder: string, output: vscode.OutputChannel): void {
	const file = path.join(folder, ".gitignore");
	// Wally (the de-facto Roblox package manager) installs packages into
	// these directories; they're regenerated from wally.toml/package.json
	// and shouldn't be committed, same as any other lockfile-driven
	// dependency directory.
	const lines = [
		".yeet/",
		"build/",
		"*.rbxm",
		"*.rbxl",
		"*.rbxlx",
		"Packages/",
		"ServerPackages/",
		"DevPackages/",
		"node_modules/",
		// Regenerated by the daemon on every structural change (each file
		// added, removed, or renamed rewrites it), so committing it produces
		// churn and merge conflicts for a file no human edits.
		"sourcemap.json",
		"*.bak",
	];
	if (fs.existsSync(file)) {
		const existing = fs.readFileSync(file, "utf8");
		// Exact line match, not substring — `existing.includes(l)` would
		// treat a negated pattern like `!build/` as covering `build/`
		// (it contains the substring), silently skipping the entry we
		// actually need.
		const existingLines = new Set(existing.split(/\r?\n/).map((s) => s.trim()));
		const missing = lines.filter((l) => !existingLines.has(l));
		if (missing.length === 0) {
			return;
		}
		const append = `${existing.endsWith("\n") ? "" : "\n"}${missing.join("\n")}\n`;
		fs.appendFileSync(file, append);
		output.appendLine(
			`[yeet:create] appended ${missing.length} entries to existing .gitignore`,
		);
		return;
	}
	fs.writeFileSync(file, `${lines.join("\n")}\n`, "utf8");
	output.appendLine("[yeet:create] wrote .gitignore");
}
