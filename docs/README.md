# SECTOR

A fast, responsive, sleek **filesystem visualizer for Windows**. SECTOR scans a
volume and draws it as an interactive treemap — every file and folder sized by
the space it occupies — so you can see at a glance where your disk went and drill
into it fluidly.

Think WizTree's speed and SpaceSniffer's visual clarity, with a modern,
GPU-rendered look.

## Goals

- **Fast.** Full-volume scans that feel near-instant on NTFS, by reading the
  Master File Table directly rather than walking directories file-by-file.
- **Responsive.** Smooth pan, zoom, and drill-down over millions of entries,
  rendered on the GPU.
- **Sleek.** A **weathered industrial-metal** look — machine-shop steel, iron and
  brass, "industrial production" as the governing metaphor — a precision
  instrument, not a utilitarian tool dialog. See DECISIONS D13.
- **A scan worth watching.** SECTOR's signature: the map *builds visibly* as it
  scans — the satisfying *feeling* of the old defragmenters, in a modern style —
  turning even a slow NAS scan into the best part. See DECISIONS D12.
- **Native.** A single self-contained Windows executable. No runtime, no install
  ceremony.
- **Sees every drive.** Local disks, external drives, **and mapped network
  drives (NAS)** that have a drive letter — each scanned the best way for its
  type.

## Non-goals (at least for now)

- **No AI / assistant integration.** SECTOR will *not* embed an LLM, chat panel,
  or "ask about your files" agent. This is a firm, permanent exclusion — unlike
  the mpfiles project that inspired us, that direction is explicitly off the
  table.
- **Not a file manager *yet*.** SECTOR *shows* the filesystem; it does not aim to
  be a copy/move/rename/delete replacement for Explorer today. This is the one
  non-goal we hold loosely: **file management is a candidate future direction**
  (see ROADMAP parking lot), so we avoid architectural choices that would slam
  that door.
- **Not cross-platform.** Windows only. We optimize for Windows internals (the
  MFT) rather than a lowest-common-denominator portable scan.
- **Not shell-integrated.** No Explorer context-menu entries, no thumbnail
  providers, no OS shell hooks. SECTOR is a standalone window.
- **Not a duplicate finder / cleaner suite.** Visualization first; feature
  sprawl later, if ever.

## Stack

- **Language:** Rust
- **UI:** [egui](https://github.com/emilk/egui) (via `eframe`)
- **Treemap rendering:** custom [`wgpu`](https://github.com/gfx-rs/wgpu) paint
  layer (GPU instancing) inside an egui paint callback
- **Scanner:** routes by drive type — direct NTFS MFT / USN enumeration for
  local NTFS volumes, and a highly concurrent directory walk for mapped network
  drives (NAS) and other non-MFT volumes

See [DECISIONS.md](DECISIONS.md) for *why* these choices, and
[ROADMAP.md](ROADMAP.md) for the build plan.
