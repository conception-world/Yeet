import * as vscode from "vscode";

// Single in-flight openProject prompt across the whole extension.
// VS Code's `showWarningMessage({ modal: true })` queues calls — if a
// buggy daemon emits N back-to-back `open_project_request` frames (or
// the URI handler is triggered N times by a malicious page), the user
// would have to dismiss N modals serially before regaining focus on
// the IDE. The latch drops anything that arrives while a prompt is
// already on screen; dropped attempts are logged so a dev tracing the
// behavior can see what happened. The latch is module-scoped because
// there's exactly one extension instance per VS Code window — no race.
let promptInFlight = false;

// Opens `folderPath` in the host IDE. Pass `forceNewWindow = true` (the
// default) so an auto-open after syncback doesn't blow away the workspace
// the user currently has in focus — they'd lose editor state without warning.
//
// The underlying command comes from VS Code's built-in command registry and
// is shared by Antigravity, so this works in both hosts without branching.
//
// SECURITY: this function is reachable from two wire-driven sources — the
// `vscode://yeet-dev.yeet/open?path=...` URI handler and the daemon-pushed
// `open-project` control frame. Either path lets a remote caller (browser
// tab, malicious local process, hijacked daemon) propose a folder to open.
// Opening a hostile folder under VS Code is RCE: the folder's
// `.vscode/tasks.json` can declare `"runOn": "folderOpen"`, executing a
// shell command the moment the workspace mounts. Therefore EVERY call
// to this function is gated behind a modal confirmation showing the full
// path. The user must explicitly approve. No silent opens.
//
// Pass `output` to log dedup drops — useful when the daemon misbehaves
// and the dev needs to know how many requests were suppressed.
export async function openProject(
	folderPath: string,
	forceNewWindow = true,
	output?: vscode.OutputChannel,
): Promise<void> {
	if (promptInFlight) {
		output?.appendLine(
			`[yeet] dropped duplicate open-project request for ${folderPath} (a prompt is already open)`,
		);
		return;
	}
	promptInFlight = true;
	try {
		const choice = await vscode.window.showWarningMessage(
			`Yeet was asked to open the following folder:\n\n${folderPath}\n\nOnly open folders you trust — opening a malicious folder can run code (.vscode/tasks.json, etc).`,
			{ modal: true },
			"Open Folder",
		);
		if (choice !== "Open Folder") {
			return;
		}
		await vscode.commands.executeCommand(
			"vscode.openFolder",
			vscode.Uri.file(folderPath),
			{ forceNewWindow },
		);
	} finally {
		promptInFlight = false;
	}
}
