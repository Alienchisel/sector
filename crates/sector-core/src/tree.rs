//! The scanned filesystem as a compact, index-based arena tree.
//!
//! Design goals (see docs/DECISIONS.md):
//! - **Index-based arena** (`Vec<Node>` + `NodeId` indices), not `Rc`-linked
//!   nodes — cache-friendly and cheap to traverse/aggregate over millions of
//!   entries (D5).
//! - **Real identity retained:** each node stores its own path *component*, and
//!   the full path is reconstructed by walking parents. This keeps per-node
//!   memory small (no full path string per node) while preserving enough to add
//!   file actions later (D8) — we never collapse to pure size-geometry.
//! - **OS-agnostic:** no path separators baked in. Callers join components with
//!   whatever separator the platform uses. This is why the type lives in
//!   `sector-core` and is testable on Linux (D6).
//! - **Streaming-friendly:** nodes are appended as the scanner discovers them,
//!   which feeds the live "discovery" visualization (D12). Size aggregation is a
//!   separate pass so it can be re-run as the scan streams in.
//!
//! Invariant: a node's parent is always added *before* the node itself, so a
//! parent's arena index is always lower than any of its children's. Size
//! aggregation relies on this.

use crate::filetype::{categorize, FileCategory};
use serde::{Deserialize, Serialize};

/// Summary of a scan, stored alongside a cached tree.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CacheStats {
    pub dirs: u64,
    pub files: u64,
    pub bytes: u64,
    /// When the scan was saved (Unix seconds).
    pub saved_unix: u64,
    /// NTFS USN journal watermark at scan time (E6 freshness). `0` = none (not a
    /// local NTFS volume, or the journal was unavailable).
    pub usn_journal_id: u64,
    pub usn_next: i64,
}

/// Cache file framing: a 4-byte magic + little-endian u16 version prefix the
/// postcard body. `postcard` is not self-describing, so without this an old or
/// foreign file could deserialize into a nonsense tree instead of failing; the
/// header lets [`Tree::from_cache_bytes`] reject anything it doesn't recognize.
/// Bump `CACHE_VERSION` whenever the body layout changes.
const CACHE_MAGIC: [u8; 4] = *b"SECT";
const CACHE_VERSION: u16 = 2; // v2 added the USN watermark to CacheStats

/// On-disk cache format (v1). Struct-of-arrays of only the essential per-node
/// fields; `children`, `subtree_size`, and `file_count` are rebuilt on load.
#[derive(Serialize, Deserialize)]
struct TreeCacheV1 {
    root_name: String,
    stats: CacheStats,
    name: Vec<String>,
    parent: Vec<u32>,
    kind: Vec<u8>, // 0 = dir, 1 = file
    own_size: Vec<u64>,
}

/// A handle into the [`Tree`]'s arena. Cheap to copy (4 bytes → up to ~4.29B
/// nodes, comfortably more than any real volume's file count).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(u32);

impl NodeId {
    /// This id's position in the arena. Stable for the life of the tree; usable
    /// to index parallel per-node arrays (e.g. from [`Tree::dominant_categories`]).
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// Whether a node is a directory or a file. (Symlinks/reparse points are
/// classified by the scanner; the tree itself only needs this coarse split.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Dir,
    File,
}

/// A single filesystem entry in the arena.
#[derive(Debug, Clone)]
pub struct Node {
    /// This entry's own path component (e.g. `"Users"`, `"photo.jpg"`, or a root
    /// like `"C:"` / `"\\\\nas\\Media"`). Not the full path — see [`Tree::path`].
    pub name: Box<str>,
    /// Parent in the arena. The root is its own parent (see [`Tree::ROOT`]).
    pub parent: NodeId,
    pub kind: NodeKind,
    /// The entry's own size in bytes. Files carry their apparent size (D10);
    /// directories are 0 here (their weight lives in [`Node::subtree_size`]).
    pub own_size: u64,
    /// Total apparent size of this entry plus all descendants. Valid only after
    /// [`Tree::recompute_sizes`]; before that it equals `own_size`.
    pub subtree_size: u64,
    /// Number of **file** descendants (this node counts as 1 if it is a file).
    /// Maintained the same way as `subtree_size` — used to give cityscape blocks
    /// a height that means "how many files", distinct from area (= bytes).
    pub file_count: u64,
    /// Child arena indices, in insertion order.
    pub children: Vec<NodeId>,
}

/// A scanned tree rooted at a single drive/share.
#[derive(Debug, Clone)]
pub struct Tree {
    nodes: Vec<Node>,
}

impl Tree {
    /// The root node's id. Always valid for a constructed `Tree`.
    pub const ROOT: NodeId = NodeId(0);

    /// Create a tree with a single root node (e.g. a drive `"C:"` or a share).
    pub fn new(root_name: impl Into<Box<str>>) -> Self {
        let root = Node {
            name: root_name.into(),
            parent: Self::ROOT, // root is its own parent
            kind: NodeKind::Dir,
            own_size: 0,
            subtree_size: 0,
            file_count: 0,
            children: Vec::new(),
        };
        Tree { nodes: vec![root] }
    }

    /// Append a child under `parent` and return its id.
    ///
    /// Panics if `parent` is not a valid id for this tree (a programming error
    /// in the scanner, not a filesystem condition).
    pub fn add_child(
        &mut self,
        parent: NodeId,
        name: impl Into<Box<str>>,
        kind: NodeKind,
        own_size: u64,
    ) -> NodeId {
        assert!(parent.index() < self.nodes.len(), "parent id out of range");
        let id = NodeId(u32::try_from(self.nodes.len()).expect("node count exceeds u32"));
        self.nodes.push(Node {
            name: name.into(),
            parent,
            kind,
            own_size,
            subtree_size: own_size,
            file_count: if matches!(kind, NodeKind::File) { 1 } else { 0 },
            children: Vec::new(),
        });
        self.nodes[parent.index()].children.push(id);
        id
    }

    /// Like [`Self::add_child`], but also folds `own_size` up the ancestor chain
    /// so every `subtree_size` stays correct *incrementally* — no
    /// [`Self::recompute_sizes`] pass needed afterwards.
    ///
    /// This is what the live "discovery" scan uses (D12): the tree is rendered
    /// while it grows, so sizes must be current at all times. Cost is O(depth)
    /// per insert (a walk to the root), negligible against the I/O of scanning.
    pub fn add_child_propagating(
        &mut self,
        parent: NodeId,
        name: impl Into<Box<str>>,
        kind: NodeKind,
        own_size: u64,
    ) -> NodeId {
        let id = self.add_child(parent, name, kind, own_size);
        let file_delta = if matches!(kind, NodeKind::File) { 1 } else { 0 };
        if own_size > 0 || file_delta > 0 {
            // Fold this node's size and file-ness into every ancestor from
            // `parent` up to (and including) the root.
            let mut cur = parent;
            loop {
                let n = &mut self.nodes[cur.index()];
                n.subtree_size += own_size;
                n.file_count += file_delta;
                let p = n.parent;
                if p == cur {
                    break;
                }
                cur = p;
            }
        }
        id
    }

    /// Number of nodes, including the root.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Always false — a `Tree` always has at least its root.
    pub fn is_empty(&self) -> bool {
        false
    }

    #[inline]
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id.index()]
    }

    #[inline]
    pub fn children(&self, id: NodeId) -> &[NodeId] {
        &self.nodes[id.index()].children
    }

    /// Iterate over every node id in arena (insertion) order.
    pub fn ids(&self) -> impl Iterator<Item = NodeId> {
        (0..self.nodes.len() as u32).map(NodeId)
    }

    /// Walk from `start` following `components` by name — case-insensitive in
    /// *Unicode* (NTFS treats "Élan" and "élan" as the same name; an ASCII-only
    /// compare would miss that) — returning the node reached or `None` if the
    /// path leaves the tree. Empty `components` returns `start`. Lets a caller
    /// locate the node for a folder path within a scanned subtree (e.g. to read
    /// its size).
    pub fn find_descendant(&self, start: NodeId, components: &[&str]) -> Option<NodeId> {
        let mut cur = start;
        for comp in components {
            // Lowercase the wanted name once; compare children char-by-char
            // without allocating (a folder can have 100k children).
            let want = comp.to_lowercase();
            let next = self.children(cur).iter().copied().find(|&c| {
                self.node(c).name.chars().flat_map(char::to_lowercase).eq(want.chars())
            })?;
            cur = next;
        }
        Some(cur)
    }

    /// Reconstruct the path to `id` as a vector of components, root first.
    ///
    /// OS-agnostic: the caller joins these with the platform separator (`\\` on
    /// Windows). E.g. `["C:", "Users", "photo.jpg"]`.
    pub fn path_components(&self, id: NodeId) -> Vec<&str> {
        let mut out = Vec::new();
        let mut cur = id;
        loop {
            out.push(&*self.nodes[cur.index()].name);
            let parent = self.nodes[cur.index()].parent;
            if parent == cur {
                break; // reached the root (its own parent)
            }
            cur = parent;
        }
        out.reverse();
        out
    }

    /// For every node, the [`FileCategory`] accounting for the most **bytes** in
    /// its subtree (a leaf file's own category; a directory's dominant content).
    /// Directories with no sized files are [`FileCategory::Other`]. The result is
    /// indexed by [`NodeId::index`].
    ///
    /// One O(n) bottom-up pass (same arena invariant as [`Self::recompute_sizes`]).
    /// Uses a transient per-node category-sum table, so call it on a *complete*
    /// tree (e.g. when a scan finishes) rather than every frame. Lets the UI
    /// color folders by what they mostly contain.
    pub fn dominant_categories(&self) -> Vec<FileCategory> {
        let n = self.nodes.len();
        // Transient: bytes-per-category for each node's subtree.
        let mut sums = vec![[0u64; FileCategory::COUNT]; n];

        // Seed each file into its own category.
        for (i, node) in self.nodes.iter().enumerate() {
            if node.kind == NodeKind::File && node.own_size > 0 {
                sums[i][categorize(&node.name) as usize] += node.own_size;
            }
        }
        // Fold children into parents (child index > parent index).
        for i in (1..n).rev() {
            let child = sums[i];
            let parent = self.nodes[i].parent.index();
            for c in 0..FileCategory::COUNT {
                sums[parent][c] += child[c];
            }
        }
        sums.iter().map(|s| dominant_of(s)).collect()
    }

    /// For a scan "replay": the subtree sizes and file counts the tree WOULD have
    /// if only the first `cutoff` nodes (in discovery / arena order) had been
    /// found. Both vectors are indexed by [`NodeId::index`]; nodes at index
    /// `>= cutoff` are 0. O(cutoff).
    pub fn partial_metrics(&self, cutoff: usize) -> (Vec<u64>, Vec<u64>) {
        let n = self.nodes.len();
        let k = cutoff.min(n);
        let mut size = vec![0u64; n];
        let mut count = vec![0u64; n];
        for i in 0..k {
            size[i] = self.nodes[i].own_size;
            count[i] = if matches!(self.nodes[i].kind, NodeKind::File) { 1 } else { 0 };
        }
        // Fold each discovered node into its parent (parent index < i < k).
        for i in (1..k).rev() {
            let (s, c) = (size[i], count[i]);
            let p = self.nodes[i].parent.index();
            size[p] += s;
            count[p] += c;
        }
        (size, count)
    }

    /// Serialize this tree (plus a stats summary) into the compact cache format.
    pub fn to_cache_bytes(&self, stats: CacheStats) -> Result<Vec<u8>, postcard::Error> {
        let n = self.nodes.len();
        let mut c = TreeCacheV1 {
            root_name: self.nodes[0].name.to_string(),
            stats,
            name: Vec::with_capacity(n),
            parent: Vec::with_capacity(n),
            kind: Vec::with_capacity(n),
            own_size: Vec::with_capacity(n),
        };
        for node in &self.nodes {
            c.name.push(node.name.to_string());
            c.parent.push(node.parent.0);
            c.kind.push(if matches!(node.kind, NodeKind::File) { 1 } else { 0 });
            c.own_size.push(node.own_size);
        }
        let body = postcard::to_stdvec(&c)?;
        let mut out = Vec::with_capacity(CACHE_MAGIC.len() + 2 + body.len());
        out.extend_from_slice(&CACHE_MAGIC);
        out.extend_from_slice(&CACHE_VERSION.to_le_bytes());
        out.extend_from_slice(&body);
        Ok(out)
    }

    /// Rebuild a tree (and its stats) from cache bytes. Returns `None` if the
    /// bytes are malformed or from an incompatible version.
    pub fn from_cache_bytes(bytes: &[u8]) -> Option<(Tree, CacheStats)> {
        // Reject anything without our magic + matching version (old/foreign files).
        let header = CACHE_MAGIC.len() + 2;
        if bytes.len() < header
            || bytes[..CACHE_MAGIC.len()] != CACHE_MAGIC
            || u16::from_le_bytes([bytes[4], bytes[5]]) != CACHE_VERSION
        {
            return None;
        }
        let c: TreeCacheV1 = postcard::from_bytes(&bytes[header..]).ok()?;
        let n = c.name.len();
        if n == 0 || c.parent.len() != n || c.kind.len() != n || c.own_size.len() != n {
            return None;
        }
        // Enforce the arena invariant on untrusted input: the root is its own
        // parent, and every other node's parent precedes it. Without this, a
        // corrupted file could index out of range (panic on load) or form a
        // cycle (an infinite loop in `path_components`).
        if c.parent[0] != 0 || (1..n).any(|i| c.parent[i] as usize >= i) {
            return None;
        }
        let mut nodes = Vec::with_capacity(n);
        for i in 0..n {
            let kind = if c.kind[i] == 1 { NodeKind::File } else { NodeKind::Dir };
            nodes.push(Node {
                name: c.name[i].clone().into_boxed_str(),
                parent: NodeId(c.parent[i]),
                kind,
                own_size: c.own_size[i],
                subtree_size: c.own_size[i],
                file_count: if matches!(kind, NodeKind::File) { 1 } else { 0 },
                children: Vec::new(),
            });
        }
        let mut tree = Tree { nodes };
        // Rebuild children (arena order preserved: parent index < child index).
        for i in 1..n {
            let parent = tree.nodes[i].parent.index();
            tree.nodes[parent].children.push(NodeId(i as u32));
        }
        tree.recompute_sizes();
        Some((tree, c.stats))
    }

    /// Save this tree to `path` in the cache format. Written to a sibling temp
    /// file and renamed into place, so an interrupted save can't leave a
    /// truncated cache behind.
    pub fn save_cache(&self, path: &std::path::Path, stats: CacheStats) -> std::io::Result<()> {
        let bytes = self
            .to_cache_bytes(stats)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)
    }

    /// Load a cached tree from `path`, or `None` if missing/corrupt.
    pub fn load_cache(path: &std::path::Path) -> Option<(Tree, CacheStats)> {
        Self::from_cache_bytes(&std::fs::read(path).ok()?)
    }

    /// Compute [`Node::subtree_size`] for every node: each node's own size plus
    /// all descendants. O(n), one pass, allocation-free.
    ///
    /// Relies on the arena invariant (parent index < child index): processing
    /// high→low means every node's subtree is fully summed before it is folded
    /// into its parent. Safe to call repeatedly (e.g. as a scan streams in).
    pub fn recompute_sizes(&mut self) {
        for n in &mut self.nodes {
            n.subtree_size = n.own_size;
            n.file_count = if matches!(n.kind, NodeKind::File) { 1 } else { 0 };
        }
        // Fold each node into its parent, skipping the root (index 0, its own
        // parent — folding it into itself would double-count).
        for i in (1..self.nodes.len()).rev() {
            let sub = self.nodes[i].subtree_size;
            let fc = self.nodes[i].file_count;
            let parent = self.nodes[i].parent.index();
            self.nodes[parent].subtree_size += sub;
            self.nodes[parent].file_count += fc;
        }
    }
}

/// Category with the greatest byte total; ties broken toward the first, empty
/// falls through to `Other` (the last category).
fn dominant_of(sums: &[u64; FileCategory::COUNT]) -> FileCategory {
    let mut best_i = FileCategory::COUNT - 1; // Other
    let mut best_v = 0u64;
    for (i, &v) in sums.iter().enumerate() {
        if v > best_v {
            best_v = v;
            best_i = i;
        }
    }
    FileCategory::ALL[best_i]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build:  C: ─ Users ─ a.txt (100)
    ///                    └ b.txt (200)
    ///         C: └ Empty (dir, no children)
    fn sample() -> (Tree, NodeId, NodeId, NodeId) {
        let mut t = Tree::new("C:");
        let users = t.add_child(Tree::ROOT, "Users", NodeKind::Dir, 0);
        let a = t.add_child(users, "a.txt", NodeKind::File, 100);
        let _b = t.add_child(users, "b.txt", NodeKind::File, 200);
        let empty = t.add_child(Tree::ROOT, "Empty", NodeKind::Dir, 0);
        (t, users, a, empty)
    }

    #[test]
    fn structure_and_counts() {
        let (t, users, _a, _empty) = sample();
        assert_eq!(t.len(), 5); // root + Users + a + b + Empty
        assert_eq!(t.children(Tree::ROOT).len(), 2); // Users, Empty
        assert_eq!(t.children(users).len(), 2); // a, b
        assert_eq!(t.node(users).kind, NodeKind::Dir);
    }

    #[test]
    fn sizes_aggregate_up() {
        let (mut t, users, a, empty) = sample();
        t.recompute_sizes();
        assert_eq!(t.node(Tree::ROOT).subtree_size, 300);
        assert_eq!(t.node(users).subtree_size, 300);
        assert_eq!(t.node(a).subtree_size, 100);
        assert_eq!(t.node(empty).subtree_size, 0);
    }

    #[test]
    fn recompute_is_idempotent() {
        let (mut t, _users, _a, _empty) = sample();
        t.recompute_sizes();
        t.recompute_sizes(); // must not double-count
        assert_eq!(t.node(Tree::ROOT).subtree_size, 300);
    }

    #[test]
    fn propagating_insert_keeps_sizes_live() {
        // Same shape as sample(), but built with propagating inserts and NO
        // recompute_sizes call — subtree_size must already be correct.
        let mut t = Tree::new("C:");
        let users = t.add_child_propagating(Tree::ROOT, "Users", NodeKind::Dir, 0);
        let a = t.add_child_propagating(users, "a.txt", NodeKind::File, 100);
        let _b = t.add_child_propagating(users, "b.txt", NodeKind::File, 200);
        let empty = t.add_child_propagating(Tree::ROOT, "Empty", NodeKind::Dir, 0);
        assert_eq!(t.node(Tree::ROOT).subtree_size, 300);
        assert_eq!(t.node(users).subtree_size, 300);
        assert_eq!(t.node(a).subtree_size, 100);
        assert_eq!(t.node(empty).subtree_size, 0);
        // file_count is maintained the same way.
        assert_eq!(t.node(Tree::ROOT).file_count, 2);
        assert_eq!(t.node(users).file_count, 2);
        assert_eq!(t.node(a).file_count, 1);
        assert_eq!(t.node(empty).file_count, 0);
    }

    #[test]
    fn partial_metrics_grows_with_cutoff() {
        // Arena order: 0=C:, 1=Users, 2=a(100), 3=b(200), 4=Empty
        let mut t = Tree::new("C:");
        let users = t.add_child(Tree::ROOT, "Users", NodeKind::Dir, 0);
        let _a = t.add_child(users, "a", NodeKind::File, 100);
        let _b = t.add_child(users, "b", NodeKind::File, 200);
        let _e = t.add_child(Tree::ROOT, "Empty", NodeKind::Dir, 0);

        // Only first 3 nodes discovered (C:, Users, a): root sees 100, 1 file.
        let (sz, ct) = t.partial_metrics(3);
        assert_eq!(sz[Tree::ROOT.index()], 100);
        assert_eq!(ct[Tree::ROOT.index()], 1);
        // Full: 300 bytes, 2 files.
        let (sz, ct) = t.partial_metrics(t.len());
        assert_eq!(sz[Tree::ROOT.index()], 300);
        assert_eq!(ct[Tree::ROOT.index()], 2);
    }

    #[test]
    fn cache_round_trip() {
        let mut t = Tree::new("C:");
        let users = t.add_child_propagating(Tree::ROOT, "Users", NodeKind::Dir, 0);
        let _a = t.add_child_propagating(users, "a.txt", NodeKind::File, 100);
        let _b = t.add_child_propagating(users, "movie.mkv", NodeKind::File, 200);
        let _empty = t.add_child_propagating(Tree::ROOT, "Empty", NodeKind::Dir, 0);

        let stats = CacheStats {
            dirs: 2,
            files: 2,
            bytes: 300,
            saved_unix: 42,
            usn_journal_id: 0,
            usn_next: 0,
        };
        let bytes = t.to_cache_bytes(stats).unwrap();
        let (r, rs) = Tree::from_cache_bytes(&bytes).unwrap();

        // Structure, sizes, and counts survive the round trip.
        assert_eq!(r.len(), t.len());
        assert_eq!(r.node(Tree::ROOT).name.as_ref(), "C:");
        assert_eq!(r.node(Tree::ROOT).subtree_size, 300);
        assert_eq!(r.node(Tree::ROOT).file_count, 2);
        assert_eq!(r.node(users).subtree_size, 300);
        assert_eq!(r.children(Tree::ROOT).len(), 2);
        assert_eq!(r.path_components(users), vec!["C:", "Users"]);
        assert_eq!(rs.bytes, 300);
        assert_eq!(rs.saved_unix, 42);

        // Hardening: the framed format carries the magic and rejects anything
        // else — empty, junk, a headerless body, or a corrupted magic.
        assert!(bytes.starts_with(b"SECT"));
        assert!(Tree::from_cache_bytes(&[]).is_none());
        assert!(Tree::from_cache_bytes(b"not a sector cache").is_none());
        assert!(Tree::from_cache_bytes(&bytes[6..]).is_none()); // raw postcard body
        let mut bad = bytes.clone();
        bad[0] = b'X';
        assert!(Tree::from_cache_bytes(&bad).is_none());
    }

    #[test]
    fn find_descendant_walks_by_name() {
        let mut t = Tree::new("C:");
        let users = t.add_child(Tree::ROOT, "Users", NodeKind::Dir, 0);
        let docs = t.add_child(users, "Docs", NodeKind::Dir, 0);

        assert_eq!(t.find_descendant(Tree::ROOT, &[]), Some(Tree::ROOT));
        assert_eq!(t.find_descendant(Tree::ROOT, &["Users"]), Some(users));
        // Case-insensitive (matches Windows).
        assert_eq!(t.find_descendant(Tree::ROOT, &["users", "docs"]), Some(docs));
        // A component that isn't there ends the walk.
        assert_eq!(t.find_descendant(Tree::ROOT, &["Users", "Nope"]), None);
    }

    #[test]
    fn find_descendant_is_unicode_case_insensitive() {
        let mut t = Tree::new("Y:");
        let elan = t.add_child(Tree::ROOT, "Élan", NodeKind::Dir, 0);
        // A non-ASCII uppercase letter must still match its lowercase form.
        assert_eq!(t.find_descendant(Tree::ROOT, &["élan"]), Some(elan));
        assert_eq!(t.find_descendant(Tree::ROOT, &["ÉLAN"]), Some(elan));
        assert_eq!(t.find_descendant(Tree::ROOT, &["elan"]), None); // not a match
    }

    #[test]
    fn cache_rejects_broken_parent_links() {
        let mut t = Tree::new("C:");
        let users = t.add_child(Tree::ROOT, "Users", NodeKind::Dir, 0);
        t.add_child(users, "a.txt", NodeKind::File, 100);
        let stats = CacheStats { dirs: 1, files: 1, bytes: 100, saved_unix: 0, usn_journal_id: 0, usn_next: 0 };

        // Forge caches with bad parent arrays via the same framing.
        let forge = |parent: Vec<u32>| {
            let c = TreeCacheV1 {
                root_name: "C:".into(),
                stats,
                name: vec!["C:".into(), "Users".into(), "a.txt".into()],
                parent,
                kind: vec![0, 0, 1],
                own_size: vec![0, 0, 100],
            };
            let body = postcard::to_stdvec(&c).unwrap();
            let mut out = CACHE_MAGIC.to_vec();
            out.extend_from_slice(&CACHE_VERSION.to_le_bytes());
            out.extend_from_slice(&body);
            out
        };
        assert!(Tree::from_cache_bytes(&forge(vec![0, 0, 1])).is_some()); // valid
        assert!(Tree::from_cache_bytes(&forge(vec![0, 0, 7])).is_none()); // out of range → would panic
        assert!(Tree::from_cache_bytes(&forge(vec![0, 2, 1])).is_none()); // cycle → would loop forever
        assert!(Tree::from_cache_bytes(&forge(vec![1, 0, 1])).is_none()); // root not its own parent
    }

    #[test]
    fn dominant_category_by_bytes() {
        // movies/ has one big video and one tiny image → dominant is Video.
        let mut t = Tree::new("root");
        let movies = t.add_child(Tree::ROOT, "movies", NodeKind::Dir, 0);
        t.add_child(movies, "film.mp4", NodeKind::File, 1000);
        t.add_child(movies, "thumb.jpg", NodeKind::File, 10);
        let empty = t.add_child(Tree::ROOT, "empty", NodeKind::Dir, 0);
        t.recompute_sizes();
        let dom = t.dominant_categories();
        assert_eq!(dom[movies.index()], FileCategory::Video);
        assert_eq!(dom[Tree::ROOT.index()], FileCategory::Video);
        assert_eq!(dom[empty.index()], FileCategory::Other); // no sized files
    }

    #[test]
    fn path_reconstruction() {
        let (t, _users, a, empty) = sample();
        assert_eq!(t.path_components(a), vec!["C:", "Users", "a.txt"]);
        assert_eq!(t.path_components(empty), vec!["C:", "Empty"]);
        assert_eq!(t.path_components(Tree::ROOT), vec!["C:"]);
    }
}
