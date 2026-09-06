//! SECTOR — a keyboard-friendly Windows file explorer with a built-in 2.5D
//! cityscape visualizer (D16: explorer first, visualizer second).
//!
//! **Files view** is the spine: a folder tree + a live, sortable file list over
//! `current_dir`, with the usual operations (open, rename, new folder, cut /
//! copy / paste, delete to the Recycle Bin) and a Details panel.
//!
//! **City view** visualizes that same folder (D18): a background scan writes
//! into a shared tree and the UI renders it *as it grows* (D12, the live
//! "discovery" build), then "crystallizes" into the final layout. Completed
//! scans are cached, and on local NTFS the USN journal reports whether a cached
//! view is still current (E6). Drill down by clicking a block, navigate with the
//! breadcrumb, hover for name/size.
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
    freshness as usn_freshness, list_dir, query_mark, scan_into, stat_entry, Entry, Freshness,
    Progress, ScanOptions, ScanStats, UsnMark,
};

/// Which part of the Files view the keyboard drives (focus-follows-click).
#[derive(Clone, Copy, PartialEq)]
enum Focus {
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
/// copy. On Windows this MIRRORS the system file clipboard (see
/// [`sys_clipboard`]), so it also holds files cut/copied in Explorer.
struct Clipboard {
    paths: Vec<PathBuf>,
    cut: bool,
}

/// A paste running on a background thread.
struct PasteJob {
    rx: Receiver<PasteOutcome>,
    cancel: Arc<AtomicBool>,
    cut: bool,
    /// Short description for the status line (e.g. "Copying “movie.mkv”").
    desc: String,
}

/// What a paste worker accomplished: every (source, destination) it completed,
/// in order, plus the error that stopped it (if any). Completed items are real
/// even when a later one failed — they're what Undo reverses.
struct PasteOutcome {
    done: Vec<(PathBuf, PathBuf)>,
    error: Option<String>,
}

impl PasteOutcome {
    #[cfg(test)]
    fn last_name(&self) -> Option<String> {
        self.done.last().and_then(|(_, d)| d.file_name()).map(|n| n.to_string_lossy().into_owned())
    }
}

/// What a delete worker accomplished: the paths that went to the Recycle Bin
/// (restorable), plus any error. Permanent (network) deletes aren't listed.
struct DeleteOutcome {
    recycled: Vec<PathBuf>,
    error: Option<String>,
}

/// A reversal to execute — what Undo (or Redo) actually does. Each one, once
/// run, yields its own inverse, which is what makes Redo possible.
#[derive(Clone, Debug, PartialEq)]
enum UndoOp {
    /// Rename `from` (the current name) to `to`.
    Rename { from: PathBuf, to: PathBuf },
    /// Remove a folder — only while it is empty.
    RemoveDir { path: PathBuf },
    /// Re-create an (empty) folder.
    MakeDir { path: PathBuf },
    /// Send these to the Recycle Bin (permanent on network drives).
    Trash { paths: Vec<PathBuf> },
    /// Move each `.1` back to `.0` (its original name at its original parent).
    MoveBack { pairs: Vec<(PathBuf, PathBuf)> },
    /// Restore these original paths from the Recycle Bin.
    Restore { paths: Vec<PathBuf> },
}

/// An undo/redo stack entry: the reversal to run, labelled with the USER's
/// action it reverses ("Undo rename", "Redo delete").
#[derive(Clone)]
struct UndoEntry {
    op: UndoOp,
    label: &'static str,
}

impl UndoEntry {
    fn new(op: UndoOp, label: &'static str) -> Self {
        UndoEntry { op, label }
    }
}

/// What a reversal achieved: the item to select, and the inverse operation
/// (built from where things actually landed) for the opposite stack.
#[derive(Debug)]
struct UndoDone {
    select: Option<PathBuf>,
    inverse: Option<UndoOp>,
}

/// How many operations Undo (and Redo) remember.
const UNDO_CAP: usize = 50;
/// How many folders the Back/Forward history menus list.
const HISTORY_MENU: usize = 12;

/// A deferred action from the folder tree's context menu (acts on THAT folder).
enum TreeAct {
    Open,
    Toggle,
    Reveal,
    CopyPath,
    Clip(bool),
    Paste,
    Delete,
    Rename,
    NewFolder,
    Props,
    Pin,
    Unpin,
}

/// The egui drag-and-drop payload for an internal drag: the paths being
/// dragged (the selection, or the single row the drag started on).
struct DragFiles {
    paths: Vec<PathBuf>,
}

impl DragFiles {
    /// Can these be dropped INTO `dest`? Not onto one of themselves (or inside
    /// one), and not back into the folder they're already in (a no-op).
    fn can_drop_into(&self, dest: &Path) -> bool {
        let onto_self = self.paths.iter().any(|s| dest.starts_with(s));
        let same_folder = self.paths.iter().all(|s| s.parent() == Some(dest));
        !onto_self && !same_folder
    }
}

/// A cached scan, read and prepared on a background thread (deserialising
/// millions of nodes and computing their dominant categories takes seconds
/// for a big drive — far too long for the UI thread).
struct LoadedCache {
    tree: Tree,
    stats: CacheStats,
    dominant: Vec<FileCategory>,
}

/// Why a cache couldn't be loaded. `unusable` means the FILE is the problem
/// (older format, corrupt, wrong folder) and may be deleted; a read error or a
/// dead worker says nothing about the file, so it must be kept.
struct CacheLoadError {
    msg: String,
    unusable: bool,
}

/// Read + validate + prepare a cache file for `dir`. Pure; runs off-thread.
fn load_cache_file(cp: &Path, dir: &Path) -> Result<LoadedCache, CacheLoadError> {
    let keep = |msg: String| CacheLoadError { msg, unusable: false };
    let bad = |msg: String| CacheLoadError { msg, unusable: true };
    let bytes = std::fs::read(cp).map_err(|e| keep(format!("couldn't read {}: {e}", cp.display())))?;
    let (tree, stats) = Tree::from_cache_bytes(&bytes).ok_or_else(|| {
        bad(match Tree::cache_format_version(&bytes) {
            Some(v) if v != Tree::CACHE_FORMAT => {
                format!("it was written by an older SECTOR (format v{v}, current v{})", Tree::CACHE_FORMAT)
            }
            _ => "the file is corrupt".to_string(),
        })
    })?;
    // Verify the cache is really THIS folder — guards against a (rare) hash-key
    // collision or a mismatched/renamed file loading the wrong tree.
    let cached_root = tree.node(Tree::ROOT).name.to_string();
    if normalize_cache_key(&cached_root) != normalize_cache_key(&dir.to_string_lossy()) {
        return Err(bad(format!("the file is for {cached_root:?}, not this folder")));
    }
    let dominant = tree.dominant_categories();
    eprintln!("[sector] cache: loaded {} nodes, {} files for {}", tree.len(), stats.files, dir.display());
    Ok(LoadedCache { tree, stats, dominant })
}

/// A rubber-band (marquee) selection in progress: a drag that started on
/// empty space in the list. Rows the band touches are selected live.
struct Marquee {
    /// Where the drag started (screen space).
    start: Pos2,
    /// The selection to ADD to (Ctrl/Shift held at the start), else empty.
    base: HashSet<String>,
}

/// A deferred action from the file list's background (empty-space) menu.
enum BgAct {
    Deselect,
    Paste,
    NewFolder,
    SelectAll,
    Refresh,
    Reveal,
    Props,
}

/// If `path` is `from` or inside it, the same path under `to`. Used to follow
/// the folder we're in when it (or an ancestor) is renamed.
fn reroot(path: &Path, from: &Path, to: &Path) -> Option<PathBuf> {
    let rest = path.strip_prefix(from).ok()?;
    Some(if rest.as_os_str().is_empty() { to.to_path_buf() } else { to.join(rest) })
}

/// Hover text for a list row: the full (unclipped) name plus the essentials.
fn row_tooltip(e: &Entry, sizes: Option<&HashMap<String, u64>>) -> String {
    let mut s = e.name.clone();
    if e.is_dir {
        match sizes.and_then(|m| m.get(&e.name.to_lowercase())) {
            Some(sz) => s.push_str(&format!("\nFolder · {}", human_size(*sz))),
            None => s.push_str("\nFolder"),
        }
    } else {
        s.push_str(&format!("\n{} · {}", categorize(&e.name).label(), human_size(e.size)));
    }
    if let Some(m) = e.modified {
        s.push_str(&format!("\nModified {} ({})", format_datetime(m), humanize_age(m)));
    }
    if e.is_symlink {
        s.push_str("\nLink / reparse point");
    }
    s
}

/// A drive-letter path with the letter in its canonical UPPER case ("y:\\Media"
/// → "Y:\\Media"). Windows treats them as the same drive, but a typed or
/// restored lower-case letter would otherwise show beside the tree's
/// upper-case one as if they were two different places.
fn canonical_drive_case(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    let b = s.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_lowercase() && b[1] == b':' {
        let mut t = String::with_capacity(s.len());
        t.push(b[0].to_ascii_uppercase() as char);
        t.push_str(&s[1..]);
        PathBuf::from(t)
    } else {
        p
    }
}

/// Are two paths the same folder, the Windows way (case-insensitive, trailing
/// separators ignored)?
fn same_folder(a: &Path, b: &Path) -> bool {
    normalize_cache_key(&a.to_string_lossy()) == normalize_cache_key(&b.to_string_lossy())
}

/// The last path component for messages (or the whole path for a root).
fn name_of(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string_lossy().into_owned())
}

/// A pending name-entry dialog (E4): create a folder, or rename an entry.
enum PromptKind {
    NewFolder,
    /// Rename an entry of the current listing.
    Rename { orig: String },
    /// Rename the folder the explorer is IN (tree-focused F2).
    RenameDir { dir: PathBuf },
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

impl SortKey {
    /// Stable name for persistence (see [`Settings`]).
    fn name(self) -> &'static str {
        match self {
            SortKey::Name => "name",
            SortKey::Size => "size",
            SortKey::Kind => "kind",
            SortKey::Modified => "modified",
        }
    }

    fn from_name(s: &str) -> Self {
        match s {
            "size" => SortKey::Size,
            "kind" => SortKey::Kind,
            "modified" => SortKey::Modified,
            _ => SortKey::Name,
        }
    }
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
    let mut key = root.trim().replace('/', "\\").to_lowercase();
    // "Y:\Media\" and "Y:\Media" are the same folder: drop trailing separators
    // (but never below a bare "\\").
    while key.len() > 1 && key.ends_with('\\') {
        key.pop();
    }
    // …except a drive root, which is canonically "y:\" (not the drive-relative "y:").
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
    age_label(secs)
}

/// The scale behind [`humanize_age`]: minutes, hours, days for about a month,
/// then months, then years with the months remainder — "496d ago" says less
/// than "1y 4mo ago".
fn age_label(secs: u64) -> String {
    const DAY: u64 = 86_400;
    let days = secs / DAY;
    if secs < 90 {
        "just now".to_string()
    } else if secs < 5400 {
        format!("{}m ago", secs / 60)
    } else if secs < 129_600 {
        format!("{}h ago", secs / 3600)
    } else if days < 30 {
        format!("{days}d ago")
    } else if days < 365 {
        format!("{}mo ago", (days / 30).max(1))
    } else {
        let years = days / 365;
        let months = (days % 365) / 30;
        if months == 0 {
            format!("{years}y ago")
        } else {
            format!("{years}y {months}mo ago")
        }
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
            // Don't make label text drag-to-select — in the file list it gives an
            // I-beam cursor and intercepts hover over a filename, so hovering the
            // text behaves differently from the rest of the row.
            cc.egui_ctx.all_styles_mut(|s| {
                s.interaction.selectable_labels = false;
                // Solid (non-floating) scrollbars everywhere, drawn in the widget
                // FILL colour rather than the text colour — egui's default draws
                // the handle with the foreground colour, which on the dark theme
                // is a near-white bar.
                s.spacing.scroll = egui::style::ScrollStyle::solid();
            });
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
                app.pane.current_dir = canonical_drive_case(PathBuf::from(&s.current_dir));
                app.pane.addr_edit = app.pane.current_dir.to_string_lossy().into_owned();
                app.show_hidden = s.show_hidden;
                app.pane.sort_key = SortKey::from_name(&s.sort_key);
                app.pane.sort_asc = s.sort_asc;
                app.auto_scan_local = s.auto_scan_local;
                app.pins = s.pins.iter().map(PathBuf::from).collect();
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

/// One explorer pane: a location and everything that hangs off it — the
/// listing, selection and cursor, filter, sort, history, address-bar state.
/// The app holds one today; a dual-pane layout (see ROADMAP) is two of these
/// sharing the clipboard, undo stack, background jobs and the folder tree.
struct Pane {
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
    entries_err: Option<String>,
    entries_dirty: bool,
    /// History: each entry is a folder plus the item you were on in it, so
    /// Back/Forward put you back where you were (like `go_up` does).
    back_stack: Vec<(PathBuf, Option<String>)>,
    fwd_stack: Vec<(PathBuf, Option<String>)>,
    /// Free / total bytes of the drive holding `current_dir` (one call per
    /// navigation), for the status bar. `None` if unknown.
    drive_space: Option<(u64, u64)>,
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
    /// Cached status-footer summary (item counts + total), recomputed only when
    /// the listing or folder sizes change — not every frame.
    status_summary: String,
    /// True while the address bar has (or just lost) focus — suppresses the file
    /// list's Enter/Backspace shortcuts so they don't fight the address bar.
    addr_active: bool,
    /// Path shown as an editable text field (true) vs a clickable breadcrumb.
    addr_editing: bool,
    /// Request focus for the address field on the next frame (entering edit mode).
    addr_edit_focus: bool,
    /// Request keyboard focus for the filter box on the next frame (Ctrl+F).
    filter_focus: bool,
    /// Shift+F10: open the context menu on the lead row next frame (one-shot).
    kbd_menu_req: bool,
    /// A keyboard-opened context menu is up — keep it anchored to its row (not
    /// the pointer) until it closes.
    kbd_menu_open: bool,
    /// `current_dir` itself as a listing entry (one stat per navigation), so
    /// Details can describe the folder the TREE has focused.
    cur_entry: Option<Entry>,
    /// The scanned subtree size of `current_dir` (with `folder_sizes`), for the
    /// tree-focused Details — recomputed with the folder sizes, not per frame.
    cur_dir_size: Option<u64>,
    /// A rubber-band selection being dragged out on the list's empty space.
    marquee: Option<Marquee>,
}

impl Default for Pane {
    fn default() -> Self {
        Pane {
            current_dir: PathBuf::from("C:\\"),
            addr_edit: "C:\\".to_string(),
            all_entries: Vec::new(),
            entries: Vec::new(),
            filter: String::new(),
            entries_err: None,
            entries_dirty: true,
            back_stack: Vec::new(),
            fwd_stack: Vec::new(),
            drive_space: None,
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
            status_summary: String::new(),
            addr_active: false,
            addr_editing: false,
            addr_edit_focus: false,
            filter_focus: false,
            kbd_menu_req: false,
            kbd_menu_open: false,
            cur_entry: None,
            cur_dir_size: None,
            marquee: None,
        }
    }
}

struct SectorApp {
    /// Worker threads to use for the next scan (tunable — see DEFAULT_THREADS).
    threads: usize,
    /// D20: scan a LOCAL folder automatically on entering it with no
    /// cityscape (never a network drive or a drive root).
    auto_scan_local: bool,
    scan: ScanState,
    /// Current drill-down root (navigates via the tree's parent chain).
    root: NodeId,
    opts: LayoutOptions,

    // Cached layout tiles + the derived cityscape + the state they're for.
    tiles: Vec<Tile>,
    /// World-space blocks (stage 1) — rebuilt with the layout.
    blocks: Vec<Block3>,
    /// The projected scene (stage 2) — rebuilt when the blocks or the camera change.
    scape: Scape,
    camera: Camera,
    /// The camera `scape` was projected with.
    last_camera: Camera,
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
    /// The explorer pane (one for now; see [`Pane`]).
    pane: Pane,
    /// Show hidden/system entries (off by default, like Explorer). App-wide.
    show_hidden: bool,
    /// Last window title we set, to avoid resending it every frame.
    last_title: String,
    /// An open New-folder / Rename dialog (E4), if any.
    prompt: Option<NamePrompt>,
    /// Cut/copy clipboard (E5).
    clipboard: Option<Clipboard>,
    /// Last seen Windows clipboard sequence number — when it changes, the
    /// system clipboard is re-read into `clipboard` (cheap to poll per frame).
    clip_seq: u32,
    /// A paste running in the background, if any.
    paste_job: Option<PasteJob>,
    /// Paths awaiting delete confirmation (the modal is up), if any.
    confirm_delete: Option<Vec<PathBuf>>,
    /// A background delete-to-Recycle-Bin in progress.
    delete_job: Option<Receiver<DeleteOutcome>>,
    /// Undo stack, newest last (session-only). Any new operation clears `redo`.
    undo: Vec<UndoEntry>,
    /// Redo stack: the inverses of what Undo has run, newest last.
    redo: Vec<UndoEntry>,
    /// A reversal running on a background thread: the receiver, the entry
    /// being run (so the view can follow a renamed/removed current folder),
    /// and whether it is a redo (which stack gets the inverse).
    undo_job: Option<(Receiver<Result<UndoDone, String>>, UndoEntry, bool)>,
    /// Transient error from the last edit/paste, shown in the footer.
    op_error: Option<String>,
    /// Show the right-hand Properties panel for the selection.
    props_visible: bool,
    /// Files currently being dragged over the window (from Explorer or any
    /// app), for the drop overlay. 0 when nothing is hovering.
    drop_hover: usize,
    /// Was a popup/menu open when this frame began? A popup closed by Esc is
    /// already gone by the time the shortcuts run, so without this the same
    /// Esc would go on to clear the selection.
    menu_open_at_start: bool,

    // ---- Folder-tree sidebar (E1b.2, Files view only — D19) ----
    sb_visible: bool,
    /// Quick access: the user's pinned folders (persisted). The Windows known
    /// folders (Desktop, Documents, …) are listed automatically before them.
    pins: Vec<PathBuf>,
    /// The known folders that exist, resolved once (`None` = not yet).
    qa_known: Option<Vec<PathBuf>>,
    /// The two tree sections' open state.
    qa_open: bool,
    drives_open: bool,
    /// Which part of the Files view the keyboard drives (focus follows your
    /// last click).
    focus_pane: Focus,
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
    /// Why the City has nothing to show (e.g. the cached scan couldn't be
    /// loaded) — shown in the City bar instead of the bare scan-prompt.
    city_note: Option<String>,
    /// A cache load in progress on a background thread, and the folder it is
    /// for (a result that arrives after navigating elsewhere is dropped).
    cache_load: Option<(Receiver<Result<LoadedCache, CacheLoadError>>, PathBuf)>,
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
            auto_scan_local: true,
            scan: ScanState::Idle,
            root: Tree::ROOT,
            // Blend "dark-but-breathing" density (D15): a touch more street
            // spacing than full-Kowloon, taller towers than dusk.
            opts: LayoutOptions { max_depth: 16, min_tile: 7.0, padding: 1.2 },
            tiles: Vec::new(),
            blocks: Vec::new(),
            scape: Scape::default(),
            camera: Camera::default(),
            last_camera: Camera::default(),
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
            pane: Pane::default(),
            show_hidden: false,
            last_title: String::new(),
            prompt: None,
            clipboard: None,
            clip_seq: 0,
            paste_job: None,
            confirm_delete: None,
            delete_job: None,
            undo: Vec::new(),
            redo: Vec::new(),
            undo_job: None,
            op_error: None,
            props_visible: false,
            drop_hover: 0,
            menu_open_at_start: false,
            sb_visible: true,
            pins: Vec::new(),
            qa_known: None,
            qa_open: true,
            drives_open: true,
            focus_pane: Focus::List,
            tree_scroll: false,
            sb_roots: Vec::new(),
            sb_expanded: HashSet::new(),
            sb_cache: HashMap::new(),
            cache_mtime: None,
            pending_usn_mark: None,
            cache_freshness: Freshness::Unknown,
            city_note: None,
            cache_load: None,
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
    /// File-list sort column (a [`SortKey::name`]) and direction.
    #[serde(default)]
    sort_key: String,
    #[serde(default = "default_true")]
    sort_asc: bool,
    /// D20: auto-scan local folders on entering them without a cityscape.
    #[serde(default = "default_true")]
    auto_scan_local: bool,
    /// Quick-access pins (folder paths).
    #[serde(default)]
    pins: Vec<String>,
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            replay_secs: REPLAY_SECS,
            threads: DEFAULT_THREADS,
            replay_mode: false,
            current_dir: "C:\\".to_string(),
            show_hidden: false,
            sort_key: "name".to_string(),
            sort_asc: true,
            auto_scan_local: true,
            pins: Vec::new(),
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

impl Pane {
    /// Keep the address bar showing the canonical current directory.
    fn sync_addr(&mut self) {
        self.addr_edit = self.current_dir.to_string_lossy().into_owned();
    }

    /// Switch the address bar to its text field, with the path selected so
    /// typing replaces it (Edit button, Alt+D, Ctrl+L, F4).
    fn begin_addr_edit(&mut self) {
        self.addr_edit = self.current_dir.to_string_lossy().into_owned();
        self.addr_editing = true;
        self.addr_edit_focus = true;
    }

    /// The name of the item the cursor is on (to restore it when we return).
    fn lead_name(&self) -> Option<String> {
        self.lead_entry().map(|e| e.name.clone())
    }

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
        let space = self
            .drive_space
            .map(|(free, total)| format!(" · {} free of {}", human_size(free), human_size(total)))
            .unwrap_or_default();
        self.status_summary = format!(
            "{prefix}{} items · {} folders · {} files · {}{space}",
            commas(n as u64),
            commas(folders as u64),
            commas((n - folders) as u64),
            human_size(total),
        );
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
        let name = validate_name_syntax(name)?;
        // Windows compares names case-insensitively in *Unicode* ("élan" ==
        // "Élan"), not just ASCII — match that, or the check can miss a clash
        // that the filesystem will treat as the same name.
        let lname = name.to_lowercase();
        let lallow = allow.map(str::to_lowercase);
        let clashes = self.all_entries.iter().any(|e| {
            let en = e.name.to_lowercase();
            en == lname && lallow.as_deref() != Some(en.as_str())
        });
        if clashes {
            return Err("An item with that name already exists.".into());
        }
        Ok(name)
    }

    /// Update the type-ahead buffer from this frame's typed text (egui Text
    /// events, so Ctrl-combos don't trigger it). Returns `(lowercased query,
    /// is_repeat)` if a letter was typed this frame, else `None`. The buffer
    /// resets after a short pause; `is_repeat` marks the same single letter again.
    fn type_ahead_input(&mut self, ui: &egui::Ui) -> Option<(String, bool)> {
        let typed: String = ui.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Text(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect()
        });
        if typed.is_empty() {
            return None;
        }
        let typed = typed.to_lowercase();
        let now = Instant::now();
        let timed_out = now.duration_since(self.type_ahead_time) >= Duration::from_millis(900);
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
        Some((self.type_ahead.clone(), repeat))
    }

    /// Returns whether the location actually changed.
    fn navigate_to(&mut self, path: PathBuf) -> bool {
        let path = canonical_drive_case(path);
        if path == self.current_dir {
            self.entries_dirty = true;
            return false;
        }
        let leaving = (self.current_dir.clone(), self.lead_name());
        self.back_stack.push(leaving);
        self.fwd_stack.clear();
        self.current_dir = path;
        self.entries_dirty = true;
        self.clear_selection();
        self.filter.clear();
        self.sync_addr();
        true
    }

    /// Jump to a history entry, returning the entry for where we were (folder +
    /// item on the cursor) for the opposite stack. Once the folder loads, the
    /// remembered item is re-selected and scrolled into view.
    fn hop(&mut self, to: (PathBuf, Option<String>)) -> (PathBuf, Option<String>) {
        let leaving = (self.current_dir.clone(), self.lead_name());
        let (path, name) = to;
        self.current_dir = path.clone();
        self.entries_dirty = true;
        self.clear_selection();
        self.filter.clear();
        self.sync_addr();
        if let Some(n) = name {
            self.select_after_reload = Some((path, n));
        }
        leaving
    }

    /// Refresh views after a successful edit, and select the affected item.
    fn after_edit(&mut self, select_name: String) {
        self.entries_dirty = true;
        self.filter.clear(); // so the new/renamed item is visible
        self.select_after_reload = Some((self.current_dir.clone(), select_name));
    }

    /// Refresh views after a background op touched `item`; if it lives in the
    /// folder we're looking at, clear the filter and select it. (An op that
    /// completed after you navigated elsewhere no longer reselects by name in
    /// the wrong folder.)
    fn after_op(&mut self, item: &Path) {
        self.entries_dirty = true;
        if item.parent() == Some(self.current_dir.as_path()) {
            self.filter.clear();
            self.select_after_reload = Some((self.current_dir.clone(), name_of(item)));
        }
    }

    /// Re-read `current_dir`. (The app refreshes folder sizes from its scan
    /// tree first — see `SectorApp::reload_entries`.)
    fn reload_entries(&mut self, show_hidden: bool) {
        self.drive_space = drive_space(&self.current_dir);
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
        self.apply_filter(show_hidden);
        self.addr_edit = self.current_dir.to_string_lossy().into_owned();
        self.entries_dirty = false;
        // The folder itself, for Details when the tree has focus (one stat).
        self.cur_entry = stat_entry(&self.current_dir).ok();
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
    fn apply_filter(&mut self, show_hidden: bool) {
        let name_of = |i: Option<usize>| i.and_then(|i| self.entries.get(i)).map(|e| e.name.clone());
        let (lead_name, anchor_name) = (name_of(self.lead), name_of(self.anchor));
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

    /// Back one step in history; `true` if there was somewhere to go.
    fn go_back(&mut self) -> bool {
        match self.back_stack.pop() {
            Some(to) => {
                let left = self.hop(to);
                self.fwd_stack.push(left);
                true
            }
            None => false,
        }
    }

    /// Forward one step in history; `true` if there was somewhere to go.
    fn go_forward(&mut self) -> bool {
        match self.fwd_stack.pop() {
            Some(to) => {
                let left = self.hop(to);
                self.back_stack.push(left);
                true
            }
            None => false,
        }
    }

    /// Up to the parent, re-selecting the folder we left once it loads;
    /// `true` if there was a parent.
    fn go_up(&mut self) -> bool {
        let Some(parent) = self.current_dir.parent().map(|p| p.to_path_buf()) else {
            return false;
        };
        let child = self.current_dir.file_name().map(|n| n.to_string_lossy().into_owned());
        self.navigate_to(parent.clone());
        if let Some(c) = child {
            self.select_after_reload = Some((parent, c));
        }
        true
    }

    /// If the folder we're IN no longer exists (deleted, undone), step out to
    /// its nearest surviving ancestor — in place, not as a history step.
    /// `true` if we moved.
    fn step_out_if_gone(&mut self) -> bool {
        if std::fs::symlink_metadata(&self.current_dir).is_ok() {
            return false;
        }
        let mut p = self.current_dir.clone();
        while let Some(parent) = p.parent().map(Path::to_path_buf) {
            p = parent;
            if std::fs::symlink_metadata(&p).is_ok() {
                break;
            }
        }
        self.current_dir = p;
        self.sync_addr();
        self.clear_selection();
        true
    }

    /// Type-ahead in the file list: jump to the first matching name; repeating a
    /// letter cycles through matches. `true` if something was selected.
    fn type_ahead_list(&mut self, ui: &egui::Ui) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        let Some((q, repeat)) = self.type_ahead_input(ui) else { return false };
        let n = self.entries.len();
        let start = if repeat { self.lead.map_or(0, |i| i + 1) } else { 0 };
        let hit = (0..n)
            .map(|off| (start + off) % n)
            .find(|&i| self.entries[i].name.to_lowercase().starts_with(&q));
        if let Some(i) = hit {
            self.select_only(i);
            self.scroll_target = Some(i);
            ui.ctx().request_repaint();
        }
        hit.is_some()
    }
}

/// Syntactic validation of a file/folder name — no listing needed: empty,
/// forbidden characters, reserved names, trailing dot. Returns the trimmed
/// name or an error.
fn validate_name_syntax(name: &str) -> Result<&str, String> {
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
    if name.ends_with('.') {
        // Windows drops a trailing dot, so "foo." would silently become "foo".
        return Err("Name can't end with a dot.".into());
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
    Ok(name)
}

impl SectorApp {
    fn start_scan(&mut self) {
        // Stop any scan already running.
        if let ScanState::Running { cancel, .. } = &self.scan {
            cancel.store(true, Ordering::Relaxed);
        }

        let path = self.pane.current_dir.clone();
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
        self.city_note = None;
        self.reveal_start = None; // a leftover cache-load animation must not drive a live scan
        self.dominant = None;
        self.menu_target = None;
        self.city_root = Some(self.pane.current_dir.clone());
        self.city_synced_dir = Some(self.pane.current_dir.clone());
        // Capture the USN watermark BEFORE the scan, so any later change counts
        // as making the cache stale (E6). None on NAS / non-NTFS.
        self.pending_usn_mark = query_mark(&self.pane.current_dir);
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

    /// Start loading the current folder's cached scan on a background thread.
    /// The City shows a loading state meanwhile; [`Self::install_cache`] runs
    /// when the result arrives (if we're still in that folder).
    fn start_cache_load(&mut self) {
        let dir = self.pane.current_dir.clone();
        let Some(cp) = cache_path_for(&dir.to_string_lossy()) else {
            eprintln!("[sector] cache: no cache dir");
            self.city_no_cache();
            return;
        };
        // The City now belongs to this load: stop any running scan and clear
        // the old scene (never leave another folder's city up mislabelled).
        self.city_idle();
        self.city_note = None;
        self.city_root = Some(dir.clone());
        self.city_synced_dir = Some(dir.clone());
        let (tx, rx) = channel();
        let key_dir = dir.clone();
        std::thread::spawn(move || {
            let _ = tx.send(load_cache_file(&cp, &key_dir));
        });
        self.cache_load = Some((rx, dir));
    }

    /// Put a loaded cache on screen as the City (the fast, UI-thread half).
    fn install_cache(&mut self, lc: LoadedCache) {
        let cs = lc.stats;
        // E6: is this cached view still current per the USN journal?
        let mark = UsnMark { journal_id: cs.usn_journal_id, next_usn: cs.usn_next };
        self.cache_freshness = usn_freshness(&self.pane.current_dir, &mark);
        self.dominant = Some(lc.dominant);
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
        self.city_root = Some(self.pane.current_dir.clone());
        self.city_synced_dir = Some(self.pane.current_dir.clone());
        // No scanner thread; the dead channel is never polled because `stats`
        // is already `Some`.
        let (_tx, rx) = channel();
        self.scan = ScanState::Running {
            tree: Arc::new(Mutex::new(lc.tree)),
            progress: Arc::new(Progress::default()),
            cancel: Arc::new(AtomicBool::new(false)),
            stats_rx: rx,
            stats: Some(stats),
            started: Instant::now(),
        };
        self.recompute_folder_sizes(); // the tree now covers this folder
        self.pane.refresh_status_summary();
    }

    /// No cityscape for the current folder: auto-scan it if that's a LOCAL,
    /// non-root folder and the setting is on (D20 — the live discovery build is
    /// the point, and a local subfolder is seconds); otherwise the idle
    /// scan-prompt (network drives and drive roots always wait for Scan).
    fn city_no_cache(&mut self) {
        let dir = &self.pane.current_dir;
        let eligible = self.auto_scan_local && dir.parent().is_some() && !is_network_path(dir);
        if eligible {
            self.start_scan();
        } else {
            self.city_idle();
        }
    }

    /// Drop the City to its idle scan-prompt: cancel any running scan and
    /// clear the scene.
    fn city_idle(&mut self) {
        if let ScanState::Running { cancel, .. } = &self.scan {
            cancel.store(true, Ordering::Relaxed);
        }
        self.scan = ScanState::Idle;
        self.root = Tree::ROOT;
        self.tiles.clear();
        self.scape = Scape::default();
        self.dominant = None;
        self.reveal_start = None;
        self.menu_target = None;
        self.cache_freshness = Freshness::Unknown;
        self.city_note = None;
    }

    /// Point the City at `current_dir` (E2). Instant cache-load if one exists;
    /// otherwise drop to an idle scan-prompt (a full scan stays deliberate).
    fn sync_city(&mut self) {
        let cur = self.pane.current_dir.clone();
        // One stat here (folder change), reused by the City top bar's Load-cached
        // button so it doesn't re-stat the file every frame.
        let md = cache_path_for(&cur.to_string_lossy())
            .and_then(|p| std::fs::metadata(p).ok());
        self.cache_mtime = md.as_ref().and_then(|m| m.modified().ok());
        let has_cache = md.is_some();
        // Load the cache in the background; if there is none, drop to an idle
        // scan-prompt rather than leaving the previous folder's cityscape on
        // screen mislabelled as this one.
        if has_cache {
            self.start_cache_load();
        } else {
            self.city_no_cache();
        }
        self.city_root = Some(cur.clone());
        self.city_synced_dir = Some(cur);
    }

    // ---- Explorer navigation (E1) ----

    // ---- Pane wrappers: the pane's own logic plus the app-level side effects
    // (revealing the folder in the shared tree, dropping the tree cache, folder
    // sizes from the scan tree, the focus flag). Call sites use these.

    fn after_nav(&mut self) {
        self.op_error = None;
        self.sb_reveal();
    }

    fn navigate_to(&mut self, path: PathBuf) {
        if self.pane.navigate_to(path) {
            self.after_nav();
        }
    }

    fn go_back(&mut self) {
        if self.pane.go_back() {
            self.sb_reveal();
        }
    }

    fn go_forward(&mut self) {
        if self.pane.go_forward() {
            self.sb_reveal();
        }
    }

    fn go_up(&mut self) {
        if self.pane.go_up() {
            self.after_nav();
        }
    }

    fn step_out_if_gone(&mut self) {
        if self.pane.step_out_if_gone() {
            self.sb_reveal();
        }
    }

    fn after_edit(&mut self, select_name: String) {
        self.pane.after_edit(select_name);
        self.sb_cache.clear(); // the folder tree may have changed
    }

    fn after_op(&mut self, item: &Path) {
        self.pane.after_op(item);
        self.sb_cache.clear();
    }

    fn reload_entries(&mut self) {
        // Folder sizes first (from the City's scan tree) so a sort by Size can
        // use them on the initial read.
        self.recompute_folder_sizes();
        self.pane.reload_entries(self.show_hidden);
    }

    fn apply_filter(&mut self) {
        self.pane.apply_filter(self.show_hidden);
    }

    fn type_ahead_list(&mut self, ui: &egui::Ui) {
        if self.pane.type_ahead_list(ui) {
            self.focus_pane = Focus::List;
        }
    }

    // ---- Selection (multi-select) ----

    /// Recompute [`Self::folder_sizes`] for the current folder from an in-memory
    /// scan tree that covers it (the City's loaded/cached tree). Cheap — a lock,
    /// a shallow name-walk, and a small map over the immediate subfolders. Leaves
    /// it `None` when no completed scan covers `current_dir`. Callers refresh the
    /// status summary (the total depends on folder sizes).
    fn recompute_folder_sizes(&mut self) {
        self.pane.folder_sizes = self.compute_folder_sizes();
        let cur = self.pane.current_dir.clone();
        self.pane.cur_dir_size = self.scanned_subtree_size(&cur);
    }

    fn compute_folder_sizes(&self) -> Option<HashMap<String, u64>> {
        let ScanState::Running { tree, stats: Some(st), .. } = &self.scan else {
            return None;
        };
        if st.cancelled {
            return None; // a partial tree would show underestimates as if real
        }
        let root = self.city_root.as_ref()?;
        // Relative components from the scan root to the current folder, compared
        // case-insensitively (Windows) since the tree walk is too. Lowercasing is
        // fine — find_descendant matches case-insensitively.
        let lower = |p: &Path| -> Vec<String> {
            p.components()
                .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
                .collect()
        };
        let (root_c, cur_c) = (lower(root), lower(&self.pane.current_dir));
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

    /// F2 with the folder tree focused: rename the folder you're in.
    fn open_rename_dir(&mut self) {
        let d = self.pane.current_dir.clone();
        self.open_rename_dir_at(d);
    }

    /// Rename any folder (tree context menu, or the one you're in).
    fn open_rename_dir_at(&mut self, dir: PathBuf) {
        let Some(name) = dir.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            return; // a drive root has no name to edit
        };
        self.prompt = Some(NamePrompt {
            kind: PromptKind::RenameDir { dir },
            buf: name,
            error: None,
            focus: true,
        });
    }

    /// Ctrl+C/X with the tree focused: the folder you're in goes on the
    /// clipboard (never a drive root).
    fn clip_current_dir(&mut self, cut: bool) {
        if self.pane.current_dir.parent().is_some() {
            self.set_clipboard(vec![self.pane.current_dir.clone()], cut);
        }
    }

    /// Delete with the tree focused: the folder you're in (never a drive root).
    fn request_delete_current_dir(&mut self) {
        let d = self.pane.current_dir.clone();
        self.request_delete_dir(d);
    }

    /// Ask to delete one folder (tree context menu / tree-focused Delete).
    fn request_delete_dir(&mut self, dir: PathBuf) {
        if self.file_op_running() {
            return; // one background file operation at a time
        }
        if dir.parent().is_some() {
            self.confirm_delete = Some(vec![dir]);
        }
    }

    fn open_new_folder(&mut self) {
        let buf = self.pane.unique_new_folder_name();
        self.prompt = Some(NamePrompt { kind: PromptKind::NewFolder, buf, error: None, focus: true });
    }

    /// Apply a prompt's action to the filesystem. On success returns `Ok`; on a
    /// validation or OS error returns `Err(message)` (the dialog stays open).
    fn commit_prompt(&mut self, prompt: &NamePrompt) -> Result<(), String> {
        match &prompt.kind {
            PromptKind::NewFolder => {
                let name = self.pane.validate_name(&prompt.buf, None)?.to_string();
                let target = self.pane.current_dir.join(&name);
                std::fs::create_dir(&target)
                    .map_err(|e| format!("Couldn't create the folder: {e}"))?;
                self.push_undo(UndoEntry::new(UndoOp::RemoveDir { path: target }, "new folder"));
                self.after_edit(name);
            }
            PromptKind::Rename { orig } => {
                let name = self.pane.validate_name(&prompt.buf, Some(orig))?.to_string();
                if name == *orig {
                    return Ok(()); // no change — just close
                }
                let src = self.pane.current_dir.join(orig);
                let dst = self.pane.current_dir.join(&name);
                // The listing we validated against can be stale (a file created
                // since the last refresh), and `fs::rename` on Windows REPLACES
                // an existing file — so re-check on disk right before renaming.
                // A case-only rename ("foo" → "FOO") legitimately "exists" (it's
                // the same file on NTFS) and is allowed through.
                let case_only = name.to_lowercase() == orig.to_lowercase();
                if !case_only && std::fs::symlink_metadata(&dst).is_ok() {
                    return Err("An item with that name already exists.".into());
                }
                std::fs::rename(&src, &dst).map_err(|e| format!("Couldn't rename: {e}"))?;
                self.push_undo(UndoEntry::new(UndoOp::Rename { from: dst, to: src }, "rename"));
                self.after_edit(name);
            }
            PromptKind::RenameDir { dir } => {
                // The folder we're IN: validate syntactically (its siblings aren't
                // the current listing) and rely on the on-disk check below.
                let name = validate_name_syntax(&prompt.buf)?.to_string();
                let orig = dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if name == orig {
                    return Ok(()); // no change — just close
                }
                let Some(parent) = dir.parent().map(Path::to_path_buf) else {
                    return Err("A drive root can't be renamed.".into());
                };
                let dst = parent.join(&name);
                let case_only = name.to_lowercase() == orig.to_lowercase();
                if !case_only && std::fs::symlink_metadata(&dst).is_ok() {
                    return Err("An item with that name already exists.".into());
                }
                std::fs::rename(dir, &dst).map_err(|e| format!("Couldn't rename: {e}"))?;
                self.push_undo(UndoEntry::new(
                    UndoOp::Rename { from: dst.clone(), to: dir.clone() },
                    "rename",
                ));
                // Follow the folder to its new name if we're in it (or inside
                // it) — in place, not a history step.
                if self.sb_expanded.remove(dir) {
                    self.sb_expanded.insert(dst.clone());
                }
                if let Some(p) = reroot(&self.pane.current_dir, dir, &dst) {
                    self.pane.current_dir = p;
                    self.pane.sync_addr();
                }
                self.sb_cache.clear();
                self.pane.entries_dirty = true;
                self.sb_reveal();
            }
        }
        Ok(())
    }

    /// Field list for the Details panel, or `None`. Describes the list's target
    /// item — or, when the tree has focus or nothing is selected, the folder
    /// you're IN (as Explorer's details pane does).
    fn selected_properties(&self) -> Option<Vec<(&'static str, String)>> {
        let tree_focus = self.focus_pane == Focus::Tree && self.sb_visible;
        let item = if tree_focus { None } else { self.pane.op_target().and_then(|i| self.pane.entries.get(i)) };
        match item {
            Some(e) => {
                let size = if e.is_dir {
                    self.pane.folder_sizes.as_ref().and_then(|m| m.get(&e.name.to_lowercase())).copied()
                } else {
                    None
                };
                Some(self.properties_of(e, &self.pane.current_dir, size))
            }
            None => {
                let e = self.pane.cur_entry.as_ref()?;
                let location = self.pane.current_dir.parent().unwrap_or(&self.pane.current_dir).to_path_buf();
                Some(self.properties_of(e, &location, self.pane.cur_dir_size))
            }
        }
    }

    /// The loaded City tree's node for `dir`, if `dir` is the City's root or
    /// inside it (case-insensitive path walk). `complete_only` ignores a
    /// cancelled scan's partial tree.
    fn city_node_for(&self, dir: &Path, complete_only: bool) -> Option<NodeId> {
        let ScanState::Running { tree, stats, .. } = &self.scan else {
            return None;
        };
        if complete_only && !matches!(stats, Some(st) if !st.cancelled) {
            return None;
        }
        let root = self.city_root.as_ref()?;
        let lower = |p: &Path| -> Vec<String> {
            p.components().map(|c| c.as_os_str().to_string_lossy().to_lowercase()).collect()
        };
        let (root_c, cur_c) = (lower(root), lower(dir));
        if !cur_c.starts_with(&root_c) {
            return None;
        }
        let comps: Vec<&str> = cur_c[root_c.len()..].iter().map(String::as_str).collect();
        let t = tree.lock().ok()?;
        t.find_descendant(Tree::ROOT, &comps)
    }

    /// The scanned subtree size of `dir`, if the City's completed (not
    /// cancelled) scan covers it.
    fn scanned_subtree_size(&self, dir: &Path) -> Option<u64> {
        let node = self.city_node_for(dir, true)?;
        let ScanState::Running { tree, .. } = &self.scan else {
            return None;
        };
        let t = tree.lock().ok()?;
        Some(t.node(node).subtree_size)
    }

    /// Build the Details fields for `e`, which lives in `location`; `dir_size`
    /// is a folder's scanned subtree size when one is known.
    fn properties_of(&self, e: &Entry, location: &Path, dir_size: Option<u64>) -> Vec<(&'static str, String)> {
        let mut f: Vec<(&'static str, String)> = Vec::new();
        f.push(("Name", e.name.clone()));
        f.push(("Location", location.to_string_lossy().into_owned()));
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
            match dir_size {
                Some(sz) => format!("{} ({} bytes)", human_size(sz), commas(sz)),
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
        f
    }

    // ---- Cut / copy / paste (E5) ----

    /// Put `paths` on the clipboard — ours AND the Windows file clipboard, so
    /// Explorer (and any other app) can paste them, and sees a Cut as a cut.
    fn set_clipboard(&mut self, paths: Vec<PathBuf>, cut: bool) {
        if !sys_clipboard::write(&paths, cut) {
            eprintln!("[sector] clipboard: system clipboard unavailable — kept internally");
        }
        self.clipboard = Some(Clipboard { paths, cut });
        self.op_error = None;
    }

    /// Put the current selection (all selected items) on the clipboard.
    fn clip_selected(&mut self, cut: bool) {
        let paths = self.pane.selected_paths();
        if !paths.is_empty() {
            self.set_clipboard(paths, cut);
        }
    }

    /// Start pasting the clipboard into the current folder (background thread).
    fn start_paste(&mut self) {
        let dest = self.pane.current_dir.clone();
        self.start_paste_into(dest);
    }

    /// Start pasting the clipboard into `dest_dir` (background thread).
    fn start_paste_into(&mut self, dest_dir: PathBuf) {
        let Some(clip) = &self.clipboard else { return };
        let (sources, cut) = (clip.paths.clone(), clip.cut);
        self.start_transfer(sources, dest_dir, cut);
    }

    /// Copy (or move, if `cut`) `sources` into `dest_dir` on a background
    /// thread — the engine behind Paste and inbound drag-and-drop.
    fn start_transfer(&mut self, sources: Vec<PathBuf>, dest_dir: PathBuf, cut: bool) {
        if self.file_op_running() {
            self.op_error = Some("Wait for the current file operation to finish.".into());
            return; // one background file operation at a time
        }
        if sources.is_empty() {
            return;
        }

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
        if self.file_op_running() {
            return; // one background file operation at a time
        }
        let paths = self.pane.selected_paths();
        if !paths.is_empty() {
            self.confirm_delete = Some(paths);
        }
    }

    /// Delete `paths` to the Recycle Bin on a background thread.
    fn start_delete(&mut self, paths: Vec<PathBuf>) {
        if self.file_op_running() || paths.is_empty() {
            return; // one background file operation at a time
        }
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let mut errors: Vec<String> = Vec::new();
            let mut recycled = Vec::new();
            for p in &paths {
                // Network drives have no Recycle Bin — the trash op fails there,
                // so delete permanently (the user confirmed a permanent delete).
                // `Ok(true)` = went to the Recycle Bin (restorable by Undo).
                let res: Result<bool, String> = if is_network_path(p) {
                    remove_any(p).map(|()| false).map_err(|e| e.to_string())
                } else {
                    trash::delete(p).map(|()| true).map_err(|e| e.to_string())
                };
                match res {
                    Ok(true) => recycled.push(p.clone()),
                    Ok(false) => {}
                    Err(e) => errors.push(format!("{}: {e}", name_of(p))),
                }
            }
            let error =
                (!errors.is_empty()).then(|| format!("Couldn't delete: {}", errors.join("; ")));
            let _ = tx.send(DeleteOutcome { recycled, error });
        });
        self.op_error = None;
        self.delete_job = Some(rx);
    }

    // ---- Undo ----

    /// Is a background file operation (paste / delete / undo) in progress?
    fn file_op_running(&self) -> bool {
        self.paste_job.is_some() || self.delete_job.is_some() || self.undo_job.is_some()
    }

    /// Record a new user operation. This forks history: the redo stack goes.
    fn push_undo(&mut self, entry: UndoEntry) {
        self.undo.push(entry);
        if self.undo.len() > UNDO_CAP {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// Ctrl+Z: reverse the most recent operation on a background thread.
    fn start_undo(&mut self) {
        self.start_reversal(false);
    }

    /// Ctrl+Y: re-apply the most recently undone operation.
    fn start_redo(&mut self) {
        self.start_reversal(true);
    }

    fn start_reversal(&mut self, redo: bool) {
        if self.file_op_running() {
            return;
        }
        let stack = if redo { &mut self.redo } else { &mut self.undo };
        let Some(entry) = stack.pop() else { return };
        let (tx, rx) = channel();
        let work = entry.op.clone();
        std::thread::spawn(move || {
            let _ = tx.send(run_undo(work));
        });
        self.op_error = None;
        self.undo_job = Some((rx, entry, redo));
    }

    // ---- Folder-tree sidebar (E1b.2) ----

    /// Expand every ANCESTOR of `current_dir` so the tree reveals where you are,
    /// leaving the current folder itself highlighted-but-not-expanded (a click
    /// expands it explicitly; keyboard arrowing just highlights).
    fn sb_reveal(&mut self) {
        let mut cur = self.pane.current_dir.clone();
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
        if self.qa_open {
            for p in self.quick_access() {
                self.sb_collect_visible(&p, &mut out);
            }
        }
        if self.drives_open {
            for root in self.sb_roots.clone() {
                self.sb_collect_visible(&root, &mut out);
            }
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

    /// Width that fits the widest currently-visible tree row (indent + triangle +
    /// name), clamped to a sane range. Used for double-click-the-divider-to-fit.
    fn tree_fit_width(&mut self, ctx: &egui::Context) -> f32 {
        let font = egui::FontId::proportional(14.0);
        let visible = self.sb_visible_nodes();
        let mut max_w = 150.0_f32;
        for p in &visible {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.to_string_lossy().into_owned());
            let depth = p.components().count().saturating_sub(2); // drive roots at 0
            let text_w = ctx.fonts_mut(|f| {
                f.layout_no_wrap(format!("📁 {name}"), font.clone(), egui::Color32::WHITE)
                    .size()
                    .x
            });
            // section indent + triangle + depth indent + text + pad
            let row_w = 18.0 + 22.0 + depth as f32 * 12.0 + text_w + 24.0;
            max_w = max_w.max(row_w);
        }
        max_w.min(520.0)
    }

    /// A folder's immediate subdirectories, cached (lazy — filled on first expand).
    fn sb_children(&mut self, path: &Path) -> Vec<PathBuf> {
        if let Some(c) = self.sb_cache.get(path) {
            return c.clone();
        }
        let show_hidden = self.show_hidden;
        let mut dirs: Vec<PathBuf> = match list_dir(path) {
            Ok(es) => es
                .into_iter()
                .filter(|e| e.is_dir && !e.is_symlink && (show_hidden || !e.is_hidden))
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

    /// Quick access: the Windows known folders that exist (Desktop, Documents,
    /// Downloads, Pictures, Music, Videos — via the known-folder API, so a
    /// OneDrive-redirected Desktop resolves to its real location), then the
    /// user's pins, deduplicated.
    fn quick_access(&mut self) -> Vec<PathBuf> {
        let known = self
            .qa_known
            .get_or_insert_with(|| {
                [
                    dirs::desktop_dir(),
                    dirs::document_dir(),
                    dirs::download_dir(),
                    dirs::picture_dir(),
                    dirs::audio_dir(),
                    dirs::video_dir(),
                ]
                .into_iter()
                .flatten()
                .filter(|p| p.is_dir())
                .collect()
            })
            .clone();
        let mut out = known;
        for p in &self.pins {
            if !out.iter().any(|k| same_folder(k, p)) {
                out.push(p.clone());
            }
        }
        out
    }

    fn is_pinned(&self, p: &Path) -> bool {
        self.pins.iter().any(|q| same_folder(q, p))
    }

    fn pin(&mut self, p: PathBuf) {
        if !self.is_pinned(&p) {
            self.pins.push(p);
        }
    }

    fn unpin(&mut self, p: &Path) {
        self.pins.retain(|q| !same_folder(q, p));
    }

    /// Render the tree: a Quick access section (known folders + pins), then
    /// the drive roots — each with (recursively) its expanded subtree.
    fn sidebar_tree(&mut self, ui: &mut egui::Ui) {
        if self.sb_roots.is_empty() {
            self.sb_roots = enumerate_drives();
            self.sb_reveal(); // open the path to wherever we start
        }
        let qa = self.quick_access();
        if !qa.is_empty() {
            let h = egui::CollapsingHeader::new("Quick access")
                .id_salt("qa")
                .open(Some(self.qa_open))
                .show(ui, |ui| {
                    for p in qa {
                        self.sb_node(ui, p, 0);
                    }
                });
            if h.header_response.clicked() {
                self.qa_open = !self.qa_open;
            }
        }
        let h = egui::CollapsingHeader::new("Drives")
            .id_salt("drives")
            .open(Some(self.drives_open))
            .show(ui, |ui| {
                for root in self.sb_roots.clone() {
                    self.sb_node(ui, root, 0);
                }
            });
        if h.header_response.clicked() {
            self.drives_open = !self.drives_open;
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
        let is_current = self.pane.current_dir == path;

        let mut toggle = false;
        let mut navigate = false;
        let scroll_here = self.tree_scroll && is_current && self.focus_pane == Focus::Tree;

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
            format!("📁 {name}"),
            egui::FontId::proportional(14.0),
            text_col,
        );
        // Drop target for an internal drag (Ctrl = copy): any folder that isn't
        // one of the dragged items (or inside one) or their current folder.
        let mut dropped: Option<Vec<PathBuf>> = None;
        if let Some(p) = resp.dnd_hover_payload::<DragFiles>() {
            if p.can_drop_into(&path) {
                painter.rect_stroke(rect, 2.0_f32, Stroke::new(2.0, sel_fg), egui::StrokeKind::Inside);
                if let Some(p) = resp.dnd_release_payload::<DragFiles>() {
                    dropped = Some(p.paths.clone());
                }
            }
        }
        if let Some(paths) = dropped {
            let copy = ui.ctx().input(|i| i.modifiers.ctrl);
            self.start_transfer(paths, path.clone(), !copy);
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if scroll_here {
            resp.scroll_to_me(Some(egui::Align::Center));
        }
        // A click on the triangle toggles; anywhere else navigates — unless the
        // click merely dismissed a menu.
        if resp.clicked() && !self.menu_open_at_start {
            if resp.interact_pointer_pos().map(|p| tri_rect.contains(p)).unwrap_or(false) {
                toggle = true;
            } else {
                navigate = true;
            }
        }

        // Right-click: act on THIS folder (not necessarily the current one).
        let is_root = path.parent().is_none();
        let can_paste = self.clipboard.is_some() && !self.file_op_running();
        let pinned = self.is_pinned(&path);
        let mut act: Option<TreeAct> = None;
        resp.context_menu(|ui| {
            let mut item = |ui: &mut egui::Ui, enabled: bool, label: &str, a: TreeAct| {
                if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
                    act = Some(a);
                    ui.close();
                }
            };
            item(ui, true, "Open", TreeAct::Open);
            item(ui, true, if open { "Collapse" } else { "Expand" }, TreeAct::Toggle);
            item(ui, true, "Open in Explorer", TreeAct::Reveal);
            item(ui, true, "Copy path", TreeAct::CopyPath);
            if pinned {
                item(ui, true, "Unpin from Quick access", TreeAct::Unpin);
            } else {
                item(ui, true, "Pin to Quick access", TreeAct::Pin);
            }
            ui.separator();
            item(ui, !is_root, "Copy", TreeAct::Clip(false));
            item(ui, !is_root, "Cut", TreeAct::Clip(true));
            item(ui, can_paste, "Paste into", TreeAct::Paste);
            item(ui, !is_root, "Delete", TreeAct::Delete);
            ui.separator();
            item(ui, !is_root, "Rename…", TreeAct::Rename);
            item(ui, true, "New folder…", TreeAct::NewFolder);
            ui.separator();
            item(ui, true, "Properties", TreeAct::Props);
        });
        if let Some(a) = act {
            self.focus_pane = Focus::Tree;
            match a {
                TreeAct::Open => navigate = true,
                TreeAct::Toggle => toggle = true,
                TreeAct::Reveal => reveal_in_explorer(&path, true),
                TreeAct::CopyPath => ui.ctx().copy_text(path.to_string_lossy().into_owned()),
                TreeAct::Clip(cut) => self.set_clipboard(vec![path.clone()], cut),
                TreeAct::Paste => self.start_paste_into(path.clone()),
                TreeAct::Delete => self.request_delete_dir(path.clone()),
                TreeAct::Rename => self.open_rename_dir_at(path.clone()),
                TreeAct::NewFolder => {
                    // Load the target folder NOW so the proposed name (and the
                    // clash check) are against its listing, not the old one's.
                    self.navigate_to(path.clone());
                    self.reload_entries();
                    self.open_new_folder();
                }
                TreeAct::Props => {
                    self.navigate_to(path.clone());
                    self.reload_entries(); // Details describes this folder at once
                    self.pane.clear_selection();
                    self.props_visible = true;
                }
                TreeAct::Pin => self.pin(path.clone()),
                TreeAct::Unpin => self.unpin(&path),
            }
        }

        if toggle {
            if open {
                self.sb_expanded.remove(&path);
            } else {
                self.sb_expanded.insert(path.clone());
            }
            self.focus_pane = Focus::Tree;
        }
        if navigate {
            // Select/navigate WITHOUT expanding — the triangle (or → key) expands.
            self.navigate_to(path.clone());
            self.focus_pane = Focus::Tree;
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
        // Back / Forward: a click steps once; a right-click lists the history so
        // you can jump straight to a folder (browser-style).
        let (mut back_n, mut fwd_n) = (0usize, 0usize);
        let back = ui
            .add_enabled(!self.pane.back_stack.is_empty(), egui::Button::new("◀"))
            .on_hover_text("Back (right-click for history)");
        if back.clicked() {
            back_n = 1;
        }
        back.context_menu(|ui| {
            for (n, (p, _)) in self.pane.back_stack.iter().rev().take(HISTORY_MENU).enumerate() {
                if ui.button(p.to_string_lossy().into_owned()).clicked() {
                    back_n = n + 1;
                    ui.close();
                }
            }
        });
        let fwd = ui
            .add_enabled(!self.pane.fwd_stack.is_empty(), egui::Button::new("▶"))
            .on_hover_text("Forward (right-click for history)");
        if fwd.clicked() {
            fwd_n = 1;
        }
        fwd.context_menu(|ui| {
            for (n, (p, _)) in self.pane.fwd_stack.iter().rev().take(HISTORY_MENU).enumerate() {
                if ui.button(p.to_string_lossy().into_owned()).clicked() {
                    fwd_n = n + 1;
                    ui.close();
                }
            }
        });
        for _ in 0..back_n {
            self.go_back();
        }
        for _ in 0..fwd_n {
            self.go_forward();
        }
        if ui
            .add_enabled(self.pane.current_dir.parent().is_some(), egui::Button::new("⬆"))
            .on_hover_text("Up")
            .clicked()
        {
            self.go_up();
        }

        if self.pane.addr_editing {
            // Editable path field.
            let addr_id = ui.id().with("addr_edit");
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.pane.addr_edit)
                    .id(addr_id)
                    .desired_width(f32::INFINITY),
            );
            if self.pane.addr_edit_focus {
                r.request_focus();
                // Select the whole path (like Explorer) so typing replaces it.
                if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), addr_id) {
                    let end = self.pane.addr_edit.chars().count();
                    state.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                        egui::text::CCursor::new(0),
                        egui::text::CCursor::new(end),
                    )));
                    egui::TextEdit::store_state(ui.ctx(), addr_id, state);
                }
                self.pane.addr_edit_focus = false;
            }
            self.pane.addr_active = r.has_focus() || r.lost_focus();
            if r.lost_focus() {
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    let raw = self.pane.addr_edit.trim();
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
                self.pane.addr_editing = false; // leave edit mode on commit or blur
            }
        } else {
            self.pane.addr_active = false;
            // The bar itself, registered FIRST so the segments (added later) sit
            // on top: a click on its empty space switches to the text field, as
            // in Explorer. A faint hover fill + I-beam cursor make that visible.
            let bar = ui.interact(
                ui.available_rect_before_wrap(),
                ui.id().with("addr_bar_bg"),
                Sense::click(),
            );
            if bar.hovered() {
                ui.painter().rect_filled(bar.rect, 4.0_f32, ui.visuals().widgets.hovered.weak_bg_fill);
                ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
            }
            let bar = bar.on_hover_text("Click to edit the path (Alt+D, Ctrl+L, F4)");
            // Clickable breadcrumb.
            let mut go: Option<PathBuf> = None;
            let segs = breadcrumb_segments(&self.pane.current_dir);
            let last = segs.len().saturating_sub(1);
            // Breathing room between segments and their chevrons (the chevrons
            // are buttons now, so a tight gap reads as one run of text).
            ui.spacing_mut().item_spacing.x = 7.0;
            for (i, (label, path)) in segs.iter().enumerate() {
                if i == last {
                    ui.strong(label); // current folder — not a link
                } else if ui.add(egui::Button::new(label).frame(false)).clicked() {
                    go = Some(path.clone());
                }
                // The chevron after a segment lists that folder's subfolders, so
                // you can step sideways into a sibling without going up first.
                let chev = ui.add(egui::Button::new("›").frame(false).small());
                let pid = egui::Popup::default_response_id(&chev);
                let kids = (chev.clicked() || egui::Popup::is_id_open(ui.ctx(), pid))
                    .then(|| self.sb_children(path));
                egui::Popup::menu(&chev).show(|ui| {
                    ui.set_min_width(160.0);
                    match kids.as_deref() {
                        Some([]) | None => {
                            ui.weak("(no subfolders)");
                        }
                        Some(list) => {
                            const CAP: usize = 200;
                            egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                                for k in list.iter().take(CAP) {
                                    if ui.button(name_of(k)).clicked() {
                                        go = Some(k.clone());
                                        ui.close();
                                    }
                                }
                                if list.len() > CAP {
                                    ui.weak(format!("… {} more", list.len() - CAP));
                                }
                            });
                        }
                    }
                });
            }
            if bar.clicked() && !self.menu_open_at_start {
                // (A click that merely dismissed a chevron menu doesn't count.)
                self.pane.begin_addr_edit();
            }
            if let Some(p) = go {
                self.navigate_to(p);
            }
        }
    }

    /// The file-explorer List view: a folder-tree sidebar + a virtualized file
    /// table for `current_dir`.
    fn show_list(&mut self, ui: &mut egui::Ui) {
        if self.pane.entries_dirty {
            self.reload_entries();
        }

        // An Esc that left the filter box is spent; it must not also fall
        // through to the list's Esc (clear filter → clear selection → …).
        let mut esc_handled = false;
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
                    egui::TextEdit::singleline(&mut self.pane.filter)
                        .id(ui.id().with("filter_edit"))
                        .desired_width(220.0)
                        .hint_text("Filter this folder… (Ctrl+F)"),
                );
                if self.pane.filter_focus {
                    r.request_focus();
                    self.pane.filter_focus = false;
                }
                let mut changed = r.changed();
                // Esc while typing here clears the filter (egui drops focus on
                // Esc but leaves the key readable this frame).
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    esc_handled = true;
                    if !self.pane.filter.is_empty() {
                        self.pane.filter.clear();
                        changed = true;
                    }
                }
                if !self.pane.filter.is_empty() && ui.button("×").on_hover_text("Clear filter").clicked() {
                    self.pane.filter.clear();
                    changed = true;
                }
                ui.separator();
                if ui
                    .checkbox(&mut self.show_hidden, "Hidden")
                    .on_hover_text("Show hidden / system files (list and folder tree)")
                    .changed()
                {
                    changed = true;
                    self.sb_cache.clear(); // the tree obeys it too
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
            let panel_id = egui::Id::new("folder_tree");
            egui::Panel::left(panel_id)
                .resizable(true)
                .default_size(240.0)
                .min_size(150.0)
                .show(ui, |ui| {
                    ui.add_space(4.0);
                    // (Scrollbars are solid app-wide — see the style setup in
                    // main — so the bar reserves its own space and the full-width
                    // row text can't run under it.)
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            self.sidebar_tree(ui);
                        });
                });
            self.tree_scroll = false; // one-shot: consumed by this render

            // Double-click the divider → fit the tree to its widest visible row.
            let resize_resp = ui.ctx().read_response(panel_id.with("__resize"));
            if resize_resp.map(|r| r.double_clicked()).unwrap_or(false) {
                let fit = self.tree_fit_width(&ui.ctx().clone());
                if let Some(mut st) = egui::PanelState::load(ui.ctx(), panel_id) {
                    st.outer_rect = egui::Rect::from_min_size(
                        st.outer_rect.min,
                        egui::vec2(fit, st.outer_rect.height()),
                    );
                    ui.ctx().data_mut(|d| d.insert_persisted(panel_id, st));
                    ui.ctx().request_repaint();
                }
            }
        }

        // Status footer: folder totals (cached) + selection details.
        {
            let detail = if self.pane.sel.len() > 1 {
                // Multiple selected: count + combined size of known items.
                let total: u64 = self
                    .pane.entries
                    .iter()
                    .filter(|e| self.pane.sel.contains(&e.name))
                    .map(|e| self.pane.entry_size(e))
                    .sum();
                Some(format!("{} selected · {}", self.pane.sel.len(), human_size(total)))
            } else {
                self.pane.lead_entry().filter(|e| self.pane.sel.contains(&e.name)).map(|e| {
                    let mut s = e.name.clone();
                    let known = !e.is_dir
                        || self
                            .pane.folder_sizes
                            .as_ref()
                            .map(|m| m.contains_key(&e.name.to_lowercase()))
                            .unwrap_or(false);
                    if known {
                        s.push_str(&format!(" · {}", human_size(self.pane.entry_size(e))));
                    }
                    if let Some(m) = e.modified {
                        s.push_str(&format!(" · {}", humanize_age(m)));
                    }
                    s
                })
            };
            let mut cancel_paste = false;
            let mut clear_err = false;
            let mut undo_req = false;
            let mut redo_req = false;
            egui::Panel::bottom("files_status").show(ui, |ui| {
                ui.horizontal(|ui| {
                    // A running paste/delete/undo takes over the footer with a spinner.
                    if let Some((_, entry, redo)) = &self.undo_job {
                        ui.spinner();
                        ui.label(format!("{} {}…", if *redo { "Redoing" } else { "Undoing" }, entry.label));
                    } else if self.delete_job.is_some() {
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
                        ui.weak(&self.pane.status_summary);
                        if let Some(d) = detail {
                            ui.separator();
                            ui.label(d);
                        }
                        if let Some(c) = &self.clipboard {
                            ui.separator();
                            let verb = if c.cut { "cut" } else { "copied" };
                            ui.weak(format!("📋 {} {verb}", c.paths.len()));
                        }
                        if let Some(e) = self.undo.last() {
                            ui.separator();
                            if ui.small_button(format!("↺ Undo {}", e.label)).on_hover_text("Ctrl+Z").clicked() {
                                undo_req = true;
                            }
                        }
                        if let Some(e) = self.redo.last() {
                            ui.separator();
                            if ui.small_button(format!("↻ Redo {}", e.label)).on_hover_text("Ctrl+Y").clicked() {
                                redo_req = true;
                            }
                        }
                    }
                });
            });
            if undo_req {
                self.start_undo();
            }
            if redo_req {
                self.start_redo();
            }
            if cancel_paste {
                if let Some(job) = &self.paste_job {
                    job.cancel.store(true, Ordering::Relaxed);
                }
            }
            if clear_err {
                self.op_error = None;
            }
        }

        egui::CentralPanel::default().show(ui, |ui| self.show_pane_list(ui));

        // Keyboard parity with Explorer: F5 refreshes; arrows/Home/End/PageUp/Down
        // move the selection; Enter opens it; Backspace goes up — but never while
        // the address bar owns the keyboard (F5 there would wipe in-progress text).
        let typing = self.prompt.is_some()
            || self.confirm_delete.is_some()
            || self.pane.addr_active
            || ui.ctx().memory(|m| m.focused()).is_some()
            || self.menu_open_at_start // a menu owns the keys while open —
            || egui::Popup::is_any_open(ui.ctx()); // including the Esc that closed it
        if !typing {
            // With the folder tree focused, item commands act on the folder you're
            // IN (the tree's highlighted node) rather than the list's selection.
            let tree_focus = self.focus_pane == Focus::Tree && self.sb_visible;
            // Esc: clear the filter, else the selection, else close Details.
            if !esc_handled && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                if !self.pane.filter.is_empty() {
                    self.pane.filter.clear();
                    self.apply_filter();
                } else if !self.pane.sel.is_empty() || self.pane.lead.is_some() {
                    self.pane.clear_selection();
                } else if self.props_visible {
                    self.props_visible = false;
                }
            }
            // Ctrl+F / Ctrl+E: jump to the filter box.
            if ui.input(|i| {
                i.modifiers.ctrl
                    && !i.modifiers.shift
                    && (i.key_pressed(egui::Key::F) || i.key_pressed(egui::Key::E))
            }) {
                self.pane.filter_focus = true;
            }
            // Alt+Up: up (alias of Backspace).
            if ui.input(|i| i.modifiers.alt && i.key_pressed(egui::Key::ArrowUp)) {
                self.go_up();
            }
            // Ctrl+Shift+C: copy the path(s) — the focused folder, or the selection.
            if ui.input(|i| i.modifiers.ctrl && i.modifiers.shift && i.key_pressed(egui::Key::C)) {
                let paths = if tree_focus { vec![self.pane.current_dir.clone()] } else { self.pane.selected_paths() };
                if !paths.is_empty() {
                    let text: Vec<String> = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
                    ui.ctx().copy_text(text.join("\n"));
                }
            }
            // Shift+F10: open the context menu on the list's target row.
            if !tree_focus && ui.input(|i| i.modifiers.shift && i.key_pressed(egui::Key::F10)) {
                if let Some(i) = self.pane.op_target().or(self.pane.lead) {
                    self.pane.lead = Some(i);
                    self.pane.scroll_target = Some(i); // the row must be rendered to host it
                    self.pane.kbd_menu_req = true;
                }
            }
            // Ctrl+Space: toggle the lead row in the selection (keyboard Ctrl+click).
            if !tree_focus && ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::Space)) {
                if let Some(l) = self.pane.lead {
                    self.pane.toggle_at(l);
                }
            }
            // F5 / Ctrl+R: re-read the current folder AND drop the (lazy) tree
            // cache, so on-disk changes show without a restart. Keep the same file
            // selected by name (the old index may not be valid after the re-read).
            let refresh = ui.input(|i| {
                i.key_pressed(egui::Key::F5) || (i.modifiers.ctrl && i.key_pressed(egui::Key::R))
            });
            if refresh {
                let keep = self.pane.lead_entry().map(|e| e.name.clone());
                self.pane.clear_selection();
                if let Some(name) = keep {
                    self.pane.select_after_reload = Some((self.pane.current_dir.clone(), name));
                }
                self.pane.entries_dirty = true;
                self.sb_cache.clear();
            }
            if ui.input(|i| i.key_pressed(egui::Key::Backspace)) {
                self.go_up();
            }
            // Tab switches focus between the tree and the list (consume it so egui
            // doesn't also use it for widget-focus traversal).
            if self.sb_visible
                && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Tab))
            {
                self.focus_pane =
                    if self.focus_pane == Focus::Tree { Focus::List } else { Focus::Tree };
                if self.focus_pane == Focus::Tree {
                    self.tree_scroll = true; // bring the current node into view
                }
            }
            // Alt+Left / Alt+Right: history back / forward (Explorer/browser style).
            if ui.input(|i| i.modifiers.alt && i.key_pressed(egui::Key::ArrowLeft)) {
                self.go_back();
            }
            if ui.input(|i| i.modifiers.alt && i.key_pressed(egui::Key::ArrowRight)) {
                self.go_forward();
            }
            // F2 renames the target item; Ctrl+Shift+N makes a new folder.
            if ui.input(|i| i.key_pressed(egui::Key::F2)) {
                if tree_focus {
                    self.open_rename_dir();
                } else if let Some(name) =
                    self.pane.op_target().and_then(|i| self.pane.entries.get(i)).map(|e| e.name.clone())
                {
                    self.open_rename(name);
                }
            }
            if ui.input(|i| i.modifiers.ctrl && i.modifiers.shift && i.key_pressed(egui::Key::N)) {
                self.open_new_folder();
            }
            // Ctrl+A selects everything in the folder.
            if ui.input(|i| i.modifiers.ctrl && !i.modifiers.shift && i.key_pressed(egui::Key::A)) {
                self.pane.sel = self.pane.entries.iter().map(|e| e.name.clone()).collect();
                self.pane.lead = self.pane.entries.len().checked_sub(1);
                self.pane.anchor = Some(0);
            }
            // Ctrl+C copy, Ctrl+X cut, Ctrl+V paste (not Shift, to avoid clashing
            // with Ctrl+Shift+N).
            if ui.input(|i| i.modifiers.ctrl && !i.modifiers.shift && i.key_pressed(egui::Key::C)) {
                if tree_focus { self.clip_current_dir(false) } else { self.clip_selected(false) }
            }
            if ui.input(|i| i.modifiers.ctrl && !i.modifiers.shift && i.key_pressed(egui::Key::X)) {
                if tree_focus { self.clip_current_dir(true) } else { self.clip_selected(true) }
            }
            if ui.input(|i| i.modifiers.ctrl && !i.modifiers.shift && i.key_pressed(egui::Key::V)) {
                self.start_paste();
            }
            // Ctrl+Z: undo the last rename / new folder / paste / delete.
            if ui.input(|i| i.modifiers.ctrl && !i.modifiers.shift && i.key_pressed(egui::Key::Z)) {
                self.start_undo();
            }
            // Ctrl+Y / Ctrl+Shift+Z: redo.
            if ui.input(|i| {
                i.modifiers.ctrl
                    && ((!i.modifiers.shift && i.key_pressed(egui::Key::Y))
                        || (i.modifiers.shift && i.key_pressed(egui::Key::Z)))
            }) {
                self.start_redo();
            }
            // Delete → Recycle Bin (with a confirmation).
            if ui.input(|i| i.key_pressed(egui::Key::Delete)) {
                if tree_focus { self.request_delete_current_dir() } else { self.request_delete() }
            }
            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                if ui.input(|i| i.modifiers.alt) {
                    // Alt+Enter: Details for the focused folder, or the target item.
                    if tree_focus || self.pane.op_target().is_some() {
                        self.props_visible = true;
                    }
                } else if tree_focus {
                    // Enter in the tree: expand/collapse the focused folder
                    // (arrowing onto it already navigated there).
                    let d = self.pane.current_dir.clone();
                    if !self.sb_expanded.remove(&d) {
                        self.sb_expanded.insert(d);
                    }
                } else if self.pane.sel.len() > 1 {
                    // Enter on a multi-selection opens every selected FILE (a
                    // folder can't be entered alongside). Capped, like Explorer.
                    const MAX_OPEN: usize = 15;
                    let files: Vec<PathBuf> = self
                        .pane.entries
                        .iter()
                        .filter(|e| !e.is_dir && self.pane.sel.contains(&e.name))
                        .map(|e| self.pane.current_dir.join(&e.name))
                        .collect();
                    if files.is_empty() {
                        self.op_error =
                            Some("Enter opens files; select a single folder to enter it.".into());
                    } else if files.len() > MAX_OPEN {
                        self.op_error =
                            Some(format!("Select {MAX_OPEN} or fewer files to open them all at once."));
                    } else {
                        for f in &files {
                            open_path(f);
                        }
                    }
                } else {
                    let action = self
                        .pane.op_target()
                        .and_then(|i| self.pane.entries.get(i))
                        .map(|e| (self.pane.current_dir.join(&e.name), e.is_dir));
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
            if self.focus_pane == Focus::Tree && self.sb_visible {
                self.type_ahead_tree(ui);
                self.tree_keys(ui);
            } else {
                self.type_ahead_list(ui);
                // Move the lead in the list (Shift extends the range from the
                // anchor), scrolling it into view.
                let n = self.pane.entries.len();
                if n > 0 {
                    const PAGE: usize = 12;
                    let cur = self.pane.lead;
                    let mut moved = cur;
                    let (shift, ctrl) = ui.input(|i| {
                        use egui::Key;
                        // Alt+Up/Left/Right are navigation, not selection moves.
                        let plain = !i.modifiers.alt;
                        if plain && i.key_pressed(Key::ArrowDown) {
                            moved = Some(cur.map_or(0, |c| (c + 1).min(n - 1)));
                        }
                        if plain && i.key_pressed(Key::ArrowUp) {
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
                        (i.modifiers.shift, i.modifiers.ctrl)
                    });
                    if let Some(m) = moved {
                        if moved != cur {
                            if shift {
                                self.pane.select_range_to(m);
                            } else if ctrl {
                                // Ctrl+move: move the focus cursor only — then
                                // Ctrl+Space toggles it (Explorer's model for a
                                // non-contiguous selection by keyboard).
                                self.pane.lead = Some(m);
                            } else {
                                self.pane.select_only(m);
                            }
                            self.pane.scroll_target = Some(m);
                            ui.ctx().request_repaint();
                        }
                    }
                }
            }
        }
    }

    /// The file list for ONE pane: the table with its selection, drag-and-drop,
    /// marquee, context menus and deferred actions. Widget ids are relative to
    /// `ui`, so a second pane rendered under its own `push_id` can't collide.
    fn show_pane_list(&mut self, ui: &mut egui::Ui) {
        // The list background, registered FIRST so the rows (added later) sit
        // on top of it: a click on empty space deselects, a right-click opens
        // the folder's own menu (Paste / New folder / …). Present even when
        // the folder is empty or unreadable.
        let bg = ui.interact(ui.max_rect(), ui.id().with("files_bg"), Sense::click_and_drag());
        let mut bg_act: Option<BgAct> = None;
        // (A click that merely dismissed a context menu doesn't deselect or
        // start a band — Windows consumes that click too.)
        let dismissing = self.menu_open_at_start;
        if bg.clicked() && !dismissing {
            bg_act = Some(BgAct::Deselect);
        }
        // A (left-button) drag from empty space starts a rubber-band
        // selection. Ctrl/Shift add to the current selection; a plain drag
        // replaces it.
        if bg.drag_started_by(egui::PointerButton::Primary) && !dismissing {
            let additive = ui.ctx().input(|i| i.modifiers.ctrl || i.modifiers.shift);
            let base = if additive { self.pane.sel.clone() } else { HashSet::new() };
            let start = bg.interact_pointer_pos().unwrap_or(bg.rect.min);
            self.pane.marquee = Some(Marquee { start, base });
            self.focus_pane = Focus::List;
        }
        let can_paste_here = self.clipboard.is_some() && !self.file_op_running();
        bg.context_menu(|ui| {
            let mut item = |ui: &mut egui::Ui, enabled: bool, label: &str, a: BgAct| {
                if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
                    bg_act = Some(a);
                    ui.close();
                }
            };
            item(ui, can_paste_here, "Paste", BgAct::Paste);
            item(ui, true, "New folder…", BgAct::NewFolder);
            ui.separator();
            item(ui, true, "Select all", BgAct::SelectAll);
            item(ui, true, "Refresh", BgAct::Refresh);
            ui.separator();
            item(ui, true, "Open in Explorer", BgAct::Reveal);
            item(ui, true, "Properties", BgAct::Props);
        });
        if let Some(a) = bg_act {
            self.focus_pane = Focus::List;
            match a {
                BgAct::Deselect => self.pane.clear_selection(),
                BgAct::Paste => self.start_paste(),
                BgAct::NewFolder => self.open_new_folder(),
                BgAct::SelectAll => {
                    self.pane.sel = self.pane.entries.iter().map(|e| e.name.clone()).collect();
                    self.pane.lead = self.pane.entries.len().checked_sub(1);
                    self.pane.anchor = Some(0);
                }
                BgAct::Refresh => {
                    self.pane.entries_dirty = true;
                    self.sb_cache.clear();
                }
                BgAct::Reveal => reveal_in_explorer(&self.pane.current_dir, true),
                BgAct::Props => {
                    self.pane.clear_selection(); // Details then shows the folder itself
                    self.props_visible = true;
                }
            }
        }

        if let Some(err) = self.pane.entries_err.clone() {
            ui.centered_and_justified(|ui| {
                ui.weak(format!("⚠  {err}"));
            });
            return;
        }
        if self.pane.entries.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.weak("(empty folder)");
            });
            return;
        }

        use egui_extras::{Column, TableBuilder};

        // Move entries out of self so the table closures don't fight the
        // borrow checker; deferred mutations are applied after.
        let entries = std::mem::take(&mut self.pane.entries);
        let folder_sizes = self.pane.folder_sizes.take();
        let cur = self.pane.current_dir.clone();
        let (sort_key, sort_asc) = (self.pane.sort_key, self.pane.sort_asc);
        let scroll_target = self.pane.scroll_target.take();
        // Paths of cut items (dimmed in the list).
        let cut_paths: HashSet<PathBuf> = self
            .clipboard
            .as_ref()
            .filter(|c| c.cut)
            .map(|c| c.paths.iter().cloned().collect())
            .unwrap_or_default();
        // Selection set (taken out so the row closures can read it), plus the
        // click intent + modifiers, applied after the table.
        let sel = std::mem::take(&mut self.pane.sel);
        let mods = ui.ctx().input(|i| i.modifiers);
        // Focus cursor + keyboard-menu state, read by the row closures.
        let lead = self.pane.lead;
        let list_focus = !(self.focus_pane == Focus::Tree && self.sb_visible);
        let lead_color = ui.visuals().selection.stroke.color;
        let (kbd_req, kbd_open) = (self.pane.kbd_menu_req, self.pane.kbd_menu_open);
        // Internal drag-and-drop bookkeeping (applied after the table).
        let mut drag_start: Option<usize> = None;
        let mut drop_target: Option<(PathBuf, Vec<PathBuf>)> = None;
        let mut drop_hover_rect: Option<Rect> = None;
        // Screen rects of the rows rendered this frame (for the marquee).
        let mut row_rects: Vec<(usize, Rect)> = Vec::new();
        let mut click: Option<usize> = None;
        let mut menu_row: Option<usize> = None;
        let mut nav_target: Option<PathBuf> = None;
        let mut new_sort: Option<SortKey> = None;
        let mut rename_req: Option<String> = None;
        let mut pin_req: Option<PathBuf> = None;
        let mut new_folder_req = false;
        let mut props_req = false;
        let mut copy_req = false;
        let mut cut_req = false;
        let mut paste_req = false;
        let mut delete_req = false;
        let can_paste = self.clipboard.is_some() && !self.file_op_running();

        let arrow = |k: SortKey| {
            // ⏶/⏷ (not ▲/▼): the latter exist only in the monospace font
            // and render as boxes in a proportional label.
            if k == sort_key {
                if sort_asc {
                    " ⏶"
                } else {
                    " ⏷"
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
            .resizable(true) // drag the header separators; widths persist
            .sense(Sense::click_and_drag()) // rows are drag sources
            // Name is the flexible column and must stay NON-resizable: a
            // resizable column keeps its stored width, so it wouldn't shrink
            // when the list narrows (e.g. the Details panel opens) and the
            // other columns would be pushed off the edge. Resize the others;
            // Name absorbs the difference.
            .column(Column::remainder().at_least(220.0).clip(true).resizable(false))
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
                    let row_index = row.index();
                    let e = &entries[row_index];
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
                                    // Painted, not a glyph: "●" isn't in egui's
                                    // proportional fonts (it rendered as a box).
                                    let (r, _) =
                                        ui.allocate_exact_size(Vec2::new(10.0, 10.0), Sense::hover());
                                    ui.painter().circle_filled(r.center(), 4.0, category_color(c));
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
                    // Hover: the full (unclipped) name + essentials, like Explorer.
                    // (`on_hover_ui` is lazy — the text is built only when shown.)
                    let resp = row.response().on_hover_ui(|ui| {
                        ui.label(row_tooltip(e, folder_sizes.as_ref()));
                    });
                    row_rects.push((row_index, resp.rect));
                    // Drag source: dragging a selected row drags the whole
                    // selection; dragging an unselected row drags just it.
                    // Primary button only — a right-drag must not move files.
                    if resp.drag_started_by(egui::PointerButton::Primary) {
                        let paths: Vec<PathBuf> = if sel.contains(&e.name) {
                            entries.iter().filter(|x| sel.contains(&x.name)).map(|x| cur.join(&x.name)).collect()
                        } else {
                            vec![full.clone()]
                        };
                        egui::DragAndDrop::set_payload(&resp.ctx, DragFiles { paths });
                        drag_start = Some(row_index);
                    }
                    // Drop target: a folder row (not one being dragged).
                    if e.is_dir {
                        if let Some(p) = resp.dnd_hover_payload::<DragFiles>() {
                            if p.can_drop_into(&full) {
                                drop_hover_rect = Some(resp.rect);
                                if let Some(p) = resp.dnd_release_payload::<DragFiles>() {
                                    drop_target = Some((full.clone(), p.paths.clone()));
                                }
                            }
                        }
                    }
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
                    // A keyboard-opened menu (Shift+F10) anchors to the row
                    // instead of the pointer, for as long as it stays open.
                    let popup = if lead == Some(row_index) && (kbd_req || kbd_open) {
                        egui::Popup::menu(&resp)
                            .open_memory(kbd_req.then_some(egui::SetOpenCommand::Bool(true)))
                    } else {
                        egui::Popup::context_menu(&resp)
                    };
                    popup.show(|ui| {
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
                        if e.is_dir && ui.button("Pin to Quick access").clicked() {
                            pin_req = Some(full.clone());
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
        self.pane.entries = entries;
        self.pane.folder_sizes = folder_sizes;
        self.pane.sel = sel;
        // Rubber-band selection: every frame of the drag, select the rows the
        // band touches (on top of `base`), and paint the band.
        if let Some((start, base)) = self.pane.marquee.as_ref().map(|m| (m.start, m.base.clone())) {
            if let Some(cur_pos) = ui.ctx().pointer_latest_pos() {
                let band = Rect::from_two_pos(start, cur_pos);
                let hit: Vec<usize> =
                    row_rects.iter().filter(|(_, r)| r.intersects(band)).map(|(i, _)| *i).collect();
                let mut sel = base;
                for &i in &hit {
                    sel.insert(self.pane.entries[i].name.clone());
                }
                self.pane.sel = sel;
                if let (Some(&lo), Some(&hi)) = (hit.iter().min(), hit.iter().max()) {
                    self.pane.anchor = Some(lo);
                    self.pane.lead = Some(hi);
                }
                let accent = lead_color;
                ui.painter().rect(
                    band,
                    0.0_f32,
                    Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 40),
                    Stroke::new(1.0, accent),
                    egui::StrokeKind::Inside,
                );
                ui.ctx().request_repaint();
            }
            if bg.drag_stopped() || !ui.ctx().input(|i| i.pointer.any_down()) {
                self.pane.marquee = None;
            }
        }
        // Focus cursor: a thin outline around the lead row — but only when it
        // says something the selection highlight doesn't: the cursor has been
        // moved off the selection (Ctrl+Arrow), or several rows are selected
        // and this is the one the keyboard acts on. A plain single selection
        // gets no extra mark.
        if list_focus {
            if let Some(l) = self.pane.lead {
                let on_selection = self.pane.entries.get(l).is_some_and(|e| self.pane.sel.contains(&e.name));
                if !on_selection || self.pane.sel.len() > 1 {
                    if let Some((_, r)) = row_rects.iter().find(|(i, _)| *i == l) {
                        ui.painter().rect_stroke(
                            r.shrink(0.5),
                            2.0_f32,
                            Stroke::new(1.0, lead_color.gamma_multiply(0.7)),
                            egui::StrokeKind::Inside,
                        );
                    }
                }
            }
        }
        // Drag-and-drop: highlight the hovered folder row; select the row a
        // drag started on (Explorer does); perform a drop (Ctrl = copy).
        if let Some(r) = drop_hover_rect {
            ui.painter().rect_stroke(r, 3.0_f32, Stroke::new(2.0, lead_color), egui::StrokeKind::Inside);
        }
        if let Some(i) = drag_start {
            if !self.pane.entries.get(i).is_some_and(|e| self.pane.sel.contains(&e.name)) {
                self.pane.select_only(i);
            }
            self.focus_pane = Focus::List;
        }
        if let Some((dest, paths)) = drop_target {
            let copy = ui.ctx().input(|i| i.modifiers.ctrl);
            self.start_transfer(paths, dest, !copy);
        }
        // Keyboard context menu: the request was consumed by the lead row
        // this frame; once the menu closes, stop anchoring to it.
        if self.pane.kbd_menu_req {
            self.pane.kbd_menu_req = false;
            self.pane.kbd_menu_open = true;
        }
        if self.pane.kbd_menu_open && !egui::Popup::is_any_open(ui.ctx()) {
            self.pane.kbd_menu_open = false;
        }
        // Apply a click: Shift = range, Ctrl = toggle, plain = select one.
        if let Some(i) = click {
            if mods.shift {
                self.pane.select_range_to(i);
            } else if mods.ctrl {
                self.pane.toggle_at(i);
            } else {
                self.pane.select_only(i);
            }
            self.focus_pane = Focus::List; // keyboard now drives the list
        }
        // A right-click on an unselected row selects just it (keeps a
        // multi-selection when right-clicking within it).
        if let Some(i) = menu_row {
            let in_sel =
                self.pane.entries.get(i).map(|e| self.pane.sel.contains(&e.name)).unwrap_or(false);
            if !in_sel {
                self.pane.select_only(i);
            }
        }
        if let Some(k) = new_sort {
            if self.pane.sort_key == k {
                self.pane.sort_asc = !self.pane.sort_asc;
            } else {
                self.pane.sort_key = k;
                self.pane.sort_asc = true;
            }
            // Re-sort the full listing IN PLACE (no directory re-read), then
            // re-apply the filter (which preserves the selection by name).
            let mut es = std::mem::take(&mut self.pane.all_entries);
            self.pane.sort_entries(&mut es);
            self.pane.all_entries = es;
            self.apply_filter();
            self.pane.scroll_target = self.pane.lead; // keep the selection visible
        }
        if let Some(t) = nav_target {
            self.navigate_to(t);
        }
        if let Some(name) = rename_req {
            self.open_rename(name);
        }
        if let Some(p) = pin_req {
            self.pin(p);
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
    }


    /// Keyboard navigation for the folder tree (when it has focus): ↑/↓ move &
    /// navigate, → expand/first-child, ← collapse/parent, Home/End to the ends.
    fn tree_keys(&mut self, ui: &egui::Ui) {
        use egui::Key;
        let (down, up, right, left, home, end) = ui.input(|i| {
            // Alt+Left/Right are history back/forward, not collapse/expand.
            let plain = !i.modifiers.alt;
            (
                plain && i.key_pressed(Key::ArrowDown),
                plain && i.key_pressed(Key::ArrowUp),
                plain && i.key_pressed(Key::ArrowRight),
                plain && i.key_pressed(Key::ArrowLeft),
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
        let cur = visible.iter().position(|p| p == &self.pane.current_dir);
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
            let cur_dir = self.pane.current_dir.clone();
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
            let cur_dir = self.pane.current_dir.clone();
            if self.sb_expanded.contains(&cur_dir) {
                self.sb_expanded.remove(&cur_dir);
            } else if let Some(parent) = cur_dir.parent().map(|p| p.to_path_buf()) {
                go(self, parent);
            }
        }
        self.focus_pane = Focus::Tree;
        ui.ctx().request_repaint();
    }

    /// Type-ahead in the folder tree: jump to the next visible node (after the
    /// current one, wrapping) whose name matches — so it cycles forward through
    /// matches rather than yanking back to the top of the tree.
    fn type_ahead_tree(&mut self, ui: &egui::Ui) {
        let Some((q, _repeat)) = self.pane.type_ahead_input(ui) else { return };
        let visible = self.sb_visible_nodes();
        let n = visible.len();
        if n == 0 {
            return;
        }
        let cur = visible.iter().position(|p| p == &self.pane.current_dir);
        let start = cur.map_or(0, |i| i + 1);
        let name_of = |p: &Path| {
            p.file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.to_string_lossy().into_owned())
                .to_lowercase()
        };
        let hit = (0..n)
            .map(|off| (start + off) % n)
            .find(|&i| name_of(&visible[i]).starts_with(&q));
        if let Some(i) = hit {
            self.navigate_to(visible[i].clone());
            self.tree_scroll = true;
            self.focus_pane = Focus::Tree;
            ui.ctx().request_repaint();
        }
    }

}

/// One extruded block (a leaf tile) in WORLD space: its footprint on the
/// layout plane, its height, and its colour — before any camera. Built by
/// [`build_blocks`]; [`project_scene`] turns these into screen quads.
struct Block3 {
    node: NodeId,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    /// Height above the plane (0 = a flat chip).
    z: f64,
    color: Color32,
}

/// A projected block: its top, the (≤ 2) side faces that face the camera with
/// their shade factor, and its ground shadow — all in screen space.
struct IsoBlock {
    node: NodeId,
    top: [Pos2; 4],
    sides: Vec<([Pos2; 4], f32)>,
    shadow: [Pos2; 4],
    color: Color32,
}

/// The whole scene: a ground plinth plus the blocks, all pre-projected and in
/// back-to-front order.
struct Scape {
    plinth_top: [Pos2; 4],
    plinth_sides: Vec<([Pos2; 4], Color32)>,
    blocks: Vec<IsoBlock>,
}

impl Default for Scape {
    fn default() -> Self {
        Scape { plinth_top: [Pos2::ZERO; 4], plinth_sides: Vec::new(), blocks: Vec::new() }
    }
}

/// The City camera. Today a 2.5D dimetric projection with a turntable (`yaw`),
/// `tilt` (how much the ground plane is foreshortened), `zoom` and a screen
/// `pan`. A perspective fly-through (FSN, Step 3b) replaces [`Camera::view`]
/// and the projection stage; the world-space blocks stay as they are.
#[derive(Clone, Copy, PartialEq)]
struct Camera {
    /// Turntable rotation of the plane about its centre, radians.
    yaw: f64,
    /// Ground-plane foreshortening — the dimetric "y" factor (0.25 = the
    /// classic look; smaller = flatter, larger = more top-down).
    tilt: f64,
    zoom: f64,
    pan: Vec2,
}

impl Default for Camera {
    fn default() -> Self {
        Camera { yaw: 0.0, tilt: ISO_AY, zoom: 1.0, pan: Vec2::ZERO }
    }
}

impl Camera {
    /// A plane point rotated about the plane centre by `yaw`.
    fn rotate(&self, x: f64, y: f64) -> (f64, f64) {
        let c = PLANE / 2.0;
        let (s, k) = self.yaw.sin_cos();
        let (dx, dy) = (x - c, y - c);
        (c + dx * k - dy * s, c + dx * s + dy * k)
    }

    /// A world direction rotated by `yaw` (for face visibility and shading).
    fn rotate_dir(&self, nx: f64, ny: f64) -> (f64, f64) {
        let (s, k) = self.yaw.sin_cos();
        (nx * k - ny * s, nx * s + ny * k)
    }

    /// The inverse of [`Self::rotate_dir`]: a view-frame direction in world terms.
    fn unrotate_dir(&self, vx: f64, vy: f64) -> (f64, f64) {
        let (s, k) = self.yaw.sin_cos();
        (vx * k + vy * s, -vx * s + vy * k)
    }

    /// World (x, y on the plane, z up) → unscaled view coordinates. The viewer
    /// looks in from the rotated +x+y side; x' grows right, y' grows down.
    fn view(&self, x: f64, y: f64, z: f64) -> (f64, f64) {
        let (rx, ry) = self.rotate(x, y);
        ((rx - ry) * ISO_AX, (rx + ry) * self.tilt - z)
    }

    /// Painter's-order depth of a plane point: larger = nearer the viewer.
    fn depth(&self, x: f64, y: f64) -> f64 {
        let (rx, ry) = self.rotate(x, y);
        rx + ry
    }

    /// Does a vertical face with outward normal (nx, ny) face the viewer?
    fn faces_viewer(&self, nx: f64, ny: f64) -> bool {
        let (rx, ry) = self.rotate_dir(nx, ny);
        rx + ry > 1e-9
    }

    /// Side-face shade: a face turned toward +x' (the classic "right" face)
    /// gets F_RIGHT, one toward +y' ("front") F_FRONT; rotation blends them.
    fn side_shade(&self, nx: f64, ny: f64) -> f32 {
        let (rx, _) = self.rotate_dir(nx, ny);
        F_FRONT + (F_RIGHT - F_FRONT) * rx.clamp(0.0, 1.0) as f32
    }
}

// Dimetric projection constants: x→right, y→left, z→up.
const ISO_AX: f64 = 0.5;
const ISO_AY: f64 = 0.25;
const PLANE: f64 = 1000.0; // ground-plane size the treemap is laid out on
const PLINTH_TH: f64 = 26.0;
const SHADOW_EXPAND: f64 = 2.5;
const SHADOW_OFF: f64 = 0.4;

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

/// A block is hit if the cursor is over ANY of its visible faces (top or a
/// side) — so tall slender towers select from their bulk, not just the tiny top.
fn point_in_block(p: Pos2, b: &IsoBlock) -> bool {
    point_in_quad(p, &b.top) || b.sides.iter().any(|(q, _)| point_in_quad(p, q))
}

/// The four vertical faces of an axis-aligned footprint: (corner a, corner b,
/// outward normal), a→b ordered so the face quad (a, b, b↑, a↑) is convex.
fn box_sides(x: f64, y: f64, w: f64, h: f64) -> [((f64, f64), (f64, f64), (f64, f64)); 4] {
    [
        ((x + w, y), (x + w, y + h), (1.0, 0.0)), // +x ("right" at yaw 0)
        ((x, y + h), (x + w, y + h), (0.0, 1.0)), // +y ("front" at yaw 0)
        ((x, y), (x, y + h), (-1.0, 0.0)),        // −x
        ((x, y), (x + w, y), (0.0, -1.0)),        // −y
    ]
}

/// Stage 1 — WORLD space. Leaf tiles become blocks: area = bytes (the tile),
/// height ∝ log(file count), colour = dominant type; heights are scaled by
/// `reveal` (the cache-load "rise"). No camera involved.
fn build_blocks(
    tree: &Tree,
    tiles: &[Tile],
    dominant: Option<&[FileCategory]>,
    reveal: f32,
    // Optional per-node file counts (for replay's partial state); falls back to
    // the tree's final counts when `None`.
    file_counts: Option<&[u64]>,
) -> Vec<Block3> {
    use std::collections::HashSet;
    let rendered: HashSet<usize> = tiles.iter().map(|t| t.node.index()).collect();
    let leaves: Vec<&Tile> = tiles
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
        return Vec::new();
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

    leaves
        .iter()
        .filter_map(|t| {
            // Discovery reveal: blocks appear in the order they were found (arena
            // index = scan order), each rising in its final spot.
            let z = if reveal >= 1.0 {
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
            Some(Block3 {
                node: t.node,
                x: t.rect.x as f64,
                y: t.rect.y as f64,
                w: t.rect.w as f64,
                h: t.rect.h as f64,
                z,
                color,
            })
        })
        .collect()
}

/// Stage 2 — SCREEN space. Project the blocks through `cam`, fitted to
/// `panel`, sorted back-to-front, with the plinth and per-block shadows.
fn project_scene(blocks: &[Block3], cam: &Camera, panel: Rect) -> Scape {
    // Fit with a yaw-INVARIANT bound (the plane's bounding circle, the tallest
    // block, the plinth), so orbiting doesn't make the city breathe. At yaw 0
    // it equals the exact bound; at other angles the city sits a little
    // smaller, never overflowing.
    let max_z = blocks.iter().map(|b| b.z).fold(0.0, f64::max);
    let (cx, cy) = cam.view(PLANE / 2.0, PLANE / 2.0, 0.0);
    let (minx, maxx) = (cx - PLANE * ISO_AX, cx + PLANE * ISO_AX);
    let (miny, maxy) = (cy - PLANE * cam.tilt - max_z, cy + PLANE * cam.tilt + PLINTH_TH);
    let pw = (maxx - minx).max(1.0);
    let ph = (maxy - miny).max(1.0);
    let s = ((panel.width() as f64 * 0.96) / pw).min((panel.height() as f64 * 0.96) / ph) * cam.zoom;
    let ox = panel.center().x as f64 - (minx + maxx) / 2.0 * s + cam.pan.x as f64;
    let oy = panel.center().y as f64 - (miny + maxy) / 2.0 * s + cam.pan.y as f64;
    let tr = |x: f64, y: f64, z: f64| -> Pos2 {
        let (vx, vy) = cam.view(x, y, z);
        Pos2::new((vx * s + ox) as f32, (vy * s + oy) as f32)
    };

    // The ground plinth: its top and whichever sides face the camera.
    let plinth_top = [tr(0.0, 0.0, 0.0), tr(PLANE, 0.0, 0.0), tr(PLANE, PLANE, 0.0), tr(0.0, PLANE, 0.0)];
    let plinth_sides = box_sides(0.0, 0.0, PLANE, PLANE)
        .into_iter()
        .filter(|(_, _, n)| cam.faces_viewer(n.0, n.1))
        .map(|(a, b, n)| {
            let quad = [tr(a.0, a.1, 0.0), tr(b.0, b.1, 0.0), tr(b.0, b.1, -PLINTH_TH), tr(a.0, a.1, -PLINTH_TH)];
            let color = if cam.rotate_dir(n.0, n.1).0 > 0.5 { PLINTH_R } else { PLINTH_F };
            (quad, color)
        })
        .collect();

    // Back-to-front by each footprint's FAR corner (its minimum view depth):
    // the classic isometric painter's order for non-overlapping tiles, and
    // exactly the old `x + y` of the min corner at yaw 0.
    let far = |b: &Block3| {
        [(b.x, b.y), (b.x + b.w, b.y), (b.x, b.y + b.h), (b.x + b.w, b.y + b.h)]
            .into_iter()
            .map(|(x, y)| cam.depth(x, y))
            .fold(f64::INFINITY, f64::min)
    };
    let mut order: Vec<&Block3> = blocks.iter().collect();
    order.sort_by(|a, b| far(a).partial_cmp(&far(b)).unwrap_or(std::cmp::Ordering::Equal));
    let blocks = order
        .iter()
        .map(|b| {
            let (x, y, w, h, z) = (b.x, b.y, b.w, b.h, b.z);
            let top = [tr(x, y, z), tr(x + w, y, z), tr(x + w, y + h, z), tr(x, y + h, z)];
            let sides = box_sides(x, y, w, h)
                .into_iter()
                .filter(|(_, _, n)| cam.faces_viewer(n.0, n.1))
                .map(|(a, c, n)| {
                    ([tr(a.0, a.1, 0.0), tr(c.0, c.1, 0.0), tr(c.0, c.1, z), tr(a.0, a.1, z)], cam.side_shade(n.0, n.1))
                })
                .collect();
            // Ground shadow: footprint expanded, offset toward the viewer by the
            // height (tall → long). The offset is a view-frame direction.
            let e = SHADOW_EXPAND;
            let (dx, dy) = cam.unrotate_dir(z * SHADOW_OFF, z * SHADOW_OFF);
            let shadow = [
                tr(x - e + dx, y - e + dy, 0.0),
                tr(x + w + e + dx, y - e + dy, 0.0),
                tr(x + w + e + dx, y + h + e + dy, 0.0),
                tr(x - e + dx, y + h + e + dy, 0.0),
            ];
            IsoBlock { node: b.node, top, sides, shadow, color: b.color }
        })
        .collect();

    Scape { plinth_top, plinth_sides, blocks }
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
    // `Path::exists` follows links and says "no" for a DANGLING symlink — a
    // paste onto that name would then write *through* the link to its target.
    // `symlink_metadata` reports the link itself, so any entry counts as taken.
    let occupied = |p: &Path| std::fs::symlink_metadata(p).is_ok();
    let first = dir.join(name);
    if !occupied(&first) {
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
    if !occupied(&c) {
        return c;
    }
    for i in 2..100_000 {
        let c = make(&format!(" - Copy ({i})"));
        if !occupied(&c) {
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
/// copy failure the partial (fresh) destination is cleaned up. Every completed
/// (source, destination) pair is reported, even when a later item fails.
fn run_paste(
    sources: Vec<PathBuf>,
    dest_dir: PathBuf,
    cut: bool,
    cancel: Arc<AtomicBool>,
) -> PasteOutcome {
    let mut done: Vec<(PathBuf, PathBuf)> = Vec::new();
    for src in &sources {
        let name = match src.file_name() {
            Some(n) => n.to_string_lossy().into_owned(),
            None => return PasteOutcome { done, error: Some("Invalid source path.".into()) },
        };
        // Refuse to copy/move a link or junction as a whole (v1).
        if std::fs::symlink_metadata(src).map(|m| is_reparse(&m)).unwrap_or(false) {
            let error = Some(format!("“{name}” is a link/junction — not copied."));
            return PasteOutcome { done, error };
        }
        let dst = unique_dest(&dest_dir, &name);

        if cut {
            // Same-volume: atomic rename (fast, preserves junctions inside).
            if std::fs::rename(src, &dst).is_ok() {
                done.push((src.clone(), dst));
                continue;
            }
            // Cross-volume: copy, then remove the source.
            if let Err(e) = copy_any(src, &dst, &cancel, 0) {
                cleanup_partial(&dst, &e);
                return PasteOutcome { done, error: Some(format!("Move failed: {e}")) };
            }
            if let Err(e) = remove_any(src) {
                // Copy is good; do NOT delete it. Report that the source remains.
                // (Not recorded for Undo: it is neither a clean move nor a copy.)
                let error = Some(format!(
                    "Copied “{name}”, but couldn't remove the original ({e}) — both remain."
                ));
                return PasteOutcome { done, error };
            }
        } else if let Err(e) = copy_any(src, &dst, &cancel, 0) {
            cleanup_partial(&dst, &e);
            return PasteOutcome { done, error: Some(format!("Copy failed: {e}")) };
        }
        done.push((src.clone(), dst));
    }
    PasteOutcome { done, error: None }
}

/// Move a pasted item back to where it came from — its ORIGINAL name at its
/// original parent (a paste may have auto-renamed it "… - Copy"). Never
/// overwrites: if something else now has the original name, it lands beside it
/// with the usual suffix. Same-volume rename, else copy-back-then-delete.
fn move_back(dst: &Path, src: &Path) -> Result<PathBuf, String> {
    let parent = src.parent().ok_or_else(|| "Invalid original path.".to_string())?;
    let target = unique_dest(parent, &name_of(src));
    if std::fs::rename(dst, &target).is_ok() {
        return Ok(target);
    }
    let cancel = AtomicBool::new(false);
    copy_any(dst, &target, &cancel, 0).map_err(|e| {
        cleanup_partial(&target, &e);
        e.to_string()
    })?;
    remove_any(dst)
        .map_err(|e| format!("copied back, but couldn't remove “{}”: {e}", name_of(dst)))?;
    Ok(target)
}

/// Worker body for Undo/Redo: execute the reversal `op`. Returns the item to
/// select afterwards and the inverse to put on the opposite stack.
fn run_undo(op: UndoOp) -> Result<UndoDone, String> {
    match op {
        UndoOp::Rename { from, to } => {
            let case_only = name_of(&from).to_lowercase() == name_of(&to).to_lowercase();
            if !case_only && std::fs::symlink_metadata(&to).is_ok() {
                return Err(format!("Can't undo the rename: something is now named “{}”.", name_of(&to)));
            }
            std::fs::rename(&from, &to).map_err(|e| format!("Couldn't undo the rename: {e}"))?;
            let inverse = Some(UndoOp::Rename { from: to.clone(), to: from });
            Ok(UndoDone { select: Some(to), inverse })
        }
        UndoOp::RemoveDir { path } => {
            // remove_dir refuses a non-empty folder — exactly the safety we want.
            std::fs::remove_dir(&path).map_err(|e| {
                format!("Couldn't remove “{}” — it may no longer be empty ({e}).", name_of(&path))
            })?;
            Ok(UndoDone { select: None, inverse: Some(UndoOp::MakeDir { path }) })
        }
        UndoOp::MakeDir { path } => {
            std::fs::create_dir(&path)
                .map_err(|e| format!("Couldn't re-create “{}”: {e}", name_of(&path)))?;
            let inverse = Some(UndoOp::RemoveDir { path: path.clone() });
            Ok(UndoDone { select: Some(path), inverse })
        }
        UndoOp::Trash { paths } => {
            let mut errors = Vec::new();
            let mut recycled = Vec::new();
            for d in &paths {
                let r = if is_network_path(d) {
                    remove_any(d).map(|()| false).map_err(|e| e.to_string())
                } else {
                    trash::delete(d).map(|()| true).map_err(|e| e.to_string())
                };
                match r {
                    Ok(true) => recycled.push(d.clone()),
                    Ok(false) => {}
                    Err(e) => errors.push(format!("{}: {e}", name_of(d))),
                }
            }
            if !errors.is_empty() {
                return Err(format!("Couldn't remove some items: {}", errors.join("; ")));
            }
            // Only what reached the Recycle Bin can be brought back.
            let inverse = (!recycled.is_empty()).then_some(UndoOp::Restore { paths: recycled });
            Ok(UndoDone { select: None, inverse })
        }
        UndoOp::MoveBack { pairs } => {
            // Reverse order (later items first), recording where each actually
            // landed — a taken name means "… - Copy" — so the inverse is exact.
            let mut landed: Vec<(PathBuf, PathBuf)> = Vec::new();
            for (src, dst) in pairs.iter().rev() {
                let t = move_back(dst, src)
                    .map_err(|e| format!("Couldn't move “{}” back: {e}", name_of(dst)))?;
                landed.push((dst.clone(), t));
            }
            landed.reverse();
            let select = landed.first().map(|(_, t)| t.clone());
            Ok(UndoDone { select, inverse: Some(UndoOp::MoveBack { pairs: landed }) })
        }
        UndoOp::Restore { paths } => {
            let restored = restore_from_trash(&paths)?;
            let select = restored.last().cloned();
            let inverse = (!restored.is_empty()).then_some(UndoOp::Trash { paths: restored });
            Ok(UndoDone { select, inverse })
        }
    }
}

/// Restore `paths` from the Recycle Bin (the most recently deleted item that
/// came from each path). Never overwrites: the trash crate refuses to restore
/// over an existing file.
#[cfg(any(windows, target_os = "linux"))]
fn restore_from_trash(paths: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    use trash::os_limited::{list, restore_all};
    let items = list().map_err(|e| format!("Couldn't read the Recycle Bin: {e}"))?;
    // The shell reports the original location in ITS casing (NTFS is
    // case-insensitive), so compare paths case-insensitively.
    let key = |p: &Path| p.to_string_lossy().to_lowercase();
    let mut picked: Vec<(usize, PathBuf)> = Vec::new();
    for p in paths {
        let want = key(p);
        let best = items
            .iter()
            .enumerate()
            .filter(|(_, it)| key(&it.original_path()) == want)
            .max_by_key(|(_, it)| it.time_deleted)
            .map(|(i, it)| (i, it.original_path()));
        match best {
            Some(b) => picked.push(b),
            None => return Err(format!("“{}” isn't in the Recycle Bin any more.", name_of(p))),
        }
    }
    let restored: Vec<PathBuf> = picked.iter().map(|(_, p)| p.clone()).collect();
    let idx: Vec<usize> = picked.iter().map(|(i, _)| *i).collect();
    let chosen = items.into_iter().enumerate().filter(|(i, _)| idx.contains(i)).map(|(_, it)| it);
    restore_all(chosen).map_err(|e| format!("Couldn't restore from the Recycle Bin: {e}"))?;
    Ok(restored)
}
#[cfg(not(any(windows, target_os = "linux")))]
fn restore_from_trash(_paths: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    Err("Restoring from the trash isn't supported on this platform.".into())
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

/// Free (available to this user) and total bytes of the volume holding `path`
/// — any folder on it will do. `None` if the volume can't be queried (e.g. a
/// disconnected share).
#[cfg(target_os = "windows")]
fn drive_space(path: &Path) -> Option<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    extern "system" {
        fn GetDiskFreeSpaceExW(
            dir: *const u16,
            free_to_caller: *mut u64,
            total: *mut u64,
            total_free: *mut u64,
        ) -> i32;
    }
    // A bare share root ("\\srv\share") needs its trailing backslash.
    let mut s = path.as_os_str().to_os_string();
    if path.file_name().is_none() && !path.to_string_lossy().ends_with('\\') {
        s.push("\\");
    }
    let wide: Vec<u16> = s.encode_wide().chain(std::iter::once(0)).collect();
    let (mut avail, mut total, mut total_free) = (0u64, 0u64, 0u64);
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, &mut total, &mut total_free) };
    (ok != 0 && total > 0).then_some((avail, total))
}
#[cfg(not(target_os = "windows"))]
fn drive_space(_path: &Path) -> Option<(u64, u64)> {
    None
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

/// The Windows file clipboard — `CF_HDROP` plus the "Preferred DropEffect"
/// flag — so Cut/Copy/Paste interoperate with Explorer and other apps. The
/// `DROPFILES` encode/decode is pure and unit-tested on Linux; only the
/// clipboard calls are Windows-specific (raw Win32, like the rest of the app).
/// Non-Windows builds get inert stubs so the app still checks and tests there.
mod sys_clipboard {
    #![cfg_attr(not(windows), allow(dead_code))]
    use std::path::PathBuf;

    /// Files on the clipboard, and whether they were Cut (= move on paste).
    pub struct Files {
        pub paths: Vec<PathBuf>,
        pub cut: bool,
    }

    /// `DROPFILES` header: offset to the list, drop point (x, y), non-client
    /// flag, wide-char flag — followed by NUL-terminated paths and a final NUL.
    const HEADER: usize = 20;
    const DROPEFFECT_COPY: u32 = 1;
    const DROPEFFECT_MOVE: u32 = 2;

    /// Build a `CF_HDROP` payload (UTF-16 paths).
    pub fn hdrop_encode(paths: &[PathBuf]) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER + 64 * paths.len());
        out.extend_from_slice(&(HEADER as u32).to_le_bytes()); // pFiles
        out.extend_from_slice(&0i32.to_le_bytes()); // pt.x
        out.extend_from_slice(&0i32.to_le_bytes()); // pt.y
        out.extend_from_slice(&0i32.to_le_bytes()); // fNC
        out.extend_from_slice(&1i32.to_le_bytes()); // fWide
        for p in paths {
            for u in p.to_string_lossy().encode_utf16() {
                out.extend_from_slice(&u.to_le_bytes());
            }
            out.extend_from_slice(&0u16.to_le_bytes());
        }
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    /// Parse a `CF_HDROP` payload (wide or, from an old app, ANSI).
    pub fn hdrop_decode(bytes: &[u8]) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if bytes.len() < HEADER {
            return out;
        }
        let u32_at = |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
        let start = u32_at(0) as usize;
        let wide = u32_at(16) != 0;
        if start > bytes.len() {
            return out;
        }
        if wide {
            let units: Vec<u16> =
                bytes[start..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            for s in units.split(|&u| u == 0) {
                if s.is_empty() {
                    break; // the double NUL
                }
                out.push(PathBuf::from(String::from_utf16_lossy(s)));
            }
        } else {
            for s in bytes[start..].split(|&b| b == 0) {
                if s.is_empty() {
                    break;
                }
                out.push(PathBuf::from(s.iter().map(|&b| b as char).collect::<String>()));
            }
        }
        out
    }

    #[cfg(windows)]
    mod imp {
        use super::{hdrop_decode, hdrop_encode, Files, DROPEFFECT_COPY, DROPEFFECT_MOVE};
        use std::ffi::c_void;
        use std::os::windows::ffi::OsStrExt;
        use std::path::PathBuf;

        const CF_HDROP: u32 = 15;
        const GMEM_MOVEABLE: u32 = 0x0002;

        #[link(name = "user32")]
        extern "system" {
            fn OpenClipboard(hwnd: *mut c_void) -> i32;
            fn CloseClipboard() -> i32;
            fn EmptyClipboard() -> i32;
            fn GetClipboardData(format: u32) -> *mut c_void;
            fn SetClipboardData(format: u32, h: *mut c_void) -> *mut c_void;
            fn IsClipboardFormatAvailable(format: u32) -> i32;
            fn RegisterClipboardFormatW(name: *const u16) -> u32;
            fn GetClipboardSequenceNumber() -> u32;
        }
        extern "system" {
            fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
            fn GlobalLock(h: *mut c_void) -> *mut c_void;
            fn GlobalUnlock(h: *mut c_void) -> i32;
            fn GlobalSize(h: *mut c_void) -> usize;
            fn GlobalFree(h: *mut c_void) -> *mut c_void;
        }

        /// Holds the clipboard open; closes it on drop. Retries briefly — another
        /// app may have it open for a moment.
        struct Open;
        impl Open {
            fn new() -> Option<Self> {
                for _ in 0..4 {
                    if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
                        return Some(Open);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                None
            }
        }
        impl Drop for Open {
            fn drop(&mut self) {
                unsafe { CloseClipboard() };
            }
        }

        /// The registered "Preferred DropEffect" format (0 if registration fails,
        /// which `GetClipboardData` then simply doesn't find).
        fn effect_format() -> u32 {
            let name: Vec<u16> = std::ffi::OsStr::new("Preferred DropEffect")
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            unsafe { RegisterClipboardFormatW(name.as_ptr()) }
        }

        /// Copy a clipboard handle's bytes out (clipboard must be open).
        fn global_bytes(h: *mut c_void) -> Option<Vec<u8>> {
            if h.is_null() {
                return None;
            }
            unsafe {
                let p = GlobalLock(h) as *const u8;
                if p.is_null() {
                    return None;
                }
                let v = std::slice::from_raw_parts(p, GlobalSize(h)).to_vec();
                GlobalUnlock(h);
                Some(v)
            }
        }

        /// Bytes → a fresh movable global block. Ownership passes to the system
        /// on a successful `SetClipboardData`; the caller frees it otherwise.
        fn global_from(bytes: &[u8]) -> *mut c_void {
            unsafe {
                let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len());
                if h.is_null() {
                    return h;
                }
                let p = GlobalLock(h) as *mut u8;
                if p.is_null() {
                    GlobalFree(h);
                    return std::ptr::null_mut();
                }
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
                GlobalUnlock(h);
                h
            }
        }

        pub fn sequence() -> u32 {
            unsafe { GetClipboardSequenceNumber() }
        }

        pub fn read() -> Option<Files> {
            if unsafe { IsClipboardFormatAvailable(CF_HDROP) } == 0 {
                return None; // cheap pre-check, no open needed
            }
            let _open = Open::new()?;
            let paths = hdrop_decode(&global_bytes(unsafe { GetClipboardData(CF_HDROP) })?);
            if paths.is_empty() {
                return None;
            }
            let effect = global_bytes(unsafe { GetClipboardData(effect_format()) })
                .filter(|b| b.len() >= 4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .unwrap_or(DROPEFFECT_COPY);
            // Explorer marks a Cut as MOVE (2) and a Copy as COPY|LINK (5).
            let cut = effect & DROPEFFECT_MOVE != 0 && effect & DROPEFFECT_COPY == 0;
            Some(Files { paths, cut })
        }

        pub fn write(paths: &[PathBuf], cut: bool) -> bool {
            let Some(_open) = Open::new() else { return false };
            unsafe { EmptyClipboard() };
            let h = global_from(&hdrop_encode(paths));
            if h.is_null() {
                return false;
            }
            if unsafe { SetClipboardData(CF_HDROP, h) }.is_null() {
                unsafe { GlobalFree(h) };
                return false;
            }
            let effect = if cut { DROPEFFECT_MOVE } else { DROPEFFECT_COPY };
            let e = global_from(&effect.to_le_bytes());
            if !e.is_null() && unsafe { SetClipboardData(effect_format(), e) }.is_null() {
                unsafe { GlobalFree(e) };
            }
            true
        }

        pub fn clear() {
            if let Some(_open) = Open::new() {
                unsafe { EmptyClipboard() };
            }
        }
    }
    #[cfg(windows)]
    pub use imp::{clear, read, sequence, write};

    #[cfg(not(windows))]
    pub fn sequence() -> u32 {
        0
    }
    #[cfg(not(windows))]
    pub fn read() -> Option<Files> {
        None
    }
    #[cfg(not(windows))]
    pub fn write(_paths: &[PathBuf], _cut: bool) -> bool {
        false
    }
    #[cfg(not(windows))]
    pub fn clear() {}
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
            current_dir: self.pane.current_dir.to_string_lossy().into_owned(),
            show_hidden: self.show_hidden,
            sort_key: self.pane.sort_key.name().to_string(),
            sort_asc: self.pane.sort_asc,
            auto_scan_local: self.auto_scan_local,
            pins: self.pins.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
        };
        eframe::set_value(storage, "settings", &s);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.menu_open_at_start = egui::Popup::is_any_open(&ctx);

        // Ctrl+wheel (and touchpad pinch) zoom the UI, like Ctrl+= / Ctrl+- do.
        // egui already turns those gestures into a zoom delta (and keeps them
        // away from the scroll areas); it just doesn't apply it by itself.
        let zoom = ctx.input(|i| i.zoom_delta());
        if zoom != 1.0 {
            let z = (ctx.zoom_factor() * zoom).clamp(0.5, 3.0);
            ctx.set_zoom_factor(z);
        }

        // Reflect the current folder in the window/taskbar title (only on change).
        let title = format!("{} — {}", sector_core::APP_NAME, self.pane.current_dir.display());
        if title != self.last_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }

        // Mirror the Windows file clipboard: files cut/copied in Explorer (or by
        // us) become our clipboard; anything else on it (text…) clears ours,
        // as in Explorer. The sequence number makes this a trivial per-frame
        // check; the clipboard is only opened when it actually changed.
        let seq = sys_clipboard::sequence();
        if seq != self.clip_seq {
            self.clip_seq = seq;
            self.clipboard = sys_clipboard::read().map(|f| Clipboard { paths: f.paths, cut: f.cut });
        }

        // Poll a background cache load (City). A result for a folder we've
        // since left is dropped; sync_city will start the right one.
        let mut loaded: Option<(Result<LoadedCache, CacheLoadError>, PathBuf)> = None;
        if let Some((rx, dir)) = &self.cache_load {
            match rx.try_recv() {
                Ok(r) => loaded = Some((r, dir.clone())),
                Err(std::sync::mpsc::TryRecvError::Empty) => ctx.request_repaint(),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // The worker died; that says nothing about the file.
                    let e = CacheLoadError { msg: "the loader stopped unexpectedly".into(), unusable: false };
                    loaded = Some((Err(e), dir.clone()));
                }
            }
        }
        if let Some((result, dir)) = loaded {
            self.cache_load = None;
            if dir == self.pane.current_dir {
                match result {
                    Ok(lc) => self.install_cache(lc),
                    Err(e) => {
                        eprintln!("[sector] cache: load failed: {}", e.msg);
                        // An UNUSABLE file (older format, corrupt, wrong folder)
                        // is deleted so the "Load cached" button stops offering
                        // it and a rescan writes a fresh one. A read error or a
                        // dead worker keeps the file.
                        if e.unusable {
                            if let Some(cp) = cache_path_for(&dir.to_string_lossy()) {
                                let _ = std::fs::remove_file(cp);
                            }
                            self.cache_mtime = None;
                        }
                        self.city_no_cache();
                        if matches!(self.scan, ScanState::Idle) {
                            self.city_note = Some(format!(
                                "Couldn't load the cached scan — {}. Press Scan to rebuild it.",
                                e.msg
                            ));
                        }
                    }
                }
            }
        }

        // Poll the background scan for completion.
        let mut just_finished = false;
        if let ScanState::Running { stats_rx, stats, progress, started, .. } = &mut self.scan {
            if stats.is_none() {
                match stats_rx.try_recv() {
                    Ok(s) => {
                        *stats = Some(s);
                        just_finished = true;
                    }
                    // keep progress + live build ticking
                    Err(std::sync::mpsc::TryRecvError::Empty) => ctx.request_repaint(),
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        // The scanner thread died without reporting (a panic).
                        // Land the scan as cancelled so the UI recovers — the
                        // partial tree stays viewable, but it is neither cached
                        // nor used for folder sizes.
                        eprintln!("[sector] scan thread stopped unexpectedly");
                        *stats = Some(ScanStats {
                            dirs: progress.dirs.load(Ordering::Relaxed),
                            files: progress.files.load(Ordering::Relaxed),
                            bytes: progress.bytes.load(Ordering::Relaxed),
                            errors: progress.errors.load(Ordering::Relaxed),
                            elapsed: started.elapsed(),
                            cancelled: true,
                        });
                        just_finished = true;
                    }
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
                    // Not for a cancelled scan, nor for a root that couldn't be
                    // read at all (nothing found + errors): that "cache" would
                    // only hide the real, unreadable folder behind an empty city.
                    let unreadable = st.dirs == 0 && st.files == 0 && st.errors > 0;
                    if !st.cancelled && !unreadable {
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
            self.pane.refresh_status_summary();
        }

        // Poll a background paste (E5) for completion.
        let mut paste_done: Option<PasteOutcome> = None;
        let mut paste_was_cut = false;
        if let Some(job) = &self.paste_job {
            match job.rx.try_recv() {
                Ok(r) => {
                    paste_done = Some(r);
                    paste_was_cut = job.cut;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => ctx.request_repaint(),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    paste_done = Some(PasteOutcome {
                        done: Vec::new(),
                        error: Some("Paste worker stopped unexpectedly.".into()),
                    });
                }
            }
        }
        if let Some(out) = paste_done {
            self.paste_job = None;
            // A cut consumes the clipboard once anything moved: on a partial
            // failure the remaining paths are stale and must not be re-pasted.
            // If NOTHING moved (refused outright), the cut is still valid.
            if paste_was_cut && !out.done.is_empty() {
                self.clipboard = None;
                sys_clipboard::clear(); // Explorer does the same after a cut is pasted
            }
            // Whatever completed is real (even on a partial failure) — that is
            // what Undo reverses.
            if !out.done.is_empty() {
                let entry = if paste_was_cut {
                    UndoEntry::new(UndoOp::MoveBack { pairs: out.done.clone() }, "move")
                } else {
                    let dsts = out.done.iter().map(|(_, d)| d.clone()).collect();
                    UndoEntry::new(UndoOp::Trash { paths: dsts }, "copy")
                };
                self.push_undo(entry);
            }
            self.op_error = out.error;
            self.pane.entries_dirty = true;
            self.sb_cache.clear();
            if let Some((_, dst)) = out.done.last() {
                self.after_op(dst); // selects it only if we're still in that folder
            }
        }

        // Poll a background delete (E5) for completion.
        let mut delete_done: Option<DeleteOutcome> = None;
        if let Some(rx) = &self.delete_job {
            match rx.try_recv() {
                Ok(r) => delete_done = Some(r),
                Err(std::sync::mpsc::TryRecvError::Empty) => ctx.request_repaint(),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    delete_done = Some(DeleteOutcome {
                        recycled: Vec::new(),
                        error: Some("Delete worker stopped unexpectedly.".into()),
                    });
                }
            }
        }
        if let Some(out) = delete_done {
            self.delete_job = None;
            // Either way, the folder may have changed on disk — refresh the view.
            self.pane.entries_dirty = true;
            self.sb_cache.clear();
            if !out.recycled.is_empty() {
                self.push_undo(UndoEntry::new(UndoOp::Restore { paths: out.recycled }, "delete"));
            }
            match out.error {
                None => {
                    self.pane.clear_selection(); // items are gone
                    self.op_error = None;
                    // If the folder we were IN is gone (tree-focused Delete), step
                    // out to its parent.
                    self.step_out_if_gone();
                }
                Some(e) => self.op_error = Some(e), // keep selection to retry
            }
        }

        // Poll a background undo for completion.
        let mut undo_done: Option<Result<UndoDone, String>> = None;
        if let Some((rx, _, _)) = &self.undo_job {
            match rx.try_recv() {
                Ok(r) => undo_done = Some(r),
                Err(std::sync::mpsc::TryRecvError::Empty) => ctx.request_repaint(),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    undo_done = Some(Err("Undo worker stopped unexpectedly.".into()));
                }
            }
        }
        if let Some(result) = undo_done {
            let (_, entry, was_redo) = self.undo_job.take().expect("undo job present");
            self.pane.entries_dirty = true;
            self.sb_cache.clear();
            match result {
                Ok(done) => {
                    self.op_error = None;
                    // The inverse goes on the opposite stack (NOT via push_undo,
                    // which would fork history and clear redo).
                    if let Some(inv) = done.inverse {
                        let e = UndoEntry::new(inv, entry.label);
                        if was_redo {
                            self.undo.push(e);
                        } else {
                            self.redo.push(e);
                        }
                    }
                    // Follow the folder we're IN if the reversal renamed it.
                    if let UndoOp::Rename { from, to } = &entry.op {
                        if let Some(p) = reroot(&self.pane.current_dir, from, to) {
                            self.pane.current_dir = p;
                            self.pane.sync_addr();
                            self.sb_reveal();
                        }
                    }
                    self.step_out_if_gone();
                    if let Some(p) = done.select {
                        self.after_op(&p);
                    }
                }
                Err(e) => self.op_error = Some(e),
            }
        }

        // ---- Top strip: identity + view mode + shared navigation -----------
        egui::Panel::top("mode").show(ui, |ui| {
            ui.horizontal(|ui| {
                // (No app-name label here: the window title already says SECTOR.)
                ui.selectable_value(&mut self.view, View::List, "Files");
                ui.selectable_value(&mut self.view, View::City, "City");
                ui.separator();
                self.nav_bar(ui); // back / forward / up + address (both views)
            });
        });

        // Address bar from the keyboard, in both views: Alt+D / Ctrl+L / F4.
        let kb_free = self.prompt.is_none()
            && self.confirm_delete.is_none()
            && !self.pane.addr_active
            && ctx.memory(|m| m.focused()).is_none()
            && !self.menu_open_at_start
            && !egui::Popup::is_any_open(&ctx);
        if kb_free
            && ctx.input(|i| {
                i.key_pressed(egui::Key::F4)
                    || (i.modifiers.alt && i.key_pressed(egui::Key::D))
                    || (i.modifiers.ctrl && i.key_pressed(egui::Key::L))
            })
        {
            self.pane.begin_addr_edit();
        }

        // Inbound drag-and-drop (from Explorer or any app): files dropped on the
        // window are COPIED into the folder you're looking at. Never a move —
        // the OS drag owns the keyboard, so Shift/Ctrl can't be read reliably,
        // and a copy is the safe default (and undoable). winit reports the
        // files but not the drop position, so the target is the current folder
        // rather than a row under the pointer.
        let dropped: Vec<PathBuf> =
            ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        if !dropped.is_empty() && self.prompt.is_none() && self.confirm_delete.is_none() {
            let dest = self.pane.current_dir.clone();
            self.start_transfer(dropped, dest, false);
        }
        self.drop_hover = ctx.input(|i| i.raw.hovered_files.len());

        // Ctrl+1 … Ctrl+9: jump to the nth Quick access folder (both views).
        if kb_free {
            const NUMS: [egui::Key; 9] = [
                egui::Key::Num1, egui::Key::Num2, egui::Key::Num3, egui::Key::Num4, egui::Key::Num5,
                egui::Key::Num6, egui::Key::Num7, egui::Key::Num8, egui::Key::Num9,
            ];
            let hit = ctx.input(|i| {
                (i.modifiers.ctrl && !i.modifiers.shift)
                    .then(|| NUMS.iter().position(|k| i.key_pressed(*k)))
                    .flatten()
            });
            if let Some(n) = hit {
                if let Some(p) = self.quick_access().get(n).cloned() {
                    self.navigate_to(p);
                }
            }
        }

        // Mouse side buttons (browser-style Back / Forward), in both views.
        if self.prompt.is_none() && self.confirm_delete.is_none() {
            let (back, fwd) = ctx.input(|i| {
                (
                    i.pointer.button_pressed(egui::PointerButton::Extra1),
                    i.pointer.button_pressed(egui::PointerButton::Extra2),
                )
            });
            if back {
                self.go_back();
            }
            if fwd {
                self.go_forward();
            }
        }

        if self.view == View::List {
            self.show_list(ui);
        }

        // ---- City view (the original visualizer) ---------------------------
        if self.view == View::City {
        // E2: keep the City pointed at the folder you're browsing. When the
        // location drifts (address bar, up/back, or a Files navigation), re-sync
        // — instant cache-load if available, else drop to a scan-prompt.
        if self.city_synced_dir.as_deref() != Some(self.pane.current_dir.as_path()) {
            // Still inside the loaded tree (the address bar, Up, Back, or the
            // Files tree moved us within it)? Re-root the City there — instant
            // — instead of reloading a separate cache for that folder.
            match self.city_node_for(&self.pane.current_dir, false) {
                Some(node) => {
                    self.root = node;
                    self.city_synced_dir = Some(self.pane.current_dir.clone());
                }
                None => self.sync_city(),
            }
        }
        // ---- Top bar --------------------------------------------------------
        egui::Panel::top("bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                let scanning = matches!(&self.scan, ScanState::Running { stats: None, .. })
                    || self.cache_load.is_some();
                ui.add_enabled_ui(!scanning, |ui| {
                    let scanned = self.city_root.as_deref() == Some(self.pane.current_dir.as_path())
                        && matches!(&self.scan, ScanState::Running { stats: Some(_), .. });
                    let scan_label = if scanned { "Rescan" } else { "Scan" };
                    if ui
                        .button(scan_label)
                        .on_hover_text("Deep-scan this folder into a cityscape (slow for a big tree; cached afterwards).")
                        .clicked()
                    {
                        self.start_scan();
                    }
                    // (The scan thread-count knob is hidden for now — a
                    // benchmarking control, not a daily one. `self.threads` is
                    // still persisted and used; default DEFAULT_THREADS.)

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

                    ui.separator();
                    ui.checkbox(&mut self.auto_scan_local, "Auto-scan local")
                        .on_hover_text("Entering a local folder that has no cityscape yet starts its scan (watch it build). Network drives and drive roots always wait for Scan.");

                    // Offer an instant load if a cache exists for this folder
                    // (age precomputed in sync_city — no per-frame stat).
                    if let Some(age) = self.cache_mtime {
                        if ui
                            .button(format!("Load cached · {}", humanize_age(age)))
                            .on_hover_text("Reopen the last scan of this path instantly, without walking the filesystem.")
                            .clicked()
                        {
                            self.start_cache_load();
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
                if self.camera != Camera::default()
                    && ui.button("Reset view").on_hover_text("Back to the default angle, zoom and position").clicked()
                {
                    self.camera = Camera::default();
                }
                if let ScanState::Running { cancel, stats: None, .. } = &self.scan {
                    if ui.button("Cancel").clicked() {
                        cancel.store(true, Ordering::Relaxed);
                    }
                }
            });

            match &self.scan {
                ScanState::Idle => {
                    if self.cache_load.is_some() {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(format!("Loading the cached scan of {}…", self.pane.current_dir.display()));
                        });
                    } else if let Some(note) = &self.city_note {
                        ui.colored_label(egui::Color32::from_rgb(230, 170, 80), note);
                    } else {
                        ui.label(format!(
                            "No cityscape for {} yet — press Scan to build one.",
                            self.pane.current_dir.display()
                        ));
                    }
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
                        // (No City-specific breadcrumb: the shared address bar
                        // navigates AND re-roots within the loaded tree.)
                        let t = tree.lock().unwrap_or_else(|e| e.into_inner());
                        ui.horizontal_wrapped(|ui| {
                            ui.weak(format!("Scan of {}:", t.node(Tree::ROOT).name));
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
                ui.weak("area = size · height = file count · hover for details · click to drill in · drag to orbit · Shift+drag to pan · wheel to zoom");
            });
        });

        // ---- Central: the treemap -------------------------------------------
        egui::CentralPanel::default().show(ui, |ui| {
            let ScanState::Running { tree, stats, .. } = &self.scan else {
                ui.centered_and_justified(|ui| {
                    ui.weak(if self.cache_load.is_some() { "Loading cached scan…" } else { "No scan yet." });
                });
                return;
            };
            let scanning = stats.is_none();

            // Explicit STABLE widget id (not allocate_painter's auto id), so the
            // right-click context menu — which egui keys on this id — stays open
            // across frames instead of flashing shut.
            let area = ui.available_rect_before_wrap();
            let response = ui.interact(area, egui::Id::new("sector_canvas"), Sense::click_and_drag());
            let painter = ui.painter_at(area);

            // Camera: drag orbits (turntable) and tilts; Shift+drag or a
            // middle-button drag pans; the wheel zooms (Ctrl+wheel stays the UI
            // zoom). A click (no drag) still drills in.
            let mid = response.dragged_by(egui::PointerButton::Middle);
            if response.dragged_by(egui::PointerButton::Primary) || mid {
                let d = response.drag_delta();
                if mid || ctx.input(|i| i.modifiers.shift) {
                    self.camera.pan += d;
                } else {
                    self.camera.yaw += d.x as f64 * 0.008;
                    self.camera.tilt = (self.camera.tilt - d.y as f64 * 0.0015).clamp(0.08, 0.6);
                }
            }
            if response.hovered() {
                let (scroll, ctrl) = ctx.input(|i| (i.smooth_scroll_delta.y, i.modifiers.ctrl));
                if scroll != 0.0 && !ctrl {
                    self.camera.zoom = (self.camera.zoom * (1.0 + scroll as f64 * 0.0015)).clamp(0.3, 8.0);
                }
            }

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
                    self.blocks = build_blocks(&t, &tiles, self.dominant.as_deref(), 1.0, Some(&counts));
                    self.scape = project_scene(&self.blocks, &self.camera, area);
                    self.last_camera = self.camera;
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
                    self.blocks = build_blocks(&t, &self.tiles, self.dominant.as_deref(), rev, None);
                    self.scape = project_scene(&self.blocks, &self.camera, area);
                    self.last_camera = self.camera;
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
            // The camera moved but the blocks didn't: re-project only (cheap).
            if self.camera != self.last_camera {
                self.scape = project_scene(&self.blocks, &self.camera, area);
                self.last_camera = self.camera;
            }

            // Night sky + ground plinth (behind the city).
            painter.rect_filled(area, 0.0_f32, BG);
            let edge = Stroke::new(0.5, BORDER);
            for (q, c) in &self.scape.plinth_sides {
                painter.add(Shape::convex_polygon(q.to_vec(), *c, edge));
            }
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
                for (q, f) in &b.sides {
                    painter.add(Shape::convex_polygon(q.to_vec(), shade(b.color, *f), edge));
                }
                painter.add(Shape::convex_polygon(b.top.to_vec(), shade(b.color, F_TOP), edge));
                if Some(i) == hovered {
                    // Halo outline: a dark stroke under a bright one, so the
                    // highlight is visible on ANY block color (incl. orange-on-
                    // orange video, or the light Document tiles).
                    let dark = Stroke::new(3.2, Color32::from_black_alpha(190));
                    let bright = Stroke::new(1.6, Color32::WHITE);
                    let faces: Vec<&[Pos2; 4]> =
                        b.sides.iter().map(|(q, _)| q).chain(std::iter::once(&b.top)).collect();
                    for f in &faces {
                        painter.add(Shape::convex_polygon(f.to_vec(), Color32::TRANSPARENT, dark));
                    }
                    for f in &faces {
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
            // Case-insensitive: the tree has the real casing, `current_dir` has
            // whatever was typed — a case-only difference is the same folder,
            // and must not push a phantom history entry.
            let same = t.to_string_lossy().to_lowercase() == self.pane.current_dir.to_string_lossy().to_lowercase();
            if !same {
                self.navigate_to(t);
            }
            self.city_synced_dir = Some(self.pane.current_dir.clone());
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
                    ui.strong("Delete from network drive?");
                    ui.add_space(6.0);
                    ui.colored_label(
                        egui::Color32::from_rgb(230, 170, 80),
                        format!(
                            "⚠ {what} won't go to the Windows Recycle Bin. If your NAS keeps its \
                             own (e.g. Synology's #recycle), it may be recoverable there — \
                             otherwise this is permanent."
                        ),
                    );
                } else {
                    ui.strong("Move to Recycle Bin?");
                    ui.add_space(6.0);
                    ui.label(format!("{what} will be moved to the Recycle Bin."));
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    let label = if permanent { "Delete" } else { "Move to Recycle Bin" };
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
                PromptKind::RenameDir { .. } => "Rename folder",
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
                        PromptKind::NewFolder | PromptKind::RenameDir { .. } => {
                            prompt.buf.chars().count()
                        }
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

        // Internal drag in progress: a badge follows the pointer saying what a
        // drop will do (Ctrl switches move → copy), with a matching cursor.
        if let Some(p) = egui::DragAndDrop::payload::<DragFiles>(&ctx) {
            let (down, released) = ctx.input(|i| (i.pointer.any_down(), i.pointer.any_released()));
            if !down && !released {
                // Released last frame onto nothing that took it: drop the payload
                // so the badge can't linger.
                egui::DragAndDrop::clear_payload(&ctx);
            } else if let Some(pos) = ctx.pointer_latest_pos() {
                let copy = ctx.input(|i| i.modifiers.ctrl);
                let n = p.paths.len();
                let what = if n == 1 { name_of(&p.paths[0]) } else { format!("{n} items") };
                let text = format!("{} {what}", if copy { "Copy" } else { "Move" });
                let painter = ctx.layer_painter(egui::LayerId::new(
                    egui::Order::Tooltip,
                    egui::Id::new("sector_drag_badge"),
                ));
                let galley = painter.layout_no_wrap(text, egui::FontId::proportional(13.0), Color32::WHITE);
                let pad = Vec2::new(8.0, 5.0);
                let bg = Rect::from_min_size(pos + Vec2::new(16.0, 16.0), galley.size() + pad * 2.0);
                painter.rect_filled(bg, 5.0_f32, Color32::from_rgba_unmultiplied(12, 16, 28, 235));
                painter.galley(bg.min + pad, galley, Color32::WHITE);
                ctx.set_cursor_icon(if copy { egui::CursorIcon::Copy } else { egui::CursorIcon::Grabbing });
                ctx.request_repaint();
            }
        }

        // Drop overlay: while files hover over the window, tint it and say what
        // a drop will do. Foreground layer, so it sits over every panel.
        if self.drop_hover > 0 {
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("sector_drop_overlay"),
            ));
            let r = ctx.content_rect().shrink(6.0);
            painter.rect_filled(r, 8.0_f32, Color32::from_rgba_unmultiplied(40, 90, 160, 70));
            painter.rect_stroke(
                r,
                8.0_f32,
                Stroke::new(2.0, Color32::from_rgb(120, 180, 255)),
                egui::StrokeKind::Inside,
            );
            let n = self.drop_hover;
            let what = if n == 1 { "1 item".to_string() } else { format!("{n} items") };
            let text = format!("Drop to copy {what} into {}", self.pane.current_dir.display());
            let galley = painter.layout_no_wrap(text, egui::FontId::proportional(20.0), Color32::WHITE);
            let pad = Vec2::new(18.0, 12.0);
            let bg = Rect::from_center_size(r.center(), galley.size() + pad * 2.0);
            painter.rect_filled(bg, 8.0_f32, Color32::from_rgba_unmultiplied(12, 16, 28, 230));
            painter.galley(bg.min + pad, galley, Color32::WHITE);
            ctx.request_repaint();
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
                        // Write to a sibling temp file, then rename over the
                        // final name: a crash/exit mid-write leaves a stray
                        // .tmp, never a truncated .bin.
                        let tmp = cp.with_extension("tmp");
                        let written = std::fs::write(&tmp, &bytes)
                            .and_then(|()| std::fs::rename(&tmp, &cp));
                        match written {
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
        let out = run_paste(vec![d.join("src")], dest.clone(), false, cancel.clone());
        assert!(out.error.is_none());
        assert_eq!(out.last_name().as_deref(), Some("src"));
        assert!(d.join("src/f.txt").exists()); // source preserved (copy)
        assert_eq!(std::fs::read(dest.join("src/f.txt")).unwrap(), b"hello");
        assert!(dest.join("src/sub/g.bin").exists());

        // Paste again → must NOT overwrite; lands as "src - Copy".
        let out2 = run_paste(vec![d.join("src")], dest.clone(), false, cancel);
        assert_eq!(out2.last_name().as_deref(), Some("src - Copy"));
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
        assert!(run_paste(vec![d.join("src")], dest.clone(), false, cancel.clone()).error.is_none());
        assert!(dest.join("src/real.txt").exists());
        assert!(!dest.join("src/link").exists()); // link skipped
        assert!(!dest.join("src/link/secret.txt").exists()); // target NOT copied

        // A symlink AS the source is refused outright.
        let err = run_paste(vec![d.join("src/link")], dest, false, cancel).error.unwrap();
        assert!(err.contains("link/junction"));
        std::fs::remove_dir_all(&d).ok();
    }

    #[cfg(unix)]
    #[test]
    fn unique_dest_treats_dangling_symlink_as_taken() {
        use std::os::unix::fs::symlink;
        let d = scratch("dangling");
        // A dangling link named "a.txt": `exists()` says false, but pasting onto
        // it would write through to /nonexistent/target — it must count as taken.
        symlink("/nonexistent/sector-target", d.join("a.txt")).unwrap();
        assert!(!d.join("a.txt").exists()); // proves the trap is real
        assert_eq!(unique_dest(&d, "a.txt"), d.join("a - Copy.txt"));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn paste_cut_moves_and_removes_source() {
        let d = scratch("cut");
        std::fs::write(d.join("m.txt"), b"data").unwrap();
        let dest = d.join("dest");
        std::fs::create_dir(&dest).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let out = run_paste(vec![d.join("m.txt")], dest.clone(), true, cancel);
        assert_eq!(out.last_name().as_deref(), Some("m.txt"));
        assert!(!d.join("m.txt").exists()); // source gone (moved)
        assert_eq!(std::fs::read(dest.join("m.txt")).unwrap(), b"data");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn paste_reports_completed_items_on_partial_failure() {
        let d = scratch("partial");
        std::fs::write(d.join("ok.txt"), b"1").unwrap();
        let dest = d.join("dest");
        std::fs::create_dir(&dest).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        // Second source doesn't exist → the paste fails there, but the first
        // item really was copied and must be reported (Undo reverses it).
        let out = run_paste(vec![d.join("ok.txt"), d.join("missing.txt")], dest.clone(), false, cancel);
        assert!(out.error.is_some());
        assert_eq!(out.done.len(), 1);
        assert_eq!(out.done[0].1, dest.join("ok.txt"));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn undo_move_restores_original_name() {
        let d = scratch("undo-move");
        std::fs::write(d.join("m.txt"), b"mine").unwrap();
        let dest = d.join("dest");
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(dest.join("m.txt"), b"theirs").unwrap(); // name taken at the destination
        let cancel = Arc::new(AtomicBool::new(false));
        let out = run_paste(vec![d.join("m.txt")], dest.clone(), true, cancel);
        assert_eq!(out.done[0].1, dest.join("m - Copy.txt")); // auto-renamed on paste

        // Undo puts it back under its ORIGINAL name, leaving "theirs" untouched.
        let done = run_undo(UndoOp::MoveBack { pairs: out.done }).unwrap();
        assert_eq!(done.select, Some(d.join("m.txt")));
        assert_eq!(std::fs::read(d.join("m.txt")).unwrap(), b"mine");
        assert_eq!(std::fs::read(dest.join("m.txt")).unwrap(), b"theirs");
        assert!(!dest.join("m - Copy.txt").exists());

        // Redo (the inverse) moves it into dest again — back to "m - Copy.txt",
        // since "m.txt" there is still taken — and yields the undo for THAT.
        let inv = done.inverse.unwrap();
        assert_eq!(inv, UndoOp::MoveBack { pairs: vec![(dest.join("m - Copy.txt"), d.join("m.txt"))] });
        let redone = run_undo(inv).unwrap();
        assert_eq!(std::fs::read(dest.join("m - Copy.txt")).unwrap(), b"mine");
        assert!(!d.join("m.txt").exists());
        assert_eq!(redone.inverse, Some(UndoOp::MoveBack { pairs: vec![(d.join("m.txt"), dest.join("m - Copy.txt"))] }));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn undo_rename_and_new_folder() {
        let d = scratch("undo-misc");
        // Rename a → b, undo → a again; and refuse to undo over a newcomer.
        std::fs::write(d.join("a.txt"), b"x").unwrap();
        std::fs::rename(d.join("a.txt"), d.join("b.txt")).unwrap();
        let op = UndoOp::Rename { from: d.join("b.txt"), to: d.join("a.txt") };
        let done = run_undo(op.clone()).unwrap();
        assert_eq!(done.select, Some(d.join("a.txt")));
        assert!(d.join("a.txt").exists() && !d.join("b.txt").exists());
        // Redo is the mirror image, and its own inverse is the original undo.
        let inv = done.inverse.unwrap();
        assert_eq!(inv, UndoOp::Rename { from: d.join("a.txt"), to: d.join("b.txt") });
        assert_eq!(run_undo(inv).unwrap().inverse, Some(op.clone()));
        assert!(d.join("b.txt").exists() && !d.join("a.txt").exists());
        std::fs::write(d.join("a.txt"), b"newcomer").unwrap();
        assert!(run_undo(op).unwrap_err().contains("something is now named"));
        assert_eq!(std::fs::read(d.join("a.txt")).unwrap(), b"newcomer"); // untouched

        // New folder: undo removes it while empty, refuses once it has content;
        // redo re-creates it.
        std::fs::create_dir(d.join("nf")).unwrap();
        let done = run_undo(UndoOp::RemoveDir { path: d.join("nf") }).unwrap();
        assert!(!d.join("nf").exists());
        assert_eq!(done.inverse, Some(UndoOp::MakeDir { path: d.join("nf") }));
        let redone = run_undo(done.inverse.unwrap()).unwrap();
        assert!(d.join("nf").is_dir());
        assert_eq!(redone.inverse, Some(UndoOp::RemoveDir { path: d.join("nf") }));
        std::fs::create_dir(d.join("nf2")).unwrap();
        std::fs::write(d.join("nf2/keep.txt"), b"!").unwrap();
        assert!(run_undo(UndoOp::RemoveDir { path: d.join("nf2") }).is_err());
        assert!(d.join("nf2/keep.txt").exists());
        std::fs::remove_dir_all(&d).ok();
    }

    /// Round-trip through the real trash (freedesktop here, Recycle Bin on
    /// Windows). Skips itself if this environment has no usable trash.
    #[test]
    fn undo_delete_restores_from_trash() {
        let d = scratch("undo-trash");
        let f = d.join("gone.txt");
        std::fs::write(&f, b"bring me back").unwrap();
        if let Err(e) = trash::delete(&f) {
            eprintln!("skipping: no usable trash here ({e})");
            std::fs::remove_dir_all(&d).ok();
            return;
        }
        assert!(!f.exists());
        let done = run_undo(UndoOp::Restore { paths: vec![f.clone()] }).unwrap();
        assert_eq!(done.select, Some(f.clone()));
        assert_eq!(std::fs::read(&f).unwrap(), b"bring me back");
        // A second undo of the same delete finds nothing to restore.
        assert!(run_undo(UndoOp::Restore { paths: vec![f.clone()] }).unwrap_err().contains("isn't in"));
        // Redo (the inverse) trashes it again, and THAT yields a restore.
        let inv = done.inverse.unwrap();
        assert_eq!(inv, UndoOp::Trash { paths: vec![f.clone()] });
        let redone = run_undo(inv).unwrap();
        assert!(!f.exists());
        assert_eq!(redone.inverse, Some(UndoOp::Restore { paths: vec![f.clone()] }));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn hdrop_round_trip() {
        use super::sys_clipboard::{hdrop_decode, hdrop_encode};
        let paths = vec![
            PathBuf::from(r"C:\Users\me\photo élan.jpg"), // non-ASCII survives UTF-16
            PathBuf::from(r"\\nas\Media\Manga"),
        ];
        let bytes = hdrop_encode(&paths);
        // DROPFILES header: pFiles = 20, fWide = 1.
        assert_eq!(&bytes[0..4], &20u32.to_le_bytes());
        assert_eq!(&bytes[16..20], &1i32.to_le_bytes());
        assert!(bytes.ends_with(&[0, 0, 0, 0])); // "…\0" then the final "\0"
        assert_eq!(hdrop_decode(&bytes), paths);

        // Empty list still has a valid header + terminator, and decodes to nothing.
        assert!(hdrop_decode(&hdrop_encode(&[])).is_empty());
        // Garbage is rejected, not panicked on.
        assert!(hdrop_decode(&[1, 2, 3]).is_empty());
        assert!(hdrop_decode(&[200, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0]).is_empty());
    }

    #[test]
    fn hdrop_decodes_ansi_lists() {
        use super::sys_clipboard::hdrop_decode;
        // An ANSI (fWide = 0) payload from an old app: "C:\a\0D:\b\0\0".
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&20u32.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 12]); // pt, fNC
        bytes.extend_from_slice(&0i32.to_le_bytes()); // fWide = 0
        bytes.extend_from_slice(b"C:\\a\0D:\\b\0\0");
        assert_eq!(hdrop_decode(&bytes), vec![PathBuf::from(r"C:\a"), PathBuf::from(r"D:\b")]);
    }

    /// A minimal listing entry for app-state tests.
    fn entry(name: &str) -> Entry {
        Entry {
            name: name.into(),
            is_dir: false,
            size: 0,
            modified: None,
            created: None,
            is_symlink: false,
            is_hidden: false,
            readonly: false,
        }
    }

    #[test]
    fn history_restores_the_item_you_were_on() {
        let mut app = SectorApp::default();
        app.pane.current_dir = PathBuf::from("/a");
        app.pane.entries = vec![entry("one"), entry("two")];
        app.pane.lead = Some(1);

        app.navigate_to(PathBuf::from("/b"));
        assert_eq!(app.pane.back_stack, vec![(PathBuf::from("/a"), Some("two".to_string()))]);
        assert!(app.pane.fwd_stack.is_empty());

        // In /b the cursor is on nothing; Back must return to /a AND re-select "two".
        app.pane.entries.clear();
        app.pane.lead = None;
        app.go_back();
        assert_eq!(app.pane.current_dir, PathBuf::from("/a"));
        assert_eq!(app.pane.select_after_reload, Some((PathBuf::from("/a"), "two".to_string())));
        assert_eq!(app.pane.fwd_stack, vec![(PathBuf::from("/b"), None)]);

        app.go_forward();
        assert_eq!(app.pane.current_dir, PathBuf::from("/b"));
        assert_eq!(app.pane.back_stack, vec![(PathBuf::from("/a"), None)]); // cursor wasn't restored yet (no reload) — recorded as-is
    }

    #[test]
    fn drag_payload_refuses_self_and_same_folder_drops() {
        let d = DragFiles { paths: vec![PathBuf::from("/a/x"), PathBuf::from("/a/y.txt")] };
        assert!(d.can_drop_into(Path::new("/b"))); // elsewhere
        assert!(d.can_drop_into(Path::new("/a/z"))); // a sibling folder
        assert!(!d.can_drop_into(Path::new("/a"))); // where they already are — a no-op
        assert!(!d.can_drop_into(Path::new("/a/x"))); // onto one of themselves
        assert!(!d.can_drop_into(Path::new("/a/x/deep"))); // inside one of themselves
        // Items from different folders: /a is no longer "where they all are".
        let d2 = DragFiles { paths: vec![PathBuf::from("/a/x"), PathBuf::from("/c/y")] };
        assert!(d2.can_drop_into(Path::new("/a")));
    }

    #[test]
    fn reroot_follows_renames() {
        let (from, to) = (Path::new("/x/old"), Path::new("/x/new"));
        assert_eq!(reroot(Path::new("/x/old"), from, to), Some(PathBuf::from("/x/new"))); // the folder itself
        assert_eq!(reroot(Path::new("/x/old/a/b"), from, to), Some(PathBuf::from("/x/new/a/b"))); // inside it
        assert_eq!(reroot(Path::new("/x/older"), from, to), None); // a sibling with a common prefix is NOT inside
        assert_eq!(reroot(Path::new("/y"), from, to), None);
    }

    #[test]
    fn drive_letter_is_canonicalised_to_upper_case() {
        assert_eq!(canonical_drive_case(PathBuf::from(r"y:\Media")), PathBuf::from(r"Y:\Media"));
        assert_eq!(canonical_drive_case(PathBuf::from(r"y:\")), PathBuf::from(r"Y:\"));
        assert_eq!(canonical_drive_case(PathBuf::from(r"Y:\Media")), PathBuf::from(r"Y:\Media")); // untouched
        assert_eq!(canonical_drive_case(PathBuf::from(r"\\nas\media")), PathBuf::from(r"\\nas\media")); // UNC untouched
        assert_eq!(canonical_drive_case(PathBuf::from("/tmp/x")), PathBuf::from("/tmp/x"));
        // Through navigation: the location and history entries come out canonical.
        let mut app = SectorApp::default();
        app.navigate_to(PathBuf::from(r"y:\Media"));
        assert_eq!(app.pane.current_dir, PathBuf::from(r"Y:\Media"));
    }

    #[test]
    fn sort_key_round_trips_through_its_name() {
        for k in [SortKey::Name, SortKey::Size, SortKey::Kind, SortKey::Modified] {
            assert!(SortKey::from_name(k.name()) == k);
        }
        assert!(SortKey::from_name("") == SortKey::Name); // old settings files
    }

    #[test]
    fn age_labels_change_unit_with_scale() {
        const D: u64 = 86_400;
        assert_eq!(age_label(30), "just now");
        assert_eq!(age_label(5 * 60), "5m ago");
        assert_eq!(age_label(3 * 3600), "3h ago");
        assert_eq!(age_label(12 * D), "12d ago");
        assert_eq!(age_label(45 * D), "1mo ago");
        assert_eq!(age_label(200 * D), "6mo ago");
        assert_eq!(age_label(365 * D), "1y ago");
        assert_eq!(age_label(496 * D), "1y 4mo ago"); // the screenshot's case
        assert_eq!(age_label(1000 * D), "2y 9mo ago");
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
    fn cache_key_ignores_trailing_separators_and_case() {
        assert_eq!(normalize_cache_key("Y:"), "y:\\");
        assert_eq!(normalize_cache_key("Y:\\"), "y:\\");
        assert_eq!(normalize_cache_key("Y:\\Media"), "y:\\media");
        assert_eq!(normalize_cache_key("Y:\\Media\\"), "y:\\media"); // trailing sep
        assert_eq!(normalize_cache_key("y:/media/"), "y:\\media"); // forward slashes
        assert_eq!(normalize_cache_key("\\\\nas\\Media\\"), "\\\\nas\\media"); // UNC
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
