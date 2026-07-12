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
	if (fs.existsSync(projectFile)) {
		const choice = await vscode.window.showWarningMessage(
			"default.project.json already exists in this folder. Overwriting replaces the " +
				"entire tree with the yeet.createTemplate scaffold — any custom mounts (Packages, " +
				"extra services, hand-edited $path entries, etc.) not in that template will be lost. " +
				"The current file will be backed up as default.project.json.bak first.",
			{ modal: true },
			"Overwrite",
			"Cancel",
		);
		if (choice !== "Overwrite") {
			output.appendLine("[yeet:create] cancelled (project already exists)");
			return;
		}
		backupExistingProjectFile(projectFile, output);
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

/// Preserves the pre-overwrite `default.project.json` as a sibling `.bak`
/// file so a user who confirmed the overwrite modal without fully reading it
/// can still recover custom mounts (Packages, extra services) by hand. Best
/// effort: a failed backup is logged but does not block the overwrite, since
/// the user already confirmed the modal's warning about data loss.
function backupExistingProjectFile(
	projectFile: string,
	output: vscode.OutputChannel,
): void {
	const backupFile = `${projectFile}.bak`;
	try {
		fs.copyFileSync(projectFile, backupFile);
		output.appendLine(`[yeet:create] backed up existing project file to ${backupFile}`);
	} catch (err) {
		const message = err instanceof Error ? err.message : String(err);
		output.appendLine(`[yeet:create] failed to back up ${projectFile}: ${message}`);
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
	const sourcemap: SourcemapNode = {
		name: project.name,
		className: rootClass,
		filePaths: [],
		children: treeToSourcemapChildren(project.tree),
	};
	const file = path.join(folder, "sourcemap.json");
	fs.writeFileSync(file, `${JSON.stringify(sourcemap, null, 2)}\n`, "utf8");
	output.appendLine("[yeet:create] wrote sourcemap.json");
}

/// Recursively converts a `default.project.json` tree's instance children into
/// sourcemap nodes. `$`-prefixed keys (`$className`, `$path`, ...) are metadata,
/// not instances, so they're skipped. A node's className comes from its
/// `$className` when set, else the instance name itself — services' ClassName
/// equals their name, so this is correct for the scaffold's KNOWN_SERVICES.
function treeToSourcemapChildren(node: TreeNode): SourcemapNode[] {
	const children: SourcemapNode[] = [];
	for (const [key, value] of Object.entries(node)) {
		if (key.startsWith("$")) {
			continue;
		}
		if (typeof value !== "object" || value === null) {
			continue;
		}
		const className = typeof value.$className === "string" ? value.$className : key;
		children.push({
			name: key,
			className,
			filePaths: [],
			children: treeToSourcemapChildren(value),
		});
	}
	return children;
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
