# SECTOR

A fast, keyboard-friendly **file explorer for Windows** with a **2.5D cityscape
visualizer** built in. Browse local disks and mapped NAS/SMB shares as equals in
a conventional two-pane explorer; flip any folder into an interactive isometric
"city" — area = size, height = file count, color = file type — to see at a
glance where your space went.

It began as a pure visualizer (think WizTree's speed and SpaceSniffer's visual
clarity, with a modern GPU-rendered look) and pivoted to **explorer first,
visualizer second** (DECISIONS D16). The cityscape is a lens on the folder you're
browsing, not a separate tool.

## Goals

- **Fast.** Folder browsing is live and instant (`read_dir` on demand). Whole-
  tree scans use a highly concurrent directory walk that hides SMB latency, and
  every completed scan is **cached** compactly so reopening even an 82 TB NAS
  takes about a second. A direct NTFS MFT read for near-instant *local* full-
  volume scans is planned (ROADMAP Step 1d) but not yet built.
- **Responsive.** Smooth drill-down and hover over more than a million entries.
- **Sleek.** An **industrial city** look — dusk palette, neon-lit block tops,
  ground plinth and shadows; districts are type zones, the skyline is file
  count. Industrial materials (steel, iron, brass) remain the vocabulary. See
  DECISIONS D13 → D15.
- **A scan worth watching.** SECTOR's signature: the map *builds visibly* as it
  scans — the satisfying *feeling* of the old defragmenters, in a modern style —
  turning even a slow NAS scan into the best part. See DECISIONS D12.
- **Safe to work in.** File operations are grown safest-first: read-only
  browsing → non-destructive edits → destructive ops last, with the Recycle Bin,
  confirmations, and never-overwrite copy semantics. See DECISIONS D16.
- **Native.** A single self-contained Windows executable. No runtime, no install
  ceremony.
- **Sees every drive.** Local disks, external drives, and mapped network
  drives (NAS). Network drives are **hidden by default for now** (a
  "Network drives" toggle at the foot of the folder tree brings them back;
  SECTOR won't touch a hidden drive — see DECISIONS D21).

## Non-goals

- **No AI / assistant integration.** SECTOR will *not* embed an LLM, chat panel,
  or "ask about your files" agent. This is a firm, permanent exclusion — unlike
  the mpfiles project that inspired us, that direction is explicitly off the
  table. See DECISIONS D8.
- **Not cross-platform.** Windows only. We lean on Windows internals (USN, the
  shell, the Recycle Bin) rather than a lowest-common-denominator portable
  approach.
- **Not shell-integrated.** No Explorer context-menu entries, no thumbnail
  providers, no OS shell hooks. SECTOR is a standalone window.
- **Not a duplicate finder / cleaner suite.** Feature sprawl later, if ever.

## Stack

- **Language:** Rust
- **UI:** [egui](https://github.com/emilk/egui) (via `eframe`, wgpu renderer —
  Vulkan on the reference machine)
- **Cityscape rendering:** egui's painter (shaded isometric polygons, back-to-
  front). A custom [`wgpu`](https://github.com/gfx-rs/wgpu) instanced draw is a
  possible later upgrade (ROADMAP Step 3b) if true orbitable 3D or far larger
  tile counts are wanted.
- **Scanner:** a highly concurrent directory walk (`std::fs`, OS-agnostic) that
  feeds an index-based arena tree; serves local and network drives alike today.
  Local NTFS gets **USN change-journal freshness** checks on cached scans; a
  direct MFT fast path is planned.
- **Cache:** serde + postcard, roughly 22 bytes per node.

See [DECISIONS.md](DECISIONS.md) for *why* these choices, and
[ROADMAP.md](ROADMAP.md) for the build plan and progress.
