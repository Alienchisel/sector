# SECTOR — Roadmap

Built one step at a time. Each step is independently runnable and de-risks the
next, so we always have something that works and something we've learned.

Status legend: ☐ not started · ◐ in progress · ☑ done

---

## Step 0 — Toolchain smoke test  ☑ DONE

A minimal Cargo **workspace** with an `eframe`/egui window (wgpu renderer) that
opens and renders "SECTOR".

- **Purpose:** prove the build/run loop works on the Windows machine *before* we
  invest. The user's machine is the compile loop, so validate it on day one.
- **Done when:** `cargo run -p sector-app` opens a window on Windows. ✅ Window
  opened (900×600 client), rendered the heading + the `sector-core` linkage
  check, empty stderr, no wgpu/DX warnings.

**Outcome:**
- ☑ Workspace on the NAS (`sector-core` lib + `sector-app` bin), linkage proven
  at runtime (core ran inside the GUI process).
- ☑ Build isolation holds: artifacts on `C:` / `~/.cargo-target`, NAS stays clean.
- ☑ Versions locked (eframe/egui 0.36.1, wgpu 30.0.1, winit 0.30.13).
- ☑ Process fix: `cargo check -p sector-app` now runs on the Linux VM (needs
  eframe default features) — API errors caught here, not via Windows round-trips.
- ☑ Added `env_logger` + a startup readout of `adapter.get_info()`. Direct
  report: **backend = Vulkan**, adapter = NVIDIA GeForce RTX 4070 (DiscreteGpu).
  (The earlier DLL-based guess of DX12 was wrong — wgpu picks Vulkan here; fine,
  it's abstracted and our shaders are WGSL.) `RUST_LOG=wgpu_core=info` now works.

## Step 1 — The scanner (the crown jewel)  ◐

A standalone crate that enumerates a volume as fast as possible, **routing by
drive type** (`GetDriveType`).

**Progress:**
- ☑ **1a — arena tree** (`sector-core::tree`): index-based `Tree`/`Node`/`NodeId`,
  own + aggregated `subtree_size`, path reconstruction, streaming-friendly.
  Unit-tested on Linux.
- ☑ **1b — concurrent directory walker** (`sector-scan::walk`): producer/consumer
  pipeline (parallel `read_dir`/`stat` workers → single-threaded tree builder),
  progress counters + cancellation, symlinks not followed. Unit-tested on Linux;
  benchmarked against the real NAS (`/mnt/nas/manga`, 2448 files / 604 GB):
  `threads=1` ≈ 1.2s → `16–32` ≈ 0.2s (**~5–6× speedup, sweet spot 16–32**).
  Confirms concurrency hides SMB latency (D7). Caveats: warm cache (real cold
  scans are slower, and concurrency helps *more*); modest file count; per-dir
  concurrency (stats within one dir are serial — revisit if a wide-shallow tree
  starves the pool).
- ◐ **1c** — drive-type routing (`GetDriveType`) [Windows] — *detection* is
  done (`GetDriveTypeW` → `is_network_path`, used to decide Recycle Bin vs
  permanent delete); routing *scans* by type waits on an MFT path (1d).
- ☐ **1d** — MFT/USN fast path for local NTFS [Windows]
- ☐ **1e** — wire routing: `DRIVE_REMOTE`→walker, `DRIVE_FIXED` NTFS→MFT [Windows]

- **Local NTFS (`DRIVE_FIXED`):** read the **MFT / USN** directly
  (`FSCTL_ENUM_USN_DATA`) for near-instant full-volume enumeration.
- **Mapped network drives / NAS (`DRIVE_REMOTE`) and other non-MFT volumes:** a
  **highly concurrent directory walk** over SMB. This is a first-class path, not
  a fallback — for network drives it is the *only* path, and its bottleneck is
  latency, so it must fan out many concurrent requests to stay fast.
- Emit a compact **arena tree** (index-based, not `Rc`-linked) with per-node
  size and aggregated child size.
- **Progress + cancellation** from the start: network scans can take a while, so
  the scanner reports progress and can be cancelled cleanly mid-scan.
- **Purpose:** this is where "fast" is actually won or lost. No UI yet.
- **Done when:** a local drive scans in seconds via the MFT, *and* a mapped NAS
  drive scans via the concurrent walk with live progress and clean cancel — both
  with benchmark numbers to point at.

## Step 2 — Treemap layout (pure logic)  ☑ DONE

A **squarified treemap** algorithm turning the arena tree into rectangles
(`sector-core::treemap`: `Rect`, `Tile`, `LayoutOptions`, `layout()`).

- Pure geometry, unit-tested. No rendering.
- **Purpose:** get the layout provably correct in isolation.
- **Done when:** given a subtree and a viewport rect, it returns stable, correct
  child rectangles, with tests. ✅
- **Outcome:**
  - Squarified algorithm (Bruls–Huizing–van Wijk); flat pre-order `Vec<Tile>`
    (parents before children) ready for one instanced GPU draw.
  - `max_depth` + `min_tile` culling bounds tile count to on-screen detail, not
    node count (test: 10k nodes → <1k tiles).
  - Unit tests: area proportionality, area conservation, viewport containment,
    nested recursion, culling.
  - **End-to-end validated on the real NAS:** `/mnt/nas/manga` (2448 files /
    604 GB) scanned in ~325ms → **2761 tiles laid out in 0.3ms** → rendered to
    SVG on Linux (see `sector-scan/examples/layout_svg.rs`). Layout confirmed
    visually correct (well-squared tiles, descending size order, nesting).

## Step 3 — GPU treemap render  ◐

**Step 3a — interactive egui-painter render: ☑ DONE.** Before the custom wgpu
draw, we wired the pipeline into `sector-app` using egui's painter: background
scan (UI stays live) with a live progress readout + cancel, then a drill-down
treemap (click a folder to descend, breadcrumb / ⬆ Up to ascend, hover for
path·size·item-count), depth-colored steel→brass. **Validated on Windows against
the full NAS: 278,487 dirs / 1,211,007 files / 82.0 TB → one interactive
treemap.** Layout of ~1.5M nodes is instant.

- **Findings from that run (drive the next steps):**
  - The **cold Windows SMB scan is the bottleneck**, not layout/render: the full
    NAS took ~600s (~2.5k entries/s cold) vs the warm Linux benchmark's ~13k/s.
    → motivates (a) **progressive render (D12)** so the panel isn't blank during
    a long scan, and (b) **Windows scan tuning** (more concurrency to hide cold
    latency; investigate per-dir round-trips).
  - Rendering-only egui painter is fine at this scale; the custom wgpu instanced
    draw (3b) is for even larger tile counts + the industrial material look.

**Step 3a.1 — live discovery build (D12): ☑ DONE (v1).** Scan writes into a
shared `Arc<Mutex<Tree>>` (`scan_into`), sizes maintained incrementally
(`Tree::add_child_propagating`, no recompute); the UI renders the growing tree.
Anti-"boiling" v1 = throttled re-layout (600ms) so it settles in steps, then a
"crystallize" relayout on completion. Confirmed on Windows: C: (SSD) subdivides
instantly; old NAS Z: shows a flat root rect for ~40s (slow top-level SMB
enumeration) then fills in — the initial-enumeration latency is a known rough
edge (progress counter keeps ticking meanwhile). Stable-order + tweening is the
next refinement if the stepping feels jarring.

**Step 3a.2 — color = file type: ☑ DONE.** `sector-core::filetype` classifies by
extension into Video/Image/Audio/Archive/Document/Code/Other (cbz/cbr→Archive for
manga; `.ts`→Video not TypeScript). Files colored by category (muted palette),
directories neutral dark, legend in a bottom panel, category shown in tooltip.
(Replaces the depth-ramp placeholder.)

**Step 3a.3 — dominant-folder coloring: ☑ DONE.** `Tree::dominant_categories()`
(one O(n) bottom-up pass, by bytes) computed when a scan finishes; every tile —
folders included — is then colored by the category dominating its subtree, so a
folder of videos reads amber even when culled to a single tile (no more grey
sea). During the live scan, folders stay neutral until the crystallize pass.
Validated on the NAS: manga renders all-blue (all Archive), correctly. (Transient
memory: a 7×u64 per-node table during the pass — fine now, optimizable later.)

**Step 3a.4 — 2.5D isometric cityscape (D14): ☑ DONE (v1).** Ported the SVG-
prototyped isometric extruded render into the app's egui painter: leaf blocks
with height = file count (live `Node::file_count`), color = dominant type, three
shaded faces, fixed dimetric camera, back-to-front draw order, point-in-quad
hover/drill hit-testing. Replaces the flat 2D render. Prototype:
`sector-scan/examples/iso_svg.rs`. Refinements open: angle/height/shading tuning,
weathered-metal material, stable-order live build. Full orbitable 3D deferred.

**Step 3a.5 — urban visual pass + interaction fixes (D15): ☑ DONE (v1).** Ported
the SVG-prototyped **"dark-but-breathing blend"** look into the app: ground
**plinth** (city no longer floats), per-block **ground shadows**, night palette
with **neon-glow tops** (F_TOP 1.12) + deep sides, `min_tile 7`/`padding 1.2`
density, taller towers (height 4+72·ln(file_count)). Plus three fixes:
**full-block hover** (hit-test all 3 faces, not just the top), **selection
highlight drawn in-order** so nearer buildings occlude it, and **right-click →
Reveal in Explorer** (`explorer /select,<path>` files / `explorer <path>`
folders; cfg-gated to Windows). Prototype presets in
`sector-scan/examples/iso_svg.rs` (dusk/kowloon/blend). **All verified on
Windows.** Two follow-up fixes also verified: tooltip is **hand-drawn at the
cursor** (egui's widget tooltip anchored to the huge panel's corner), and the
canvas uses an **explicit stable widget id** (`ui.interact`, not
`allocate_painter`) so the right-click context menu stays open instead of
flashing shut.

**Step 3b — custom wgpu instanced draw: ☐** (the original Step 3 below) — only if
we later want true orbitable 3D or tile counts beyond the painter's comfort.

- One draw call for a very large number of rects; smooth pan / zoom / hover.
- **Live discovery build (signature — D12):** the map builds visibly as the
  scanner streams nodes in, defrag-*feeling* in a modern style. Choose the layout
  technique here (growth-then-crystallize vs. reserve-and-fill) to avoid treemap
  "boiling". Keep already-discovered regions explorable and offer a skip path.
- **First pass at the look (D13):** metal/chrome tiles via a per-tile SDF shader
  (specular/beveled edges) — this is where the visual identity starts to show.
- **Purpose:** the moment it starts looking like the product.
- **Done when:** a scanned drive renders as a treemap that pans and zooms
  smoothly, and builds visibly during a scan without thrashing.
- **Benchmark caveat:** debug builds load the **D3D12 debug layer**
  (`D3D12SDKLayers.dll`), which is meaningfully slower. Benchmark rendering in
  `--release`; don't misread debug-layer overhead as a treemap perf problem.

## Step 4 — Interaction + polish  ◐

*The interaction half landed in Step 3a; what remains is the visual-identity
and animation polish below.*

- ☑ Drill-down / zoom into folders; breadcrumb of the current path.
- ☑ Hover tooltips (name, size, item count).
- ☑ Color by file type (industrial palette per D13: steel/iron/brass,
  sparing hazard accent). Primary channels: size = area, type = hue.
- The full visual-identity pass (D13) — **weathered lit-metal surfaces** (matcap
  sheen + procedural noise + SDF bevel), industrial "machine shop" palette,
  mechanical motion, gauge/stencil type, HUD readouts, tabular size figures.
- **Prototype wear-encodes-age (D12/D13):** surface weathering maps to file
  age/coldness (old/cold = oxidised, hot/new = freshly machined) — subject to the
  legibility rule (must not fight size/hue).
- Polish the discovery animation (D12) — the "stamped/machined into place"
  motion, survey sweeps, crystallize transition.
- **Done when:** it feels like a real, pleasant tool to explore a drive with.

---

## Later / maybe (parking lot)

Deliberately *not* scheduled — captured so they don't distract us:

- **"Complete" mode — scan all drives at once.** Enumerate every fixed + mapped
  drive and show them together (a metro area of cities, or a drive picker /
  combined root). Fits D9 (multiple drives are a normal case). User idea, 2026-09.
- ~~**Urban visual pass (D15).**~~ **Done** — see Step 3a.5 (plinth, shadows,
  night palette, neon tops). Remaining: haze / atmosphere, ambient occlusion.
- **Camera controls (later).** Much is achievable *within* 2.5D, no engine
  rebuild: pan (offset), zoom (scale), and **turntable orbit** (rotate the
  footprint x,y about center *before* the fixed iso projection) + tilt
  (parameterize projection). Only a free-flying *perspective* camera (fly-through,
  vanishing point) needs the true-3D wgpu upgrade (Step 3b). User idea, 2026-09.
- ~~**Reveal in Explorer (small, near-term; first D8 action).**~~ **Done**
  (Step 3a.5 in the City; E3 in Files). Kept for the original reasoning: Right-click a
  block → context menu → open its location in Windows Explorer (`explorer
  /select,<path>` for files, `explorer <path>` for folders). Left-click stays
  drill-down. cfg-gated to Windows. Extensible menu (later: copy path, open,
  delete-to-Recycle-Bin). User idea, 2026-09.
- ~~**File management (candidate future direction).**~~ **Superseded by D16 /
  Phase E** — it is now the main road (E3–E5 shipped). Original note: growing
  from a pure visualizer toward light file actions — "reveal in Explorer", delete to Recycle
  Bin, and potentially fuller browse/copy/move/rename. Not planned now, but a
  door we keep open (see README non-goals). AI/assistant features are *not* in
  this parking lot — they are a firm permanent non-goal.
- **Adjustable text size** (user request, 2026-09-05). egui's Ctrl+= / Ctrl+-
  / Ctrl+0 zoom already scales the whole UI (persisted). The proper item is a
  text-size setting independent of zoom — scale the style's text sizes, expose
  it somewhere discoverable, persist it with the other settings.
- **Dual-pane explorer** (Midnight Commander style; user idea, 2026-09-05).
  Two independent panes (location, listing, selection, cursor, filter, sort,
  history each), sharing the clipboard, undo stack and background jobs; Tab
  switches sides; copy/move/drag between them. **Prerequisite ✅ (2026-09-05):**
  the per-pane state now lives in a `Pane` struct (`SectorApp::pane`, one for
  now); the tree/list focus enum became `Focus`; the pane's logic lives on
  `impl Pane` (app wrappers apply the shared side effects); the list renders
  via `show_pane_list` with pane-relative widget ids. Remaining: a second
  `Pane`, an active index, Tab between panes, F5/F6 to the other pane, and
  the City following the *active* pane (D18 generalised).
- **FSN-style City** (SGI File System Navigator; user idea, 2026-09-05). A
  free perspective fly-through: folders as plinths connected to their
  children, files as blocks on them. Attaches to Step 3b (true-3D wgpu +
  camera) and the camera-controls item above — FSN is the visual reference.
- Multiple / all-drives view.
- Saved scans, scan history, diff between two scans.
- Filters (by type, age, size threshold).
- Export (image / CSV).

---

## Phase E — File Explorer pivot (D16, 2026-09-02)

SECTOR becomes a **file explorer first, visualizer second.** Conventional layout
(address bar + back/up, folder-tree sidebar, content area) with a **List / City**
toggle. Live per-folder navigation is the new spine; the cityscape (Steps 0–4)
becomes the "City" mode. Built incrementally on the working visualizer,
safest-first — destructive ops must be bulletproof (see D16, D1).

- **E1 — Live navigation + list view** (read-only skeleton): browse folders
  live (`read_dir` the current folder), a details list (name, size, type, date),
  sortable; address bar, back/forward/up, folder-tree sidebar. *This is the new
  default view.* — **E1a ✅** (data layer) · **E1b.1 ✅** (shell + list) ·
  **E1b.2 ✅** (folder-tree sidebar — Files view only, per D19; collapsible,
  lazy-loaded, drive roots, auto-reveals the current path).
- **E2 — List / City toggle  ✅**: one shared location (`current_dir`) drives both
  views; the City visualizes the folder you're browsing — instant cache-load if
  present, else a scan-prompt; drilling in the City syncs back to Files. See D18.
  **Within-tree re-root ✅** (2026-09-05): navigating to a folder inside the
  loaded tree (address bar, Up, Back, the Files tree) re-roots the City there
  instantly instead of reloading; the City's own breadcrumb is gone.
  **Auto-scan local ✅** (D20): a local, non-root folder with no cityscape is
  scanned on entry; network drives and drive roots still prompt.
- **E3 — Read-only actions  ✅**: double-click / Enter opens (default app for
  files, in-app navigate for folders), right-click menu (Open · Reveal/Open in
  Explorer · Copy path · Copy name), Backspace = up. **Properties/Details panel ✅** (in-app,
  `Alt+Enter` / toolbar toggle: path, sizes, dates, attributes).
- **E4 — Non-destructive edits  ✅**: New folder (Ctrl+Shift+N / ＋ button /
  context menu) and Rename (F2 / context menu), via a validated modal dialog
  (empty / invalid-char / reserved / duplicate checks; OS errors shown in-dialog,
  which stays open). Refreshes the listing + tree and re-selects the affected
  item afterwards.
- **E5 — Destructive ops (careful)  ✅**: **cut/copy/paste** — background copy/move
  (Ctrl+C/X/V + context menu), never overwrites (auto-renames to "- Copy"), a cut
  source is deleted only after its copy fully succeeds, cross-volume move falls
  back to copy-then-delete, folder-into-itself rejected, partial results cleaned
  up on cancel/failure; links/junctions skipped. **Multi-select** (click/Ctrl/
  Shift, Ctrl+A) drives batch copy/cut/delete. **Delete → Recycle Bin** (Del /
  context menu) via the `trash` crate (IFileOperation), with a confirmation and a
  background worker. *Shift+Delete permanent-delete intentionally not offered.*
  **System clipboard ✅** — Cut/Copy write `CF_HDROP` + "Preferred DropEffect"
  to the Windows clipboard and the app mirrors it (via the clipboard sequence
  number), so it interoperates with Explorer in both directions.
  **Undo ✅** (Ctrl+Z): a session undo stack of rename / new folder / paste
  (per-item records, so a partial paste undoes what actually happened) /
  delete (restored from the Recycle Bin via `trash::os_limited`). Reversals run
  on the background worker and never overwrite. **Redo ✅** (Ctrl+Y /
  Ctrl+Shift+Z): every reversal yields its exact inverse (from where things
  actually landed), which goes on the opposite stack; a new operation clears
  redo.
  **Inbound drag-and-drop ✅** — files dropped on the window (from Explorer or
  any app) are copied into the current folder via the paste engine, with a
  full-window drop overlay. Copy only: the OS drag owns the keyboard, and
  winit reports no drop position, so it's the current folder rather than a
  row. **Internal drag-and-drop ✅** — rows drag the selection (egui's
  payload API); folder rows and tree nodes accept drops: move, or copy with
  Ctrl; refuses drops onto/inside a dragged item or back into its own folder.
  *Outbound drag (to Explorer) needs OLE and is not done.*
- **E6 — Incremental freshness (D17)  ◐**: **slice 1 ✅** — the cache stores a
  USN watermark (journal id + next USN) at scan completion and, on reload,
  reports the cached view as Current / Stale / Unknown (`sector-scan::usn`).
  **Still open:** apply the deltas. **local NTFS** — store last USN, read
  the USN Change Journal on reload and apply deltas to the cached tree (near-
  instant "update"); build alongside the local MFT fast-path (Step 1d). **NAS** —
  no cheap incremental (no MFT/USN over SMB); refresh = re-walk, cached. Optional:
  live watching (`ReadDirectoryChangesW`/`notify`) to keep the open session fresh.
