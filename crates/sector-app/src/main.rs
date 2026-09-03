//! SECTOR — interactive treemap with a live "discovery" build (D12).
//!
//! The scan runs on a background thread and writes into a shared tree; the UI
//! reads that same tree and renders the treemap *as it grows*, so a long NAS
//! scan fills in before your eyes instead of leaving a blank panel. When the
//! scan finishes it "crystallizes" into the final layout. Drill down by clicking
//! a folder, navigate with the breadcrumb, hover for name/size.
//!
//! Anti-"boiling" measure (v1): during a scan the layout is recomputed at most
//! every `RELAYOUT_THROTTLE`, so the map settles in visible steps rather than
//! churning every frame. (Stable-order layout + tweening are a later refinement.)

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui;
use egui::{Color32, Pos2, Rect, Sense, Shape, Stroke, Vec2};

use sector_core::{
    categorize, human_size, layout, layout_partial, CacheStats, FileCategory, LayoutOptions,
    NodeId, NodeKind, Tile, Tree,
};
use sector_scan::{
    freshness as usn_freshness, list_dir, query_mark, scan_into, Entry, Freshness, Progress,
    ScanOptions, ScanStats, UsnMark,
};

/// Which Files-view pane the keyboard drives (focus-follows-click).
#[derive(Clone, Copy, PartialEq)]
enum Pane {
    Tree,
    List,
}

/// Which view the content area shows.
#[derive(Clone, Copy, PartialEq)]
enum View {
    /// The file-explorer list (the new default; live navigation).
    List,
    /// The cityscape visualization (the original tool, now a mode).
    City,
}

/// Clipboard contents for cut/copy/paste (E5). `cut` = move on paste; otherwise
/// copy. Holds paths (a Vec for future multi-select; currently one).
struct Clipboard {
    paths: Vec<PathBuf>,
    cut: bool,
}

/// A paste running on a background thread.
struct PasteJob {
    rx: Receiver<Result<String, String>>,
    cancel: Arc<AtomicBool>,
    cut: bool,
    /// Short description for the status line (e.g. "Copying “movie.mkv”").
    desc: String,
}

/// A pending name-entry dialog (E4): create a folder, or rename an entry.
enum PromptKind {
    NewFolder,
    Rename { orig: String },
}

struct NamePrompt {
    kind: PromptKind,
    buf: String,
    error: Option<String>,
    /// Request keyboard focus for the text field on the next frame.
    focus: bool,
}

/// Characters Windows forbids in a file/folder name.
const INVALID_NAME_CHARS: &[char] = &['\\', '/', ':', '*', '?', '"', '<', '>', '|'];

/// File-list sort column.
#[derive(Clone, Copy, PartialEq)]
enum SortKey {
    Name,
    Size,
    Kind,
    Modified,
}

/// A short human "kind" for a listing entry (a `&'static str`, so sorting and
/// drawing the Type column don't allocate).
fn entry_kind(e: &Entry) -> &'static str {
    if e.is_dir {
        "Folder"
    } else {
        categorize(&e.name).label()
    }
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Group digits with thousands separators: 278589 → "278,589".
fn commas(n: u64) -> String {
    let s = n.to_string();
    let len = s.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Join path components cleanly (trim any trailing separator on each, e.g. the
/// drive root "Y:\\"), avoiding the doubled `\\` in displayed paths.
/// Split a path into clickable breadcrumb segments: `(label, navigable path)`,
/// root first. Drive-letter paths (`C:\Users\Docs`) — a bare drive segment
/// navigates to the drive root. UNC paths (`\\server\share\dir`) keep the
/// `\\server\share` as one root segment so its target stays absolute.
fn breadcrumb_segments(dir: &Path) -> Vec<(String, PathBuf)> {
    let s = dir.to_string_lossy().replace('/', "\\");
    let parts: Vec<&str> = s.split('\\').filter(|p| !p.is_empty()).collect();
    let mut out = Vec::with_capacity(parts.len());

    if s.starts_with("\\\\") {
        // UNC: parts = [server, share, sub, …]; the root is \\server\share.
        if parts.len() < 2 {
            return out; // malformed \\server with no share — nothing to click
        }
        let root = format!("\\\\{}\\{}", parts[0], parts[1]);
        let mut acc = root.clone();
        out.push((root, PathBuf::from(&acc)));
        for p in &parts[2..] {
            acc.push('\\');
            acc.push_str(p);
            out.push((p.to_string(), PathBuf::from(&acc)));
        }
        return out;
    }

    for i in 0..parts.len() {
        let mut path = parts[..=i].join("\\");
        if i == 0 && path.ends_with(':') {
            path.push('\\'); // drive root, not the drive-relative dir
        }
        out.push((parts[i].to_string(), PathBuf::from(path)));
    }
    out
}

fn joined_path(comps: &[&str]) -> String {
    comps
        .iter()
        .map(|c| c.trim_end_matches(['\\', '/']))
        .collect::<Vec<_>>()
        .join(std::path::MAIN_SEPARATOR_STR)
}

/// Normalize a scan-root path into the cache key string, so `"Y:"`, `"Y:\"`, and
/// `"y:\"` all map to the same cache entry. Also used to verify a loaded cache is
/// really the folder we asked for.
fn normalize_cache_key(root: &str) -> String {
    let mut key = root.trim().to_lowercase();
    if key.ends_with(':') {
        key.push('\\');
    }
    key
}

/// The cache file path for a given scan root, or `None` if the cache dir is
/// unavailable. Keyed by a hash of the (normalized) path. Pure — does NO
/// filesystem I/O (it's called every frame in the City view); the cache dir is
/// created lazily at save time.
fn cache_path_for(root: &str) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    let dir = dirs::cache_dir()?.join("sector");
    let mut h = std::collections::hash_map::DefaultHasher::new();
    normalize_cache_key(root).hash(&mut h);
    Some(dir.join(format!("{:016x}.bin", h.finish())))
}

/// Keep the cache directory from growing without bound: if the total size of its
/// `.bin` files exceeds `keep_bytes`, delete the oldest (by mtime) until under
/// the cap. Never deletes `keep_path` (the file we just wrote). Best-effort.
fn prune_cache_dir(dir: &std::path::Path, keep_path: &std::path::Path, keep_bytes: u64) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(PathBuf, u64, SystemTime)> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            if p.extension().map(|x| x == "bin").unwrap_or(false) {
                let md = e.metadata().ok()?;
                Some((p, md.len(), md.modified().unwrap_or(UNIX_EPOCH)))
            } else {
                None
            }
        })
        .collect();
    let total: u64 = files.iter().map(|(_, s, _)| *s).sum();
    if total <= keep_bytes {
        return;
    }
    files.sort_by_key(|(_, _, t)| *t); // oldest first
    let mut over = total - keep_bytes;
    for (p, sz, _) in files {
        if over == 0 {
            break;
        }
        if p == keep_path {
            continue; // never evict the file we just wrote
        }
        if std::fs::remove_file(&p).is_ok() {
            eprintln!("[sector] cache: pruned {} ({} bytes)", p.display(), sz);
            over = over.saturating_sub(sz);
        }
    }
}

/// Rough "N ago" from a file-modified time.
fn humanize_age(modified: SystemTime) -> String {
    let secs = SystemTime::now().duration_since(modified).map(|d| d.as_secs()).unwrap_or(0);
    if secs < 90 {
        "just now".to_string()
    } else if secs < 5400 {
        format!("{}m ago", secs / 60)
    } else if secs < 129_600 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// An absolute UTC timestamp "YYYY-MM-DD HH:MM" (no timezone/date dependency).
/// Uses Howard Hinnant's civil-from-days algorithm.
fn format_datetime(t: SystemTime) -> String {
    let secs = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(_) => return "—".to_string(),
    };
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (hh, mm) = (tod / 3600, (tod % 3600) / 60);
    // civil_from_days: days since 1970-01-01 → (year, month, day)
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02} UTC")
}

/// Max re-layout frequency while a scan is in progress.
const RELAYOUT_THROTTLE: Duration = Duration::from_millis(600);
/// Default worker threads. Higher helps cold SMB (lots of latency to hide);
/// tunable in the UI so it can be benchmarked on the real NAS.
const DEFAULT_THREADS: usize = 48;

fn main() -> eframe::Result<()> {
    env_logger::init();

    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([640.0, 420.0])
            .with_title(sector_core::APP_NAME),
        ..Default::default()
    };

    eframe::run_native(
        sector_core::APP_NAME,
        options,
        Box::new(|cc| {
            // Dark theme so the bars match the night cityscape.
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            if let Some(rs) = &cc.wgpu_render_state {
                let info = rs.adapter.get_info();
                eprintln!(
                    "[sector] wgpu backend={:?}  adapter=\"{}\"  type={:?}",
                    info.backend, info.name, info.device_type
                );
            }
            let mut app = SectorApp::default();
            // Restore persisted settings from a previous launch.
            if let Some(s) = cc.storage.and_then(|st| eframe::get_value::<Settings>(st, "settings")) {
                app.replay_secs = s.replay_secs.clamp(0.5, 60.0);
                app.threads = s.threads.clamp(1, 256);
                app.anim_mode = if s.replay_mode { AnimMode::Replay } else { AnimMode::Reveal };
                app.current_dir = PathBuf::from(&s.current_dir);
                app.addr_edit = s.current_dir;
                app.show_hidden = s.show_hidden;
            }
            Ok(Box::new(app))
        }),
    )
}

/// The scan lifecycle. `Running` covers both in-progress and finished — the tree
/// is shared throughout; `stats` becomes `Some` when the scan thread completes.
enum ScanState {
    Idle,
    Running {
        tree: Arc<Mutex<Tree>>,
        progress: Arc<Progress>,
        cancel: Arc<AtomicBool>,
        stats_rx: Receiver<ScanStats>,
        stats: Option<ScanStats>,
        started: Instant,
    },
}

struct SectorApp {
    /// Worker threads to use for the next scan (tunable — see DEFAULT_THREADS).
    threads: usize,
    scan: ScanState,
    /// Current drill-down root (navigates via the tree's parent chain).
    root: NodeId,
    opts: LayoutOptions,

    // Cached layout tiles + the derived cityscape + the state they're for.
    tiles: Vec<Tile>,
    scape: Scape,
    last_layout: Instant,
    last_root: NodeId,
    last_size: Vec2,
    /// Force one re-layout when a scan finishes (the "crystallize" moment).
    crystallize: bool,
    /// When set, the city is animating its build (e.g. after a cache load).
    reveal_start: Option<Instant>,
    anim_mode: AnimMode,
    /// Replay duration in seconds (live-adjustable).
    replay_secs: f32,
    /// Dominant content category per node (by bytes), computed once a scan
    /// completes. `None` while scanning (folders stay neutral until then).
    dominant: Option<Vec<FileCategory>>,
    /// The (path, is_dir) a right-click context menu currently targets.
    menu_target: Option<(PathBuf, bool)>,
    /// A finished scan's tree queued to write to cache off the UI thread.
    pending_save: Option<(Arc<Mutex<Tree>>, PathBuf, CacheStats)>,

    // ---- Explorer (E1) ----
    view: View,
    current_dir: PathBuf,
    addr_edit: String,
    /// The full listing of `current_dir` (all entries, sorted).
    all_entries: Vec<Entry>,
    /// The filtered/visible subset actually shown (hidden toggle + text filter).
    /// Everything — selection, keyboard, footer — indexes THIS.
    entries: Vec<Entry>,
    /// Case-insensitive name filter for the current folder (transient; cleared on
    /// navigation).
    filter: String,
    /// Show hidden/system entries (off by default, like Explorer).
    show_hidden: bool,
    entries_err: Option<String>,
    entries_dirty: bool,
    back_stack: Vec<PathBuf>,
    fwd_stack: Vec<PathBuf>,
    sort_key: SortKey,
    sort_asc: bool,
    /// Selected entries, by name (stable across sort/filter). Source of truth for
    /// highlighting and multi-item operations.
    sel: HashSet<String>,
    /// The focus/cursor row (index into `entries`): the single-item-op target and
    /// the moving end of a shift-range.
    lead: Option<usize>,
    /// The fixed base of a shift-range selection (index into `entries`).
    anchor: Option<usize>,
    /// When set, scroll the file table to this row next build (keyboard nav).
    scroll_target: Option<usize>,
    /// Type-ahead search buffer + when it was last appended to (resets after a
    /// short pause), so typing letters jumps to the matching file.
    type_ahead: String,
    type_ahead_time: Instant,
    /// Subtree sizes for the current folder's SUBFOLDERS (lowercased name → bytes),
    /// derived from an in-memory scan tree that covers this folder — so the list's
    /// Size column can show folder sizes. `None` when no scan covers it.
    folder_sizes: Option<HashMap<String, u64>>,
    /// After the listing for `.0` loads, select the entry named `.1` — used by
    /// "up" (re-select the folder you left) and F5 (keep the selection). The path
    /// guards against applying it in an unrelated folder.
    select_after_reload: Option<(PathBuf, String)>,
    /// Last window title we set, to avoid resending it every frame.
    last_title: String,
    /// Cached status-footer summary (item counts + total), recomputed only when
    /// the listing or folder sizes change — not every frame.
    status_summary: String,
    /// An open New-folder / Rename dialog (E4), if any.
    prompt: Option<NamePrompt>,
    /// Cut/copy clipboard (E5).
    clipboard: Option<Clipboard>,
    /// A paste running in the background, if any.
    paste_job: Option<PasteJob>,
    /// Paths awaiting delete confirmation (the modal is up), if any.
    confirm_delete: Option<Vec<PathBuf>>,
    /// A background delete-to-Recycle-Bin in progress: `Ok(())` or `Err(msg)`.
    delete_job: Option<Receiver<Result<(), String>>>,
    /// Transient error from the last edit/paste, shown in the footer.
    op_error: Option<String>,
    /// True while the address bar has (or just lost) focus — suppresses the file
    /// list's Enter/Backspace shortcuts so they don't fight the address bar.
    addr_active: bool,
    /// Path shown as an editable text field (true) vs a clickable breadcrumb.
    addr_editing: bool,
    /// Request focus for the address field on the next frame (entering edit mode).
    addr_edit_focus: bool,
    /// Show the right-hand Properties panel for the selection.
    props_visible: bool,

    // ---- Folder-tree sidebar (E1b.2, Files view only — D19) ----
    sb_visible: bool,
    /// Which pane the keyboard drives (focus follows your last click).
    focus_pane: Pane,
    /// Request to scroll the tree so the current node is visible (after a
    /// keyboard move in the tree).
    tree_scroll: bool,
    /// Drive roots (C:\, Y:\, …), enumerated once (empty = not yet computed).
    sb_roots: Vec<PathBuf>,
    /// Which tree nodes are expanded.
    sb_expanded: HashSet<PathBuf>,
    /// Lazily-filled cache: dir → its immediate subdirectories (sorted).
    sb_cache: HashMap<PathBuf, Vec<PathBuf>>,
    /// Modified-time of the current folder's cache file, if one exists — computed
    /// on folder change (in sync_city) so the City top bar doesn't stat it every
    /// frame. `None` = no cache for the current folder.
    cache_mtime: Option<SystemTime>,
    /// USN watermark captured at the START of the current scan (stored in the
    /// cache when it completes, for E6 freshness).
    pending_usn_mark: Option<UsnMark>,
    /// Freshness of the currently-loaded cache vs the live volume (E6). Only
    /// meaningful right after a cache-load.
    cache_freshness: Freshness,
    /// The path the loaded City tree is rooted at (its scan/cache-load target).
    city_root: Option<PathBuf>,
    /// The location the City view currently represents (root, or a drilled-in
    /// subfolder). When this drifts from `current_dir`, the City re-syncs.
    city_synced_dir: Option<PathBuf>,
}

impl Default for SectorApp {
    fn default() -> Self {
        SectorApp {
            threads: DEFAULT_THREADS,
            scan: ScanState::Idle,
            root: Tree::ROOT,
            // Blend "dark-but-breathing" density (D15): a touch more street
            // spacing than full-Kowloon, taller towers than dusk.
            opts: LayoutOptions { max_depth: 16, min_tile: 7.0, padding: 1.2 },
            tiles: Vec::new(),
            scape: Scape::default(),
            last_layout: Instant::now(),
            last_root: Tree::ROOT,
            last_size: Vec2::ZERO,
            crystallize: false,
            reveal_start: None,
            anim_mode: AnimMode::Reveal,
            replay_secs: REPLAY_SECS,
            dominant: None,
            menu_target: None,
            pending_save: None,

            view: View::List,
            current_dir: PathBuf::from("C:\\"),
            addr_edit: "C:\\".to_string(),
            all_entries: Vec::new(),
            entries: Vec::new(),
            filter: String::new(),
            show_hidden: false,
            entries_err: None,
            entries_dirty: true,
            back_stack: Vec::new(),
            fwd_stack: Vec::new(),
            sort_key: SortKey::Name,
            sort_asc: true,
            sel: HashSet::new(),
            lead: None,
            anchor: None,
            scroll_target: None,
            type_ahead: String::new(),
            type_ahead_time: Instant::now(),
            folder_sizes: None,
            select_after_reload: None,
            last_title: String::new(),
            status_summary: String::new(),
            prompt: None,
            clipboard: None,
            paste_job: None,
            confirm_delete: None,
            delete_job: None,
            op_error: None,
            cache_mtime: None,
            pending_usn_mark: None,
            cache_freshness: Freshness::Unknown,
            addr_active: false,
            addr_editing: false,
            addr_edit_focus: false,
            props_visible: false,
            sb_visible: true,
            focus_pane: Pane::List,
            tree_scroll: false,
            sb_roots: Vec::new(),
            sb_expanded: HashSet::new(),
            sb_cache: HashMap::new(),
            city_root: None,
            city_synced_dir: None,
        }
    }
}

/// How long the "rise" reveal takes after a cache load.
const REVEAL_SECS: f32 = 1.1;
/// How long the "replay" (structure-evolving) animation takes.
const REPLAY_SECS: f32 = 5.0;
/// Min interval between replay re-layout steps (creates visible stepping + caps cost).
const REPLAY_STEP: Duration = Duration::from_millis(110);

/// Cache-load animation style.
#[derive(Clone, Copy, PartialEq)]
enum AnimMode {
    /// Final layout; blocks appear in discovery order and rise in place (smooth).
    Reveal,
    /// Replay the scan: the tree grows and re-lays-out, so structure evolves.
    Replay,
}

/// Persisted UI settings (via eframe storage), remembered across launches.
#[derive(serde::Serialize, serde::Deserialize)]
struct Settings {
    replay_secs: f32,
    threads: usize,
    replay_mode: bool,
    /// The folder to reopen at (the shared explorer/City location, E2).
    current_dir: String,
    #[serde(default)]
    show_hidden: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            replay_secs: REPLAY_SECS,
            threads: DEFAULT_THREADS,
            replay_mode: false,
            current_dir: "C:\\".to_string(),
            show_hidden: false,
        }
    }
}

// "Dark-but-breathing" cityscape palette (D15).
const BG: Color32 = Color32::from_rgb(0x0c, 0x0f, 0x17); // night sky
const PLINTH_TOP: Color32 = Color32::from_rgb(0x1c, 0x21, 0x2a);
const PLINTH_R: Color32 = Color32::from_rgb(0x12, 0x15, 0x1b);
const PLINTH_F: Color32 = Color32::from_rgb(0x0d, 0x10, 0x14);
const BORDER: Color32 = Color32::from_rgb(0x08, 0x0a, 0x0d); // near-black block edges
const DIR_COLOR: Color32 = Color32::from_rgb(0x22, 0x27, 0x30); // folders (during scan)
/// Semi-transparent black for ground shadows.
const SHADOW: Color32 = Color32::from_rgba_premultiplied(0, 0, 0, 100);
// Face shading: top slightly boosted (neon glow in the gloom), sides deep.
const F_TOP: f32 = 1.12;
const F_RIGHT: f32 = 0.5;
const F_FRONT: f32 = 0.34;

/// Color for a file category. Muted-but-distinct, tuned for the dark ground.
fn category_color(cat: FileCategory) -> Color32 {
    match cat {
        FileCategory::Video => Color32::from_rgb(0xcf, 0x8a, 0x3e),    // amber
        FileCategory::Image => Color32::from_rgb(0x4f, 0xa8, 0x8f),    // teal
        FileCategory::Audio => Color32::from_rgb(0x9a, 0x7c, 0xc4),    // violet
        FileCategory::Archive => Color32::from_rgb(0x5b, 0x86, 0xb4),  // steel blue
        FileCategory::Document => Color32::from_rgb(0xb2, 0xba, 0xc6), // paper
        FileCategory::Code => Color32::from_rgb(0x9f, 0xb1, 0x55),     // olive
        FileCategory::System => Color32::from_rgb(0xb0, 0x5a, 0x4e),   // rust red
        FileCategory::Other => Color32::from_rgb(0x6a, 0x72, 0x80),    // neutral grey
    }
}

impl SectorApp {
    fn start_scan(&mut self) {
        // Stop any scan already running.
        if let ScanState::Running { cancel, .. } = &self.scan {
            cancel.store(true, Ordering::Relaxed);
        }

        let path = self.current_dir.clone();
        let tree = Arc::new(Mutex::new(Tree::new(path.to_string_lossy().into_owned())));
        let progress = Arc::new(Progress::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();

        let (t2, p2, c2) = (Arc::clone(&tree), Arc::clone(&progress), Arc::clone(&cancel));
        let threads = self.threads.max(1);
        std::thread::spawn(move || {
            let opts = ScanOptions { threads };
            let stats = scan_into(&path, &t2, &opts, &c2, Some(p2.as_ref()));
            let _ = tx.send(stats); // ignore if the app closed
        });

        self.root = Tree::ROOT;
        self.tiles.clear();
        self.scape = Scape::default();
        self.crystallize = false;
        self.dominant = None;
        self.menu_target = None;
        self.city_root = Some(self.current_dir.clone());
        self.city_synced_dir = Some(self.current_dir.clone());
        // Capture the USN watermark BEFORE the scan, so any later change counts
        // as making the cache stale (E6). None on NAS / non-NTFS.
        self.pending_usn_mark = query_mark(&self.current_dir);
        self.cache_freshness = Freshness::Unknown;
        self.scan = ScanState::Running {
            tree,
            progress,
            cancel,
            stats_rx: rx,
            stats: None,
            started: Instant::now(),
        };
    }

    /// Load a previously-cached scan for the current folder — instant, no walk.
    /// Returns `false` if there was no readable/valid cache (caller decides what
    /// to show instead).
    fn load_cached(&mut self) -> bool {
        let Some(cp) = cache_path_for(&self.current_dir.to_string_lossy()) else {
            eprintln!("[sector] load_cached: no cache dir");
            return false;
        };
        let bytes = match std::fs::read(&cp) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[sector] load_cached: read {} failed: {e}", cp.display());
                return false;
            }
        };
        eprintln!("[sector] load_cached: read {} bytes from {}", bytes.len(), cp.display());
        let Some((tree, cs)) = Tree::from_cache_bytes(&bytes) else {
            eprintln!("[sector] load_cached: DESERIALIZE FAILED");
            return false;
        };
        // Verify the cache is really THIS folder — guards against a (rare) hash-key
        // collision or a mismatched/renamed file loading the wrong tree.
        let cached_root = tree.node(Tree::ROOT).name.to_string();
        if normalize_cache_key(&cached_root)
            != normalize_cache_key(&self.current_dir.to_string_lossy())
        {
            eprintln!(
                "[sector] load_cached: root mismatch — cached {cached_root:?} != {:?}",
                self.current_dir
            );
            return false;
        }
        eprintln!("[sector] load_cached: OK — {} nodes, {} files", tree.len(), cs.files);
        // E6: is this cached view still current per the USN journal?
        let mark = UsnMark { journal_id: cs.usn_journal_id, next_usn: cs.usn_next };
        self.cache_freshness = usn_freshness(&self.current_dir, &mark);
        eprintln!("[sector] load_cached: freshness {:?}", self.cache_freshness);
        let tree = Arc::new(Mutex::new(tree));
        self.dominant = Some(tree.lock().unwrap_or_else(|e| e.into_inner()).dominant_categories());
        let stats = ScanStats {
            dirs: cs.dirs,
            files: cs.files,
            bytes: cs.bytes,
            errors: 0,
            elapsed: Duration::ZERO,
            cancelled: false,
        };
        self.root = Tree::ROOT;
        self.scape = Scape::default();
        self.crystallize = true;
        self.reveal_start = Some(Instant::now()); // animate the city rising
        self.menu_target = None;
        self.city_root = Some(self.current_dir.clone());
        self.city_synced_dir = Some(self.current_dir.clone());
        // No scanner thread; the dead channel is never polled because `stats`
        // is already `Some`.
        let (_tx, rx) = channel();
        self.scan = ScanState::Running {
            tree,
            progress: Arc::new(Progress::default()),
            cancel: Arc::new(AtomicBool::new(false)),
            stats_rx: rx,
            stats: Some(stats),
            started: Instant::now(),
        };
        self.recompute_folder_sizes(); // the tree now covers this folder
        self.refresh_status_summary();
        true
    }

    /// Point the City at `current_dir` (E2). Instant cache-load if one exists;
    /// otherwise drop to an idle scan-prompt (a full scan stays deliberate).
    fn sync_city(&mut self) {
        let cur = self.current_dir.clone();
        // One stat here (folder change), reused by the City top bar's Load-cached
        // button so it doesn't re-stat the file every frame.
        let md = cache_path_for(&cur.to_string_lossy())
            .and_then(|p| std::fs::metadata(p).ok());
        self.cache_mtime = md.as_ref().and_then(|m| m.modified().ok());
        let has_cache = md.is_some();
        // Try the cache; if it's missing OR fails to load (corrupt/truncated),
        // drop to an idle scan-prompt rather than leaving the previous folder's
        // cityscape on screen mislabeled as this one.
        if !(has_cache && self.load_cached()) {
            // Cancel any scan of the old location and clear the cityscape.
            if let ScanState::Running { cancel, .. } = &self.scan {
                cancel.store(true, Ordering::Relaxed);
            }
            self.scan = ScanState::Idle;
            self.root = Tree::ROOT;
            self.tiles.clear();
            self.scape = Scape::default();
            self.dominant = None;
            self.reveal_start = None;
        }
        self.city_root = Some(cur.clone());
        self.city_synced_dir = Some(cur);
    }

    // ---- Explorer navigation (E1) ----

    /// Keep the address bar showing the canonical current directory.
    fn sync_addr(&mut self) {
        self.addr_edit = self.current_dir.to_string_lossy().into_owned();
    }

    fn navigate_to(&mut self, path: PathBuf) {
        if path == self.current_dir {
            self.entries_dirty = true;
            return;
        }
        self.back_stack.push(self.current_dir.clone());
        self.fwd_stack.clear();
        self.current_dir = path;
        self.entries_dirty = true;
        self.clear_selection();
        self.filter.clear();
        self.op_error = None;
        self.sync_addr();
        self.sb_reveal();
    }

    fn go_back(&mut self) {
        if let Some(p) = self.back_stack.pop() {
            self.fwd_stack.push(std::mem::replace(&mut self.current_dir, p));
            self.entries_dirty = true;
            self.clear_selection();
            self.filter.clear();
            self.sync_addr();
            self.sb_reveal();
        }
    }

    fn go_forward(&mut self) {
        if let Some(p) = self.fwd_stack.pop() {
            self.back_stack.push(std::mem::replace(&mut self.current_dir, p));
            self.entries_dirty = true;
            self.clear_selection();
            self.filter.clear();
            self.sync_addr();
            self.sb_reveal();
        }
    }

    fn go_up(&mut self) {
        if let Some(parent) = self.current_dir.parent().map(|p| p.to_path_buf()) {
            // Re-select the folder we're stepping out of, once the parent loads.
            let child = self
                .current_dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned());
            self.navigate_to(parent.clone());
            if let Some(c) = child {
                self.select_after_reload = Some((parent, c));
            }
        }
    }

    // ---- Selection (multi-select) ----

    fn clear_selection(&mut self) {
        self.sel.clear();
        self.lead = None;
        self.anchor = None;
    }

    /// Select exactly row `i` (plain click / arrow).
    fn select_only(&mut self, i: usize) {
        self.sel.clear();
        if let Some(e) = self.entries.get(i) {
            self.sel.insert(e.name.clone());
            self.lead = Some(i);
            self.anchor = Some(i);
        }
    }

    /// Toggle row `i` in the selection (Ctrl+click).
    fn toggle_at(&mut self, i: usize) {
        if let Some(e) = self.entries.get(i) {
            let name = e.name.clone();
            if !self.sel.remove(&name) {
                self.sel.insert(name);
            }
            self.lead = Some(i);
            self.anchor = Some(i);
        }
    }

    /// Select the inclusive range `anchor..=i` (Shift+click / Shift+arrow),
    /// keeping `anchor` fixed and moving `lead` to `i`. If there's no anchor yet
    /// (extending from an empty state), establish one at `i` so the next extend
    /// grows the range instead of resetting it.
    fn select_range_to(&mut self, i: usize) {
        let a = self.anchor.unwrap_or(i);
        self.anchor = Some(a);
        let (lo, hi) = (a.min(i), a.max(i));
        self.sel.clear();
        for e in self.entries.iter().skip(lo).take(hi - lo + 1) {
            self.sel.insert(e.name.clone());
        }
        self.lead = Some(i);
    }

    /// The item single-item ops (Open / Rename / Properties) should target: the
    /// lead if it's actually selected, else the sole selected item, else none.
    /// (Guards against Ctrl+click deselecting the lead but ops still hitting it.)
    fn op_target(&self) -> Option<usize> {
        if let Some(i) = self.lead {
            if self.entries.get(i).is_some_and(|e| self.sel.contains(&e.name)) {
                return Some(i);
            }
        }
        if self.sel.len() == 1 {
            let name = self.sel.iter().next()?;
            return self.entries.iter().position(|e| &e.name == name);
        }
        None
    }

    fn lead_entry(&self) -> Option<&Entry> {
        self.lead.and_then(|i| self.entries.get(i))
    }

    /// Selected entries' absolute paths, in listing order.
    fn selected_paths(&self) -> Vec<PathBuf> {
        self.entries
            .iter()
            .filter(|e| self.sel.contains(&e.name))
            .map(|e| self.current_dir.join(&e.name))
            .collect()
    }

    /// The size to sort/show for an entry: files use their own size; folders use
    /// their scanned subtree size when one is known (else 0).
    fn entry_size(&self, e: &Entry) -> u64 {
        if e.is_dir {
            self.folder_sizes
                .as_ref()
                .and_then(|m| m.get(&e.name.to_lowercase()))
                .copied()
                .unwrap_or(0)
        } else {
            e.size
        }
    }

    fn sort_entries(&self, es: &mut [Entry]) {
        let (key, asc) = (self.sort_key, self.sort_asc);
        es.sort_by(|a, b| {
            // Folders always first, then by the chosen key.
            b.is_dir.cmp(&a.is_dir).then_with(|| {
                let o = match key {
                    SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                    SortKey::Size => self.entry_size(a).cmp(&self.entry_size(b)),
                    SortKey::Kind => entry_kind(a).cmp(&entry_kind(b)),
                    SortKey::Modified => a.modified.cmp(&b.modified),
                };
                if asc {
                    o
                } else {
                    o.reverse()
                }
            })
        });
    }

    fn reload_entries(&mut self) {
        // Refresh folder sizes first so the initial sort (by Size) can use them.
        self.recompute_folder_sizes();
        match list_dir(&self.current_dir) {
            Ok(mut es) => {
                self.sort_entries(&mut es);
                self.all_entries = es;
                self.entries_err = None;
            }
            Err(e) => {
                self.all_entries.clear();
                self.entries_err = Some(e.to_string());
            }
        }
        self.apply_filter();
        self.addr_edit = self.current_dir.to_string_lossy().into_owned();
        self.entries_dirty = false;
        // Honor a pending select-by-name (from "up" or F5) — but only in the
        // folder it was meant for, so a stale value can't select elsewhere.
        if let Some((dir, name)) = self.select_after_reload.take() {
            if dir == self.current_dir {
                if let Some(i) = self
                    .entries
                    .iter()
                    .position(|e| e.name.eq_ignore_ascii_case(&name))
                {
                    self.select_only(i);
                    self.scroll_target = Some(i);
                }
            }
        }
        // (apply_filter already refreshed the status summary for the new listing.)
    }

    /// Rebuild the visible `entries` from `all_entries` by the hidden toggle and
    /// the text filter, preserving the selection by name. Cheap re-clone; call it
    /// whenever the filter or the hidden toggle changes.
    fn apply_filter(&mut self) {
        let name_of = |i: Option<usize>| i.and_then(|i| self.entries.get(i)).map(|e| e.name.clone());
        let (lead_name, anchor_name) = (name_of(self.lead), name_of(self.anchor));
        let show_hidden = self.show_hidden;
        let f = self.filter.trim().to_lowercase();
        self.entries = self
            .all_entries
            .iter()
            .filter(|e| {
                (show_hidden || !e.is_hidden)
                    && (f.is_empty() || e.name.to_lowercase().contains(&f))
            })
            .cloned()
            .collect();
        // Keep only selected names that are still visible; remap lead & anchor by
        // their OWN names (don't collapse the range anchor into the lead).
        let visible: HashSet<String> = self.entries.iter().map(|e| e.name.clone()).collect();
        self.sel.retain(|n| visible.contains(n));
        let pos = |n: Option<String>| n.and_then(|n| self.entries.iter().position(|e| e.name == n));
        self.lead = pos(lead_name);
        self.anchor = pos(anchor_name);
        // NB: scrolling to the selection is done by the sort handler, not here —
        // a filter keystroke shouldn't yank the viewport.
        self.refresh_status_summary();
    }

    /// Recompute [`Self::folder_sizes`] for the current folder from an in-memory
    /// scan tree that covers it (the City's loaded/cached tree). Cheap — a lock,
    /// a shallow name-walk, and a small map over the immediate subfolders. Leaves
    /// it `None` when no completed scan covers `current_dir`. Callers refresh the
    /// status summary (the total depends on folder sizes).
    fn recompute_folder_sizes(&mut self) {
        self.folder_sizes = self.compute_folder_sizes();
    }

    /// Recompute the cached status-footer summary. O(n) over the listing, but
    /// only when it (or folder sizes) changes — never per frame.
    fn refresh_status_summary(&mut self) {
        if self.entries_err.is_some() {
            self.status_summary.clear();
            return;
        }
        let n = self.entries.len();
        let folders = self.entries.iter().filter(|e| e.is_dir).count();
        let total: u64 = self.entries.iter().map(|e| self.entry_size(e)).sum();
        // If a filter or the hidden toggle is narrowing the listing, lead with the
        // shown-of-total count.
        let hidden_or_filtered = n != self.all_entries.len();
        let prefix = if hidden_or_filtered {
            format!("{} of {} shown · ", commas(n as u64), commas(self.all_entries.len() as u64))
        } else {
            String::new()
        };
        self.status_summary = format!(
            "{prefix}{} items · {} folders · {} files · {}",
            commas(n as u64),
            commas(folders as u64),
            commas((n - folders) as u64),
            human_size(total),
        );
    }

    fn compute_folder_sizes(&self) -> Option<HashMap<String, u64>> {
        let ScanState::Running { tree, stats: Some(_), .. } = &self.scan else {
            return None;
        };
        let root = self.city_root.as_ref()?;
        // Relative components from the scan root to the current folder, compared
        // case-insensitively (Windows) since the tree walk is too. Lowercasing is
        // fine — find_descendant matches case-insensitively.
        let lower = |p: &Path| -> Vec<String> {
            p.components()
                .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
                .collect()
        };
        let (root_c, cur_c) = (lower(root), lower(&self.current_dir));
        if !cur_c.starts_with(&root_c) {
            return None; // current folder isn't inside the scanned root
        }
        let comps: Vec<&str> = cur_c[root_c.len()..].iter().map(String::as_str).collect();
        let t = tree.lock().ok()?;
        let node = t.find_descendant(Tree::ROOT, &comps)?;
        let mut map = HashMap::new();
        for &child in t.children(node) {
            let n = t.node(child);
            if n.kind == NodeKind::Dir {
                map.insert(n.name.to_lowercase(), n.subtree_size);
            }
        }
        Some(map)
    }

    // ---- Edits: New folder / Rename (E4) ----

    fn open_rename(&mut self, orig: String) {
        self.prompt = Some(NamePrompt {
            kind: PromptKind::Rename { orig: orig.clone() },
            buf: orig,
            error: None,
            focus: true,
        });
    }

    fn open_new_folder(&mut self) {
        let buf = self.unique_new_folder_name();
        self.prompt = Some(NamePrompt { kind: PromptKind::NewFolder, buf, error: None, focus: true });
    }

    /// "New folder", or "New folder (2)", … — the first name not already taken
    /// (case-insensitively) in the current listing.
    fn unique_new_folder_name(&self) -> String {
        let taken =
            |name: &str| self.all_entries.iter().any(|e| e.name.eq_ignore_ascii_case(name));
        if !taken("New folder") {
            return "New folder".to_string();
        }
        for i in 2..10_000 {
            let cand = format!("New folder ({i})");
            if !taken(&cand) {
                return cand;
            }
        }
        format!("New folder ({})", now_unix())
    }

    /// Validate `name` for the current folder; `allow` is the one existing name a
    /// rename is allowed to keep (its own). Returns the trimmed name or an error.
    fn validate_name<'a>(&self, name: &'a str, allow: Option<&str>) -> Result<&'a str, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("Name can't be empty.".into());
        }
        if name.contains(INVALID_NAME_CHARS) {
            return Err("Name can't contain \\ / : * ? \" < > |".into());
        }
        if name == "." || name == ".." {
            return Err("That name is reserved.".into());
        }
        // Windows reserved device names (also when used as the base of an
        // extension, e.g. "CON.txt"). A tailored message beats the raw OS error.
        let base = name.split('.').next().unwrap_or(name).trim_end();
        let up = base.to_ascii_uppercase();
        let reserved = matches!(up.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ((up.starts_with("COM") || up.starts_with("LPT"))
                && up.len() == 4
                && matches!(up.as_bytes()[3], b'1'..=b'9'));
        if reserved {
            return Err(format!("\"{base}\" is a name reserved by Windows."));
        }
        let clashes = self.all_entries.iter().any(|e| {
            e.name.eq_ignore_ascii_case(name)
                && !allow.map(|a| e.name.eq_ignore_ascii_case(a)).unwrap_or(false)
        });
        if clashes {
            return Err("An item with that name already exists.".into());
        }
        Ok(name)
    }

    /// Apply a prompt's action to the filesystem. On success returns `Ok`; on a
    /// validation or OS error returns `Err(message)` (the dialog stays open).
    fn commit_prompt(&mut self, prompt: &NamePrompt) -> Result<(), String> {
        match &prompt.kind {
            PromptKind::NewFolder => {
                let name = self.validate_name(&prompt.buf, None)?.to_string();
                let target = self.current_dir.join(&name);
                std::fs::create_dir(&target)
                    .map_err(|e| format!("Couldn't create the folder: {e}"))?;
                self.after_edit(name);
            }
            PromptKind::Rename { orig } => {
                let name = self.validate_name(&prompt.buf, Some(orig))?.to_string();
                if name == *orig {
                    return Ok(()); // no change — just close
                }
                let src = self.current_dir.join(orig);
                let dst = self.current_dir.join(&name);
                std::fs::rename(&src, &dst).map_err(|e| format!("Couldn't rename: {e}"))?;
                self.after_edit(name);
            }
        }
        Ok(())
    }

    /// Refresh views after a successful edit, and select the affected item.
    fn after_edit(&mut self, select_name: String) {
        self.entries_dirty = true;
        self.sb_cache.clear(); // the folder tree may have changed
        self.filter.clear(); // so the new/renamed item is visible
        self.select_after_reload = Some((self.current_dir.clone(), select_name));
    }

    /// Field list for the Properties panel (the current selection), or `None`.
    fn selected_properties(&self) -> Option<Vec<(&'static str, String)>> {
        let e = self.entries.get(self.op_target()?)?;
        let mut f: Vec<(&'static str, String)> = Vec::new();
        f.push(("Name", e.name.clone()));
        f.push(("Location", self.current_dir.to_string_lossy().into_owned()));
        let type_str = if e.is_dir {
            "Folder".to_string()
        } else {
            let cat = categorize(&e.name).label();
            match Path::new(&e.name).extension().and_then(|x| x.to_str()) {
                Some(ext) => format!("{cat}  ·  .{}", ext.to_lowercase()),
                None => cat.to_string(),
            }
        };
        f.push(("Type", type_str));
        let size_str = if e.is_dir {
            match self.folder_sizes.as_ref().and_then(|m| m.get(&e.name.to_lowercase())) {
                Some(sz) => format!("{} ({} bytes)", human_size(*sz), commas(*sz)),
                None => "— (scan for folder size)".to_string(),
            }
        } else {
            format!("{} ({} bytes)", human_size(e.size), commas(e.size))
        };
        f.push(("Size", size_str));
        if let Some(m) = e.modified {
            f.push(("Modified", format!("{}  ·  {}", format_datetime(m), humanize_age(m))));
        }
        if let Some(c) = e.created {
            f.push(("Created", format_datetime(c)));
        }
        let mut attrs = Vec::new();
        if e.is_hidden {
            attrs.push("Hidden");
        }
        if e.readonly {
            attrs.push("Read-only");
        }
        if e.is_symlink {
            attrs.push("Link / reparse");
        }
        f.push(("Attributes", if attrs.is_empty() { "—".to_string() } else { attrs.join(", ") }));
        Some(f)
    }

    // ---- Cut / copy / paste (E5) ----

    /// Put the current selection (all selected items) on the clipboard.
    fn clip_selected(&mut self, cut: bool) {
        let paths = self.selected_paths();
        if !paths.is_empty() {
            self.clipboard = Some(Clipboard { paths, cut });
            self.op_error = None;
        }
    }

    /// Start pasting the clipboard into the current folder (background thread).
    fn start_paste(&mut self) {
        if self.paste_job.is_some() || self.delete_job.is_some() {
            return; // one background file operation at a time
        }
        let Some(clip) = &self.clipboard else { return };
        let dest_dir = self.current_dir.clone();
        let sources = clip.paths.clone();
        let cut = clip.cut;

        // Validate on the UI thread (fast) before spawning the worker. Compare
        // CANONICAL paths so case differences and junction/mapped-drive aliases
        // can't sneak a folder into itself (which would recurse until the disk
        // fills). Falls back to the raw path if canonicalize fails.
        let dest_c = std::fs::canonicalize(&dest_dir).unwrap_or_else(|_| dest_dir.clone());
        for src in &sources {
            let src_c = std::fs::canonicalize(src).unwrap_or_else(|_| src.clone());
            if src.is_dir() && (dest_c == src_c || dest_c.starts_with(&src_c)) {
                self.op_error = Some("Can't paste a folder into itself.".into());
                return;
            }
            if cut && src_c.parent() == Some(dest_c.as_path()) {
                self.op_error = Some("It's already in this folder.".into());
                return;
            }
        }

        let desc = {
            let name = sources
                .first()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let n = sources.len();
            let what = if n > 1 { format!("{n} items") } else { format!("“{name}”") };
            format!("{} {what}…", if cut { "Moving" } else { "Copying" })
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let (c2, s2, d2) = (Arc::clone(&cancel), sources, dest_dir);
        std::thread::spawn(move || {
            let _ = tx.send(run_paste(s2, d2, cut, c2));
        });
        self.op_error = None;
        self.paste_job = Some(PasteJob { rx, cancel, cut, desc });
    }

    /// Ask to delete the current selection (opens the confirmation modal).
    fn request_delete(&mut self) {
        // One background file operation at a time.
        if self.delete_job.is_some() || self.paste_job.is_some() {
            return;
        }
        let paths = self.selected_paths();
        if !paths.is_empty() {
            self.confirm_delete = Some(paths);
        }
    }

    /// Delete `paths` to the Recycle Bin on a background thread.
    fn start_delete(&mut self, paths: Vec<PathBuf>) {
        if self.delete_job.is_some() || self.paste_job.is_some() || paths.is_empty() {
            return; // one background file operation at a time
        }
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let result = trash::delete_all(&paths).map_err(|e| format!("Couldn't delete: {e}"));
            let _ = tx.send(result);
        });
        self.op_error = None;
        self.delete_job = Some(rx);
    }

    // ---- Folder-tree sidebar (E1b.2) ----

    /// Expand every ANCESTOR of `current_dir` so the tree reveals where you are,
    /// leaving the current folder itself highlighted-but-not-expanded (a click
    /// expands it explicitly; keyboard arrowing just highlights).
    fn sb_reveal(&mut self) {
        let mut cur = self.current_dir.clone();
        while let Some(parent) = cur.parent().map(|p| p.to_path_buf()) {
            self.sb_expanded.insert(parent.clone());
            cur = parent;
        }
    }

    /// The currently-visible tree nodes, in display order (roots + expanded
    /// subtrees, matching `sb_node`'s render order incl. the 400-child cap).
    fn sb_visible_nodes(&mut self) -> Vec<PathBuf> {
        if self.sb_roots.is_empty() {
            self.sb_roots = enumerate_drives();
            self.sb_reveal();
        }
        let mut out = Vec::new();
        for root in self.sb_roots.clone() {
            self.sb_collect_visible(&root, &mut out);
        }
        out
    }

    fn sb_collect_visible(&mut self, path: &Path, out: &mut Vec<PathBuf>) {
        out.push(path.to_path_buf());
        if self.sb_expanded.contains(path) {
            for child in self.sb_children(path).into_iter().take(400) {
                self.sb_collect_visible(&child, out);
            }
        }
    }

    /// A folder's immediate subdirectories, cached (lazy — filled on first expand).
    fn sb_children(&mut self, path: &Path) -> Vec<PathBuf> {
        if let Some(c) = self.sb_cache.get(path) {
            return c.clone();
        }
        let mut dirs: Vec<PathBuf> = match list_dir(path) {
            Ok(es) => es
                .into_iter()
                .filter(|e| e.is_dir && !e.is_symlink)
                .map(|e| path.join(&e.name))
                .collect(),
            Err(_) => Vec::new(),
        };
        dirs.sort_by_key(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default()
        });
        self.sb_cache.insert(path.to_path_buf(), dirs.clone());
        dirs
    }

    /// Render the drive roots and (recursively) their expanded subtrees.
    fn sidebar_tree(&mut self, ui: &mut egui::Ui) {
        if self.sb_roots.is_empty() {
            self.sb_roots = enumerate_drives();
            self.sb_reveal(); // open the path to wherever we start
        }
        for root in self.sb_roots.clone() {
            self.sb_node(ui, root, 0);
        }
    }

    /// One tree row: an expand triangle + a clickable folder label, then (if
    /// expanded) its children indented beneath. Navigating and toggling are
    /// deferred out of the row closure to keep the borrows simple.
    fn sb_node(&mut self, ui: &mut egui::Ui, path: PathBuf, depth: usize) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        let open = self.sb_expanded.contains(&path);
        let is_current = self.current_dir == path;

        let mut toggle = false;
        let mut navigate = false;
        let scroll_here = self.tree_scroll && is_current && self.focus_pane == Pane::Tree;

        // The whole row is ONE full-width interactive element, so hovering
        // anywhere on it (text or the space beside it) highlights uniformly.
        // Colors pulled up front so the visuals borrow doesn't clash with painting.
        let (sel_bg, sel_fg, hover_bg, weak, strong, text) = {
            let v = ui.visuals();
            (
                v.selection.bg_fill,
                v.selection.stroke.color,
                v.widgets.hovered.weak_bg_fill,
                v.weak_text_color(),
                v.strong_text_color(),
                v.text_color(),
            )
        };
        let row_h = 18.0;
        let (rect, resp) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), Sense::click());
        let painter = ui.painter();
        if is_current {
            painter.rect_filled(rect, 2.0, sel_bg);
        } else if resp.hovered() {
            painter.rect_filled(rect, 2.0, hover_bg);
        }
        let indent = depth as f32 * 12.0;
        // Disclosure triangle (painted — a font glyph renders as a missing-glyph box).
        let tri_c = egui::pos2(rect.left() + indent + 11.0, rect.center().y);
        let tri_rect = egui::Rect::from_center_size(tri_c, egui::vec2(16.0, row_h));
        let tri_col = if is_current {
            sel_fg
        } else if resp.hovered() {
            strong
        } else {
            weak
        };
        let pts = if open {
            vec![tri_c + Vec2::new(-4.0, -2.0), tri_c + Vec2::new(4.0, -2.0), tri_c + Vec2::new(0.0, 3.0)]
        } else {
            vec![tri_c + Vec2::new(-2.0, -4.0), tri_c + Vec2::new(-2.0, 4.0), tri_c + Vec2::new(3.0, 0.0)]
        };
        painter.add(egui::Shape::convex_polygon(pts, tri_col, Stroke::NONE));
        let text_col = if is_current { sel_fg } else { text };
        painter.text(
            egui::pos2(rect.left() + indent + 22.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            format!("🗀 {name}"),
            egui::FontId::proportional(14.0),
            text_col,
        );
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if scroll_here {
            resp.scroll_to_me(Some(egui::Align::Center));
        }
        // A click on the triangle toggles; anywhere else navigates.
        if resp.clicked() {
            if resp.interact_pointer_pos().map(|p| tri_rect.contains(p)).unwrap_or(false) {
                toggle = true;
            } else {
                navigate = true;
            }
        }

        if toggle {
            if open {
                self.sb_expanded.remove(&path);
            } else {
                self.sb_expanded.insert(path.clone());
            }
            self.focus_pane = Pane::Tree;
        }
        if navigate {
            self.navigate_to(path.clone());
            self.sb_expanded.insert(path.clone()); // reveal children on select
            self.focus_pane = Pane::Tree;
        }

        if self.sb_expanded.contains(&path) {
            // Cap children per node: the tree isn't virtualized, so a folder with
            // thousands of subfolders would otherwise render thousands of rows
            // every frame. Beyond the cap, point to the (virtualized) list.
            const CAP: usize = 400;
            let kids = self.sb_children(&path);
            for child in kids.iter().take(CAP) {
                self.sb_node(ui, child.clone(), depth + 1);
            }
            if kids.len() > CAP {
                ui.horizontal(|ui| {
                    ui.add_space((depth as f32 + 1.0) * 12.0);
                    ui.weak(format!("… {} more — open in the list", kids.len() - CAP));
                });
            }
        }
    }

    /// The shared navigation strip: back / forward / up + the address bar. Drives
    /// `current_dir`, which BOTH views follow (E2).
    fn nav_bar(&mut self, ui: &mut egui::Ui) {
        if ui.add_enabled(!self.back_stack.is_empty(), egui::Button::new("◀")).on_hover_text("Back").clicked() {
            self.go_back();
        }
        if ui.add_enabled(!self.fwd_stack.is_empty(), egui::Button::new("▶")).on_hover_text("Forward").clicked() {
            self.go_forward();
        }
        if ui
            .add_enabled(self.current_dir.parent().is_some(), egui::Button::new("↑"))
            .on_hover_text("Up")
            .clicked()
        {
            self.go_up();
        }

        if self.addr_editing {
            // Editable path field.
            let r = ui
                .add(egui::TextEdit::singleline(&mut self.addr_edit).desired_width(f32::INFINITY));
            if self.addr_edit_focus {
                r.request_focus();
                self.addr_edit_focus = false;
            }
            self.addr_active = r.has_focus() || r.lost_focus();
            if r.lost_focus() {
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    let raw = self.addr_edit.trim();
                    // "C:" is drive-relative; the user means the drive root.
                    let norm = if raw.len() == 2
                        && raw.as_bytes()[0].is_ascii_alphabetic()
                        && raw.as_bytes()[1] == b':'
                    {
                        format!("{raw}\\")
                    } else {
                        raw.to_string()
                    };
                    self.navigate_to(PathBuf::from(norm));
                }
                self.addr_editing = false; // leave edit mode on commit or blur
            }
        } else {
            self.addr_active = false;
            // Clickable breadcrumb, then a pencil to switch to the text field.
            let mut go: Option<PathBuf> = None;
            let segs = breadcrumb_segments(&self.current_dir);
            let last = segs.len().saturating_sub(1);
            ui.spacing_mut().item_spacing.x = 2.0;
            for (i, (label, path)) in segs.iter().enumerate() {
                if i > 0 {
                    ui.weak("›");
                }
                if i == last {
                    ui.strong(label); // current folder — not a link
                } else if ui.add(egui::Button::new(label).frame(false)).clicked() {
                    go = Some(path.clone());
                }
            }
            if ui.button("Edit").on_hover_text("Edit the path as text").clicked() {
                self.addr_edit = self.current_dir.to_string_lossy().into_owned();
                self.addr_editing = true;
                self.addr_edit_focus = true;
            }
            if let Some(p) = go {
                self.navigate_to(p);
            }
        }
    }

    /// The file-explorer List view: a folder-tree sidebar + a virtualized file
    /// table for `current_dir`.
    fn show_list(&mut self, ui: &mut egui::Ui) {
        if self.entries_dirty {
            self.reload_entries();
        }

        // Files toolbar: folder-tree toggle, New folder, name filter, hidden.
        egui::Panel::top("files_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui
                    .selectable_label(self.sb_visible, "Folders")
                    .on_hover_text("Show/hide the folder tree")
                    .clicked()
                {
                    self.sb_visible = !self.sb_visible;
                }
                if ui
                    .button("New folder")
                    .on_hover_text("New folder (Ctrl+Shift+N)")
                    .clicked()
                {
                    self.open_new_folder();
                }
                ui.separator();
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.filter)
                        .desired_width(220.0)
                        .hint_text("Filter this folder…"),
                );
                let mut changed = r.changed();
                if !self.filter.is_empty() && ui.button("×").on_hover_text("Clear filter").clicked() {
                    self.filter.clear();
                    changed = true;
                }
                ui.separator();
                if ui
                    .checkbox(&mut self.show_hidden, "Hidden")
                    .on_hover_text("Show hidden / system files")
                    .changed()
                {
                    changed = true;
                }
                if changed {
                    self.apply_filter();
                }
                ui.separator();
                if ui
                    .selectable_label(self.props_visible, "Details")
                    .on_hover_text("Properties panel (Alt+Enter)")
                    .clicked()
                {
                    self.props_visible = !self.props_visible;
                }
            });
        });

        // Right-hand Properties panel for the selection.
        if self.props_visible {
            let props = self.selected_properties();
            let mut close = false;
            egui::Panel::right("props_panel")
                .resizable(true)
                .default_size(300.0)
                .min_size(220.0)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.strong("Properties");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("×").clicked() {
                                close = true;
                            }
                        });
                    });
                    ui.separator();
                    match &props {
                        Some(fields) => {
                            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                                egui::Grid::new("props_grid")
                                    .num_columns(2)
                                    .spacing([12.0, 6.0])
                                    .striped(true)
                                    .show(ui, |ui| {
                                        for (k, v) in fields {
                                            ui.strong(*k);
                                            ui.add(egui::Label::new(v).wrap());
                                            ui.end_row();
                                        }
                                    });
                            });
                        }
                        None => {
                            ui.weak("Select an item to see its details.");
                        }
                    }
                });
            if close {
                self.props_visible = false;
            }
        }

        if self.sb_visible {
            egui::Panel::left("folder_tree")
                .resizable(true)
                .default_size(240.0)
                .min_size(150.0)
                .show(ui, |ui| {
                    ui.add_space(4.0);
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            self.sidebar_tree(ui);
                        });
                });
            self.tree_scroll = false; // one-shot: consumed by this render
        }

        // Status footer: folder totals (cached) + selection details.
        {
            let detail = if self.sel.len() > 1 {
                // Multiple selected: count + combined size of known items.
                let total: u64 = self
                    .entries
                    .iter()
                    .filter(|e| self.sel.contains(&e.name))
                    .map(|e| self.entry_size(e))
                    .sum();
                Some(format!("{} selected · {}", self.sel.len(), human_size(total)))
            } else {
                self.lead_entry().filter(|e| self.sel.contains(&e.name)).map(|e| {
                    let mut s = e.name.clone();
                    let known = !e.is_dir
                        || self
                            .folder_sizes
                            .as_ref()
                            .map(|m| m.contains_key(&e.name.to_lowercase()))
                            .unwrap_or(false);
                    if known {
                        s.push_str(&format!(" · {}", human_size(self.entry_size(e))));
                    }
                    if let Some(m) = e.modified {
                        s.push_str(&format!(" · {}", humanize_age(m)));
                    }
                    s
                })
            };
            let mut cancel_paste = false;
            let mut clear_err = false;
            egui::Panel::bottom("files_status").show(ui, |ui| {
                ui.horizontal(|ui| {
                    // A running paste/delete takes over the footer with a spinner.
                    if self.delete_job.is_some() {
                        ui.spinner();
                        ui.label("Moving to Recycle Bin…");
                    } else if let Some(job) = &self.paste_job {
                        ui.spinner();
                        ui.label(&job.desc);
                        if ui.button("Cancel").clicked() {
                            cancel_paste = true;
                        }
                    } else if let Some(err) = &self.op_error {
                        ui.colored_label(egui::Color32::from_rgb(224, 108, 108), format!("⚠ {err}"));
                        if ui.button("×").on_hover_text("Dismiss").clicked() {
                            clear_err = true;
                        }
                    } else {
                        ui.weak(&self.status_summary);
                        if let Some(d) = detail {
                            ui.separator();
                            ui.label(d);
                        }
                        if let Some(c) = &self.clipboard {
                            ui.separator();
                            let verb = if c.cut { "cut" } else { "copied" };
                            ui.weak(format!("📋 {} {verb}", c.paths.len()));
                        }
                    }
                });
            });
            if cancel_paste {
                if let Some(job) = &self.paste_job {
                    job.cancel.store(true, Ordering::Relaxed);
                }
            }
            if clear_err {
                self.op_error = None;
            }
        }

        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(err) = self.entries_err.clone() {
                ui.centered_and_justified(|ui| {
                    ui.weak(format!("⚠  {err}"));
                });
                return;
            }
            if self.entries.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.weak("(empty folder)");
                });
                return;
            }

            use egui_extras::{Column, TableBuilder};

            // Move entries out of self so the table closures don't fight the
            // borrow checker; deferred mutations are applied after.
            let entries = std::mem::take(&mut self.entries);
            let folder_sizes = self.folder_sizes.take();
            let cur = self.current_dir.clone();
            let (sort_key, sort_asc) = (self.sort_key, self.sort_asc);
            let scroll_target = self.scroll_target.take();
            // Paths of cut items (dimmed in the list).
            let cut_paths: HashSet<PathBuf> = self
                .clipboard
                .as_ref()
                .filter(|c| c.cut)
                .map(|c| c.paths.iter().cloned().collect())
                .unwrap_or_default();
            // Selection set (taken out so the row closures can read it), plus the
            // click intent + modifiers, applied after the table.
            let sel = std::mem::take(&mut self.sel);
            let mods = ui.ctx().input(|i| i.modifiers);
            let mut click: Option<usize> = None;
            let mut menu_row: Option<usize> = None;
            let mut nav_target: Option<PathBuf> = None;
            let mut new_sort: Option<SortKey> = None;
            let mut rename_req: Option<String> = None;
            let mut new_folder_req = false;
            let mut props_req = false;
            let mut copy_req = false;
            let mut cut_req = false;
            let mut paste_req = false;
            let mut delete_req = false;
            let can_paste = self.clipboard.is_some() && self.paste_job.is_none();

            let arrow = |k: SortKey| {
                if k == sort_key {
                    if sort_asc {
                        " ▲"
                    } else {
                        " ▼"
                    }
                } else {
                    ""
                }
            };
            // Left-aligned header, vertically centered (so it lines up with the
            // right-aligned headers, which are also center-aligned).
            let header_cell = |ui: &mut egui::Ui, label: String, out: &mut Option<SortKey>, k: SortKey| {
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    if ui
                        .add(egui::Label::new(egui::RichText::new(label).strong()).sense(Sense::click()))
                        .clicked()
                    {
                        *out = Some(k);
                    }
                });
            };
            // Right-aligned header, for the numeric/date columns. The leading
            // add_space (right_to_left places it at the far right) keeps the text
            // off the panel edge.
            let header_cell_r = |ui: &mut egui::Ui, label: String, out: &mut Option<SortKey>, k: SortKey| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(8.0);
                    if ui
                        .add(egui::Label::new(egui::RichText::new(label).strong()).sense(Sense::click()))
                        .clicked()
                    {
                        *out = Some(k);
                    }
                });
            };
            // Right-aligned data cell wrapper (with the same right padding).
            let cell_r = |ui: &mut egui::Ui, add: &mut dyn FnMut(&mut egui::Ui)| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(8.0);
                    add(ui);
                });
            };

            let mut table = TableBuilder::new(ui)
                .striped(true)
                .sense(Sense::click())
                .column(Column::remainder().at_least(220.0).clip(true))
                .column(Column::auto().at_least(90.0))
                .column(Column::auto().at_least(90.0))
                .column(Column::auto().at_least(90.0));
            if let Some(row) = scroll_target {
                table = table.scroll_to_row(row, None);
            }
            table
                .header(22.0, |mut h| {
                    h.col(|ui| header_cell(ui, format!("Name{}", arrow(SortKey::Name)), &mut new_sort, SortKey::Name));
                    h.col(|ui| header_cell_r(ui, format!("Size{}", arrow(SortKey::Size)), &mut new_sort, SortKey::Size));
                    h.col(|ui| header_cell(ui, format!("Type{}", arrow(SortKey::Kind)), &mut new_sort, SortKey::Kind));
                    h.col(|ui| header_cell_r(ui, format!("Modified{}", arrow(SortKey::Modified)), &mut new_sort, SortKey::Modified));
                })
                .body(|body| {
                    body.rows(20.0, entries.len(), |mut row| {
                        let e = &entries[row.index()];
                        // File-type color, shared with the City palette so the two
                        // views read the same. Folders keep the folder glyph.
                        let cat = (!e.is_dir).then(|| categorize(&e.name));
                        let is_cut = cut_paths.contains(&cur.join(&e.name));
                        row.set_selected(sel.contains(&e.name));
                        row.col(|ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                match cat {
                                    None => {
                                        ui.label("📁");
                                    }
                                    Some(c) => {
                                        ui.colored_label(category_color(c), "●");
                                    }
                                }
                                // A cut item is dimmed until the paste completes.
                                if is_cut {
                                    ui.weak(&e.name);
                                } else {
                                    ui.label(&e.name);
                                }
                            });
                        });
                        row.col(|ui| {
                            cell_r(ui, &mut |ui| {
                                if e.is_dir {
                                    // Folder size from a covering scan, if we have one.
                                    if let Some(sz) = folder_sizes
                                        .as_ref()
                                        .and_then(|m| m.get(&e.name.to_lowercase()))
                                    {
                                        ui.monospace(human_size(*sz));
                                    }
                                } else {
                                    ui.monospace(human_size(e.size));
                                }
                            });
                        });
                        row.col(|ui| match cat {
                            None => {
                                ui.weak("Folder");
                            }
                            Some(c) => {
                                ui.colored_label(category_color(c), c.label());
                            }
                        });
                        row.col(|ui| {
                            cell_r(ui, &mut |ui| {
                                if let Some(m) = e.modified {
                                    ui.weak(humanize_age(m));
                                }
                            });
                        });
                        let full = cur.join(&e.name);
                        let resp = row.response();
                        if resp.clicked() {
                            click = Some(row.index());
                        }
                        if resp.secondary_clicked() {
                            menu_row = Some(row.index());
                        }
                        if resp.double_clicked() {
                            if e.is_dir {
                                nav_target = Some(full.clone());
                            } else {
                                open_path(&full);
                            }
                        }
                        resp.context_menu(|ui| {
                            if ui.button("Open").clicked() {
                                if e.is_dir {
                                    nav_target = Some(full.clone()); // enter it in SECTOR
                                } else {
                                    open_path(&full); // launch its default app
                                }
                                ui.close();
                            }
                            let reveal_label =
                                if e.is_dir { "Open in Explorer" } else { "Reveal in Explorer" };
                            if ui.button(reveal_label).clicked() {
                                reveal_in_explorer(&full, e.is_dir);
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("Copy path").clicked() {
                                ui.ctx().copy_text(full.to_string_lossy().into_owned());
                                ui.close();
                            }
                            if ui.button("Copy name").clicked() {
                                ui.ctx().copy_text(e.name.clone());
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("Copy").clicked() {
                                copy_req = true;
                                ui.close();
                            }
                            if ui.button("Cut").clicked() {
                                cut_req = true;
                                ui.close();
                            }
                            if ui.add_enabled(can_paste, egui::Button::new("Paste")).clicked() {
                                paste_req = true;
                                ui.close();
                            }
                            if ui.button("Delete").clicked() {
                                delete_req = true;
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("Rename…").clicked() {
                                rename_req = Some(e.name.clone());
                                ui.close();
                            }
                            if ui.button("New folder…").clicked() {
                                new_folder_req = true;
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("Properties").clicked() {
                                props_req = true;
                                ui.close();
                            }
                        });
                    });
                });

            // Restore entries + selection, then apply deferred mutations.
            self.entries = entries;
            self.folder_sizes = folder_sizes;
            self.sel = sel;
            // Apply a click: Shift = range, Ctrl = toggle, plain = select one.
            if let Some(i) = click {
                if mods.shift {
                    self.select_range_to(i);
                } else if mods.ctrl {
                    self.toggle_at(i);
                } else {
                    self.select_only(i);
                }
                self.focus_pane = Pane::List; // keyboard now drives the list
            }
            // A right-click on an unselected row selects just it (keeps a
            // multi-selection when right-clicking within it).
            if let Some(i) = menu_row {
                let in_sel =
                    self.entries.get(i).map(|e| self.sel.contains(&e.name)).unwrap_or(false);
                if !in_sel {
                    self.select_only(i);
                }
            }
            if let Some(k) = new_sort {
                if self.sort_key == k {
                    self.sort_asc = !self.sort_asc;
                } else {
                    self.sort_key = k;
                    self.sort_asc = true;
                }
                // Re-sort the full listing IN PLACE (no directory re-read), then
                // re-apply the filter (which preserves the selection by name).
                let mut es = std::mem::take(&mut self.all_entries);
                self.sort_entries(&mut es);
                self.all_entries = es;
                self.apply_filter();
                self.scroll_target = self.lead; // keep the selection visible
            }
            if let Some(t) = nav_target {
                self.navigate_to(t);
            }
            if let Some(name) = rename_req {
                self.open_rename(name);
            }
            if new_folder_req {
                self.open_new_folder();
            }
            if props_req {
                self.props_visible = true;
            }
            // The selection now reflects the right-clicked row (or a kept
            // multi-selection); copy/cut act on all of it.
            if copy_req {
                self.clip_selected(false);
            }
            if cut_req {
                self.clip_selected(true);
            }
            if paste_req {
                self.start_paste();
            }
            if delete_req {
                self.request_delete();
            }
        });

        // Keyboard parity with Explorer: F5 refreshes; arrows/Home/End/PageUp/Down
        // move the selection; Enter opens it; Backspace goes up — but never while
        // the address bar owns the keyboard (F5 there would wipe in-progress text).
        let typing = self.prompt.is_some()
            || self.confirm_delete.is_some()
            || self.addr_active
            || ui.ctx().memory(|m| m.focused()).is_some();
        if !typing {
            // F5: re-read the current folder AND drop the (lazy) tree cache, so
            // on-disk changes show without a restart. Keep the same file selected
            // by name (the old index may no longer be valid after the re-read).
            if ui.input(|i| i.key_pressed(egui::Key::F5)) {
                let keep = self.lead_entry().map(|e| e.name.clone());
                self.clear_selection();
                if let Some(name) = keep {
                    self.select_after_reload = Some((self.current_dir.clone(), name));
                }
                self.entries_dirty = true;
                self.sb_cache.clear();
            }
            if ui.input(|i| i.key_pressed(egui::Key::Backspace)) {
                self.go_up();
            }
            // F2 renames the target item; Ctrl+Shift+N makes a new folder.
            if ui.input(|i| i.key_pressed(egui::Key::F2)) {
                if let Some(name) =
                    self.op_target().and_then(|i| self.entries.get(i)).map(|e| e.name.clone())
                {
                    self.open_rename(name);
                }
            }
            if ui.input(|i| i.modifiers.ctrl && i.modifiers.shift && i.key_pressed(egui::Key::N)) {
                self.open_new_folder();
            }
            // Ctrl+A selects everything in the folder.
            if ui.input(|i| i.modifiers.ctrl && !i.modifiers.shift && i.key_pressed(egui::Key::A)) {
                self.sel = self.entries.iter().map(|e| e.name.clone()).collect();
                self.lead = self.entries.len().checked_sub(1);
                self.anchor = Some(0);
            }
            // Ctrl+C copy, Ctrl+X cut, Ctrl+V paste (not Shift, to avoid clashing
            // with Ctrl+Shift+N).
            if ui.input(|i| i.modifiers.ctrl && !i.modifiers.shift && i.key_pressed(egui::Key::C)) {
                self.clip_selected(false);
            }
            if ui.input(|i| i.modifiers.ctrl && !i.modifiers.shift && i.key_pressed(egui::Key::X)) {
                self.clip_selected(true);
            }
            if ui.input(|i| i.modifiers.ctrl && !i.modifiers.shift && i.key_pressed(egui::Key::V)) {
                self.start_paste();
            }
            // Delete → Recycle Bin (with a confirmation).
            if ui.input(|i| i.key_pressed(egui::Key::Delete)) {
                self.request_delete();
            }
            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                if ui.input(|i| i.modifiers.alt) {
                    // Alt+Enter: show properties for the target item.
                    if self.op_target().is_some() {
                        self.props_visible = true;
                    }
                } else {
                    let action = self
                        .op_target()
                        .and_then(|i| self.entries.get(i))
                        .map(|e| (self.current_dir.join(&e.name), e.is_dir));
                    if let Some((full, is_dir)) = action {
                        if is_dir {
                            self.navigate_to(full);
                        } else {
                            open_path(&full);
                        }
                    }
                }
            }

            // Arrows drive whichever pane has focus. The tree only when it's shown.
            if self.focus_pane == Pane::Tree && self.sb_visible {
                self.tree_keys(ui);
            } else {
                self.type_ahead_list(ui);
                // Move the lead in the list (Shift extends the range from the
                // anchor), scrolling it into view.
                let n = self.entries.len();
                if n > 0 {
                    const PAGE: usize = 12;
                    let cur = self.lead;
                    let mut moved = cur;
                    ui.input(|i| {
                        use egui::Key;
                        if i.key_pressed(Key::ArrowDown) {
                            moved = Some(cur.map_or(0, |c| (c + 1).min(n - 1)));
                        }
                        if i.key_pressed(Key::ArrowUp) {
                            moved = Some(cur.map_or(0, |c| c.saturating_sub(1)));
                        }
                        if i.key_pressed(Key::PageDown) {
                            moved = Some(cur.map_or(0, |c| (c + PAGE).min(n - 1)));
                        }
                        if i.key_pressed(Key::PageUp) {
                            moved = Some(cur.map_or(0, |c| c.saturating_sub(PAGE)));
                        }
                        if i.key_pressed(Key::Home) {
                            moved = Some(0);
                        }
                        if i.key_pressed(Key::End) {
                            moved = Some(n - 1);
                        }
                    });
                    if let Some(m) = moved {
                        if moved != cur {
                            if ui.input(|i| i.modifiers.shift) {
                                self.select_range_to(m);
                            } else {
                                self.select_only(m);
                            }
                            self.scroll_target = Some(m);
                            ui.ctx().request_repaint();
                        }
                    }
                }
            }
        }
    }

    /// Keyboard navigation for the folder tree (when it has focus): ↑/↓ move &
    /// navigate, → expand/first-child, ← collapse/parent, Home/End to the ends.
    fn tree_keys(&mut self, ui: &egui::Ui) {
        use egui::Key;
        let (down, up, right, left, home, end) = ui.input(|i| {
            (
                i.key_pressed(Key::ArrowDown),
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::ArrowRight),
                i.key_pressed(Key::ArrowLeft),
                i.key_pressed(Key::Home),
                i.key_pressed(Key::End),
            )
        });
        if !(down || up || right || left || home || end) {
            return;
        }
        let visible = self.sb_visible_nodes();
        if visible.is_empty() {
            return;
        }
        let cur = visible.iter().position(|p| p == &self.current_dir);
        let go = |app: &mut Self, path: PathBuf| {
            app.navigate_to(path);
            app.tree_scroll = true;
        };
        if down {
            let ni = cur.map_or(0, |i| (i + 1).min(visible.len() - 1));
            go(self, visible[ni].clone());
        } else if up {
            if let Some(i) = cur {
                if i > 0 {
                    go(self, visible[i - 1].clone());
                }
            }
        } else if home {
            go(self, visible[0].clone());
        } else if end {
            go(self, visible[visible.len() - 1].clone());
        } else if right {
            // Expand a collapsed folder; if already expanded, step into its first child.
            let cur_dir = self.current_dir.clone();
            let mut kids = self.sb_children(&cur_dir);
            if !kids.is_empty() {
                if !self.sb_expanded.contains(&cur_dir) {
                    self.sb_expanded.insert(cur_dir);
                } else {
                    go(self, kids.remove(0));
                }
            }
        } else if left {
            // Collapse an expanded folder; otherwise step out to the parent.
            let cur_dir = self.current_dir.clone();
            if self.sb_expanded.contains(&cur_dir) {
                self.sb_expanded.remove(&cur_dir);
            } else if let Some(parent) = cur_dir.parent().map(|p| p.to_path_buf()) {
                go(self, parent);
            }
        }
        self.focus_pane = Pane::Tree;
        ui.ctx().request_repaint();
    }

    /// Type-ahead in the file list: typing letters jumps to the first matching
    /// name; repeating the same letter cycles through matches; the buffer resets
    /// after a short pause. Uses egui Text events, so Ctrl-combos don't trigger it.
    fn type_ahead_list(&mut self, ui: &egui::Ui) {
        let typed: String = ui.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Text(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect()
        });
        if typed.is_empty() || self.entries.is_empty() {
            return;
        }
        let typed = typed.to_lowercase();
        let now = Instant::now();
        let timed_out = now.duration_since(self.type_ahead_time) >= Duration::from_millis(900);
        // Same single letter again (within the window) → cycle to the next match.
        let repeat = !timed_out
            && self.type_ahead.chars().count() == 1
            && typed.chars().count() == 1
            && self.type_ahead == typed;
        if timed_out {
            self.type_ahead = typed;
        } else if !repeat {
            self.type_ahead.push_str(&typed);
        }
        self.type_ahead_time = now;

        let q = self.type_ahead.clone();
        let n = self.entries.len();
        let start = if repeat { self.lead.map_or(0, |i| i + 1) } else { 0 };
        let hit = (0..n)
            .map(|off| (start + off) % n)
            .find(|&i| self.entries[i].name.to_lowercase().starts_with(&q));
        if let Some(i) = hit {
            self.select_only(i);
            self.scroll_target = Some(i);
            self.focus_pane = Pane::List;
            ui.ctx().request_repaint();
        }
    }

    /// Ancestors of `root`, from the tree root down to `root` (for the breadcrumb).
    fn breadcrumb(tree: &Tree, root: NodeId) -> Vec<NodeId> {
        let mut chain = vec![root];
        let mut cur = root;
        loop {
            let parent = tree.node(cur).parent;
            if parent == cur {
                break;
            }
            chain.push(parent);
            cur = parent;
        }
        chain.reverse();
        chain
    }
}

/// One extruded block (a leaf tile), pre-projected to screen space.
struct IsoBlock {
    node: NodeId,
    top: [Pos2; 4],
    right: [Pos2; 4],
    front: [Pos2; 4],
    shadow: [Pos2; 4],
    color: Color32,
}

/// The whole scene: a ground plinth plus the blocks, all pre-projected.
struct Scape {
    plinth_top: [Pos2; 4],
    plinth_right: [Pos2; 4],
    plinth_front: [Pos2; 4],
    blocks: Vec<IsoBlock>,
}

impl Default for Scape {
    fn default() -> Self {
        let z = [Pos2::ZERO; 4];
        Scape { plinth_top: z, plinth_right: z, plinth_front: z, blocks: Vec::new() }
    }
}

// Dimetric projection: x→right, y→left, z→up.
const ISO_AX: f64 = 0.5;
const ISO_AY: f64 = 0.25;
const PLANE: f64 = 1000.0; // ground-plane size the treemap is laid out on
const PLINTH_TH: f64 = 26.0;
const SHADOW_EXPAND: f64 = 2.5;
const SHADOW_OFF: f64 = 0.4;

fn iso(x: f64, y: f64, z: f64) -> (f64, f64) {
    ((x - y) * ISO_AX, (x + y) * ISO_AY - z)
}

fn shade(c: Color32, f: f32) -> Color32 {
    let m = |v: u8| (v as f32 * f).clamp(0.0, 255.0) as u8;
    Color32::from_rgb(m(c.r()), m(c.g()), m(c.b()))
}

/// Point-in-convex-quad test (screen space).
fn point_in_quad(p: Pos2, q: &[Pos2; 4]) -> bool {
    let mut sign = 0.0f32;
    for i in 0..4 {
        let a = q[i];
        let b = q[(i + 1) % 4];
        let cross = (b.x - a.x) * (p.y - a.y) - (b.y - a.y) * (p.x - a.x);
        if cross.abs() > f32::EPSILON {
            if sign == 0.0 {
                sign = cross.signum();
            } else if cross.signum() != sign {
                return false;
            }
        }
    }
    true
}

/// A block is hit if the cursor is over ANY of its visible faces (top or the two
/// sides) — so tall slender towers select from their bulk, not just the tiny top.
fn point_in_block(p: Pos2, b: &IsoBlock) -> bool {
    point_in_quad(p, &b.top) || point_in_quad(p, &b.right) || point_in_quad(p, &b.front)
}

/// Turn layout tiles into a projected cityscape: keep leaf tiles, extrude each by
/// its file count, add a ground plinth + per-block ground shadow, fit to `panel`,
/// sort back-to-front for correct painter's-order occlusion.
#[allow(clippy::too_many_arguments)]
fn build_scape(
    tree: &Tree,
    tiles: &[Tile],
    dominant: Option<&[FileCategory]>,
    panel: Rect,
    reveal: f32,
    // Optional per-node file counts (for replay's partial state); falls back to
    // the tree's final counts when `None`.
    file_counts: Option<&[u64]>,
) -> Scape {
    use std::collections::HashSet;
    let rendered: HashSet<usize> = tiles.iter().map(|t| t.node.index()).collect();
    let mut leaves: Vec<&Tile> = tiles
        .iter()
        .filter(|t| {
            t.depth > 0
                && tree
                    .children(t.node)
                    .iter()
                    .all(|c| !rendered.contains(&c.index()))
        })
        .collect();
    if leaves.is_empty() {
        return Scape::default();
    }

    // Height ∝ log(file count): files stay flat chips, folders of many files rise
    // into towers — a second signal independent of area (= bytes).
    let fc = |t: &Tile| match file_counts {
        Some(c) => c[t.node.index()],
        None => tree.node(t.node).file_count,
    };
    let max_fc = leaves.iter().map(|t| fc(t)).max().unwrap_or(1).max(1);
    let ln_max = ((max_fc + 1) as f64).ln().max(1.0);
    // Height from file count, but FLATTENED for sliver footprints so a thin tile
    // (inevitable when one folder dominates a drive) can't extrude into a tall
    // "wall". Square-ish footprints (towers) keep full height.
    let height = |t: &Tile| {
        let base = 4.0 + 72.0 * (((fc(t) + 1) as f64).ln() / ln_max);
        let (w, h) = (t.rect.w.max(0.01) as f64, t.rect.h.max(0.01) as f64);
        let aspect = w.max(h) / w.min(h);
        if aspect > 4.0 {
            base * (4.0 / aspect).max(0.08)
        } else {
            base
        }
    };

    // Fit: project every corner (blocks + plinth) to find bounds, then center.
    let (mut minx, mut miny, mut maxx, mut maxy) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    let mut ext = |x: f64, y: f64, z: f64| {
        let (sx, sy) = iso(x, y, z);
        minx = minx.min(sx);
        maxx = maxx.max(sx);
        miny = miny.min(sy);
        maxy = maxy.max(sy);
    };
    for t in &leaves {
        let (x, y, w, h) = (t.rect.x as f64, t.rect.y as f64, t.rect.w as f64, t.rect.h as f64);
        let hz = height(t);
        ext(x, y, hz);
        ext(x + w, y + h, hz);
        ext(x + w, y + h, 0.0);
    }
    for &(cx, cy) in &[(0.0, 0.0), (PLANE, PLANE), (PLANE, 0.0), (0.0, PLANE)] {
        ext(cx, cy, 0.0);
        ext(cx, cy, -PLINTH_TH);
    }

    let pw = (maxx - minx).max(1.0);
    let ph = (maxy - miny).max(1.0);
    let s = ((panel.width() as f64 * 0.96) / pw).min((panel.height() as f64 * 0.96) / ph);
    let ox = panel.center().x as f64 - (minx + maxx) / 2.0 * s;
    let oy = panel.center().y as f64 - (miny + maxy) / 2.0 * s;
    let tr = |wx: f64, wy: f64, wz: f64| -> Pos2 {
        let (sx, sy) = iso(wx, wy, wz);
        Pos2::new((sx * s + ox) as f32, (sy * s + oy) as f32)
    };

    let plinth_top = [tr(0.0, 0.0, 0.0), tr(PLANE, 0.0, 0.0), tr(PLANE, PLANE, 0.0), tr(0.0, PLANE, 0.0)];
    let plinth_right = [tr(PLANE, 0.0, 0.0), tr(PLANE, PLANE, 0.0), tr(PLANE, PLANE, -PLINTH_TH), tr(PLANE, 0.0, -PLINTH_TH)];
    let plinth_front = [tr(0.0, PLANE, 0.0), tr(PLANE, PLANE, 0.0), tr(PLANE, PLANE, -PLINTH_TH), tr(0.0, PLANE, -PLINTH_TH)];

    // Back-to-front: far (small x+y) first.
    leaves.sort_by(|a, b| {
        (a.rect.x + a.rect.y)
            .partial_cmp(&(b.rect.x + b.rect.y))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let blocks = leaves
        .iter()
        .filter_map(|t| {
            let (x, y, w, h) = (t.rect.x as f64, t.rect.y as f64, t.rect.w as f64, t.rect.h as f64);
            // Discovery reveal: blocks appear in the order they were found (arena
            // index = scan order), each rising in its final spot — the closest
            // honest representation of "added one at a time" from a cached tree.
            let hz = if reveal >= 1.0 {
                height(t)
            } else {
                let p = (t.node.index() as f64 / tree.len().max(1) as f64).clamp(0.0, 1.0);
                let lead = 0.85; // wide front so several appear at once
                let local = (reveal as f64 * (1.0 + lead) - p * lead).clamp(0.0, 1.0);
                if local <= 0.0 {
                    return None; // not discovered yet — don't draw it at all
                }
                let ease = 1.0 - (1.0 - local).powi(3); // easeOutCubic
                height(t) * ease
            };
            let color = match dominant {
                Some(dom) => category_color(dom[t.node.index()]),
                None => {
                    let node = tree.node(t.node);
                    if node.kind == NodeKind::Dir {
                        DIR_COLOR
                    } else {
                        category_color(categorize(&node.name))
                    }
                }
            };
            // Ground shadow: footprint expanded and offset by height (tall→long).
            let (e, off) = (SHADOW_EXPAND, hz * SHADOW_OFF);
            Some(IsoBlock {
                node: t.node,
                top: [tr(x, y, hz), tr(x + w, y, hz), tr(x + w, y + h, hz), tr(x, y + h, hz)],
                right: [tr(x + w, y, 0.0), tr(x + w, y + h, 0.0), tr(x + w, y + h, hz), tr(x + w, y, hz)],
                front: [tr(x, y + h, 0.0), tr(x + w, y + h, 0.0), tr(x + w, y + h, hz), tr(x, y + h, hz)],
                shadow: [
                    tr(x - e + off, y - e + off, 0.0),
                    tr(x + w + e + off, y - e + off, 0.0),
                    tr(x + w + e + off, y + h + e + off, 0.0),
                    tr(x - e + off, y + h + e + off, 0.0),
                ],
                color,
            })
        })
        .collect();

    Scape { plinth_top, plinth_right, plinth_front, blocks }
}

/// Open a path's location in Windows Explorer (files get selected; folders open).
#[cfg(target_os = "windows")]
fn reveal_in_explorer(path: &std::path::Path, is_dir: bool) {
    use std::ffi::OsString;
    let arg = if is_dir {
        path.as_os_str().to_os_string()
    } else {
        let mut s = OsString::from("/select,");
        s.push(path.as_os_str());
        s
    };
    let _ = std::process::Command::new("explorer").arg(arg).spawn();
}
#[cfg(not(target_os = "windows"))]
fn reveal_in_explorer(_path: &std::path::Path, _is_dir: bool) {}

/// Open a file/folder with its default handler — the double-click behaviour.
/// `explorer <path>` shell-executes files (default app) and opens folders, with
/// no console-window flash. (Some rare file types may need a fuller ShellExecute;
/// that's a later upgrade.)
#[cfg(target_os = "windows")]
fn open_path(path: &std::path::Path) {
    let _ = std::process::Command::new("explorer").arg(path.as_os_str()).spawn();
}
#[cfg(not(target_os = "windows"))]
fn open_path(_path: &std::path::Path) {}

/// A non-clashing destination path in `dir` for `name`: `name`, else
/// `name - Copy`, `name - Copy (2)`, … inserting the suffix before the
/// extension. Guarantees a FRESH path — paste never overwrites existing data.
fn unique_dest(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let p = Path::new(name);
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_string());
    let ext = p.extension().map(|e| e.to_string_lossy().into_owned());
    let make = |suffix: &str| -> PathBuf {
        let n = match &ext {
            Some(e) => format!("{stem}{suffix}.{e}"),
            None => format!("{stem}{suffix}"),
        };
        dir.join(n)
    };
    let c = make(" - Copy");
    if !c.exists() {
        return c;
    }
    for i in 2..100_000 {
        let c = make(&format!(" - Copy ({i})"));
        if !c.exists() {
            return c;
        }
    }
    make(&format!(" - Copy ({})", now_unix()))
}

/// Is this metadata a symlink or Windows reparse point (junction / mount point)?
/// We neither follow nor materialize these during a copy.
fn is_reparse(md: &std::fs::Metadata) -> bool {
    if md.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        return md.file_attributes() & 0x400 != 0; // FILE_ATTRIBUTE_REPARSE_POINT
    }
    #[cfg(not(windows))]
    false
}

const COPY_MAX_DEPTH: u32 = 512;
const COPY_BUF: usize = 1 << 20; // 1 MiB

/// Copy one file, checking `cancel` between chunks so a large file stays
/// interruptible. `dst` is a fresh path (see [`unique_dest`]).
fn copy_file_cancellable(src: &Path, dst: &Path, cancel: &AtomicBool) -> std::io::Result<()> {
    use std::io::{Read, Write};
    let mut r = std::fs::File::open(src)?;
    let mut w = std::fs::File::create(dst)?;
    let mut buf = vec![0u8; COPY_BUF];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled"));
        }
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        w.write_all(&buf[..n])?;
    }
    Ok(())
}

/// Recursively copy `src` to a fresh `dst`, aborting if `cancel` is set.
/// Symlinks/junctions are SKIPPED (never followed or dereferenced) so a
/// directory junction can't send us into an unrelated or ancestor tree.
/// Depth-capped as a stack-overflow safety net. Assumes `dst` doesn't exist.
fn copy_any(src: &Path, dst: &Path, cancel: &AtomicBool, depth: u32) -> std::io::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled"));
    }
    if depth > COPY_MAX_DEPTH {
        return Err(std::io::Error::other("directory tree too deep to copy"));
    }
    let md = std::fs::symlink_metadata(src)?;
    if is_reparse(&md) {
        return Ok(()); // skip the link/junction rather than follow it
    }
    if md.is_dir() {
        std::fs::create_dir(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_any(&entry.path(), &dst.join(entry.file_name()), cancel, depth + 1)?;
        }
        Ok(())
    } else {
        copy_file_cancellable(src, dst, cancel)
    }
}

/// Remove a path whether it's a file or a directory tree.
fn remove_any(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// Clean up a partial destination after a failed copy — but NOT if the failure
/// was that `dst` already existed (a race: it isn't ours to delete).
fn cleanup_partial(dst: &Path, err: &std::io::Error) {
    if err.kind() != std::io::ErrorKind::AlreadyExists {
        let _ = remove_any(dst);
    }
}

/// Worker body for a paste: copy or move each source into `dest_dir`, each to a
/// unique non-overwriting destination. Safety: a Cut uses an atomic rename on the
/// same volume, else copy-then-delete — and the source is removed ONLY after its
/// copy fully succeeds; if that removal fails, the good copy is kept and the
/// outcome is reported honestly. Links/junctions as a source are refused. On any
/// copy failure the partial (fresh) destination is cleaned up. Returns the last
/// pasted name.
fn run_paste(
    sources: Vec<PathBuf>,
    dest_dir: PathBuf,
    cut: bool,
    cancel: Arc<AtomicBool>,
) -> Result<String, String> {
    let mut last = String::new();
    for src in &sources {
        let name = match src.file_name() {
            Some(n) => n.to_string_lossy().into_owned(),
            None => return Err("Invalid source path.".to_string()),
        };
        // Refuse to copy/move a link or junction as a whole (v1).
        if std::fs::symlink_metadata(src).map(|m| is_reparse(&m)).unwrap_or(false) {
            return Err(format!("“{name}” is a link/junction — not copied."));
        }
        let dst = unique_dest(&dest_dir, &name);

        if cut {
            // Same-volume: atomic rename (fast, preserves junctions inside).
            if std::fs::rename(src, &dst).is_ok() {
                last = dst.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(name);
                continue;
            }
            // Cross-volume: copy, then remove the source.
            if let Err(e) = copy_any(src, &dst, &cancel, 0) {
                cleanup_partial(&dst, &e);
                return Err(format!("Move failed: {e}"));
            }
            if let Err(e) = remove_any(src) {
                // Copy is good; do NOT delete it. Report that the source remains.
                return Err(format!(
                    "Copied “{name}”, but couldn't remove the original ({e}) — both remain."
                ));
            }
        } else if let Err(e) = copy_any(src, &dst, &cancel, 0) {
            cleanup_partial(&dst, &e);
            return Err(format!("Copy failed: {e}"));
        }
        last = dst.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(name);
    }
    Ok(last)
}

/// Enumerate drive roots for the folder-tree sidebar. Uses `GetLogicalDrives`
/// (a kernel bitmask — no per-drive I/O), so a mapped-but-disconnected network
/// drive can't stall the first paint the way probing `metadata()` on it would,
/// and such drives still appear in the tree. Computed once.
#[cfg(target_os = "windows")]
fn enumerate_drives() -> Vec<PathBuf> {
    // kernel32 is always linked on Windows; the bitmask sets bit 0 = A:, 1 = B:…
    extern "system" {
        fn GetLogicalDrives() -> u32;
    }
    let mask = unsafe { GetLogicalDrives() };
    ('A'..='Z')
        .enumerate()
        .filter_map(|(i, c)| (mask & (1 << i) != 0).then(|| PathBuf::from(format!("{c}:\\"))))
        .collect()
}
#[cfg(not(target_os = "windows"))]
fn enumerate_drives() -> Vec<PathBuf> {
    vec![PathBuf::from("/")]
}

/// Is `path` on a network drive? Network locations have NO Recycle Bin, so a
/// "delete" there is permanent — the confirmation must say so. Uses
/// `GetDriveTypeW` on the volume root (mapped drive "Y:\" or UNC "\\srv\share\").
#[cfg(target_os = "windows")]
fn is_network_path(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    extern "system" {
        fn GetDriveTypeW(root: *const u16) -> u32;
    }
    const DRIVE_REMOTE: u32 = 4;
    let mut root = std::ffi::OsString::new();
    for comp in path.components() {
        use std::path::Component::*;
        match comp {
            Prefix(p) => root.push(p.as_os_str()),
            RootDir => {
                root.push("\\");
                break;
            }
            _ => break,
        }
    }
    if root.is_empty() {
        return false;
    }
    let wide: Vec<u16> = root.encode_wide().chain(std::iter::once(0)).collect();
    unsafe { GetDriveTypeW(wide.as_ptr()) == DRIVE_REMOTE }
}
#[cfg(not(target_os = "windows"))]
fn is_network_path(_path: &Path) -> bool {
    false
}

/// Hand-drawn tooltip near the cursor. egui's widget tooltip anchors to the
/// widget rect — our widget is the whole panel, so it would land in the corner;
/// we draw our own at the pointer instead.
fn draw_tooltip(painter: &egui::Painter, area: Rect, at: Pos2, line1: &str, line2: &str) {
    let c1 = Color32::from_gray(225);
    let c2 = Color32::from_gray(180);
    let g1 = painter.layout_no_wrap(line1.to_owned(), egui::FontId::monospace(12.0), c1);
    let g2 = painter.layout_no_wrap(line2.to_owned(), egui::FontId::proportional(12.0), c2);
    let (s1, s2) = (g1.size(), g2.size());
    let pad = Vec2::new(8.0, 6.0);
    let box_size = Vec2::new(s1.x.max(s2.x), s1.y + s2.y + 2.0) + pad * 2.0;
    let mut o = at + Vec2::new(16.0, 18.0);
    if o.x + box_size.x > area.right() {
        o.x = (at.x - box_size.x - 12.0).max(area.left());
    }
    if o.y + box_size.y > area.bottom() {
        o.y = (at.y - box_size.y - 12.0).max(area.top());
    }
    let bg = Rect::from_min_size(o, box_size);
    painter.rect_filled(bg, 4.0_f32, Color32::from_rgba_premultiplied(16, 18, 24, 242));
    painter.galley(bg.min + pad, g1, c1);
    painter.galley(bg.min + pad + Vec2::new(0.0, s1.y + 2.0), g2, c2);
}

impl eframe::App for SectorApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let s = Settings {
            replay_secs: self.replay_secs,
            threads: self.threads,
            replay_mode: self.anim_mode == AnimMode::Replay,
            current_dir: self.current_dir.to_string_lossy().into_owned(),
            show_hidden: self.show_hidden,
        };
        eframe::set_value(storage, "settings", &s);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Reflect the current folder in the window/taskbar title (only on change).
        let title = format!("{} — {}", sector_core::APP_NAME, self.current_dir.display());
        if title != self.last_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }

        // Poll the background scan for completion.
        let mut just_finished = false;
        if let ScanState::Running { stats_rx, stats, .. } = &mut self.scan {
            if stats.is_none() {
                match stats_rx.try_recv() {
                    Ok(s) => {
                        *stats = Some(s);
                        just_finished = true;
                    }
                    Err(_) => ctx.request_repaint(), // keep progress + live build ticking
                }
            }
        }
        if just_finished {
            self.crystallize = true;
            // Compute dominant content category per node now (one O(n) pass on
            // the finished tree) so folders can be colored by what they contain.
            if let ScanState::Running { tree, stats, .. } = &self.scan {
                let t = tree.lock().unwrap_or_else(|e| e.into_inner());
                self.dominant = Some(t.dominant_categories());
                let root_name = t.node(Tree::ROOT).name.to_string();
                drop(t);
                // Queue the cache write to run OFF the UI thread (spawned at the
                // end of this frame so the crystallize re-layout gets the lock
                // first). Skip cancelled/partial scans.
                if let Some(st) = stats {
                    if !st.cancelled {
                        if let Some(cp) = cache_path_for(&root_name) {
                            let mark = self.pending_usn_mark.unwrap_or(UsnMark::NONE);
                            let cs = CacheStats {
                                dirs: st.dirs,
                                files: st.files,
                                bytes: st.bytes,
                                saved_unix: now_unix(),
                                usn_journal_id: mark.journal_id,
                                usn_next: mark.next_usn,
                            };
                            self.pending_save = Some((Arc::clone(tree), cp, cs));
                        } else {
                            eprintln!("[sector] cache: no cache dir available");
                        }
                    }
                }
            }
            // The finished scan now covers the current folder — surface its
            // subfolder sizes in the list.
            self.recompute_folder_sizes();
            self.refresh_status_summary();
        }

        // Poll a background paste (E5) for completion.
        let mut paste_done: Option<Result<String, String>> = None;
        let mut paste_was_cut = false;
        if let Some(job) = &self.paste_job {
            match job.rx.try_recv() {
                Ok(r) => {
                    paste_done = Some(r);
                    paste_was_cut = job.cut;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => ctx.request_repaint(),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    paste_done = Some(Err("Paste worker stopped unexpectedly.".into()));
                }
            }
        }
        if let Some(result) = paste_done {
            self.paste_job = None;
            // A cut consumes the clipboard whatever the outcome: on partial
            // failure some sources have already moved, so the remaining paths are
            // stale and must not be re-pasted.
            if paste_was_cut {
                self.clipboard = None;
            }
            match result {
                Ok(name) => {
                    // Refresh listing + tree, clear the filter so the pasted item
                    // is visible, and select it by name.
                    self.after_edit(name);
                    self.op_error = None;
                }
                Err(e) => {
                    self.op_error = Some(e);
                    // Some items may still have changed on disk — refresh the view.
                    self.entries_dirty = true;
                    self.sb_cache.clear();
                }
            }
        }

        // Poll a background delete (E5) for completion.
        let mut delete_done: Option<Result<(), String>> = None;
        if let Some(rx) = &self.delete_job {
            match rx.try_recv() {
                Ok(r) => delete_done = Some(r),
                Err(std::sync::mpsc::TryRecvError::Empty) => ctx.request_repaint(),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    delete_done = Some(Err("Delete worker stopped unexpectedly.".into()));
                }
            }
        }
        if let Some(result) = delete_done {
            self.delete_job = None;
            // Either way, the folder may have changed on disk — refresh the view.
            self.entries_dirty = true;
            self.sb_cache.clear();
            match result {
                Ok(()) => {
                    self.clear_selection(); // items are gone
                    self.op_error = None;
                }
                Err(e) => self.op_error = Some(e), // keep selection to retry
            }
        }

        // ---- Top strip: identity + view mode + shared navigation -----------
        egui::Panel::top("mode").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong(sector_core::APP_NAME);
                ui.separator();
                ui.selectable_value(&mut self.view, View::List, "Files");
                ui.selectable_value(&mut self.view, View::City, "City");
                ui.separator();
                self.nav_bar(ui); // back / forward / up + address (both views)
            });
        });

        if self.view == View::List {
            self.show_list(ui);
        }

        // ---- City view (the original visualizer) ---------------------------
        if self.view == View::City {
        // E2: keep the City pointed at the folder you're browsing. When the
        // location drifts (address bar, up/back, or a Files navigation), re-sync
        // — instant cache-load if available, else drop to a scan-prompt.
        if self.city_synced_dir.as_deref() != Some(self.current_dir.as_path()) {
            self.sync_city();
        }
        // ---- Top bar --------------------------------------------------------
        egui::Panel::top("bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                let scanning = matches!(&self.scan, ScanState::Running { stats: None, .. });
                ui.add_enabled_ui(!scanning, |ui| {
                    let scanned = self.city_root.as_deref() == Some(self.current_dir.as_path())
                        && matches!(&self.scan, ScanState::Running { stats: Some(_), .. });
                    let scan_label = if scanned { "Rescan" } else { "Scan" };
                    if ui
                        .button(scan_label)
                        .on_hover_text("Deep-scan this folder into a cityscape (slow for a big tree; cached afterwards).")
                        .clicked()
                    {
                        self.start_scan();
                    }
                    ui.add(
                        egui::DragValue::new(&mut self.threads)
                            .range(1..=256)
                            .prefix("threads "),
                    )
                    .on_hover_text("Worker threads for the scan. Higher hides SMB latency on a cold NAS; benchmark on a subfolder to find the sweet spot.");

                    ui.separator();
                    ui.selectable_value(&mut self.anim_mode, AnimMode::Reveal, "Reveal")
                        .on_hover_text("Cache-load animation: blocks rise into place in discovery order (smooth).");
                    ui.selectable_value(&mut self.anim_mode, AnimMode::Replay, "Replay")
                        .on_hover_text("Cache-load animation: replays the scan — the structure grows and re-lays-out (authentic, jumpier).");
                    if self.anim_mode == AnimMode::Replay {
                        ui.add(
                            egui::Slider::new(&mut self.replay_secs, 1.0..=60.0)
                                .suffix("s")
                                .text("dur"),
                        )
                        .on_hover_text("Replay duration — drag to change the pace, then Load cached again.");
                    }

                    // Offer an instant load if a cache exists for this folder
                    // (age precomputed in sync_city — no per-frame stat).
                    if let Some(age) = self.cache_mtime {
                        if ui
                            .button(format!("Load cached · {}", humanize_age(age)))
                            .on_hover_text("Reopen the last scan of this path instantly, without walking the filesystem.")
                            .clicked()
                        {
                            self.load_cached();
                        }
                    }
                    // E6: freshness of the loaded cache vs the live NTFS volume.
                    match self.cache_freshness {
                        Freshness::Stale => {
                            ui.separator();
                            ui.colored_label(egui::Color32::from_rgb(230, 170, 80), "volume changed")
                                .on_hover_text(
                                    "Something on this volume changed since the scan (per the USN \
                                     journal). This is volume-wide, so it may be unrelated activity \
                                     — Rescan if you want the City refreshed.",
                                );
                        }
                        Freshness::Current => {
                            ui.separator();
                            ui.colored_label(egui::Color32::from_rgb(120, 200, 120), "up to date")
                                .on_hover_text("No changes on this volume since the scan (per the NTFS USN journal).");
                        }
                        Freshness::Unknown => {}
                    }
                });
                if let ScanState::Running { cancel, stats: None, .. } = &self.scan {
                    if ui.button("Cancel").clicked() {
                        cancel.store(true, Ordering::Relaxed);
                    }
                }
            });

            match &self.scan {
                ScanState::Idle => {
                    ui.label(format!(
                        "No cityscape for {} yet — press Scan to build one.",
                        self.current_dir.display()
                    ));
                }
                ScanState::Running {
                    tree,
                    progress,
                    stats,
                    started,
                    ..
                } => match stats {
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(format!(
                                "Scanning… {} dirs, {} files, {} ({:.1}s)",
                                commas(progress.dirs.load(Ordering::Relaxed)),
                                commas(progress.files.load(Ordering::Relaxed)),
                                human_size(progress.bytes.load(Ordering::Relaxed)),
                                started.elapsed().as_secs_f32()
                            ));
                        });
                    }
                    Some(st) => {
                        let t = tree.lock().unwrap_or_else(|e| e.into_inner());
                        ui.horizontal_wrapped(|ui| {
                            if self.root != Tree::ROOT && ui.button("↑ Up").clicked() {
                                self.root = t.node(self.root).parent;
                            }
                            for (i, id) in Self::breadcrumb(&t, self.root).into_iter().enumerate() {
                                if i > 0 {
                                    ui.label("›");
                                }
                                if ui.link(t.node(id).name.as_ref()).clicked() {
                                    self.root = id;
                                }
                            }
                            ui.separator();
                            let mut note = String::new();
                            if st.errors > 0 {
                                note.push_str(&format!(" · {} unreadable", commas(st.errors)));
                            }
                            if st.cancelled {
                                note.push_str(" · (cancelled)");
                            }
                            ui.weak(format!(
                                "{} dirs · {} files · {} · {:.1}s{note}",
                                commas(st.dirs),
                                commas(st.files),
                                human_size(st.bytes),
                                st.elapsed.as_secs_f32(),
                            ));
                        });
                    }
                },
            }
        });

        // ---- Legend ---------------------------------------------------------
        egui::Panel::bottom("legend").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.weak("Color = file type:");
                for cat in FileCategory::ALL {
                    ui.colored_label(category_color(cat), format!("■ {}", cat.label()));
                }
                ui.separator();
                ui.weak("area = size · height = file count · hover for details · click to drill in");
            });
        });

        // ---- Central: the treemap -------------------------------------------
        egui::CentralPanel::default().show(ui, |ui| {
            let ScanState::Running { tree, stats, .. } = &self.scan else {
                ui.centered_and_justified(|ui| {
                    ui.weak("No scan yet.");
                });
                return;
            };
            let scanning = stats.is_none();

            // Explicit STABLE widget id (not allocate_painter's auto id), so the
            // right-click context menu — which egui keys on this id — stays open
            // across frames instead of flashing shut.
            let area = ui.available_rect_before_wrap();
            let response = ui.interact(area, egui::Id::new("sector_canvas"), Sense::click());
            let painter = ui.painter_at(area);

            // Backspace goes up a level (ignored while typing in the path box).
            if self.root != Tree::ROOT
                && ctx.memory(|m| m.focused()).is_none()
                && ctx.input(|i| i.key_pressed(egui::Key::Backspace))
            {
                let t = tree.lock().unwrap_or_else(|e| e.into_inner());
                self.root = t.node(self.root).parent;
            }

            // (Re)build the cityscape when the view changed, on crystallize, or —
            // while scanning — at most once per throttle interval.
            // Cache-load animation progress (0..1). Replay runs longer.
            let dur = if self.anim_mode == AnimMode::Replay {
                self.replay_secs.max(0.2)
            } else {
                REVEAL_SECS
            };
            let reveal = self
                .reveal_start
                .map(|t| (t.elapsed().as_secs_f32() / dur).min(1.0))
                .unwrap_or(1.0);
            let revealing = self.reveal_start.is_some();
            if revealing && reveal < 1.0 {
                ctx.request_repaint();
            }

            let plane = sector_core::Rect::new(0.0, 0.0, PLANE as f32, PLANE as f32);

            if revealing && reveal < 1.0 && self.anim_mode == AnimMode::Replay {
                // REPLAY: throttled partial-layout steps — the tree "grows" and
                // re-lays-out, so the structure visibly evolves (like a fast scan).
                let restep = self.scape.blocks.is_empty()
                    || self.last_size != area.size()
                    || self.last_root != self.root
                    || self.last_layout.elapsed() >= REPLAY_STEP;
                if restep {
                    let t = tree.lock().unwrap_or_else(|e| e.into_inner());
                    let k = (((reveal as f64) * t.len() as f64) as usize).max(1);
                    let (sizes, counts) = t.partial_metrics(k);
                    let tiles = layout_partial(&t, self.root, plane, &self.opts, k, &sizes);
                    self.scape =
                        build_scape(&t, &tiles, self.dominant.as_deref(), area, 1.0, Some(&counts));
                    drop(t);
                    self.last_layout = Instant::now();
                    self.last_size = area.size();
                    self.last_root = self.root;
                }
            } else {
                // Normal build. In Reveal mode the per-block "rise" comes from the
                // reveal factor; otherwise full height. When a Replay ends, this
                // branch runs once at reveal==1.0 to draw the complete tree.
                let rev = if revealing && self.anim_mode == AnimMode::Reveal { reveal } else { 1.0 };
                let need = revealing
                    || self.scape.blocks.is_empty()
                    || self.last_root != self.root
                    || self.last_size != area.size()
                    || self.crystallize
                    || (scanning && self.last_layout.elapsed() >= RELAYOUT_THROTTLE);
                if need {
                    let t = tree.lock().unwrap_or_else(|e| e.into_inner());
                    self.tiles = layout(&t, self.root, plane, &self.opts);
                    self.scape =
                        build_scape(&t, &self.tiles, self.dominant.as_deref(), area, rev, None);
                    drop(t);
                    self.last_layout = Instant::now();
                    self.last_root = self.root;
                    self.last_size = area.size();
                    self.crystallize = false;
                }
            }
            if reveal >= 1.0 {
                self.reveal_start = None; // animation complete
            }

            // Night sky + ground plinth (behind the city).
            painter.rect_filled(area, 0.0_f32, BG);
            let edge = Stroke::new(0.5, BORDER);
            painter.add(Shape::convex_polygon(self.scape.plinth_right.to_vec(), PLINTH_R, edge));
            painter.add(Shape::convex_polygon(self.scape.plinth_front.to_vec(), PLINTH_F, edge));
            painter.add(Shape::convex_polygon(self.scape.plinth_top.to_vec(), PLINTH_TOP, edge));

            // Topmost (nearest) block under the cursor.
            let hover_pos = response.hover_pos();
            let hovered =
                hover_pos.and_then(|p| self.scape.blocks.iter().rposition(|b| point_in_block(p, b)));

            // Draw back-to-front: shadow, then the three shaded faces. Draw the
            // hover highlight IN-ORDER (right after its block) so nearer buildings
            // occlude it correctly instead of it floating over everything.
            for (i, b) in self.scape.blocks.iter().enumerate() {
                painter.add(Shape::convex_polygon(b.shadow.to_vec(), SHADOW, Stroke::NONE));
                painter.add(Shape::convex_polygon(b.right.to_vec(), shade(b.color, F_RIGHT), edge));
                painter.add(Shape::convex_polygon(b.front.to_vec(), shade(b.color, F_FRONT), edge));
                painter.add(Shape::convex_polygon(b.top.to_vec(), shade(b.color, F_TOP), edge));
                if Some(i) == hovered {
                    // Halo outline: a dark stroke under a bright one, so the
                    // highlight is visible on ANY block color (incl. orange-on-
                    // orange video, or the light Document tiles).
                    let dark = Stroke::new(3.2, Color32::from_black_alpha(190));
                    let bright = Stroke::new(1.6, Color32::WHITE);
                    let faces = [&b.right, &b.front, &b.top];
                    for f in faces {
                        painter.add(Shape::convex_polygon(f.to_vec(), Color32::TRANSPARENT, dark));
                    }
                    for f in faces {
                        painter.add(Shape::convex_polygon(f.to_vec(), Color32::TRANSPARENT, bright));
                    }
                }
            }

            // Hovered block: tooltip (drawn at the cursor) + left-click to drill.
            if let (Some(i), Some(p)) = (hovered, hover_pos) {
                let node_id = self.scape.blocks[i].node;
                let (path, size, kind_str, drillable) = {
                    let t = tree.lock().unwrap_or_else(|e| e.into_inner());
                    let node = t.node(node_id);
                    let is_dir = node.kind == NodeKind::Dir;
                    let n_children = t.children(node_id).len();
                    let kind_str = if is_dir {
                        format!("{} items · {} files", commas(n_children as u64), commas(node.file_count))
                    } else {
                        categorize(&node.name).label().to_string()
                    };
                    (
                        joined_path(&t.path_components(node_id)),
                        node.subtree_size,
                        kind_str,
                        is_dir && n_children > 0,
                    )
                };
                if !response.context_menu_opened() {
                    let line2 = format!("{}  ·  {}", human_size(size), kind_str);
                    draw_tooltip(&painter, area, p, &path, &line2);
                }
                if drillable {
                    ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                if response.clicked() && drillable {
                    self.root = node_id;
                }
            }

            // Right-click a block → context menu → reveal its location in Explorer.
            if response.secondary_clicked() {
                self.menu_target = response.interact_pointer_pos().and_then(|p| {
                    self.scape.blocks.iter().rev().find(|b| point_in_block(p, b)).map(|b| {
                        let t = tree.lock().unwrap_or_else(|e| e.into_inner());
                        let is_dir = t.node(b.node).kind == NodeKind::Dir;
                        let mut pb = PathBuf::new();
                        for comp in t.path_components(b.node) {
                            pb.push(comp);
                        }
                        (pb, is_dir)
                    })
                });
            }
            response.context_menu(|ui| match &self.menu_target {
                Some((path, is_dir)) => {
                    if ui.button("Reveal in Explorer").clicked() {
                        reveal_in_explorer(path, *is_dir);
                        ui.close();
                    }
                    if ui.button("Copy path").clicked() {
                        ui.ctx().copy_text(path.to_string_lossy().into_owned());
                        ui.close();
                    }
                }
                None => {
                    ui.label("Nothing here");
                }
            });

            if scanning {
                ctx.request_repaint();
            }
        });

        // E2: reflect any in-City drill (block click / breadcrumb) back into the
        // shared location, so Files follows — without re-triggering a re-sync.
        let drilled: Option<PathBuf> =
            if let ScanState::Running { tree, stats: Some(_), .. } = &self.scan {
                if self.root == Tree::ROOT {
                    self.city_root.clone()
                } else {
                    let t = tree.lock().unwrap_or_else(|e| e.into_inner());
                    Some(PathBuf::from(joined_path(&t.path_components(self.root))))
                }
            } else {
                None
            };
        if let Some(t) = drilled {
            // Route through navigate_to so a City drill joins the shared history
            // (Back undoes it), updates the address bar, and reveals the Files
            // tree — then mark it synced so this doesn't trigger a City re-sync.
            if t != self.current_dir {
                self.navigate_to(t);
            }
            self.city_synced_dir = Some(self.current_dir.clone());
        }
        } // end City view

        // ---- Delete confirmation (E5) --------------------------------------
        if let Some(paths) = self.confirm_delete.take() {
            let mut confirm = false;
            let mut cancel = false;
            // Network drives have no Recycle Bin — such a delete is PERMANENT.
            let permanent = paths.iter().any(|p| is_network_path(p));
            let what = if paths.len() == 1 {
                paths[0]
                    .file_name()
                    .map(|n| format!("“{}”", n.to_string_lossy()))
                    .unwrap_or_else(|| "1 item".into())
            } else {
                format!("{} items", paths.len())
            };
            let modal = egui::Modal::new(egui::Id::new("sector_delete_confirm")).show(&ctx, |ui| {
                ui.set_width(380.0);
                if permanent {
                    ui.strong("Permanently delete?");
                    ui.add_space(6.0);
                    ui.colored_label(
                        egui::Color32::from_rgb(224, 108, 108),
                        format!(
                            "⚠ {what} on a network drive will be PERMANENTLY deleted — \
                             network drives have no Recycle Bin, so this can't be undone."
                        ),
                    );
                } else {
                    ui.strong("Move to Recycle Bin?");
                    ui.add_space(6.0);
                    ui.label(format!("{what} will be moved to the Recycle Bin."));
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    let label = if permanent { "Delete permanently" } else { "Move to Recycle Bin" };
                    if ui.button(label).clicked() {
                        confirm = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
                // Enter confirms a recoverable recycle; a PERMANENT delete must be
                // an explicit click (no accidental Enter). Esc/backdrop cancels.
                if !permanent && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    confirm = true;
                }
            });
            if confirm {
                self.start_delete(paths);
            } else if !(cancel || modal.should_close()) {
                self.confirm_delete = Some(paths); // keep the dialog open
            }
        }

        // ---- New folder / Rename dialog (E4) --------------------------------
        if let Some(mut prompt) = self.prompt.take() {
            let title = match &prompt.kind {
                PromptKind::NewFolder => "New folder",
                PromptKind::Rename { .. } => "Rename",
            };
            let mut commit = false;
            let mut cancel = false;
            let modal = egui::Modal::new(egui::Id::new("sector_name_prompt")).show(&ctx, |ui| {
                ui.set_width(340.0);
                ui.strong(title);
                ui.add_space(6.0);
                let edit_id = egui::Id::new("sector_name_prompt_edit");
                let r = ui.add(
                    egui::TextEdit::singleline(&mut prompt.buf)
                        .id(edit_id)
                        .desired_width(f32::INFINITY)
                        .hint_text("Name"),
                );
                if prompt.focus {
                    r.request_focus();
                    // Preselect: for a rename, the stem (up to the last dot) so
                    // typing replaces the name but keeps the extension — like
                    // Explorer; for a new folder, the whole name.
                    let end = match &prompt.kind {
                        PromptKind::Rename { .. } => match prompt.buf.rfind('.') {
                            Some(dot) if dot > 0 => prompt.buf[..dot].chars().count(),
                            _ => prompt.buf.chars().count(),
                        },
                        PromptKind::NewFolder => prompt.buf.chars().count(),
                    };
                    if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), edit_id) {
                        let range = egui::text::CCursorRange::two(
                            egui::text::CCursor::new(0),
                            egui::text::CCursor::new(end),
                        );
                        state.cursor.set_char_range(Some(range));
                        egui::TextEdit::store_state(ui.ctx(), edit_id, state);
                    }
                    prompt.focus = false;
                }
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    commit = true;
                }
                if let Some(e) = &prompt.error {
                    ui.add_space(4.0);
                    ui.colored_label(egui::Color32::from_rgb(224, 108, 108), e);
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("OK").clicked() {
                        commit = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
            // Esc or a click on the backdrop closes (cancels) the dialog.
            if cancel || modal.should_close() {
                // dropped: prompt stays taken → closed
            } else if commit {
                match self.commit_prompt(&prompt) {
                    Ok(()) => {} // success → closed
                    Err(e) => {
                        prompt.error = Some(e);
                        prompt.focus = true;
                        self.prompt = Some(prompt); // keep open, show the error
                    }
                }
            } else {
                self.prompt = Some(prompt); // still open
            }
        }

        // Kick off the queued cache write now that this frame's render (and its
        // tree lock) is done, so serialization on the background thread doesn't
        // block the crystallize re-layout.
        if let Some((arc, cp, cs)) = self.pending_save.take() {
            std::thread::spawn(move || {
                let result = {
                    let g = arc.lock().unwrap_or_else(|e| e.into_inner());
                    g.to_cache_bytes(cs)
                };
                match result {
                    Ok(bytes) => {
                        // Create the cache dir here (cache_path_for is pure now).
                        if let Some(parent) = cp.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        match std::fs::write(&cp, &bytes) {
                            Ok(()) => {
                                eprintln!("[sector] cache saved: {} ({} bytes)", cp.display(), bytes.len());
                                // Keep the cache dir bounded (1 GiB), newest-first.
                                if let Some(parent) = cp.parent() {
                                    prune_cache_dir(parent, &cp, 1_073_741_824);
                                }
                            }
                            Err(e) => eprintln!("[sector] cache SAVE (write) FAILED: {e}"),
                        }
                    }
                    Err(e) => eprintln!("[sector] cache SAVE (serialize) FAILED: {e}"),
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "sector-paste-{}-{}-{}",
            tag,
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn unique_dest_never_clashes() {
        let d = scratch("uniq");
        assert_eq!(unique_dest(&d, "a.txt"), d.join("a.txt")); // free
        std::fs::write(d.join("a.txt"), b"x").unwrap();
        assert_eq!(unique_dest(&d, "a.txt"), d.join("a - Copy.txt")); // clash → suffix before ext
        std::fs::write(d.join("a - Copy.txt"), b"x").unwrap();
        assert_eq!(unique_dest(&d, "a.txt"), d.join("a - Copy (2).txt"));
        // No-extension (folder) name.
        std::fs::create_dir(d.join("dir")).unwrap();
        assert_eq!(unique_dest(&d, "dir"), d.join("dir - Copy"));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn paste_copy_preserves_source_and_never_overwrites() {
        let d = scratch("copy");
        // A folder tree to copy.
        std::fs::create_dir(d.join("src")).unwrap();
        std::fs::write(d.join("src/f.txt"), b"hello").unwrap();
        std::fs::create_dir(d.join("src/sub")).unwrap();
        std::fs::write(d.join("src/sub/g.bin"), vec![7u8; 10]).unwrap();
        let dest = d.join("dest");
        std::fs::create_dir(&dest).unwrap();

        let cancel = Arc::new(AtomicBool::new(false));
        let name = run_paste(vec![d.join("src")], dest.clone(), false, cancel.clone()).unwrap();
        assert_eq!(name, "src");
        assert!(d.join("src/f.txt").exists()); // source preserved (copy)
        assert_eq!(std::fs::read(dest.join("src/f.txt")).unwrap(), b"hello");
        assert!(dest.join("src/sub/g.bin").exists());

        // Paste again → must NOT overwrite; lands as "src - Copy".
        let name2 = run_paste(vec![d.join("src")], dest.clone(), false, cancel).unwrap();
        assert_eq!(name2, "src - Copy");
        assert!(dest.join("src/f.txt").exists()); // first copy untouched
        assert!(dest.join("src - Copy/f.txt").exists());
        std::fs::remove_dir_all(&d).ok();
    }

    #[cfg(unix)]
    #[test]
    fn paste_skips_and_refuses_symlinks() {
        use std::os::unix::fs::symlink;
        let d = scratch("link");
        // A source folder containing a symlink to an OUTSIDE directory.
        let outside = d.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"do not copy me").unwrap();
        std::fs::create_dir(d.join("src")).unwrap();
        std::fs::write(d.join("src/real.txt"), b"ok").unwrap();
        symlink(&outside, d.join("src/link")).unwrap();

        let dest = d.join("dest");
        std::fs::create_dir(&dest).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));

        // Copying the folder skips the symlink (never follows into `outside`).
        run_paste(vec![d.join("src")], dest.clone(), false, cancel.clone()).unwrap();
        assert!(dest.join("src/real.txt").exists());
        assert!(!dest.join("src/link").exists()); // link skipped
        assert!(!dest.join("src/link/secret.txt").exists()); // target NOT copied

        // A symlink AS the source is refused outright.
        let err = run_paste(vec![d.join("src/link")], dest, false, cancel).unwrap_err();
        assert!(err.contains("link/junction"));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn paste_cut_moves_and_removes_source() {
        let d = scratch("cut");
        std::fs::write(d.join("m.txt"), b"data").unwrap();
        let dest = d.join("dest");
        std::fs::create_dir(&dest).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let name = run_paste(vec![d.join("m.txt")], dest.clone(), true, cancel).unwrap();
        assert_eq!(name, "m.txt");
        assert!(!d.join("m.txt").exists()); // source gone (moved)
        assert_eq!(std::fs::read(dest.join("m.txt")).unwrap(), b"data");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn datetime_known_values() {
        assert_eq!(format_datetime(UNIX_EPOCH), "1970-01-01 00:00 UTC");
        // 2021-01-01 00:00:00 UTC = 1_609_459_200 s.
        let t = UNIX_EPOCH + Duration::from_secs(1_609_459_200);
        assert_eq!(format_datetime(t), "2021-01-01 00:00 UTC");
        // + 13h 37m.
        let t2 = UNIX_EPOCH + Duration::from_secs(1_609_459_200 + 13 * 3600 + 37 * 60);
        assert_eq!(format_datetime(t2), "2021-01-01 13:37 UTC");
    }

    #[test]
    fn breadcrumb_drive_paths() {
        let segs = breadcrumb_segments(Path::new(r"C:\Users\Docs"));
        assert_eq!(
            segs,
            vec![
                ("C:".to_string(), PathBuf::from(r"C:\")),
                ("Users".to_string(), PathBuf::from(r"C:\Users")),
                ("Docs".to_string(), PathBuf::from(r"C:\Users\Docs")),
            ]
        );
        // Drive root stays a root ("C:\", not the drive-relative "C:").
        let root = breadcrumb_segments(Path::new(r"C:\"));
        assert_eq!(root, vec![("C:".to_string(), PathBuf::from(r"C:\"))]);
    }

    #[test]
    fn breadcrumb_unc_paths() {
        let segs = breadcrumb_segments(Path::new(r"\\nas\Media\Manga"));
        assert_eq!(
            segs,
            vec![
                (r"\\nas\Media".to_string(), PathBuf::from(r"\\nas\Media")),
                ("Manga".to_string(), PathBuf::from(r"\\nas\Media\Manga")),
            ]
        );
        // A malformed \\server with no share yields nothing to click.
        assert!(breadcrumb_segments(Path::new(r"\\server")).is_empty());
    }
}
