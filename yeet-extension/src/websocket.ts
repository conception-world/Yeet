import { EventEmitter } from "node:events";
import WebSocket from "ws";
// `vscode` is imported as a value (not just type) because the
// scheduleReconnect give-up path calls `vscode.window.showErrorMessage`
// and `vscode.commands.executeCommand` to surface a fatal
// notification with a one-click retry. The earlier `import type` was
// fine when this file only used `vscode` for the OutputChannel type.
import * as vscode from "vscode";

// Wire-format type aliases. The daemon's Rust enum is the source of truth
// (yeet-daemon/src/protocol.rs); these are narrow views the extension uses.
// Keeping them here means we don't need to ship a generated types package.
interface OpenProjectRequest {
	readonly type: "open_project_request";
	readonly path: string;
}

interface PickFolderRequest {
	readonly type: "pick_folder_request";
	readonly request_id: string;
	readonly prompt: string;
}

type InboundFrame = OpenProjectRequest | PickFolderRequest | { readonly type: string };

export interface PickFolderResponse {
	readonly type: "pick_folder_response";
	readonly request_id: string;
	readonly path: string | null;
}

export interface BulkSyncFromStudioRequest {
	readonly type: "bulk_sync_from_studio_request";
}

export interface BulkSyncFromIdeRequest {
	readonly type: "bulk_sync_from_ide_request";
}

export type OutboundFrame =
	| PickFolderResponse
	| BulkSyncFromStudioRequest
	| BulkSyncFromIdeRequest;

// EventEmitter typings: declare the channel-specific events so callers get
// completion + parameter types. This is the standard `interface ... extends`
// trick to overload `on`/`emit` while keeping EventEmitter's runtime intact.
export interface YeetControlChannel {
	on(event: "connected", listener: () => void): this;
	on(event: "disconnected", listener: () => void): this;
	on(event: "open-project", listener: (path: string) => void): this;
	on(event: "pick-folder", listener: (requestId: string, prompt: string) => void): this;
	on(event: "error", listener: (message: string) => void): this;
}

// Ambient identifier textually substituted at bundle time by esbuild's
// `define` (see esbuild.js) with the JSON-stringified `version` field
// from package.json. `tsc --noEmit` only needs the type — the value is
// never read outside the esbuild-produced bundle, which is the only
// way this extension ships or runs.
declare const __YEET_CLIENT_VERSION__: string;

// Extension's own wire-protocol version, derived from package.json at
// build time so it can't drift from the extension's release version the
// way a second hand-maintained literal did (this used to be a hardcoded
// "0.3.0" long after package.json had moved on to "0.4.1"). Daemon
// parses with semver and requires `>= MIN_COMPATIBLE_PLUGIN_VERSION`
// (currently "0.2.0" in yeet-daemon/src/main.rs). When breaking the
// wire, bump package.json's version and the plugin's `Widget.luau`
// literal in lockstep with the daemon's constant so all three
// components agree on the wire shape.
const CLIENT_VERSION = __YEET_CLIENT_VERSION__;
const ROLE = "extension";
const INITIAL_BACKOFF_MS = 1_000;
const MAX_BACKOFF_MS = 60_000;
// Cap on consecutive failed reconnect attempts. After this many,
// the channel gives up and surfaces a fatal user-visible
// notification (`one-click "Restart Yeet"`). Without the cap, an
// extension session left running for days against a daemon that
// never started spams the Output channel forever and burns trace
// memory. Sized for flaky networks: 30 attempts × ~60s ceiling =
// ~30 min of patient retry before the loud notification, which
// covers WiFi drops during meetings, VPN flaps, hotel networks
// reauthenticating, and Studio reloads that take a while to
// re-pair. The earlier 10×30s = 5 min budget triggered false
// positives for users on flaky networks who came back from a
// short break to find Yeet "given up".
const MAX_RECONNECT_ATTEMPTS = 30;

export class YeetControlChannel extends EventEmitter {
	private readonly url: string;
	private readonly output: vscode.OutputChannel;
	private socket: WebSocket | undefined;
	private closing = false;
	private backoffMs = INITIAL_BACKOFF_MS;
	private reconnectTimer: NodeJS.Timeout | undefined;
	// Counter for consecutive failed reconnect attempts. Reset to 0
	// on each successful socket open. When it crosses
	// MAX_RECONNECT_ATTEMPTS, `scheduleReconnect` emits a fatal
	// notification and stops scheduling. The counter is also reset
	// when the user explicitly invokes `connect()` again (one-click
	// retry from the fatal notification routes through there).
	private reconnectAttempts = 0;
	// True once we've shown the fatal "daemon unreachable"
	// notification for this run, so we don't spam the user with one
	// per attempt. Cleared on next successful open.
	private gaveUp = false;
	// Auth token captured from the daemon's stdout (or read from the
	// `<root>/.yeet/auth-token` file). When set, the channel includes
	// it in `Hello.auth_token` so the daemon's auth gate accepts the
	// connection. When absent, the channel still tries the handshake
	// — the daemon will reject and close the socket; the resulting
	// reconnect loop then keeps polling until the host extension
	// captures the token from a running daemon's stdout.
	private authToken: string | undefined;
	// Optional fallback: when set, the channel calls this just before
	// sending Hello to lazily obtain the token (e.g. by reading
	// `.yeet/auth-token` off disk). Lets the host plug in any token
	// source without coupling the channel to filesystem layout.
	private readonly authTokenProvider: (() => string | undefined) | undefined;

	constructor(
		url: string,
		output: vscode.OutputChannel,
		authTokenProvider?: () => string | undefined,
	) {
		super();
		this.url = url;
		this.output = output;
		this.authTokenProvider = authTokenProvider;
	}

	/// Updates the in-memory auth token. Called by the spawn handler
	/// when it scrapes the daemon's `yeet-auth-token: <hex>` stdout
	/// line. The next reconnect picks up the new value automatically;
	/// for an already-open socket the change is a no-op until the next
	/// re-handshake (acceptable — the existing socket already passed
	/// auth via whatever token the daemon issued at its last bootstrap).
	setAuthToken(token: string): void {
		this.authToken = token;
	}

	/// Opens the connection and resolves once the WS handshake completes — the
	/// "connected" event also fires at that moment, after the role-Hello has
	/// been sent. Subsequent reconnects do NOT re-resolve this promise; they
	/// just emit the event.
	connect(): Promise<void> {
		this.closing = false;
		// Explicit user-initiated connect (typical: clicked "Restart
		// Yeet" on the give-up notification) clears the give-up
		// budget so the channel doesn't immediately re-trigger the
		// fatal notification. A successful open also resets these
		// in `openOnce`, but resetting here too covers the case
		// where the user retries during a give-up state.
		this.reconnectAttempts = 0;
		this.gaveUp = false;
		return this.openOnce(true);
	}

	dispose(): void {
		this.closing = true;
		if (this.reconnectTimer !== undefined) {
			clearTimeout(this.reconnectTimer);
			this.reconnectTimer = undefined;
		}
		const sock = this.socket;
		this.socket = undefined;
		if (sock !== undefined) {
			try {
				sock.close();
			} catch {
				// Already closed or in a weird state — nothing actionable.
			}
		}
	}

	/// Sends a frame and reports whether it actually went out. When the
	/// socket isn't OPEN (daemon not connected yet, mid-reconnect, or
	/// disposed), the frame is dropped: previously this happened
	/// silently apart from an Output log line, so callers like
	/// `runBulkSync` had no way to tell success from failure and logged
	/// "sent" regardless. Also surfaces a warning toast here — once,
	/// for every caller — so a dropped control command (bulk sync
	/// request, pick-folder response, ...) is never invisible just
	/// because the daemon happened to be unreachable at that moment.
	send(msg: OutboundFrame): boolean {
		const sock = this.socket;
		if (sock === undefined || sock.readyState !== WebSocket.OPEN) {
			this.output.appendLine(
				`[yeet:ctl] dropping send (socket not open): ${msg.type}`,
			);
			void vscode.window.showWarningMessage(
				`Yeet: "${msg.type}" was not sent — the daemon control channel is disconnected.`,
			);
			return false;
		}
		sock.send(JSON.stringify(msg));
		return true;
	}

	private openOnce(isInitial: boolean): Promise<void> {
		return new Promise((resolve, reject) => {
			let settled = false;
			const sock = new WebSocket(this.url);
			this.socket = sock;

			sock.once("open", () => {
				// Resolve auth_token at hello time so a token captured
				// AFTER the channel was constructed (typical: daemon
				// stdout scrape happens just-after-spawn) is included.
				// Provider fallback hits the FS only if no in-memory
				// token is set, keeping the hot path cheap.
				const token = this.authToken ?? this.authTokenProvider?.();
				const hello: Record<string, unknown> = {
					type: "hello",
					version: CLIENT_VERSION,
					role: ROLE,
					studio_snapshot: [],
				};
				if (token !== undefined && token.length > 0) {
					hello["auth_token"] = token;
				}
				sock.send(JSON.stringify(hello));
				this.backoffMs = INITIAL_BACKOFF_MS;
				// Successful open resets the give-up state — if the
				// user's daemon comes back online after a flap, the
				// channel returns to its normal retry budget instead
				// of staying stuck in "fatal: gave up" mode.
				this.reconnectAttempts = 0;
				this.gaveUp = false;
				this.output.appendLine("[yeet:ctl] extension control channel connected");
				this.emit("connected");
				if (!settled) {
					settled = true;
					resolve();
				}
			});

			sock.on("message", (data) => {
				this.handleMessage(data);
			});

			sock.on("error", (err: Error) => {
				this.output.appendLine(`[yeet:ctl] socket error: ${err.message}`);
				this.emit("error", err.message);
				// On the very first connect attempt, surface the error to the
				// caller so it can decide whether to keep trying. Reconnects
				// scheduled after a successful open just retry silently.
				if (isInitial && !settled) {
					settled = true;
					reject(err);
				}
			});

			sock.on("close", () => {
				this.socket = undefined;
				this.emit("disconnected");
				if (this.closing) {
					return;
				}
				this.scheduleReconnect();
			});
		});
	}

	private scheduleReconnect(): void {
		if (this.reconnectTimer !== undefined) {
			return;
		}
		// Hit the give-up threshold. Stop retrying, fire a single
		// fatal user notification with a one-click retry that resets
		// the counter via `connect()`. Set `gaveUp` so `close`
		// handlers triggered by the abort don't re-enter and notify
		// twice.
		this.reconnectAttempts += 1;
		if (this.reconnectAttempts > MAX_RECONNECT_ATTEMPTS) {
			if (!this.gaveUp) {
				this.gaveUp = true;
				this.output.appendLine(
					`[yeet:ctl] giving up after ${MAX_RECONNECT_ATTEMPTS} failed reconnects; daemon appears unreachable`,
				);
				void vscode.window
					.showErrorMessage(
						`Yeet: daemon unreachable after ${MAX_RECONNECT_ATTEMPTS} attempts. The daemon may not be running, or the configured URL may be wrong.`,
						"Restart Yeet",
						"Open Settings",
					)
					.then((choice) => {
						if (choice === "Restart Yeet") {
							void vscode.commands.executeCommand("yeet.start");
						} else if (choice === "Open Settings") {
							void vscode.commands.executeCommand(
								"workbench.action.openSettings",
								"yeet.daemonPath",
							);
						}
					});
			}
			return;
		}
		const delay = this.backoffMs;
		this.backoffMs = Math.min(this.backoffMs * 2, MAX_BACKOFF_MS);
		this.output.appendLine(
			`[yeet:ctl] reconnecting in ${delay}ms (attempt ${this.reconnectAttempts}/${MAX_RECONNECT_ATTEMPTS})`,
		);
		this.reconnectTimer = setTimeout(() => {
			this.reconnectTimer = undefined;
			if (this.closing) {
				return;
			}
			void this.openOnce(false).catch(() => {
				// Failure already logged + emitted in openOnce; the close
				// handler will reschedule.
			});
		}, delay);
	}

	private handleMessage(data: WebSocket.RawData): void {
		const text = typeof data === "string" ? data : data.toString("utf8");
		let parsed: InboundFrame;
		try {
			parsed = JSON.parse(text) as InboundFrame;
		} catch (err) {
			const message = err instanceof Error ? err.message : String(err);
			this.output.appendLine(`[yeet:ctl] dropped malformed frame: ${message}`);
			return;
		}
		switch (parsed.type) {
			case "open_project_request": {
				const frame = parsed as OpenProjectRequest;
				this.emit("open-project", frame.path);
				return;
			}
			case "pick_folder_request": {
				const frame = parsed as PickFolderRequest;
				this.emit("pick-folder", frame.request_id, frame.prompt);
				return;
			}
			default: {
				// Daemon broadcasts plugin-targeted frames on a different code
				// path (this extension's tx is a private mpsc), so anything
				// arriving here that isn't recognized is genuinely unexpected.
				this.output.appendLine(`[yeet:ctl] ignoring frame type: ${parsed.type}`);
			}
		}
	}
}
