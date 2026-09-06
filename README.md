# SECTOR

> A fast, keyboard-friendly **file explorer for Windows** — with a 2.5D cityscape
> view of your disk built in.

![SECTOR cityscape](docs/preview.png)

SECTOR is a native Windows file explorer that treats **local disks and mapped
NAS/SMB shares as equals**. It's a normal two-pane explorer first — folder tree,
file list, the usual operations — and a **visualizer second**: flip any folder
into an interactive isometric "cityscape" to see at a glance where your space
went.

## Files view (the explorer)

- **Quick access** at the top of the folder tree: Desktop, Documents,
  Downloads, Pictures, Music and Videos (resolved through Windows, so a
  OneDrive-redirected Desktop is found), plus your own pins — right-click any
  folder → **Pin to Quick access**. `Ctrl+1`…`Ctrl+9` jump to them.
- **Folder tree + file list**, fully keyboard-drivable. **Tab** switches panes;
  arrows move; **→/←** open/close tree folders; type letters to **jump**
  (type-ahead) in either pane. `Ctrl+↑/↓` moves the cursor without selecting
  and `Ctrl+Space` toggles it, so non-contiguous selections work by keyboard.
  `Shift+F10` opens the context menu; `Esc` clears the filter, then the
  selection, then closes Details. With the tree focused, `F2` / `Del` /
  `Ctrl+C` / `Ctrl+X` / `Alt+Enter` act on the folder you're in, and `Enter`
  expands or collapses it. Drag the divider to resize the tree, or
  **double-click it** to fit the widest visible row.
- **Navigation** — a clickable breadcrumb; click the bar's empty space (or
  `Alt+D` / `Ctrl+L` / `F4`) to type a path instead. Plus **Back/Forward/Up** (`Alt+←` / `Alt+→` / `Backspace` or `Alt+↑`)
  with history — each puts you back on the item you were on.
- **List** — Name / Size / Type / Modified, sortable (remembered across
  launches), resizable, virtualized for huge folders, file-type–colored. Folder
  sizes appear when a scan covers the folder. The status bar shows the folder's
  totals and the drive's free space.
- **Filter** the current folder as you type (`Ctrl+F`), and a **hidden-files**
  toggle that applies to the list and the folder tree alike.
- **Details** panel (`Alt+Enter`, or the toolbar toggle) — path, sizes, dates,
  attributes.
- **Multi-select** — click / `Ctrl`-click / `Shift`-click / `Ctrl+A`, or drag a
  rubber band from empty space (`Ctrl`/`Shift` to add); click empty space to
  deselect.
- **Mouse** — side buttons go Back/Forward, and right-clicking those buttons
  lists the history. The `›` chevrons in the breadcrumb list a folder's
  subfolders. Right-click empty space for the folder's own menu (Paste, New
  folder, …), right-click a folder in the tree for its menu, and hover a row for
  its full name and details. `Ctrl`+wheel (or `Ctrl+=` / `Ctrl+-` / `Ctrl+0`)
  zooms the whole UI, remembered across launches.

### File operations

- Open (`Enter` / double-click; on a multi-selection, every selected file),
  **Reveal in Explorer**, **Copy path** (`Ctrl+Shift+C`) **/ name**.
- **New folder** (`Ctrl+Shift+N`) and **Rename** (`F2`) — validated (bad chars,
  reserved names, duplicates), extension preserved on rename.
- **Cut / Copy / Paste** (`Ctrl+X/C/V`) — shares the **Windows clipboard** with
  Explorer, so files copied or cut there paste here and vice versa. Runs in the
  background, **never overwrites** (auto-renames to "… - Copy"), a cut source is
  removed only after its copy succeeds, cross-volume moves fall back to
  copy-then-delete.
- **Delete** (`Del`) → the **Recycle Bin** on local drives, with a confirmation.
  Network drives have no Windows Recycle Bin, so those delete for real (and are
  caught by your NAS's own recycle bin, e.g. Synology's `#recycle`, if enabled) —
  the dialog says so.
- **Drag and drop** — drag rows onto a folder (in the list or the tree) to
  move them, or hold `Ctrl` to copy; a badge follows the pointer saying which.
  Drop files from Explorer (or any app) onto the window to copy them into the
  current folder (always a copy, never a move). Both undoable.
- **Undo / Redo** (`Ctrl+Z` / `Ctrl+Y`, or the footer buttons) — reverses the
  last rename, new folder, paste (copy or move, even a partial one), or delete
  (restored from the Recycle Bin), and re-applies it again. Never overwrites: a
  reversal that would collide stops and says so.
- **Refresh** with `F5` / `Ctrl+R`.

## City view (the visualizer)

Flip to **City** to render the current folder as a 2.5D isometric cityscape:

- **Area = size** · **Height = file count** · **Color = file type**
- **Live discovery build** — the map rises as it scans, not a blank spinner.
- **Auto-scan** — entering a local folder with no cityscape yet starts its
  scan so you watch it build; network drives and drive roots wait for **Scan**
  (a toggle in the City bar turns this off).
- **Instant reopen** — each scan is cached compactly, so reopening an 82 TB drive
  takes ~1 second instead of minutes.
- **Freshness (local NTFS)** — the USN change journal tells you whether a cached
  view is still current, so you know when to rescan.

## Build & run

Windows, with the Rust MSVC toolchain:

```
cargo run -p sector-app
```

The core, layout, and file-op logic are OS-agnostic and unit-tested; only the GUI
and the Windows-specific paths (USN, drive/network detection, Recycle Bin) require
Windows.

## Layout

| Crate | Role |
|---|---|
| `crates/sector-core` | Data model (arena tree), squarified treemap layout, file-type classification, cache serialization |
| `crates/sector-scan` | Concurrent filesystem scanner, single-directory listing, NTFS USN freshness |
| `crates/sector-app`  | The egui / wgpu GUI (explorer + cityscape) |

## Docs

- [`docs/ROADMAP.md`](docs/ROADMAP.md) — build plan and progress
- [`docs/DECISIONS.md`](docs/DECISIONS.md) — design decisions (D1–D19)
- [`docs/README.md`](docs/README.md) — goals & non-goals

## Status

Active development. **Windows-only by design** — it leans on Windows filesystem
specifics (MFT/USN, the shell) rather than a lowest-common-denominator portable
approach. **No AI/assistant integration** — a firm, permanent non-goal.

---

Built with [Claude Code](https://claude.com/claude-code).
