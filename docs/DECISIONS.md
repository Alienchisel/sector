# SECTOR — Decisions

Short records of the choices we've made and *why*, so we don't re-argue settled
questions. Newest at the bottom. Format: what we decided, the alternatives, and
the reasoning. Revisit an entry only with a new entry that supersedes it.

---

## D1 — Windows-only, native desktop

**Decided:** Target Windows exclusively, as a native desktop app.

**Why:** The best version of this tool is fast *because* it exploits Windows
internals (the NTFS MFT). A cross-platform scan would default to the slow,
lowest-common-denominator directory walk and throw away the main advantage.
Focusing lets us go deep instead of wide.

**Cost accepted:** No macOS/Linux, no web. Revisiting means rearchitecting the
scanner.

## D2 — Inspiration, not template: makepad's `mpfiles`

**Decided:** Use makepad's `apps/mpfiles` (a native Rust file manager, `work`
branch) as *design inspiration* — its treemap, previews, and sleek panels — but
not as a code base or dependency source.

**Why:** `mpfiles` is a full file manager and an internal proving ground for the
makepad framework; it depends on unpublished in-repo libs (`mp-theme`,
`mp-wm-api`) and an in-process local LLM. Most of that is scope we explicitly
don't want (see README non-goals). We take the *ideas*, not the *baggage*.

## D3 — UI toolkit: egui (not Makepad)

**Decided:** Build the UI with **egui** (`eframe`), not Makepad, despite Makepad
being the original inspiration.

**Why:**
- **Windows-only removes Makepad's main edge.** Makepad's headline advantage is
  one codebase across native + web + mobile. We're Windows-only, so we'd pay its
  immaturity/thin-docs tax without using its cross-platform payoff.
- **Development velocity / correctness.** Claude is doing the code heavy-lifting
  and the user runs the builds. egui is well-documented and heavily represented
  in training data, so first-try correctness is high. Makepad's sparse, churning
  docs would push us into a slow, iteration-hungry compile loop that leans harder
  on the user — a poor fit for how we work.
- **Fit for the task.** A visualizer is a small, contained widget surface plus
  one big custom canvas. egui excels at the former and lets us drop to raw wgpu
  for the latter (see D4).

**Alternatives considered:** Makepad (highest visual ceiling, but the costs
above); Slint (polished and better-documented than Makepad, but weaker for a huge
*custom* treemap canvas — its sweet spot is declarative standard widgets).

**Cost accepted:** egui's default look is "clean tool," not "designed." We invest
in Step 4 styling and a custom treemap to reach "sleek."

## D4 — Treemap rendered via custom wgpu, inside egui

**Decided:** Render the treemap itself with a **custom `wgpu` instanced draw** in
an egui paint callback, rather than as thousands of egui shapes.

**Why:** A treemap of a full drive can be millions of rectangles. Immediate-mode
per-shape drawing won't stay smooth at that scale; GPU instancing draws them in
essentially one call and keeps pan/zoom fluid. egui handles the chrome
(toolbar, breadcrumb, panels); wgpu handles the heavy canvas.

## D5 — Scanner: read the NTFS MFT/USN directly

**Decided:** Enumerate **local NTFS** volumes by reading the **Master File Table
/ USN** directly (`DeviceIoControl` + `FSCTL_ENUM_USN_DATA`). Use a **highly
concurrent directory walk** for everything else — see D7 for routing.

**Why:** This is the WizTree trick and the single biggest lever on perceived
speed *for local disks*. Per-directory syscalls over millions of files take
minutes; reading the MFT takes seconds.

**Important limit:** the MFT path is **local-only**. It requires a raw handle to
the physical NTFS volume, which does not exist for a network drive — you cannot
read a NAS's MFT across SMB. So the MFT trick does nothing for the user's NAS
drives; those depend entirely on the walk (D7).

**Data structure:** store the result as an **index-based arena tree** (nodes hold
indices, not `Rc`/pointers) so millions of entries stay cache-friendly and cheap
to aggregate and traverse.

## D6 — Working model: Claude writes, user builds

**Decided:** Claude does the code heavy-lifting; the user compiles and runs on
their Windows machine and reports back errors/results.

**Why:** Claude's environment can't compile or run a Windows/wgpu GUI. The
compile-feedback loop is the user. Implication: work in tight, verifiable
increments and prefer APIs with high first-try correctness (reinforces D3).

## D7 — Route scanning by drive type; network drives (NAS) are first-class

**Decided:** At scan time, classify each drive with **`GetDriveType()`** and
route it: `DRIVE_FIXED` local NTFS → MFT path (D5); `DRIVE_REMOTE` (mapped
network drives / NAS) and any other non-MFT volume → a **highly concurrent
directory walk**. The network walk is a **first-class, well-optimized path**, not
a degraded fallback.

**Why:** The user keeps most of their data on two NAS devices exposed as mapped
network drives with drive letters. Those cannot use the MFT trick at all (see
D5), so their entire experience rides on the walk. A naive single-threaded
`readdir` over SMB is dominated by per-request latency and would feel slow, so
the walk must **fan out many concurrent directory/stat requests** to saturate the
link and hide latency.

**Consequences:**
- **Progress + cancellation are requirements,** not polish — network scans can be
  long (see ROADMAP Step 1).
- The concurrency level likely wants to be **tunable / adaptive** (a good value
  for SMB is very different from a good value for a local SSD walk).
  - **Empirical (1b, vs the real NAS over SMB):** throughput scales steeply from
    1→16 workers and plateaus by 16–32 (64 ≈ 32, sometimes slightly worse).
    ~5–6× faster than single-threaded. So a **default of ~16–32 workers for
    network drives** is well-justified; local drives (not latency-bound) will
    want far fewer. Measured warm-cache, so cold scans should favor concurrency
    even more. Revisit with a larger/colder dataset and once local scanning
    exists to set that default.
- Watch for **network-specific correctness**: disconnects mid-scan, permission
  errors on some shares, reparse points / symlinks and NAS-side dedup or hard
  links that could double-count size. Handle these gracefully rather than
  aborting the whole scan.
- The **admin/elevation question (open)** matters less for the NAS use case:
  elevation buys the MFT path, which network drives can't use anyway. It still
  matters for scanning local system drives fully.

## D8 — No AI integration (permanent); file management is a deferred maybe

**Decided:** Two scope calls, opposite in firmness:
- **No AI / assistant integration, ever.** No embedded LLM, chat panel, or
  agent that acts on files. This is a firm permanent exclusion.
- **File management is a candidate future direction,** not a current goal and not
  a permanent no. SECTOR starts as a pure visualizer; growing toward light file
  actions (reveal in Explorer, delete to Recycle Bin, eventually fuller browse/
  operations) stays open.

**Why:** The mpfiles inspiration bundled a full file manager *and* a local-LLM
agent. The user has no interest in the AI facet — it adds heavy dependencies
(in-process model, GGUF weights), footprint, and complexity for zero desired
value here. File management, by contrast, is a plausible organic evolution of a
tool people already use to *find* the big/old files they then want to act on.

**How this steers design now:** Don't bake in AI hooks. *Do* keep the scanner and
data model general enough that per-node file actions could be added later — e.g.
retain real paths and node identity so a future "delete this / open this" is a
small addition, not a rewrite. Avoid a visualization-only model that discards the
information a file action would need.

## D9 — Local drives and NAS are co-equal first-class citizens

**Decided:** Both **local drives** and **mapped network drives (NAS)** are
first-class targets, given equal care. Neither is a second-class afterthought.

**Why:** The user's collections live on the NAS, so NAS support is essential —
but their regular drives matter too and must not feel like a degraded mode. There
is no single "primary" drive type to optimize for at the expense of the other;
both must be excellent.

**Principle — the scan source is invisible to the UI:** the two scan strategies
(local MFT per D5, concurrent SMB walk per D7) are an *implementation detail*.
Both produce the **same arena tree** and feed the **same treemap/interaction
layer**, so the experience — navigation, drill-down, size accuracy, polish — is
identical regardless of where the data came from. The one legitimate difference a
user may notice is **scan speed** (SMB latency is physics, not us cutting
corners), which is exactly why progress + cancellation (Step 1) apply to both
paths, not just the network one.

**Consequence for the UI:** SECTOR must handle **multiple drives** as a normal
case — a clear way to pick which drive/root to scan and to move between local and
network drives — rather than assuming a single system drive.

## D10 — Report apparent size (provisional)

**Decided:** SECTOR reports **apparent size** — the straightforward sum of file
sizes up the tree — and does not attempt dedup-, hard-link-, sparse-, or
snapshot-aware "actual on-disk" accounting.

**Why:** Actual-size accounting across SMB to a NAS (which may dedup or use
snapshots) is near-impossible to do reliably, and apparent size is what users
intuitively expect from a "where did my space go" view. Simple and honest beats
subtly-wrong-but-clever.

**Revisit if:** double-counting on the NAS turns out to be visibly misleading, or
we later care about matching the drive's reported free space exactly. **Not set
in stone.**

## D11 — Run unprivileged by default (provisional)

**Decided:** SECTOR runs **without elevation by default** — no UAC prompt on
launch. The fast MFT path (D5), which needs admin, is treated as an *opportunistic
bonus* when SECTOR happens to be run elevated, not a requirement.

**Why:** The user's core use case is the NAS, which can't use the MFT path
anyway (D7), so elevation buys little for it while adding first-run friction (UAC
+ SmartScreen). Unprivileged-by-default gives a clean launch for the common case;
local drives still scan fine via the walk, just not MFT-fast.

**Open detail for later:** how to *offer* the fast path — e.g. a "scan faster
(needs admin)" affordance that relaunches elevated for local NTFS drives — vs.
just silently using MFT when already elevated. **Provisional; revisit when we
build the local scanner.**

## D12 — Signature: discovery-as-spectacle (the defrag *feeling*)

**Decided:** Turn the scan — especially a slow NAS scan — from dead time into
SECTOR's signature moment. As the streaming scanner (D7) discovers the tree, the
map **builds visibly**: territory fills in, big files land as big tiles, dense
folders stipple in as fine texture. The reference feeling is the old Win9x
defragmenter — hypnotic, satisfying, *worth watching* — rendered in a modern
style (see D13), **not** as retro pastiche.

**Why:** The NAS scan is unavoidably slow (SMB latency, D7). Rather than hide it
behind a spinner, we make it the thing people enjoy. It's also a clean fit for
the stack: the concurrent walk already *streams* nodes, egui is immediate-mode
(continuous animation is natural), and wgpu instancing eats the churn. The
weakness and the delight are the same mechanism seen from two ends.

**The hard problem to respect:** live treemaps are **unstable** — re-laying-out
every frame as sizes stream in causes nauseating "boiling" (rectangles thrashing
and swapping). Getting this right is what separates charming from seizure-
inducing. Favored approaches (decide during Step 3):
- **Growth-then-crystallize:** during scan show a *stable* discovery view (a
  filling-in field/territory that is honest about being a progress metaphor,
  not an accurate treemap), then **morph** into the real explorable treemap when
  it settles.
- **Reserve-and-fill:** lay out stable top-level territory from cheap early
  counts, then resolve each region's interior detail as its walk completes —
  boundaries hold still, interiors survey in.
(An always-accurate stable/ordered treemap with tweened transitions is the more
ambitious alternative if the above feel too "fake.")

**Non-negotiables:**
- **Never lock the user into the animation.** Already-discovered regions stay
  explorable while the rest fills in, and there's a skip / "just show me the
  result" path — charming on run 1, never tedious on run 50.
- **The show never slows the scan.** Scan threads run flat out; the render reads
  a snapshot each frame. Decoupled.
- **Aesthetic, not physical.** Our tiles/cells are an evocative fiction, not a
  map of physical disk clusters (we have no physical layout, least of all over
  SMB). Be deliberate about not implying a layout we don't have.

**Candidate — wear encodes age:** as the map settles, let **surface weathering
carry file age/coldness** (old/cold = oxidised & patinated; hot/new = freshly
machined). Makes the industrial material (D13) do informational work and fits the
"production line surveying territory" framing. Prototype in the visual pass
(Step 4); see D13.

**Status:** a design *direction*, molten in the details. Locks in the "what makes
SECTOR distinctive" question; the exact layout technique is chosen when we build
Step 3.

## D13 — Visual identity: weathered industrial metal, "industrial production" as the governing metaphor

**Supersedes the original D13** ("metal / chrome / slightly futuristic"). The
governing idea is unchanged — SECTOR is a precision *instrument*, engineered and
weighty, not a clean egui tool dialog — but the surface treatment shifts from
**glossy chrome/reflections** to **weathered, matte, lit industrial metal**, for
reasons that are both technical and aesthetic.

**Why the shift away from chrome:**
- **Reflections don't survive the treemap.** Real chrome needs an environment to
  reflect and updates every frame; on a field of thousands of small tiles that
  pan/zoom/rebuild live, reflections **shimmer, alias, and fight legibility**
  (the tile's job is to convey size + type, not dazzle). Chrome flatters one hero
  object and turns a field of many into visual noise.
- **Weathered matte metal is cheaper AND calmer at scale**, and it reads as
  *engineered/serious* rather than *showroom/consumer*. The cheaper path is also
  the more on-brand one.

**Governing metaphor — industrial *production* (foundry / machine shop):** this
unifies the look (D13) with the motion (D12). The scan is a **production line**:
territory surveyed, tiles **stamped / cast / machined** into place as the scanner
streams them in — the defrag-*feeling* rendered as a factory floor filling up.
Compass for future "should it look like X?" questions: *what would this be on a
factory floor?*

**Direction (molten — current thinking, not a spec):**
- **Surfaces & tiles (the key change):** treemap tiles as **lit, weathered metal
  panels** — SDF rounded-rect + **beveled edge** for the machined-panel 3D read;
  a **matcap** (sampling a small pre-baked lit-metal sphere by the bevel normal)
  for a convincing metallic *sheen* **without any real reflection**; and
  **procedural noise** (fbm / Voronoi in-shader) for brushed grain, cast-iron
  pitting, worn paint, patina. All cheap in a wgpu fragment shader, resolution-
  independent, and anti-aliasing-friendly — reinforces D4.
- **"Well-used machine shop," not "abandoned foundry":** weathered but
  *maintained* — brushed steel, cast iron, oil-darkened metal, anodized
  aluminium, brass/copper. Avoid literal decay reading as "broken" (but see the
  wear-as-age idea below, which turns wear into a signal).
- **Palette:** steel greys, iron, oil-darkened metal, with brass/copper warmth;
  a **sparing hazard accent** (caution-amber / signal-red) for focus/selection —
  peak-industrial *and* maximally legible for a HUD accent. Restraint over
  rainbow.
- **Chrome demoted to a rare accent:** one glossy element (a selected tile's
  edge, a HUD line) popping against the matte field — never the field itself.
- **Motion:** precise, weighty, mechanical — tiles *machined/stamped* into place,
  survey/scanline sweeps — not bouncy or organic.
- **Type & UI chrome:** technical/stencil or precise mono, **gauge/dial
  readouts**, tabular size figures, thin instrument lines. Panels around the
  treemap read like machine-tool control surfaces.

**Legibility discipline (hard rule):** the primary data channels are **size =
tile area** and **type = hue**. Material/weathering is a *third, subtle* layer
that must never muddy them. Texture earns its pixels only if it stays out of the
way of the signal (or carries signal itself — see wear-as-age).

**Candidate idea — wear encodes age (cross-refs D12 / Step 4):** map **surface
weathering to file age / coldness** — old, untouched, cold data looks oxidised
and patinated; recently-touched, hot data looks freshly machined. This turns the
"rust" from decoration into a genuine data channel and neutralises the
decay-reads-as-broken risk. Subject to the legibility rule above (must not fight
size/hue). Prototype during the visual pass (Step 4); noted also in D12.

**Status:** direction re-aimed, execution molten. Revisit freely as we prototype
the look in Steps 3–4.

## D14 — 2.5D isometric "cityscape" render (fixed-angle, not full 3D)

**Decided:** Render the treemap as an **isometric extruded cityscape** — each leaf
block has height, drawn with three shaded faces (top lit, sides darker) at a
**fixed dimetric camera angle**, using egui's painter (convex polygons). *Not* a
real orbitable 3D engine.

**Encoding:** area = bytes (treemap), **height = file count** (log-scaled, a live
`Node::file_count`), color = dominant file type (D13 palette). So the third
dimension *informs* ("folder crammed with many files" = a tower), not just
decorates.

**Why fixed-angle 2.5D over true 3D:** ~90% of the visual impact for a fraction of
the effort/risk — painter polygons, no camera/lighting/depth-buffer engine, no
WGSL. Prototyped as SVG on Linux first (see `sector-scan/examples/iso_svg.rs`),
confirmed the look, then ported to the app. Full orbitable 3D (wgpu) remains a
clean future upgrade if wanted.

**Cost accepted:** iso perspective distorts area comparison and occludes small
blocks behind tall ones — a known treemap-in-3D tradeoff (beauty over strict
clarity). We keep drill-down + hover so exact values stay one interaction away.

**Inspiration, not emulation:** sparked by makepad mpfiles' 3D view, but our look
is our own (industrial type-colors, not their anodized purple).

**Status:** v1 shipped (replaces the flat 2D render). Refinements open: tune
angle/height/shading, per-face lighting, the weathered-metal material (D13),
smoother live-build (stable-order).

## D15 — Aesthetic evolves toward urbanism (an "industrial city")

**Decided:** With the 2.5D cityscape (D14), the governing metaphor grows from pure
industrial machinery (D13) toward **urbanism** — the render *is* a city, so we
lean into urban visual language. The two reconcile as an **industrial city**:
industrial *materials*, urban *form*.

**What the urban lens gives us (design vocabulary, molten):**
- **Districts / zoning:** color-by-type already reads as city zones (a video
  quarter, an image district). Lean in.
- **Skyline:** height = file count already makes a skyline; downtown = folders
  dense with files.
- **Streets:** the padding gaps between blocks read as streets — emphasize with
  darker "street level" / ambient occlusion at block bases.
- **Grounding:** a base plinth/ground slab so the city sits on something.
- **Mood:** a city-at-dusk feel — dark ground, lit building faces, distant
  atmospheric haze for depth; maybe faint window texture on tall towers.

**Why:** The form the user responded to is urban, not machine-shop. Urbanism is a
richer, more coherent language for a 3D block city, and it keeps the industrial
material palette. Cheap near-term wins (plinth, street shadows, atmosphere) are
painter-level, no engine change.

**Reference — Kowloon Walled City (user, 2026-09):** the mood north star. A
dense, ungoverned megastructure — extreme density (no streets, one solid mass),
jury-rigged capped verticality, weathered accreted concrete/rust, dark sunless
atmosphere, neon in the gloom; the root of the cyberpunk city aesthetic. It
*reconciles* industrial (weathered/grimy) + urban (a city): **the filesystem as a
weathered data-megacity.** Dials it suggests: crank padding DOWN (density over
spacing), palette toward NIGHT (type-colors as neon glowing from a dark mass,
haze, deep crevice shadow), heavy weathering (D13 material), emphasized
verticality.

**Tension to respect:** SECTOR is a data viz — legibility ("read where the space
went at a glance") must survive. So treat KWC as a **skin/atmosphere/density
inspiration layered on the ordered treemap**, not literal labyrinthine chaos. The
structure stays; the material and mood go full Kowloon. Plan: mock a KWC-leaning
variant (dense, dark, neon-in-gloom) *alongside* a cleaner dusk-city variant to
compare.

**Status:** direction, molten. Supersedes D13's framing as the *governing*
metaphor while keeping D13's palette/material ideas. Prototype refinements in the
visual pass.

## D16 — Pivot: file explorer first, visualizer second

**Decided (2026-09-02):** SECTOR becomes a **graphical file explorer first, and a
visualizer second** — reversing D8's "not a file manager". After using the
visualizer, the user wants day-to-day file browsing as the primary job, with the
cityscape as a killer secondary *mode*. (AI integration stays permanently out,
per D8.)

**Layout model (chosen):** a **conventional explorer** — address bar + back/up,
a folder-tree sidebar, and a central content area — with a **List / City toggle**.
Explorer is home base; the cityscape is a lens you switch to for the current
folder/drive.

**The core architectural shift — snapshot → live:**
- What we built is a *snapshot* tool: scan a whole drive once, hold the tree,
  explore the frozen picture. Stays as the "City" mode (reusing scanner + arena
  tree + treemap + cache).
- An explorer is *live*: browse folder-by-folder, reading each folder's *current*
  contents on demand (fast per-folder `read_dir`), and act on files. **Live
  per-folder navigation becomes the primary spine.**

**Scope reality + strategy (safest-first):** a file explorer is a much bigger,
higher-stakes build than a read-only visualizer — destructive ops (delete, move,
overwrite) must be bulletproof (Recycle Bin, confirmations, conflict + permission
handling — the Windows specifics from D1). So we **grow the explorer incrementally
on top of the working visualizer**, never destabilizing it:
- **E1** live navigation + a file **list/details view** (read-only skeleton)
- **E2** List / **City mode** toggle (wraps today's cityscape)
- **E3** read-only actions: open, reveal, copy-path, properties
- **E4** non-destructive edits: new folder, rename
- **E5** destructive ops last, carefully: delete-to-Recycle-Bin, copy/move with
  conflict handling, cut/paste

**Supersedes:** D8's "not a file manager" non-goal (that door is now the main
road). The README non-goal is updated accordingly. D8's *no-AI* rule still holds.

## D17 — Freshness / incremental-update strategy (splits by drive type)

**Decided (2026-09-02):** Keep data current with minimal scanning via a
drive-type-aware strategy, since the network genuinely denies us cheap hooks.

- **Browsing is live** (E1a): navigating a folder `read_dir`s it right then —
  always current, no cache, no staleness. The pivot to explorer means day-to-day
  freshness is free.
- **City view (whole-tree viz)** uses the cache: **auto-load** it on opening a
  drive (instant big-picture) with an explicit **Rescan** to refresh. Some
  staleness between deliberate rescans is acceptable there.
- **Local NTFS incremental — the good case:** store the last **USN** in the cache;
  on reload, read the **USN Change Journal** (`FSCTL_READ_USN_JOURNAL`) for all
  changes since, apply the deltas to the cached tree, re-cache. Near-instant,
  precise. Build this *alongside* the local MFT fast-path (Step 1d).
- **NAS / SMB — no cheap magic (honest limit):** no MFT and no USN journal over
  SMB. Directory-mtime diffing is rejected — it still stats every dir (~278k SMB
  round-trips), can't skip subtrees, and **misses in-place file growth** (a file's
  content change bumps the *file's* mtime, not the *directory's*), which is
  disqualifying for a size viz. So NAS City-view refresh = **re-walk** (made fast,
  cached so it's rarely paid).
- **Live watching (optional, complementary):** while the app runs, watch for
  changes (`ReadDirectoryChangesW` / `notify`) and patch the tree in real time —
  works for local, limited/lossy over SMB, session-only. Nice-to-have, not the
  primary mechanism.

## D18 — One shared location; the City visualizes the current folder (E2)

**Decision.** Files and City are two views of a *single* location, `current_dir`,
driven by one shared nav strip (mode toggle + back/forward/up + address bar). The
City always visualizes **the folder you're browsing**, not a separately-typed path.

- **Files → City.** Switching to City *reconciles* to `current_dir`: instant
  **cache-load** if a scan of that folder exists, otherwise a **scan-prompt**
  (a deep scan stays deliberate — never auto-fired on a toggle). Caches are keyed
  per folder, so each visited folder reopens instantly once scanned.
- **City → Files.** Drilling in the City (clicking a block / breadcrumb) writes
  the drilled folder back into `current_dir`, so toggling to Files lands there.
- **No re-scan on internal drill.** `city_synced_dir` tracks what the City is
  showing; the reconcile fires only when `current_dir` drifts from it (i.e. an
  *external* navigation), never for in-City drilling.

**Why.** This is what "explorer first, visualizer second" (D16) means in practice:
the visualization is a *lens on where you are*, not a separate tool with its own
address. It also removes the confusing dual-path state (Files at `C:\`, City at
`Y:\`) that the two-independent-views design produced.

**Deferred.** Bidirectional *within-tree* reuse — navigating into a subfolder via
Files then toggling to City currently re-loads that folder's own cache rather than
re-rooting the already-loaded parent tree. Correct and instant (per-folder cache),
just not maximally clever; a find-node-by-path optimization can come later.

## D19 — Layout: tree in Files only, City full-bleed

**Decision.** The folder-tree sidebar (E1b.2) lives **only in the Files view**.
The City view is **full-bleed** — no sidebar — and you navigate it in its own
idiom (click a district to drill, breadcrumb to climb, address bar to jump).

**Why.** *Tree = navigation; the Files/City toggle = representation.* They're
orthogonal, so the tree doesn't make either content pane redundant — you still
need one pane to draw the folder, and list-vs-city answer different questions:

- **Files (list)** = *"what exactly is in here?"* — names, exact sizes, dates,
  sortable, selectable; the surface you **work on** (Open / rename / delete).
- **City** = *"where did the space and file-count go — what's the shape?"* — the
  surface you **read**.

Bolting a tree onto the City would make it a list-view's chrome wrapped around a
picture, instead of a committed spatial map. Keeping the tree with the working
view gives each mode a crisp reason to exist: **Files is for working, City is for
seeing.** ("for now" — a later split/side-by-side layout isn't foreclosed.)
