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
use sector_scan::{list_dir, scan_into, Entry, Progress, ScanOptions, ScanStats};

/// Which view the content area shows.
#[derive(Clone, Copy, PartialEq)]
enum View {
    /// The file-explorer list (the new default; live navigation).
    List,
    /// The cityscape visualization (the original tool, now a mode).
    City,
}

/// File-list sort column.
#[derive(Clone, Copy, PartialEq)]
enum SortKey {
    Name,
    Size,
    Kind,
    Modified,
}

/// A short human "kind" for a listing entry.
fn entry_kind(e: &Entry) -> String {
    if e.is_dir {
        "Folder".to_string()
    } else {
        categorize(&e.name).label().to_string()
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
fn joined_path(comps: &[&str]) -> String {
    comps
        .iter()
        .map(|c| c.trim_end_matches(['\\', '/']))
        .collect::<Vec<_>>()
        .join(std::path::MAIN_SEPARATOR_STR)
}

/// The cache file path for a given scan root, or `None` if the cache dir is
/// unavailable. Keyed by a hash of the (normalized) path.
fn cache_path_for(root: &str) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    let dir = dirs::cache_dir()?.join("sector");
    std::fs::create_dir_all(&dir).ok()?;
    // Normalize so "Y:" and "Y:\" (and case) map to the same cache key. Appending
    // the separator (rather than stripping it) keeps existing "X:\" caches valid.
    let mut key = root.trim().to_lowercase();
    if key.ends_with(':') {
        key.push('\\');
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    Some(dir.join(format!("{:016x}.bin", h.finish())))
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
    entries: Vec<Entry>,
    entries_err: Option<String>,
    entries_dirty: bool,
    back_stack: Vec<PathBuf>,
    fwd_stack: Vec<PathBuf>,
    sort_key: SortKey,
    sort_asc: bool,
    selected: Option<usize>,
    /// True while the address bar has (or just lost) focus — suppresses the file
    /// list's Enter/Backspace shortcuts so they don't fight the address bar.
    addr_active: bool,

    // ---- Folder-tree sidebar (E1b.2, Files view only — D19) ----
    sb_visible: bool,
    /// Drive roots (C:\, Y:\, …), enumerated once (empty = not yet computed).
    sb_roots: Vec<PathBuf>,
    /// Which tree nodes are expanded.
    sb_expanded: HashSet<PathBuf>,
    /// Lazily-filled cache: dir → its immediate subdirectories (sorted).
    sb_cache: HashMap<PathBuf, Vec<PathBuf>>,
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
            entries: Vec::new(),
            entries_err: None,
            entries_dirty: true,
            back_stack: Vec::new(),
            fwd_stack: Vec::new(),
            sort_key: SortKey::Name,
            sort_asc: true,
            selected: None,
            addr_active: false,
            sb_visible: true,
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
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            replay_secs: REPLAY_SECS,
            threads: DEFAULT_THREADS,
            replay_mode: false,
            current_dir: "C:\\".to_string(),
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
    fn load_cached(&mut self) {
        let Some(cp) = cache_path_for(&self.current_dir.to_string_lossy()) else {
            eprintln!("[sector] load_cached: no cache dir");
            return;
        };
        let bytes = match std::fs::read(&cp) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[sector] load_cached: read {} failed: {e}", cp.display());
                return;
            }
        };
        eprintln!("[sector] load_cached: read {} bytes from {}", bytes.len(), cp.display());
        let Some((tree, cs)) = Tree::from_cache_bytes(&bytes) else {
            eprintln!("[sector] load_cached: DESERIALIZE FAILED");
            return;
        };
        eprintln!("[sector] load_cached: OK — {} nodes, {} files", tree.len(), cs.files);
        let tree = Arc::new(Mutex::new(tree));
        self.dominant = Some(tree.lock().expect("tree").dominant_categories());
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
    }

    /// Point the City at `current_dir` (E2). Instant cache-load if one exists;
    /// otherwise drop to an idle scan-prompt (a full scan stays deliberate).
    fn sync_city(&mut self) {
        let cur = self.current_dir.clone();
        let has_cache = cache_path_for(&cur.to_string_lossy())
            .map(|p| p.exists())
            .unwrap_or(false);
        if has_cache {
            self.load_cached();
        } else {
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
        self.selected = None;
        self.sync_addr();
        self.sb_reveal();
    }

    fn go_back(&mut self) {
        if let Some(p) = self.back_stack.pop() {
            self.fwd_stack.push(std::mem::replace(&mut self.current_dir, p));
            self.entries_dirty = true;
            self.selected = None;
            self.sync_addr();
            self.sb_reveal();
        }
    }

    fn go_forward(&mut self) {
        if let Some(p) = self.fwd_stack.pop() {
            self.back_stack.push(std::mem::replace(&mut self.current_dir, p));
            self.entries_dirty = true;
            self.selected = None;
            self.sync_addr();
            self.sb_reveal();
        }
    }

    fn go_up(&mut self) {
        if let Some(parent) = self.current_dir.parent().map(|p| p.to_path_buf()) {
            self.navigate_to(parent);
        }
    }

    fn sort_entries(&self, es: &mut [Entry]) {
        let (key, asc) = (self.sort_key, self.sort_asc);
        es.sort_by(|a, b| {
            // Folders always first, then by the chosen key.
            b.is_dir.cmp(&a.is_dir).then_with(|| {
                let o = match key {
                    SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                    SortKey::Size => a.size.cmp(&b.size),
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
        match list_dir(&self.current_dir) {
            Ok(mut es) => {
                self.sort_entries(&mut es);
                self.entries = es;
                self.entries_err = None;
            }
            Err(e) => {
                self.entries.clear();
                self.entries_err = Some(e.to_string());
            }
        }
        self.addr_edit = self.current_dir.to_string_lossy().into_owned();
        self.entries_dirty = false;
    }

    // ---- Folder-tree sidebar (E1b.2) ----

    /// Expand every ancestor of `current_dir` so the tree reveals where you are.
    fn sb_reveal(&mut self) {
        let mut cur = self.current_dir.clone();
        loop {
            self.sb_expanded.insert(cur.clone());
            match cur.parent() {
                Some(par) => cur = par.to_path_buf(),
                None => break,
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
        ui.horizontal(|ui| {
            ui.add_space(depth as f32 * 12.0);
            let tri = if open { "▾" } else { "▸" };
            if ui.add(egui::Button::new(tri).frame(false)).clicked() {
                toggle = true;
            }
            if ui.selectable_label(is_current, format!("🗀 {name}")).clicked() {
                navigate = true;
            }
        });

        if toggle {
            if open {
                self.sb_expanded.remove(&path);
            } else {
                self.sb_expanded.insert(path.clone());
            }
        }
        if navigate {
            self.navigate_to(path.clone());
            self.sb_expanded.insert(path.clone()); // reveal children on select
        }

        if self.sb_expanded.contains(&path) {
            for child in self.sb_children(&path) {
                self.sb_node(ui, child, depth + 1);
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
            .add_enabled(self.current_dir.parent().is_some(), egui::Button::new("⬆"))
            .on_hover_text("Up")
            .clicked()
        {
            self.go_up();
        }
        let r = ui.add(egui::TextEdit::singleline(&mut self.addr_edit).desired_width(f32::INFINITY));
        // Remember whether the address bar owns the keyboard this frame, so the
        // file list won't also act on Enter/Backspace.
        self.addr_active = r.has_focus() || r.lost_focus();
        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            let p = PathBuf::from(self.addr_edit.trim());
            self.navigate_to(p);
        }
    }

    /// The file-explorer List view: a folder-tree sidebar + a virtualized file
    /// table for `current_dir`.
    fn show_list(&mut self, ui: &mut egui::Ui) {
        if self.entries_dirty {
            self.reload_entries();
        }

        if self.sb_visible {
            egui::Panel::left("folder_tree")
                .resizable(true)
                .default_size(240.0)
                .min_size(150.0)
                .show(ui, |ui| {
                    ui.add_space(4.0);
                    egui::ScrollArea::both()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            self.sidebar_tree(ui);
                        });
                });
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
            let cur = self.current_dir.clone();
            let (sort_key, sort_asc) = (self.sort_key, self.sort_asc);
            let mut new_selected = self.selected;
            let mut nav_target: Option<PathBuf> = None;
            let mut new_sort: Option<SortKey> = None;

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
            let header_cell = |ui: &mut egui::Ui, label: String, out: &mut Option<SortKey>, k: SortKey| {
                if ui
                    .add(egui::Label::new(egui::RichText::new(label).strong()).sense(Sense::click()))
                    .clicked()
                {
                    *out = Some(k);
                }
            };

            TableBuilder::new(ui)
                .striped(true)
                .sense(Sense::click())
                .column(Column::remainder().at_least(220.0).clip(true))
                .column(Column::auto().at_least(90.0))
                .column(Column::auto().at_least(90.0))
                .column(Column::auto().at_least(90.0))
                .header(22.0, |mut h| {
                    h.col(|ui| header_cell(ui, format!("Name{}", arrow(SortKey::Name)), &mut new_sort, SortKey::Name));
                    h.col(|ui| header_cell(ui, format!("Size{}", arrow(SortKey::Size)), &mut new_sort, SortKey::Size));
                    h.col(|ui| header_cell(ui, format!("Type{}", arrow(SortKey::Kind)), &mut new_sort, SortKey::Kind));
                    h.col(|ui| header_cell(ui, format!("Modified{}", arrow(SortKey::Modified)), &mut new_sort, SortKey::Modified));
                })
                .body(|body| {
                    body.rows(20.0, entries.len(), |mut row| {
                        let e = &entries[row.index()];
                        row.set_selected(new_selected == Some(row.index()));
                        row.col(|ui| {
                            let icon = if e.is_dir { "📁" } else { "📄" };
                            ui.label(format!("{icon}  {}", e.name));
                        });
                        row.col(|ui| {
                            if !e.is_dir {
                                ui.monospace(human_size(e.size));
                            }
                        });
                        row.col(|ui| {
                            ui.label(entry_kind(e));
                        });
                        row.col(|ui| {
                            if let Some(m) = e.modified {
                                ui.weak(humanize_age(m));
                            }
                        });
                        let full = cur.join(&e.name);
                        let resp = row.response();
                        if resp.clicked() {
                            new_selected = Some(row.index());
                        }
                        if resp.double_clicked() {
                            if e.is_dir {
                                nav_target = Some(full.clone());
                            } else {
                                open_path(&full);
                            }
                        }
                        resp.context_menu(|ui| {
                            new_selected = Some(row.index());
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
                        });
                    });
                });

            // Restore entries and apply deferred mutations.
            self.entries = entries;
            self.selected = new_selected;
            if let Some(k) = new_sort {
                if self.sort_key == k {
                    self.sort_asc = !self.sort_asc;
                } else {
                    self.sort_key = k;
                    self.sort_asc = true;
                }
                self.entries_dirty = true; // re-sort next frame
            }
            if let Some(t) = nav_target {
                self.navigate_to(t);
            }
        });

        // Keyboard parity with Explorer: Enter opens the selection, Backspace
        // goes up — but never while the address bar owns the keyboard.
        let typing = self.addr_active || ui.ctx().memory(|m| m.focused()).is_some();
        if !typing {
            if ui.input(|i| i.key_pressed(egui::Key::Backspace)) {
                self.go_up();
            }
            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let action = self
                    .selected
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
    leaves.sort_by(|a, b| (a.rect.x + a.rect.y).partial_cmp(&(b.rect.x + b.rect.y)).unwrap());

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

/// Enumerate drive roots for the folder-tree sidebar. Probes A:..Z: (a mounted
/// letter answers `metadata` cheaply; an absent one fails fast). Computed once.
#[cfg(target_os = "windows")]
fn enumerate_drives() -> Vec<PathBuf> {
    ('A'..='Z')
        .filter_map(|c| {
            let p = PathBuf::from(format!("{c}:\\"));
            p.metadata().is_ok().then_some(p)
        })
        .collect()
}
#[cfg(not(target_os = "windows"))]
fn enumerate_drives() -> Vec<PathBuf> {
    vec![PathBuf::from("/")]
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
        };
        eframe::set_value(storage, "settings", &s);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

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
                let t = tree.lock().expect("tree");
                self.dominant = Some(t.dominant_categories());
                let root_name = t.node(Tree::ROOT).name.to_string();
                drop(t);
                // Queue the cache write to run OFF the UI thread (spawned at the
                // end of this frame so the crystallize re-layout gets the lock
                // first). Skip cancelled/partial scans.
                if let Some(st) = stats {
                    if !st.cancelled {
                        if let Some(cp) = cache_path_for(&root_name) {
                            let cs = CacheStats {
                                dirs: st.dirs,
                                files: st.files,
                                bytes: st.bytes,
                                saved_unix: now_unix(),
                            };
                            self.pending_save = Some((Arc::clone(tree), cp, cs));
                        } else {
                            eprintln!("[sector] cache: no cache dir available");
                        }
                    }
                }
            }
        }

        // ---- Shared nav: mode toggle + back/forward/up + address bar -------
        egui::Panel::top("mode").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong(sector_core::APP_NAME);
                ui.separator();
                ui.selectable_value(&mut self.view, View::List, "📁 Files");
                ui.selectable_value(&mut self.view, View::City, "🏙 City");
                ui.separator();
                if self.view == View::List
                    && ui
                        .selectable_label(self.sb_visible, "☰")
                        .on_hover_text("Toggle the folder tree")
                        .clicked()
                {
                    self.sb_visible = !self.sb_visible;
                }
                self.nav_bar(ui);
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

                    // Offer an instant load if a cache exists for this folder.
                    if let Some(cp) = cache_path_for(&self.current_dir.to_string_lossy()) {
                        if let Ok(age) = std::fs::metadata(&cp).and_then(|m| m.modified()) {
                            if ui
                                .button(format!("⟳ Load cached · {}", humanize_age(age)))
                                .on_hover_text("Reopen the last scan of this path instantly, without walking the filesystem.")
                                .clicked()
                            {
                                self.load_cached();
                            }
                        }
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
                        let t = tree.lock().expect("tree");
                        ui.horizontal_wrapped(|ui| {
                            if self.root != Tree::ROOT && ui.button("⬆ Up").clicked() {
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
                let t = tree.lock().expect("tree");
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
                    let t = tree.lock().expect("tree");
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
                    let t = tree.lock().expect("tree");
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
                    let t = tree.lock().expect("tree");
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
                        let t = tree.lock().expect("tree");
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
                    let t = tree.lock().expect("tree");
                    Some(PathBuf::from(joined_path(&t.path_components(self.root))))
                }
            } else {
                None
            };
        if let Some(t) = drilled {
            if t != self.current_dir {
                self.current_dir = t;
                self.entries_dirty = true;
                self.selected = None;
                self.sync_addr();
            }
            self.city_synced_dir = Some(self.current_dir.clone());
        }
        } // end City view

        // Kick off the queued cache write now that this frame's render (and its
        // tree lock) is done, so serialization on the background thread doesn't
        // block the crystallize re-layout.
        if let Some((arc, cp, cs)) = self.pending_save.take() {
            std::thread::spawn(move || {
                let result = {
                    let g = arc.lock().expect("tree");
                    g.to_cache_bytes(cs)
                };
                match result {
                    Ok(bytes) => match std::fs::write(&cp, &bytes) {
                        Ok(()) => eprintln!("[sector] cache saved: {} ({} bytes)", cp.display(), bytes.len()),
                        Err(e) => eprintln!("[sector] cache SAVE (write) FAILED: {e}"),
                    },
                    Err(e) => eprintln!("[sector] cache SAVE (serialize) FAILED: {e}"),
                }
            });
        }
    }
}
