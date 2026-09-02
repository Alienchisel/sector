//! Squarified treemap layout: turn a [`Tree`] into positioned rectangles.
//!
//! Uses the squarified algorithm (Bruls, Huizing & van Wijk, 2000), which packs
//! each node's children into its rectangle while keeping every tile's aspect
//! ratio as close to 1:1 as possible — far more legible than naive slice-and-dice
//! strips, and the standard for disk visualizers.
//!
//! The output is a **flat `Vec<Tile>` in pre-order** (parents before children),
//! which is exactly what the GPU renderer wants: one instanced draw, painter's
//! order (a child is drawn after — on top of — its parent's frame).
//!
//! Two knobs keep the tile count bounded regardless of how many millions of
//! nodes the tree has: `max_depth` and `min_tile`. A node whose allocated
//! rectangle is smaller than `min_tile` is not subdivided (its children are
//! invisible at this zoom and simply aren't emitted), so cost scales with
//! on-screen detail, not total node count. This is OS-agnostic and unit-tested
//! on Linux (D6); the renderer maps [`Rect`] into screen/GPU coordinates.

use crate::{NodeId, Tree};

/// An axis-aligned rectangle in layout space (origin top-left, y grows down).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Rect { x, y, w, h }
    }

    pub fn area(&self) -> f64 {
        self.w as f64 * self.h as f64
    }

    /// Shrink by `pad` on every side, clamped so width/height never go negative.
    fn inset(&self, pad: f32) -> Rect {
        let w = (self.w - 2.0 * pad).max(0.0);
        let h = (self.h - 2.0 * pad).max(0.0);
        Rect {
            x: self.x + pad,
            y: self.y + pad,
            w,
            h,
        }
    }
}

/// One laid-out node: which node, where, and how deep (for coloring/borders).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tile {
    pub node: NodeId,
    pub rect: Rect,
    pub depth: u16,
}

/// Layout tuning.
#[derive(Debug, Clone)]
pub struct LayoutOptions {
    /// Stop subdividing beyond this depth (root is depth 0).
    pub max_depth: u16,
    /// Don't subdivide a rectangle smaller than this (layout units, ~= pixels)
    /// in either dimension, and don't emit tiles below it. Bounds tile count.
    pub min_tile: f32,
    /// Inset applied to a node's rectangle before laying out its children,
    /// leaving a visible "frame" of the parent around them (the nested look).
    pub padding: f32,
}

impl Default for LayoutOptions {
    fn default() -> Self {
        LayoutOptions {
            max_depth: 16,
            min_tile: 4.0,
            padding: 1.0,
        }
    }
}

/// Lay out `root`'s subtree into `viewport`, returning tiles in pre-order.
///
/// Requires that [`Tree::recompute_sizes`] has been called so `subtree_size` is
/// valid (areas are proportional to `subtree_size`).
pub fn layout(tree: &Tree, root: NodeId, viewport: Rect, opts: &LayoutOptions) -> Vec<Tile> {
    let mut out = Vec::new();
    layout_node(tree, root, viewport, 0, opts, &mut out);
    out
}

fn layout_node(
    tree: &Tree,
    node: NodeId,
    rect: Rect,
    depth: u16,
    opts: &LayoutOptions,
    out: &mut Vec<Tile>,
) {
    out.push(Tile { node, rect, depth });

    if depth >= opts.max_depth || rect.w < opts.min_tile || rect.h < opts.min_tile {
        return;
    }

    // Children that occupy space, largest first (squarify requires descending).
    let mut kids: Vec<Item> = tree
        .children(node)
        .iter()
        .filter_map(|&c| {
            let v = tree.node(c).subtree_size;
            (v > 0).then_some(Item { node: c, value: v as f64 })
        })
        .collect();
    if kids.is_empty() {
        return;
    }
    kids.sort_unstable_by(|a, b| b.value.partial_cmp(&a.value).unwrap());

    let inner = rect.inset(opts.padding);
    if inner.w < opts.min_tile || inner.h < opts.min_tile {
        return;
    }

    // Scale values so the total exactly equals the inner rectangle's area, so the
    // strip math fills it precisely.
    let total: f64 = kids.iter().map(|k| k.value).sum();
    let factor = inner.area() / total;
    for k in &mut kids {
        k.value *= factor;
    }

    let mut placed = Vec::with_capacity(kids.len());
    squarify(&kids, inner, &mut placed);

    for (child, crect) in placed {
        if crect.w >= opts.min_tile && crect.h >= opts.min_tile {
            layout_node(tree, child, crect, depth + 1, opts, out);
        }
        // Sub-min_tile children are invisible at this zoom: not emitted. The
        // parent's own tile shows through where they would have been.
    }
}

/// Like [`layout`], but for a scan REPLAY: only nodes with index `< cutoff` are
/// laid out, weighted by `sizes` (partial subtree sizes from
/// [`crate::Tree::partial_metrics`]). Shows the treemap as it was mid-discovery,
/// so the structure visibly evolves as `cutoff` grows.
pub fn layout_partial(
    tree: &Tree,
    root: NodeId,
    viewport: Rect,
    opts: &LayoutOptions,
    cutoff: usize,
    sizes: &[u64],
) -> Vec<Tile> {
    let mut out = Vec::new();
    layout_node_partial(tree, root, viewport, 0, opts, cutoff, sizes, &mut out);
    out
}

#[allow(clippy::too_many_arguments)]
fn layout_node_partial(
    tree: &Tree,
    node: NodeId,
    rect: Rect,
    depth: u16,
    opts: &LayoutOptions,
    cutoff: usize,
    sizes: &[u64],
    out: &mut Vec<Tile>,
) {
    out.push(Tile { node, rect, depth });
    if depth >= opts.max_depth || rect.w < opts.min_tile || rect.h < opts.min_tile {
        return;
    }
    let mut kids: Vec<Item> = tree
        .children(node)
        .iter()
        .filter_map(|&c| {
            if c.index() >= cutoff {
                return None; // not discovered yet at this replay step
            }
            let v = sizes[c.index()];
            (v > 0).then_some(Item { node: c, value: v as f64 })
        })
        .collect();
    if kids.is_empty() {
        return;
    }
    kids.sort_unstable_by(|a, b| b.value.partial_cmp(&a.value).unwrap());
    let inner = rect.inset(opts.padding);
    if inner.w < opts.min_tile || inner.h < opts.min_tile {
        return;
    }
    let total: f64 = kids.iter().map(|k| k.value).sum();
    let factor = inner.area() / total;
    for k in &mut kids {
        k.value *= factor;
    }
    let mut placed = Vec::with_capacity(kids.len());
    squarify(&kids, inner, &mut placed);
    for (child, crect) in placed {
        if crect.w >= opts.min_tile && crect.h >= opts.min_tile {
            layout_node_partial(tree, child, crect, depth + 1, opts, cutoff, sizes, out);
        }
    }
}

/// An item to place, with its area already in the same units as the rectangle.
struct Item {
    node: NodeId,
    value: f64, // area
}

/// Worst (largest) aspect ratio in a row of given total area `s`, laid along a
/// side of length `len`, with smallest/largest item areas `min`/`max`.
/// From the squarified-treemap paper. Larger = worse; 1.0 = perfect squares.
fn worst(min: f64, max: f64, s: f64, len: f64) -> f64 {
    let len2_s2 = (len * len) / (s * s);
    (max * len2_s2).max(1.0 / (min * len2_s2))
}

/// Place `items` (areas summing to `rect.area()`, descending) into `rect`.
fn squarify(items: &[Item], mut rect: Rect, out: &mut Vec<(NodeId, Rect)>) {
    let mut i = 0;
    while i < items.len() {
        let len = rect.w.min(rect.h) as f64;
        if len <= 0.0 {
            break;
        }

        // Greedily grow the current row while it improves the worst aspect ratio.
        let mut j = i;
        let mut row_sum = items[i].value;
        let mut row_min = items[i].value;
        let mut row_max = items[i].value;
        let mut cur_worst = worst(row_min, row_max, row_sum, len);
        while j + 1 < items.len() {
            let a = items[j + 1].value;
            let new_sum = row_sum + a;
            let new_min = row_min.min(a);
            let new_max = row_max.max(a);
            let new_worst = worst(new_min, new_max, new_sum, len);
            if new_worst <= cur_worst {
                j += 1;
                row_sum = new_sum;
                row_min = new_min;
                row_max = new_max;
                cur_worst = new_worst;
            } else {
                break;
            }
        }

        lay_row(&mut rect, &items[i..=j], row_sum, out);
        i = j + 1;
    }
}

/// Lay one row along the shorter side of `rect`, then shrink `rect` to the rest.
fn lay_row(rect: &mut Rect, row: &[Item], row_sum: f64, out: &mut Vec<(NodeId, Rect)>) {
    if rect.w >= rect.h {
        // Vertical strip down the left edge; items stacked top→bottom.
        let strip_w = (row_sum / rect.h as f64) as f32;
        let mut y = rect.y;
        for it in row {
            let h = (it.value / row_sum * rect.h as f64) as f32;
            out.push((it.node, Rect::new(rect.x, y, strip_w, h)));
            y += h;
        }
        rect.x += strip_w;
        rect.w -= strip_w;
    } else {
        // Horizontal strip across the top; items left→right.
        let strip_h = (row_sum / rect.w as f64) as f32;
        let mut x = rect.x;
        for it in row {
            let w = (it.value / row_sum * rect.w as f64) as f32;
            out.push((it.node, Rect::new(x, rect.y, w, strip_h)));
            x += w;
        }
        rect.y += strip_h;
        rect.h -= strip_h;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NodeKind;

    fn opts_exact() -> LayoutOptions {
        // No padding, no culling: isolates the pure squarify geometry.
        LayoutOptions {
            max_depth: 100,
            min_tile: 0.0,
            padding: 0.0,
        }
    }

    /// Collect only the leaf/child tiles (skip the root tile at depth 0).
    fn non_root(tiles: &[Tile]) -> Vec<Tile> {
        tiles.iter().copied().filter(|t| t.depth > 0).collect()
    }

    #[test]
    fn single_child_fills_viewport() {
        let mut t = Tree::new("root");
        let c = t.add_child(Tree::ROOT, "only", NodeKind::File, 100);
        t.recompute_sizes();
        let vp = Rect::new(0.0, 0.0, 200.0, 100.0);
        let tiles = layout(&t, Tree::ROOT, vp, &opts_exact());
        let child = tiles.iter().find(|t| t.node == c).unwrap();
        assert!((child.rect.w - 200.0).abs() < 1e-3);
        assert!((child.rect.h - 100.0).abs() < 1e-3);
    }

    #[test]
    fn areas_are_proportional_and_conserved() {
        let mut t = Tree::new("root");
        let sizes = [60u64, 30, 10];
        for (i, s) in sizes.iter().enumerate() {
            t.add_child(Tree::ROOT, format!("f{i}"), NodeKind::File, *s);
        }
        t.recompute_sizes();
        let vp = Rect::new(0.0, 0.0, 100.0, 100.0);
        let tiles = layout(&t, Tree::ROOT, vp, &opts_exact());
        let leaves = non_root(&tiles);
        assert_eq!(leaves.len(), 3);

        let vp_area = vp.area();
        let total: u64 = sizes.iter().sum();
        // Each tile's area ~ proportional to its size.
        for (tile, size) in leaves.iter().zip(sizes.iter()) {
            let expected = *size as f64 / total as f64 * vp_area;
            let got = tile.rect.area();
            assert!(
                (got - expected).abs() < 1e-2 * expected,
                "tile area {got} vs expected {expected}"
            );
        }
        // Conservation: leaf areas sum to the viewport area (no padding/culling).
        let sum: f64 = leaves.iter().map(|t| t.rect.area()).sum();
        assert!((sum - vp_area).abs() < 1e-2 * vp_area, "sum {sum} vs {vp_area}");
    }

    #[test]
    fn tiles_stay_within_viewport() {
        let mut t = Tree::new("root");
        for i in 0..25 {
            t.add_child(Tree::ROOT, format!("f{i}"), NodeKind::File, (i + 1) as u64 * 7);
        }
        t.recompute_sizes();
        let vp = Rect::new(10.0, 5.0, 300.0, 180.0);
        let tiles = layout(&t, Tree::ROOT, vp, &opts_exact());
        let eps = 1e-3;
        for tile in &tiles {
            let r = tile.rect;
            assert!(r.x >= vp.x - eps && r.y >= vp.y - eps, "{r:?} left/top of vp");
            assert!(
                r.x + r.w <= vp.x + vp.w + eps && r.y + r.h <= vp.y + vp.h + eps,
                "{r:?} exceeds vp"
            );
            assert!(r.w >= -eps && r.h >= -eps, "negative extent {r:?}");
        }
    }

    #[test]
    fn nested_children_recurse() {
        // root ─ dir(subtree 300) ─ a(100), b(200)
        //      └ file(size 100)
        let mut t = Tree::new("root");
        let dir = t.add_child(Tree::ROOT, "dir", NodeKind::Dir, 0);
        let a = t.add_child(dir, "a", NodeKind::File, 100);
        let b = t.add_child(dir, "b", NodeKind::File, 200);
        let _f = t.add_child(Tree::ROOT, "file", NodeKind::File, 100);
        t.recompute_sizes();
        let vp = Rect::new(0.0, 0.0, 400.0, 400.0);
        let tiles = layout(&t, Tree::ROOT, vp, &opts_exact());
        // We should see tiles for the nested leaves a and b, deeper than dir.
        let dir_depth = tiles.iter().find(|t| t.node == dir).unwrap().depth;
        let a_tile = tiles.iter().find(|t| t.node == a).unwrap();
        let b_tile = tiles.iter().find(|t| t.node == b).unwrap();
        assert_eq!(a_tile.depth, dir_depth + 1);
        assert_eq!(b_tile.depth, dir_depth + 1);
        // b is twice a's size → about twice the area.
        let ratio = b_tile.rect.area() / a_tile.rect.area();
        assert!((ratio - 2.0).abs() < 0.05, "b/a area ratio {ratio}");
    }

    #[test]
    fn min_tile_bounds_output() {
        // Many tiny children under a small viewport: culling must keep the tile
        // count far below the node count.
        let mut t = Tree::new("root");
        for i in 0..10_000 {
            t.add_child(Tree::ROOT, format!("f{i}"), NodeKind::File, 1);
        }
        t.recompute_sizes();
        let vp = Rect::new(0.0, 0.0, 100.0, 100.0);
        let tiles = layout(&t, Tree::ROOT, vp, &LayoutOptions::default());
        assert!(
            tiles.len() < 1000,
            "expected culling to bound tiles, got {}",
            tiles.len()
        );
    }
}
