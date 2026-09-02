//! End-to-end sanity check with a *visible* result:
//!   scan a path -> build tree -> squarified layout -> write an SVG.
//!
//!   cargo run --release --example layout_svg -- <path> [out.svg] [WxH]
//!
//! No GPU involved — this validates the layout geometry on real data and lets us
//! eyeball the treemap before building the wgpu renderer.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use sector_core::{human_size, layout, FileCategory, LayoutOptions, Rect, Tree};
use sector_scan::{scan, ScanOptions};

fn cat_hex(c: FileCategory) -> &'static str {
    match c {
        FileCategory::Video => "#cf8a3e",
        FileCategory::Image => "#4fa88f",
        FileCategory::Audio => "#9a7cc4",
        FileCategory::Archive => "#5b86b4",
        FileCategory::Document => "#b2bac6",
        FileCategory::Code => "#9fb155",
        FileCategory::System => "#b05a4e",
        FileCategory::Other => "#6a7280",
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = PathBuf::from(args.next().unwrap_or_else(|| {
        eprintln!("usage: layout_svg <path> [out.svg] [WxH]");
        std::process::exit(2);
    }));
    let out_path = args.next().unwrap_or_else(|| "treemap.svg".to_string());
    let (vw, vh) = args
        .next()
        .and_then(|s| {
            let (a, b) = s.split_once('x')?;
            Some((a.parse().ok()?, b.parse().ok()?))
        })
        .unwrap_or((1200.0f32, 800.0f32));

    let cancel = AtomicBool::new(false);
    let t0 = Instant::now();
    let (tree, stats) = scan(&path, &ScanOptions { threads: 32 }, &cancel, None);
    let scan_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let viewport = Rect::new(0.0, 0.0, vw, vh);
    let t1 = Instant::now();
    let tiles = layout(&tree, Tree::ROOT, viewport, &LayoutOptions::default());
    let layout_ms = t1.elapsed().as_secs_f64() * 1000.0;

    println!(
        "scanned {} dirs / {} files / {} in {scan_ms:.0}ms; laid out {} tiles ({}x{}) in {layout_ms:.1}ms",
        stats.dirs,
        stats.files,
        human_size(stats.bytes),
        tiles.len(),
        vw as u32,
        vh as u32,
    );

    // Color every tile by its subtree's dominant content category (folders
    // included) — the same scheme as the app.
    let dom = tree.dominant_categories();

    let mut svg = String::new();
    writeln!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{vw}" height="{vh}" viewBox="0 0 {vw} {vh}">"#
    )
    .unwrap();
    writeln!(svg, r##"<rect width="{vw}" height="{vh}" fill="#15181d"/>"##).unwrap();

    // Pre-order tiles: drawing in order paints children over parents.
    for tile in &tiles {
        let r = tile.rect;
        let fill = cat_hex(dom[tile.node.index()]);
        writeln!(
            svg,
            r##"<rect x="{:.2}" y="{:.2}" width="{:.2}" height="{:.2}" fill="{fill}" stroke="#12151b" stroke-width="0.5"/>"##,
            r.x, r.y, r.w.max(0.0), r.h.max(0.0)
        )
        .unwrap();
    }
    writeln!(svg, "</svg>").unwrap();

    std::fs::write(&out_path, svg).unwrap();
    println!("wrote {out_path}");
}
