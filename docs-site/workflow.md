# Daily workflow

Once Studio and your IDE are connected (see
[Getting started](/getting-started)), Yeet sits in the background.
You edit where you want; both sides converge.

## Editing in your IDE

You edit a `.luau` (or `.lua`) file in VS Code, save, and within
~200 ms Studio shows the change in the matching script. The plugin
applies edits via Studio's standard script API — undo history is
preserved on the Studio side, so `Ctrl+Z` still works as expected.

Because both sides sync continuously, you can keep Studio open in
the background for play-testing while doing all editing in your IDE.
No "publish" or "push" step.

## Editing in Studio

You can also edit directly in Studio (the built-in script editor or
any plugin editor). The plugin watches script `.Source` changes and
mirrors them to disk within ~200 ms. Useful when:

- You're iterating on something that needs Studio APIs you can only
  test in-engine (e.g. `Workspace`, `Players`)
- You're pairing with someone who only knows Studio
- You want to make a quick fix without alt-tabbing

Yeet's echo-detection prevents ping-pong: if a Studio change came
*from* a disk write, it isn't echoed back to disk (and vice versa).

## Conflicts

If both sides change the same file between two sync ticks, Yeet
shows a **3-pane line-by-line merge** in the plugin dock:

| Pane | Contents |
|---|---|
| Left | Your IDE's version |
| Centre | The common ancestor (last synced version) |
| Right | Studio's version |

You pick lines (or hunks) to keep. The result becomes the new shared
version on both sides. There's no "auto-resolve" — Yeet always asks,
because silent merging is how lost work happens.

If you need to abort and pick one side wholesale, the dock has
**Take IDE** and **Take Studio** buttons.

## Bulk sync

Two one-shot operations, both driven from the **Studio plugin's
BulkSync dock** (button in the plugin toolbar).

### Sync From Studio

Studio's DataModel becomes the source of truth. Disk files are
recreated to match the Studio tree exactly. Use this when:

- You opened an existing Studio project that has scripts not yet on
  disk (typical for places shared via Roblox without a Rojo setup)
- You made a bunch of edits in Studio offline and want to land them
  all at once

A **preview dock** appears first, showing every file that would be
created / overwritten / deleted. You confirm before anything is
written.

### Sync From IDE

The inverse: disk files overwrite Studio's DataModel. Use this when:

- You're rebasing onto a teammate's branch that has many edits
- You want to discard Studio-side experimentation without manual
  cleanup

Same preview-then-confirm flow.

::: warning
Bulk syncs can delete scripts on the destination side. Always read
the preview before clicking Apply.
:::

## Reverse bootstrap from Studio

If a place exists in Studio but has **no on-disk project yet**, the
plugin's "Open in IDE" button bootstraps everything in one shot:

1. Plugin reads the entire DataModel
2. Plugin asks the daemon to scaffold a `default.project.json` and
   `src/` tree to match
3. The daemon emits an `open_project_request` to your IDE
4. The IDE pops a confirmation modal showing the path
5. You click **Open Folder** — VS Code opens the new project

After bootstrap you're in the same state as if you'd run
`Yeet: Create` followed by a Sync From Studio in the plugin
manually.

The confirmation modal is mandatory — opening an arbitrary folder
in VS Code can run code (`.vscode/tasks.json` with `"runOn":
"folderOpen"`), so Yeet never opens a folder silently.

## Working on two projects at once

Each project gets its own daemon, so two projects sync side by side.

1. Open project A in one editor window and run **`Yeet: Start`**. Do
   the same for project B in a second window. The first takes port
   `34872`, the second `34873` — the status bar shows which.
2. Open place A in Studio. In the Yeet panel, the **Project** list
   shows both daemons: project name, root path and port. Click the row
   for A, then **Connect**.
3. Open place B in another Studio window and pick B's row there.

Studio remembers each place's choice, so from then on both reconnect
to the right project on their own.

The picker marks a daemon another place already holds with an **in
use** badge, so you do not take a connection away by accident. Read
the root path before connecting when two projects share a name — that
path is what tells them apart.

If a project is missing from the list, hit **Refresh**. Still missing
usually means its daemon is not running; see
[Troubleshooting](/troubleshooting).

## Disconnecting

You can leave the daemon running indefinitely; it idles cheap. To
stop sync:

- **`Yeet: Stop`** in your IDE — stops the daemon this window started
  and reverts Studio's Connect button to "Disconnected". If this window
  attached to a daemon another window spawned, it only detaches; that
  daemon keeps serving whoever else is using it.
- Click **Pause** in the plugin dock — Studio stops applying remote
  edits but the disk side keeps watching (useful when you want to
  test a quick change without writing it back)

Closing VS Code triggers `Yeet: Stop` automatically via the extension's
deactivate hook.

## What's next

- [Settings reference](/settings) — every setting and what it does
- [Troubleshooting](/troubleshooting) — common issues and fixes
- [How it works](/architecture) — the 3-component design
