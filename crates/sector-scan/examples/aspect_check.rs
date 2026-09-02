//! Reproduce the "sliver" problem: a mid-sized item among huge ones. Reports the
//! worst leaf aspect ratio produced by the treemap layout.
use sector_core::{layout, LayoutOptions, NodeKind, Rect, Tree};

fn main() {
    let mut t = Tree::new("Y:");
    let gb = 1_000_000_000u64;
    let tb = 1000 * gb;
    // One child dominates the drive (~95%), the rest squeezed into a thin
    // remainder — like Y: where "Media" is almost everything.
    for (name, sz) in [
        ("Media", 78 * tb),
        ("Backup", 633 * gb),
        ("Claude", 40 * gb),
        ("Photos", 120 * gb),
        ("Misc", 8 * gb),
    ] {
        t.add_child(Tree::ROOT, name, NodeKind::File, sz);
    }
    t.recompute_sizes();

    let opts = LayoutOptions { max_depth: 4, min_tile: 4.0, padding: 1.0 };
    let tiles = layout(&t, Tree::ROOT, Rect::new(0.0, 0.0, 1000.0, 1000.0), &opts);

    let mut worst = 1.0f32;
    let mut worst_name = String::new();
    for tile in &tiles {
        if tile.depth == 0 {
            continue;
        }
        let (w, h) = (tile.rect.w.max(0.01), tile.rect.h.max(0.01));
        let aspect = (w / h).max(h / w);
        let name = t.node(tile.node).name.to_string();
        if name == "Backup" {
            println!("Backup rect: {:.1} x {:.1}  (aspect {:.1}:1)", w, h, aspect);
        }
        if aspect > worst {
            worst = aspect;
            worst_name = name;
        }
    }
    println!("worst aspect among {} tiles: {:.1}:1  ({worst_name})", tiles.len(), worst);
}
