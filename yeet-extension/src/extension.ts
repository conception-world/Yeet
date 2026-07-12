import { type ChildProcess, spawn } from "node:child_process";
import * as fs from "node:fs";
import * as net from "node:net";
import * as path from "node:path";
import * as vscode from "vscode";
import { createProject } from "./create";
import { openProject } from "./openProject";
import { type OutboundFrame, YeetControlChannel } from "./websocket";

const DAEMON_PORT = 34872;
const KILL_GRACE_MS = 2000;
// Daemon version this extension build was tested against. Bumped in
// lockstep with `package.json:version` and the daemon's
// `CARGO_PKG_VERSION`. The bundled daemon at `bin/<os>-<arch>/` is
// always exactly this version (CI rebuilds + bundles before package).
// The check exists for users who set `yeet.daemonPath` to a custom
// build that's silently out of sync — typically a dev forgot to
// `cargo build --release` after pulling. Mismatch surfaces a non-modal
// warning naming both versions; sync continues to operate (semver
// minor compat usually holds), but the user knows what to fix when
// behavior gets weird.
const EXPECTED_DAEMON_VERSION = "0.5.0";
// How long the pre-spawn TCP probe waits for `connect` to settle
// before declaring the port unbound. Short enough that startDaemon
// stays responsive; long enough to catch a daemon whose accept
// thread is briefly busy. Loopback connects normally complete in
// <5 ms, so 500 ms is generous.
const DAEMON_PROBE_TIMEOUT_MS = 500;

// Captured at activate(); needed to resolve the bundled daemon
// binary's path under `<extensionPath>/bin/<os>-<arch>/`. Without
// this, fresh users have no daemon at all — the Marketplace install
// drops the extension in but the daemon is a separate component. Set
// once on activate, never reassigned.
let extensionContext: vscode.ExtensionContext | undefined;

type DaemonState = "stopped" | "starting" | "running" | "crashed";

let daemon: ChildProcess | undefined;
let output: vscode.OutputChannel | undefined;
let statusBar: vscode.StatusBarItem | undefined;
let channel: YeetControlChannel | undefined;
// Auth token the daemon emitted on stdout during startup. Captured by
// the spawn handler and forwarded to the control channel so its Hello
// frame proves to the daemon we have local FS access (== legitimate
// extension, not a browser tab via DNS rebinding).
let daemonAuthToken: string | undefined;
// Auto-pair state. The extension keeps a `yeet-pairing.txt`
// breadcrumb live for the entire duration the daemon is running so
// the plugin can auto-pair on first connect with zero clicks. Refresh
// runs every PAIRING_REFRESH_MS so the daemon's TTL check (60s) never
// catches a stale file. Cleared on stopDaemon.
let pairingRefreshTimer: NodeJS.Timeout | undefined;
let pairingFilePath: string | undefined;
const PAIRING_REFRESH_MS = 30_000;
// Re-entrancy guard for startDaemon. The simple `if (daemon)` check
// is non-atomic: between the check and the `daemon = child` assignment
// (~10 ms with spawn overhead), a second concurrent invocation can
// pass the check and spawn a second daemon process. The second one
// then bind-fails on port 34872 silently and leaks as an orphan
// after `Yeet: Stop` kills only the tracked child. This flag closes
// the window — it's set the moment startDaemon enters and cleared
// only when spawn settles (success OR failure).
let starting = false;

export function activate(context: vscode.ExtensionContext): void {
	extensionContext = context;
	output = vscode.window.createOutputChannel("Yeet");
	statusBar = vscode.window.createStatusBarItem(
		vscode.StatusBarAlignment.Right,
		100,
	);
	setStatus("stopped");
	statusBar.show();

	context.subscriptions.push(
		output,
		statusBar,
		vscode.commands.registerCommand("yeet.start", startDaemon),
		vscode.commands.registerCommand("yeet.stop", stopDaemon),
		vscode.commands.registerCommand("yeet.create", () => {
			if (output === undefined) {
				return;
			}
			void createProject(output);
		}),
		vscode.commands.registerCommand("yeet.syncFromStudio", () => {
			void runBulkSync("from-studio");
		}),
		vscode.commands.registerCommand("yeet.syncFromIde", () => {
			void runBulkSync("from-ide");
		}),
		vscode.commands.registerCommand("yeet.pairStudio", () => {
			void pairStudio();
		}),
		// Lets the user reopen a materialized folder from anywhere without
		// bouncing through the daemon — useful when the auto-open window got
		// closed and the plugin isn't in front. The URI matches the
		// `<publisher>.<name>` scheme derived from package.json: yeet-dev.yeet.
		vscode.window.registerUriHandler({
			handleUri(uri) {
				handleYeetUri(uri);
			},
		}),
	);
}

// Pure: maps the command-facing direction to the wire frame. Pulled out of
// runBulkSync so the type-to-direction mapping is checkable on its own,
// independent of the send/log/warn side effects around it.
function buildBulkSyncRequest(direction: "from-studio" | "from-ide"): OutboundFrame {
	return direction === "from-studio"
		? { type: "bulk_sync_from_studio_request" }
		: { type: "bulk_sync_from_ide_request" };
}

async function runBulkSync(direction: "from-studio" | "from-ide"): Promise<void> {
	if (channel === undefined || daemon === undefined) {
		const label = direction === "from-studio" ? "Sync From Studio" : "Sync From Ide";
		void vscode.window.showErrorMessage(
			`Yeet: ${label} needs a running daemon. Run Yeet: Start first.`,
		);
		return;
	}
	const request = buildBulkSyncRequest(direction);
	// `send` already warns the user when it drops a frame (disconnected
	// socket), but it can't know here whether the *attempt* succeeded —
	// only the caller can decide whether "sent" is true. Previously this
	// logged "sent" unconditionally, which was actively misleading when
	// the daemon was unreachable and the frame never left the process.
	if (channel.send(request)) {
		output?.appendLine(`[yeet] sent ${request.type}`);
	} else {
		void vscode.window.showWarningMessage(
			"Yeet: bulk sync request was not sent — the daemon control channel is disconnected.",
		);
	}
}

// Writes the pairing breadcrumb file. Called both proactively (on
// daemon start, kept alive by the refresh timer) and reactively (the
// `Yeet: Pair Studio` command, in case the user is debugging a flow
// where the auto-pair didn't reach the plugin in time). Returns true
// on successful write. Errors are surfaced via the output channel —
// no modal, since this runs in the background as part of the
// auto-pair maintenance loop.
function writePairingBreadcrumb(projectRoot: string): boolean {
	// Breadcrumb lives at `<root>/.yeet/pairing` (no extension). The
	// `.yeet/` directory is treated as plugin-internal — the
	// `Yeet: Create` scaffolder writes a `.gitignore` that excludes
	// it, so the breadcrumb never gets accidentally committed.
	// Earlier versions kept it at `<root>/yeet-pairing.txt` which
	// produced `git status` noise on every 30s refresh.
	const yeetDir = path.join(projectRoot, ".yeet");
	const path_ = path.join(yeetDir, "pairing");
	const now = Math.floor(Date.now() / 1000);
	try {
		fs.mkdirSync(yeetDir, { recursive: true });
		fs.writeFileSync(path_, String(now), { encoding: "utf8" });
		pairingFilePath = path_;
		return true;
	} catch (err) {
		const message = err instanceof Error ? err.message : String(err);
		output?.appendLine(`[yeet] failed to write pairing file: ${message}`);
		return false;
	}
}

// Kicks off the background loop that keeps `yeet-pairing.txt` fresh
// for as long as the daemon is alive. The breadcrumb's TTL on the
// daemon is 60s; refreshing every 30s guarantees the file is always
// young enough when the plugin sends `pair_request`. Idempotent —
// re-calling does nothing if a refresh timer is already running.
function startAutoPairing(projectRoot: string): void {
	if (pairingRefreshTimer !== undefined) {
		return;
	}
	if (writePairingBreadcrumb(projectRoot)) {
		output?.appendLine(
			`[yeet] auto-pair: wrote ${pairingFilePath ?? "(unknown path)"}; refreshing every ${PAIRING_REFRESH_MS / 1000}s`,
		);
	}
	pairingRefreshTimer = setInterval(() => {
		writePairingBreadcrumb(projectRoot);
	}, PAIRING_REFRESH_MS);
}

// Stops the auto-pair refresh and removes the breadcrumb. Safe to
// call multiple times (no-op when nothing is running). Always invoked
// from `stopDaemon` / `killDaemon` so the breadcrumb's lifetime
// matches the daemon's — once Yeet is stopped, no plugin can
// auto-pair until the next `Yeet: Start`.
function stopAutoPairing(): void {
	if (pairingRefreshTimer !== undefined) {
		clearInterval(pairingRefreshTimer);
		pairingRefreshTimer = undefined;
	}
	if (pairingFilePath !== undefined) {
		try {
			fs.unlinkSync(pairingFilePath);
		} catch {
			// Already gone (manual delete, daemon consumed it, etc.) —
			// fine. Nothing to clean up.
		}
		pairingFilePath = undefined;
	}
}

// "Yeet: Pair Studio" command. Manual escape hatch: if auto-pair
// failed (e.g. plugin reloaded after a long idle gap and the
// breadcrumb timer somehow drifted), this rewrites the breadcrumb so
// the user can retry from Studio. In normal operation
// (auto-pair running since `Yeet: Start`) the user never needs this.
async function pairStudio(): Promise<void> {
	const projectRoot = resolveProjectRoot();
	if (projectRoot === undefined) {
		return;
	}
	if (!writePairingBreadcrumb(projectRoot)) {
		void vscode.window.showErrorMessage(
			"Yeet: failed to write pairing file (see Output for details).",
		);
		return;
	}
	output?.appendLine(`[yeet] manual pair: wrote ${pairingFilePath ?? "(unknown path)"} (TTL 60s)`);
	void vscode.window.showInformationMessage(
		"Yeet: pairing window refreshed. Studio should pair automatically.",
	);
}

export async function deactivate(): Promise<void> {
	await killDaemon(KILL_GRACE_MS);
}

function handleYeetUri(uri: vscode.Uri): void {
	// URI shape: vscode://yeet-dev.yeet/open?path=<encoded>
	// Anything else is silently ignored — VS Code dispatches every yeet-dev.yeet
	// URI here, including ones future versions may add.
	if (uri.path !== "/open") {
		return;
	}
	const query = new URLSearchParams(uri.query);
	const folderPath = query.get("path");
	if (folderPath === null || folderPath.length === 0) {
		output?.appendLine("[yeet:uri] open request missing ?path");
		return;
	}
	output?.appendLine(`[yeet:uri] opening ${folderPath}`);
	openProject(folderPath, true, output).then(
		() => {},
		(err: unknown) => {
			const message = err instanceof Error ? err.message : String(err);
			output?.appendLine(`[yeet:uri] openFolder failed: ${message}`);
		},
	);
}

function setStatus(state: DaemonState): void {
	if (!statusBar) {
		return;
	}
	switch (state) {
		case "stopped":
			statusBar.text = "$(circle-slash) Yeet: stopped";
			statusBar.command = "yeet.start";
			statusBar.tooltip = "Click to start the Yeet daemon";
			break;
		case "starting":
			statusBar.text = "$(sync~spin) Yeet: starting";
			statusBar.command = undefined;
			statusBar.tooltip = undefined;
			break;
		case "running":
			statusBar.text = `$(zap) Yeet: running (:${DAEMON_PORT})`;
			statusBar.command = "yeet.stop";
			statusBar.tooltip = "Click to stop the Yeet daemon";
			break;
		case "crashed":
			statusBar.text = "$(error) Yeet: crashed";
			statusBar.command = "yeet.start";
			statusBar.tooltip = "Daemon exited unexpectedly. Click to restart.";
			break;
	}
}

async function startDaemon(): Promise<void> {
	if (daemon) {
		void vscode.window.showInformationMessage("Yeet daemon is already running.");
		return;
	}
	if (starting) {
		// Another invocation is already in flight (typical: user
		// double-clicked the status bar). The first call will finish
		// the spawn and assign `daemon` shortly — let it.
		void vscode.window.showInformationMessage(
			"Yeet daemon is starting — please wait.",
		);
		return;
	}
	starting = true;
	try {
		await startDaemonInner();
	} finally {
		// Always clear so a thrown spawn doesn't permanently lock
		// the user out of retrying. `daemon` is the source of truth
		// for "is the daemon running"; `starting` is just the
		// transient race guard.
		starting = false;
	}
}

// Resolves the daemon binary that ships inside the .vsix. Returns
// `<extensionPath>/bin/<platform>-<arch>/yeet-daemon[.exe]` for the
// host the user is running on. The directory layout matches what
// the build pipeline drops into (`bin/win-x64/`, `bin/macos-arm64/`,
// `bin/linux-x64/`, etc.) so a fresh marketplace install needs zero
// configuration. Returns undefined when no binary is shipped for
// this platform — the caller falls back to `yeet.daemonPath`.
function resolveBundledDaemon(): string | undefined {
	if (extensionContext === undefined) {
		return undefined;
	}
	const platform = process.platform; // "win32" | "darwin" | "linux"
	const arch = process.arch; // "x64" | "arm64" | ...
	const platformKey =
		platform === "win32" ? "win" : platform === "darwin" ? "macos" : "linux";
	const subdir = `${platformKey}-${arch}`;
	const exeName = platform === "win32" ? "yeet-daemon.exe" : "yeet-daemon";
	const candidate = path.join(extensionContext.extensionPath, "bin", subdir, exeName);
	if (fs.existsSync(candidate)) {
		return candidate;
	}
	return undefined;
}

// Pings 127.0.0.1:DAEMON_PORT to detect a daemon already running
// before we try to spawn another. Resolves true if the port is
// already bound (something — possibly a previous Yeet daemon
// orphaned by a force-quit, possibly a different VS Code window
// running Yeet at the same time, possibly an unrelated app
// squatting the port). Resolves false if the port is unbound.
//
// This closes two production failure modes audited as HIGH:
//   * H1 — two VS Code windows on different projects both trying
//     to spawn a daemon, second one bind-fails silently and the
//     plugin in window B ends up syncing window A's project.
//   * H4 — user force-quits VS Code, daemon stays alive, user
//     reopens VS Code and `Yeet: Start` spawns a phantom second
//     daemon that bind-fails into the void.
function probeDaemonAlive(): Promise<boolean> {
	return new Promise((resolve) => {
		const socket = new net.Socket();
		// Race a connect() against a timeout. Either branch resolves
		// the promise exactly once and tears down the socket.
		let settled = false;
		const finish = (alive: boolean) => {
			if (settled) return;
			settled = true;
			try {
				socket.destroy();
			} catch {
				// Socket already destroyed — fine.
			}
			resolve(alive);
		};
		socket.setTimeout(DAEMON_PROBE_TIMEOUT_MS);
		socket.once("connect", () => finish(true));
		socket.once("timeout", () => finish(false));
		socket.once("error", () => finish(false));
		socket.connect(DAEMON_PORT, "127.0.0.1");
	});
}

async function startDaemonInner(): Promise<void> {
	// A daemon crash leaves the previous control channel's reconnect
	// loop running: `child.on("exit")` only clears `daemon`, and the
	// clean-stop path (`killDaemon`) isn't in play here since nobody
	// asked the daemon to stop. Without this, restarting after a crash
	// constructs a second `YeetControlChannel` at the end of this
	// function while the first one is still alive and reconnecting —
	// both connect with `role=extension`, the daemon keeps only the
	// latest, and the loser reconnects immediately, producing a ~1s
	// ping-pong forever. Disposing unconditionally is safe: `dispose()`
	// is idempotent and a no-op if the channel was never assigned.
	if (channel !== undefined) {
		channel.dispose();
		channel = undefined;
	}
	// Short-circuit if a daemon already answers on the expected
	// port. Re-use it instead of trying to spawn a duplicate that
	// would fail to bind. The control channel's connect+hello will
	// surface auth/version/project mismatches AS errors the user
	// can act on, instead of the silent "spawn-and-vanish" we had
	// before (which left orphan processes on every accidental
	// double-start).
	if (await probeDaemonAlive()) {
		// Either (a) a previous Yeet daemon orphaned by a force-
		// quit, (b) another VS Code window already running Yeet,
		// (c) an unrelated app squatting :34872. We can't tell
		// them apart from out here, but ANY case that's not "we
		// own this daemon" is bad and surfacing it loudly is the
		// right move. Don't try to spawn a duplicate (the bind
		// would silently fail and we'd be left with a phantom
		// process); refuse with an actionable error.
		output?.appendLine(
			`[yeet] port 127.0.0.1:${DAEMON_PORT} already in use; refusing to spawn a second daemon`,
		);
		const choice = await vscode.window.showErrorMessage(
			`Yeet: port ${DAEMON_PORT} is already in use. This usually means:\n\n`
				+ "  1. Another VS Code window is running Yeet on this machine.\n"
				+ "  2. A previous Yeet daemon was orphaned (force-quit) and is still running.\n"
				+ "  3. An unrelated app is using the port.\n\n"
				+ "Yeet supports one daemon at a time. Close other windows first, or "
				+ "kill the orphan from your task manager.",
			"Open Output",
		);
		if (choice === "Open Output") {
			output?.show();
		}
		// Mark stopped so the user can retry after they handle the
		// conflict. Do NOT auto-reuse: the running daemon may be
		// on a different project (silently corrupting THIS project
		// if we reattached blindly).
		setStatus("stopped");
		return;
	}
	const cfg = vscode.workspace.getConfiguration("yeet");
	let daemonPath = (cfg.get<string>("daemonPath") ?? "").trim();
	if (daemonPath.length === 0) {
		// No user override — try the bundled binary first. This is the
		// happy path for marketplace installs.
		const bundled = resolveBundledDaemon();
		if (bundled !== undefined) {
			daemonPath = bundled;
			output?.appendLine(`[yeet] using bundled daemon: ${daemonPath}`);
		}
	}
	if (daemonPath.length === 0) {
		// Bundled binary missing for this OS/arch (the .vsix only
		// ships win-x64 today; macOS/Linux users self-provision) OR
		// the win-x64 binary was deleted (antivirus quarantine, the
		// extension files got corrupted). Either way, give the user
		// a direct link to the Releases page and a Settings shortcut
		// instead of a dead-end error message.
		const releasesUrl = "https://github.com/conception-world/Yeet/releases";
		const choice = await vscode.window.showErrorMessage(
			`Yeet: no daemon binary available for ${process.platform}-${process.arch}.\n\n`
				+ "Two options:\n"
				+ `  1. Download a pre-built release (${releasesUrl}) and point yeet.daemonPath at it.\n`
				+ "  2. Clone the repo and run `cargo build --release` in yeet-daemon/.\n\n"
				+ "If you're on Windows and JUST installed the extension, the bundled "
				+ "yeet-daemon.exe may have been quarantined by antivirus — try "
				+ "reinstalling the extension.",
			"Open Releases Page",
			"Open Settings",
		);
		if (choice === "Open Releases Page") {
			void vscode.env.openExternal(vscode.Uri.parse(releasesUrl));
		} else if (choice === "Open Settings") {
			void vscode.commands.executeCommand(
				"workbench.action.openSettings",
				"yeet.daemonPath",
			);
		}
		return;
	}
	if (!fs.existsSync(daemonPath)) {
		void vscode.window.showErrorMessage(
			`yeet.daemonPath does not exist: ${daemonPath}`,
		);
		return;
	}
	// Validate the binary is executable BEFORE handing it to spawn().
	// Without this, a user pointing at the wrong file (e.g. a `.txt`
	// or a non-`+x` binary on Unix) gets a cryptic
	// `EACCES`/`ENOEXEC` from spawn that surfaces only in the
	// Output channel — they have no idea what to fix.
	//
	// On Windows, `X_OK` is best-effort: NTFS doesn't track an
	// "executable" bit, so the call mostly verifies "file is
	// readable and unlocked". That still catches "user pointed at a
	// directory" and "file held open by AV scanner" cases.
	try {
		fs.accessSync(daemonPath, fs.constants.X_OK);
	} catch (err) {
		const message = err instanceof Error ? err.message : String(err);
		void vscode.window.showErrorMessage(
			`yeet.daemonPath is not executable: ${daemonPath}\n\n${message}\n\n`
				+ "Verify the path is correct, the file has execute "
				+ "permission, and your antivirus isn't blocking it. Or "
				+ "clear yeet.daemonPath to fall back to the bundled daemon.",
		);
		return;
	}

	const projectRoot = resolveProjectRoot();
	if (projectRoot === undefined) {
		return;
	}

	const debugEcho = cfg.get<boolean>("debugEcho") ?? false;
	const args: string[] = [projectRoot];
	if (debugEcho) {
		args.push("--debug-echo");
	}

	// Re-probe immediately before spawn. The earlier probe at the top
	// of `startDaemonInner` happened ~50-200ms ago (validation, path
	// resolution); in that window an antivirus / cloud-sync agent /
	// concurrent VS Code window may have bound :34872. Without this
	// double-check, spawn races and the new daemon fails to bind
	// (logged to stderr but the user sees only "starting...crashed").
	if (await probeDaemonAlive()) {
		output?.appendLine(
			`[yeet] port ${DAEMON_PORT} bound between probe and spawn — aborting`,
		);
		void vscode.window.showErrorMessage(
			`Yeet: another process bound 127.0.0.1:${DAEMON_PORT} just now. `
				+ "Close any other Yeet instance / app using the port, then run "
				+ "Yeet: Start again.",
		);
		setStatus("stopped");
		return;
	}

	setStatus("starting");
	output?.appendLine(
		`[yeet] spawning ${daemonPath} ${args.join(" ")}`,
	);

	const child = spawn(daemonPath, args, {
		stdio: ["ignore", "pipe", "pipe"],
		env: {
			...process.env,
			RUST_LOG: process.env["RUST_LOG"] ?? "yeet_daemon=info",
		},
	});

	// Track timing so an early daemon exit (within ~2s) gets diagnosed
	// as "couldn't bind" — the most common cause of a fast crash. The
	// daemon's stderr captures the actual error (`Address already in
	// use`, etc.) but most users don't read the Output channel by
	// default; surfacing a focused modal at the actual failure point
	// is what makes this debuggable.
	const spawnedAt = Date.now();
	const stderrBuf: string[] = [];
	let earlyExitDiagnosed = false;
	// True once we've parsed the daemon's startup version line and
	// either confirmed it matches `EXPECTED_DAEMON_VERSION` or warned
	// about the mismatch. The version line is the very first INFO log
	// the daemon emits (`yeet-daemon starting version="x.y.z"`); after
	// that we don't need to re-check. Latched so a noisy daemon
	// (RUST_LOG=trace) doesn't spam the warning toast.
	let daemonVersionChecked = false;

	// Sniffs the daemon's startup version banner from a single stdout
	// line and surfaces a non-modal warning if it doesn't match what
	// this extension was built against. `line` must already have ANSI
	// escapes stripped by the caller (see the stdout handler below) —
	// `tracing` emits them whenever it detects a TTY-like stream, and
	// RUST_LOG_STYLE=always forces them even over a pipe, which
	// otherwise hides the quoted version inside
	// `yeet-daemon starting version="x.y.z"` from this regex. Latched
	// via `daemonVersionChecked` so a noisy daemon (RUST_LOG=trace)
	// doesn't spam the warning toast.
	function checkDaemonVersion(line: string): void {
		if (daemonVersionChecked) {
			return;
		}
		const versionMatch = line.match(/yeet-daemon starting.*version="([^"]+)"/);
		if (versionMatch === null) {
			return;
		}
		daemonVersionChecked = true;
		const actual = versionMatch[1];
		if (actual === undefined) {
			return;
		}
		if (actual !== EXPECTED_DAEMON_VERSION) {
			output?.appendLine(
				`[yeet] daemon version mismatch: extension expects ${EXPECTED_DAEMON_VERSION}, daemon reports ${actual}`,
			);
			void vscode.window.showWarningMessage(
				`Yeet daemon version mismatch: extension v${EXPECTED_DAEMON_VERSION}, daemon v${actual}. `
					+ `Sync may misbehave on protocol-level changes. `
					+ `Rebuild the daemon (cargo build --release) or clear `
					+ `the yeet.daemonPath setting to use the bundled binary.`,
			);
		} else {
			output?.appendLine(`[yeet] daemon version ${actual} matches extension`);
		}
	}

	// Scrape stdout for the daemon's `yeet-auth-token: <hex>` line so
	// we don't have to read `<root>/.yeet/auth-token` ourselves, and for
	// the startup version banner (see `checkDaemonVersion`). Both live
	// on stdout: `tracing`'s default formatter writes there, and only
	// genuine `warn!`/`error!` records go to stderr. The daemon emits
	// the token line exactly once during bootstrap; subsequent stdout
	// content is normal log output. We also copy everything to the
	// output channel so the user sees it.
	let stdoutCarry = "";
	child.stdout?.on("data", (chunk: Buffer) => {
		const text = chunk.toString("utf8");
		output?.append(text);
		stdoutCarry += text;
		const newlineIdx = stdoutCarry.lastIndexOf("\n");
		if (newlineIdx === -1) {
			return;
		}
		const lines = stdoutCarry.slice(0, newlineIdx).split("\n");
		stdoutCarry = stdoutCarry.slice(newlineIdx + 1);
		for (const rawLine of lines) {
			// Strip ANSI color escapes before matching either regex
			// below — see `checkDaemonVersion` for why they'd otherwise
			// hide the version banner from its match.
			const line = rawLine.replace(/\x1b\[[0-9;]*m/g, "");
			const match = line.match(/^yeet-auth-token:\s*([0-9a-fA-F]+)\s*$/);
			if (match !== null) {
				const token = match[1];
				if (token !== undefined && token.length > 0) {
					daemonAuthToken = token;
					output?.appendLine(
						`[yeet] daemon auth token captured (${token.length} chars)`,
					);
					// If the control channel was already created (it's
					// constructed eagerly to start its reconnect loop),
					// hand it the token so the next handshake includes it.
					if (channel !== undefined) {
						channel.setAuthToken(token);
					}
				}
			}
			checkDaemonVersion(line);
		}
	});
	child.stderr?.on("data", (chunk: Buffer) => {
		const text = chunk.toString("utf8");
		output?.append(text);
		// Buffer stderr lines for the early-exit diagnostic. Cap at 20
		// lines so a noisy daemon (e.g. RUST_LOG=trace) doesn't grow
		// the buffer unboundedly across a long-running session.
		for (const line of text.split("\n")) {
			if (line.length === 0) {
				continue;
			}
			stderrBuf.push(line);
			if (stderrBuf.length > 20) {
				stderrBuf.shift();
			}
		}
	});

	child.on("error", (err: Error) => {
		output?.appendLine(`[yeet] spawn error: ${err.message}`);
		daemon = undefined;
		setStatus("crashed");
	});

	child.on("exit", (code: number | null, signal: NodeJS.Signals | null) => {
		output?.appendLine(
			`[yeet] daemon exited (code=${code ?? "?"}, signal=${signal ?? "?"})`,
		);
		daemon = undefined;
		// SIGTERM means we asked it to stop; anything else is unexpected.
		const clean = code === 0 || signal === "SIGTERM";
		setStatus(clean ? "stopped" : "crashed");

		// Crash path: the clean-stop path (`killDaemon`) already
		// disposes `channel` before signaling the child, so by the time
		// a clean exit reaches here `channel` is already undefined and
		// this is a no-op. An unexpected exit skips that teardown, so
		// without this the control channel is left connected (or
		// reconnecting) against a daemon that's gone — dispose it now
		// so a subsequent `Yeet: Start` doesn't inherit an orphaned
		// channel racing the fresh one for the daemon's single-slot
		// extension connection.
		if (!clean && channel !== undefined) {
			channel.dispose();
			channel = undefined;
		}

		// If the daemon died within ~2s of spawn AND we asked it to run
		// (not a SIGTERM from killDaemon), surface a modal with the
		// captured stderr context. This is the actionable failure path —
		// the daemon's own error message names the cause (Address already
		// in use, permission denied, missing arg) and the user shouldn't
		// have to dig through Output to find it. A healthy daemon binds
		// :34872 within ~50–200ms; anything dying inside 2s is a startup
		// failure, not a runtime crash.
		const elapsedMs = Date.now() - spawnedAt;
		if (!earlyExitDiagnosed && !clean && elapsedMs < 2000) {
			earlyExitDiagnosed = true;
			const tail = stderrBuf.length > 0
				? stderrBuf.slice(-5).join("\n")
				: "(no stderr captured)";
			void vscode.window.showErrorMessage(
				`Yeet daemon exited within ${elapsedMs}ms of starting. `
					+ `Most often this means another process is holding `
					+ `127.0.0.1:${DAEMON_PORT}. Daemon stderr:\n\n${tail}`,
				{ modal: true },
			);
		}
	});

	daemon = child;
	setStatus("running");

	// Start the auto-pair maintenance loop. While the daemon is alive,
	// the breadcrumb stays fresh so any Studio plugin that connects
	// can auto-pair on first attempt without the user running a
	// manual command. Stopped in `killDaemon` along with the daemon.
	startAutoPairing(projectRoot);

	// Opening the control channel eagerly is fine — the daemon might not be
	// listening yet, but YeetControlChannel's reconnect loop will keep trying
	// with exponential backoff until the socket comes up.
	if (output !== undefined) {
		const log = output;
		// Provider fallback for the auth token: if stdout scraping
		// missed the line (extension picked up a daemon someone else
		// started, e.g. via `cargo run`), read the file the daemon
		// persisted on disk. We know the project root because we
		// just spawned the daemon at it.
		const tokenFilePath = path.join(projectRoot, ".yeet", "auth-token");
		const ctl = new YeetControlChannel(
			`ws://127.0.0.1:${DAEMON_PORT}`,
			log,
			() => {
				if (daemonAuthToken !== undefined) {
					return daemonAuthToken;
				}
				try {
					const fileToken = fs.readFileSync(tokenFilePath, "utf8").trim();
					if (fileToken.length > 0) {
						daemonAuthToken = fileToken;
						return fileToken;
					}
				} catch {
					// File missing or unreadable — daemon hasn't written
					// it yet, or we're racing startup. Caller's reconnect
					// loop will retry; eventually the stdout scrape lands.
				}
				return undefined;
			},
		);
		ctl.on("open-project", (projectPath) => {
			log.appendLine(`[yeet:ctl] opening ${projectPath}`);
			openProject(projectPath, true, log).then(
				() => {},
				(err: unknown) => {
					const message = err instanceof Error ? err.message : String(err);
					log.appendLine(`[yeet:ctl] openFolder failed: ${message}`);
				},
			);
		});
		ctl.on("pick-folder", (requestId, prompt) => {
			void handleFolderPick(ctl, log, requestId, prompt);
		});
		ctl.connect().catch(() => {
			// First attempt failed; the reconnect loop inside the channel will
			// retry, and the "error" listener inside the channel already logs.
		});
		channel = ctl;
	}
}

async function handleFolderPick(
	ctl: YeetControlChannel,
	log: vscode.OutputChannel,
	requestId: string,
	prompt: string,
): Promise<void> {
	log.appendLine(`[yeet:ctl] pick-folder (${requestId}): ${prompt}`);
	let resolved: string | null = null;
	try {
		const picked = await vscode.window.showOpenDialog({
			canSelectFolders: true,
			canSelectFiles: false,
			canSelectMany: false,
			openLabel: prompt.length > 0 ? prompt : "Select target folder",
		});
		resolved = picked?.[0]?.fsPath ?? null;
	} catch (err) {
		const message = err instanceof Error ? err.message : String(err);
		log.appendLine(`[yeet:ctl] pick-folder failed: ${message}`);
		resolved = null;
	}
	log.appendLine(
		`[yeet:ctl] pick-folder (${requestId}) → ${resolved ?? "(cancelled)"}`,
	);
	ctl.send({
		type: "pick_folder_response",
		request_id: requestId,
		path: resolved,
	});
}

async function stopDaemon(): Promise<void> {
	await killDaemon(KILL_GRACE_MS);
}

async function killDaemon(timeoutMs: number): Promise<void> {
	// Stop the auto-pair refresh and remove the breadcrumb FIRST, so
	// nothing tries to write to a project root the user might be
	// about to delete or move. The breadcrumb has no value once the
	// daemon is going away.
	stopAutoPairing();
	if (channel !== undefined) {
		channel.dispose();
		channel = undefined;
	}
	// Reset captured token — the next daemon start generates a fresh
	// one, and we don't want the channel's provider fallback to
	// resurrect a stale value from in-memory state.
	daemonAuthToken = undefined;
	const child = daemon;
	if (!child) {
		return;
	}
	child.kill("SIGTERM");
	const exited = await waitForExit(child, timeoutMs);
	if (!exited) {
		output?.appendLine("[yeet] SIGTERM grace expired, sending SIGKILL");
		child.kill("SIGKILL");
	}
	daemon = undefined;
}

function resolveProjectRoot(): string | undefined {
	const folders = vscode.workspace.workspaceFolders;
	if (!folders || folders.length === 0) {
		void vscode.window.showErrorMessage(
			"Yeet needs an open folder that contains default.project.json.",
		);
		return undefined;
	}
	// Phase 1 only supports a single project root. If the workspace has several,
	// we pick the first that contains default.project.json and log the choice.
	for (const folder of folders) {
		const candidate = folder.uri.fsPath;
		if (fs.existsSync(path.join(candidate, "default.project.json"))) {
			if (folders.length > 1) {
				output?.appendLine(
					`[yeet] multiple workspace folders open; using ${candidate}`,
				);
			}
			return candidate;
		}
	}
	void vscode.window.showErrorMessage(
		"No workspace folder contains default.project.json. Yeet uses Rojo's project file to decide what to sync.",
	);
	return undefined;
}

function waitForExit(child: ChildProcess, timeoutMs: number): Promise<boolean> {
	return new Promise((resolve) => {
		const timer = setTimeout(() => {
			resolve(false);
		}, timeoutMs);
		child.once("exit", () => {
			clearTimeout(timer);
			resolve(true);
		});
	});
}
