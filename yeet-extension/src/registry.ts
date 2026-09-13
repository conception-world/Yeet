/// Reader for `~/.yeet/daemons.json`, the cross-project index of running
/// daemons that `yeet-daemon/src/registry.rs` maintains. That module is the
/// source of truth for the schema; this file only has to parse it.
///
/// Why the extension needs it: before multi-place, "is a daemon already
/// running?" was answered by probing the fixed port 34872, and any answer at
/// all meant "refuse to spawn". With one daemon per project nothing collides,
/// and the question that actually matters is "is one already serving MY project
/// root?" — which a port probe cannot answer, but this index can.
///
/// Everything here is advisory. A missing, stale, or unparseable registry must
/// degrade to "spawn my own daemon", never to an error the user sees: a
/// duplicate daemon on another port is harmless, while a blocked startup is
/// not.

import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import WebSocket from "ws";

/// Schema version written by `registry.rs`. A file claiming anything else is
/// ignored rather than guessed at, so a newer daemon's format can never be
/// misread by an older extension.
export const REGISTRY_VERSION = 1;

/// One daemon, mirroring `registry::Entry`.
export interface DaemonEntry {
	readonly daemon_id: string;
	/// Diagnostic only — `registry.rs` explicitly does not use the pid to decide
	/// liveness (it would need an unsafe `kill(pid, 0)`), and neither do we. It
	/// is here so an error message can name the process to kill.
	readonly pid: number;
	readonly port: number;
	readonly project_root: string;
	readonly project_name: string;
	readonly daemon_version: string;
	readonly started_at: number;
}

/// `~/.yeet/daemons.json`. Matches `registry::registry_path` — `USERPROFILE` on
/// Windows, `HOME` elsewhere, deliberately not a per-OS config dir.
export function registryPath(): string {
	return path.join(os.homedir(), ".yeet", "daemons.json");
}

function isDaemonEntry(value: unknown): value is DaemonEntry {
	if (typeof value !== "object" || value === null) {
		return false;
	}
	const e = value as Record<string, unknown>;
	return (
		typeof e["daemon_id"] === "string"
		&& typeof e["pid"] === "number"
		&& typeof e["port"] === "number"
		&& typeof e["project_root"] === "string"
		&& typeof e["project_name"] === "string"
		&& typeof e["daemon_version"] === "string"
	);
}

/// Parses registry file contents. Pure, so it is testable without a filesystem.
///
/// Any shape we do not recognize yields `[]`. Individual malformed entries are
/// skipped rather than failing the whole parse, so one bad row written by a
/// future daemon cannot hide the rows we do understand.
export function parseRegistry(raw: string): DaemonEntry[] {
	let parsed: unknown;
	try {
		parsed = JSON.parse(raw);
	} catch {
		return [];
	}
	if (typeof parsed !== "object" || parsed === null) {
		return [];
	}
	const file = parsed as Record<string, unknown>;
	if (file["version"] !== REGISTRY_VERSION) {
		return [];
	}
	const daemons = file["daemons"];
	if (!Array.isArray(daemons)) {
		return [];
	}
	return daemons.filter(isDaemonEntry);
}

/// Reads and parses the registry. A missing or unreadable file is an empty
/// registry, never a throw.
export function readRegistry(file = registryPath()): DaemonEntry[] {
	try {
		return parseRegistry(fs.readFileSync(file, "utf8"));
	} catch {
		return [];
	}
}

/// Normalizes a filesystem path for comparison between the daemon's canonical
/// form and VS Code's plain one.
///
/// Three things have to line up or a match silently fails and the extension
/// spawns a duplicate daemon for a project that already has one:
///
///   * Windows' `\\?\` verbatim prefix, which `fs::canonicalize` adds and
///     `registry.rs` already strips before writing — stripped here too so this
///     function is safe on either form.
///   * Separators: `C:/dev/game` from a URI vs `C:\dev\game` from the daemon.
///   * Case: Windows paths are case-insensitive. Folded on Windows only, since
///     two POSIX paths differing by case really are different directories.
///
/// Also drops a trailing separator so `C:\dev\game\` and `C:\dev\game` agree.
export function normalizeRoot(p: string, platform: NodeJS.Platform = process.platform): string {
	let s = p;
	if (s.startsWith("\\\\?\\UNC\\")) {
		s = `\\\\${s.slice("\\\\?\\UNC\\".length)}`;
	} else if (s.startsWith("\\\\?\\")) {
		s = s.slice("\\\\?\\".length);
	}
	const isWindows = platform === "win32";
	if (isWindows) {
		s = s.replace(/\//g, "\\");
		s = s.toLowerCase();
	}
	// Trim a trailing separator, but never turn a root ("/" or "C:\") into "".
	const sep = isWindows ? "\\" : "/";
	while (s.length > 1 && s.endsWith(sep) && !s.endsWith(`:${sep}`)) {
		s = s.slice(0, -1);
	}
	return s;
}

/// The registered daemon serving `projectRoot`, if any.
///
/// Pure over its inputs so it can be tested without a filesystem or a live
/// daemon — the extension has no test harness in-repo, so keeping the decision
/// logic free of I/O is what makes it verifiable at all.
///
/// Returning an entry is NOT proof the daemon is alive, nor that it is really
/// ours: a recycled port could be anything. The caller confirms with a
/// `role="discover"` handshake before reusing it, and falls back to spawning on
/// any doubt.
///
/// When several rows claim the same root (possible after a crash left a stale
/// row that has not been pruned), the most recently started one wins — it is
/// the one most likely to still be alive.
export function findDaemonForRoot(
	entries: readonly DaemonEntry[],
	projectRoot: string,
	platform: NodeJS.Platform = process.platform,
): DaemonEntry | undefined {
	const target = normalizeRoot(projectRoot, platform);
	const matches = entries.filter((e) => normalizeRoot(e.project_root, platform) === target);
	if (matches.length <= 1) {
		return matches[0];
	}
	return matches.reduce((newest, e) => (e.started_at > newest.started_at ? e : newest));
}

/// What a daemon reports about itself in response to a `role="discover"` probe.
/// Mirrors `ServerMsg::DaemonInfo`.
export interface DaemonInfo {
	readonly type: "daemon_info";
	readonly daemon_id: string;
	readonly project_name: string;
	readonly project_root: string;
	readonly port: number;
	readonly daemon_version: string;
	readonly plugin_connected: boolean;
}

/// How long to wait for a discovery reply before giving up on a port.
const DISCOVERY_TIMEOUT_MS = 1500;

function isDaemonInfo(value: unknown): value is DaemonInfo {
	if (typeof value !== "object" || value === null) {
		return false;
	}
	const v = value as Record<string, unknown>;
	return (
		v["type"] === "daemon_info"
		&& typeof v["daemon_id"] === "string"
		&& typeof v["project_root"] === "string"
	);
}

/// Asks whoever is listening on `port` to identify itself.
///
/// Resolves `undefined` for every failure mode — nothing listening, a
/// non-daemon process squatting the port, a daemon too old to know the
/// `discover` role (it answers `auth_rejected` and closes), or a timeout. The
/// caller treats all of those the same way: do not reuse, spawn instead.
///
/// This is the confirmation step a registry row cannot provide on its own: the
/// row proves a daemon was there, the handshake proves it is still there and is
/// the daemon we think it is.
export function probeDaemonInfo(
	port: number,
	timeoutMs = DISCOVERY_TIMEOUT_MS,
): Promise<DaemonInfo | undefined> {
	return new Promise((resolve) => {
		let settled = false;
		let ws: WebSocket | undefined;
		const finish = (info: DaemonInfo | undefined): void => {
			if (settled) {
				return;
			}
			settled = true;
			try {
				ws?.close();
			} catch {
				// Already closing/closed — nothing to do.
			}
			resolve(info);
		};
		const timer = setTimeout(() => finish(undefined), timeoutMs);
		// Do not hold the extension host alive just for a probe.
		timer.unref?.();
		try {
			ws = new WebSocket(`ws://127.0.0.1:${port}`);
		} catch {
			clearTimeout(timer);
			finish(undefined);
			return;
		}
		ws.on("open", () => {
			try {
				ws?.send(
					JSON.stringify({
						type: "hello",
						// Any version is accepted: the daemon answers discovery
						// before its version gate precisely so a mismatched
						// client can still identify what is running.
						version: "0.5.0",
						role: "discover",
					}),
				);
			} catch {
				finish(undefined);
			}
		});
		ws.on("message", (raw: WebSocket.RawData) => {
			clearTimeout(timer);
			try {
				const parsed: unknown = JSON.parse(raw.toString());
				finish(isDaemonInfo(parsed) ? parsed : undefined);
			} catch {
				finish(undefined);
			}
		});
		ws.on("error", () => {
			clearTimeout(timer);
			finish(undefined);
		});
		ws.on("close", () => {
			clearTimeout(timer);
			finish(undefined);
		});
	});
}

/// Finds a live daemon already serving `projectRoot`, confirming it over the
/// wire before reporting it as reusable.
///
/// Two-step on purpose: the registry narrows the search to one port, and the
/// handshake proves that port still hosts the daemon for this exact project. If
/// either step is inconclusive the answer is `undefined` and the caller spawns
/// its own — attaching to the wrong project's daemon would be far worse than an
/// extra process.
export async function findLiveDaemonForRoot(
	projectRoot: string,
	entries: readonly DaemonEntry[] = readRegistry(),
	platform: NodeJS.Platform = process.platform,
): Promise<DaemonInfo | undefined> {
	const candidate = findDaemonForRoot(entries, projectRoot, platform);
	if (candidate === undefined) {
		return undefined;
	}
	const info = await probeDaemonInfo(candidate.port);
	if (info === undefined) {
		return undefined;
	}
	// The port answered, but is it still the same project? A daemon can exit
	// and another one bind that port for a different root before the registry
	// catches up.
	const sameRoot =
		normalizeRoot(info.project_root, platform) === normalizeRoot(projectRoot, platform);
	return sameRoot ? info : undefined;
}
