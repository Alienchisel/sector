//! 2.5D mock: an isometric extruded treemap ("cityscape"), rendered to SVG so we
//! can judge the vibe on Linux before building it into the app.
//!
//!   cargo run --release --example iso_svg -- <path> [out.svg] [dusk|kowloon]
//!
//! Leaf blocks: footprint = treemap rect, height ∝ log(size), color = file type,
//! three shaded faces, drawn back-to-front. A ground plinth grounds the city; a
//! per-block ground shadow (offset by height) adds depth. Two style presets:
//! `dusk` (clean city at dusk) and `kowloon` (dense, dark, neon-in-gloom).

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use sector_core::{layout, FileCategory, LayoutOptions, Rect, Tile, Tree};
use sector_scan::{scan, ScanOptions};

fn cat_rgb(c: FileCategory) -> (f64, f64, f64) {
    let (r, g, b) = match c {
        FileCategory::Video => (0xcf, 0x8a, 0x3e),
        FileCategory::Image => (0x4f, 0xa8, 0x8f),
        FileCategory::Audio => (0x9a, 0x7c, 0xc4),
        FileCategory::Archive => (0x5b, 0x86, 0xb4),
        FileCategory::Document => (0xb2, 0xba, 0xc6),
        FileCategory::Code => (0x9f, 0xb1, 0x55),
        FileCategory::System => (0xb0, 0x5a, 0x4e),
        FileCategory::Other => (0x6a, 0x72, 0x80),
    };
    (r as f64, g as f64, b as f64)
}

fn hex(rgb: (f64, f64, f64), f: f64) -> String {
    let c = |v: f64| (v * f).clamp(0.0, 255.0) as u8;
    format!("#{:02x}{:02x}{:02x}", c(rgb.0), c(rgb.1), c(rgb.2))
}

/// Dimetric projection: x→right, y→left, z→up.
fn project(x: f64, y: f64, z: f64) -> (f64, f64) {
    ((x - y) * 0.5, (x + y) * 0.25 - z)
}

struct Style {
    bg_top: &'static str,
    bg_bot: &'static str,
    plinth_top: &'static str,
    plinth_r: &'static str,
    plinth_f: &'static str,
    padding: f32,
    min_tile: f32,
    height_base: f64,
    height_scale: f64,
    top_f: f64,
    right_f: f64,
    front_f: f64,
    shadow_alpha: f64,
    shadow_off: f64,
    shadow_expand: f64,
    stroke: &'static str,
}

impl Style {
    fn for_mode(mode: &str) -> Style {
        match mode {
            // Dense megastructure, near-black night, neon-bright tops in the gloom.
            "kowloon" => Style {
                bg_top: "#050608",
                bg_bot: "#0e1014",
                plinth_top: "#15171c",
                plinth_r: "#0c0d10",
                plinth_f: "#090a0c",
                padding: 0.6,
                min_tile: 6.0,
                height_base: 4.0,
                height_scale: 80.0,
                top_f: 1.18,
                right_f: 0.42,
                front_f: 0.28,
                shadow_alpha: 0.5,
                shadow_off: 0.45,
                shadow_expand: 2.0,
                stroke: "#050607",
            },
            // Dark-but-breathing: Kowloon's night + neon glow + taller towers,
            // but with more street spacing so it stays readable. (Chosen look.)
            "blend" => Style {
                bg_top: "#070910",
                bg_bot: "#12161f",
                plinth_top: "#1c212a",
                plinth_r: "#12151b",
                plinth_f: "#0d1014",
                padding: 1.2,
                min_tile: 7.0,
                height_base: 4.0,
                height_scale: 72.0,
                top_f: 1.12,
                right_f: 0.5,
                front_f: 0.34,
                shadow_alpha: 0.4,
                shadow_off: 0.4,
                shadow_expand: 2.5,
                stroke: "#080a0d",
            },
            // Clean city at dusk.
            _ => Style {
                bg_top: "#0b0d11",
                bg_bot: "#1d222c",
                plinth_top: "#2b3039",
                plinth_r: "#191c22",
                plinth_f: "#141619",
                padding: 2.0,
                min_tile: 8.0,
                height_base: 3.0,
                height_scale: 55.0,
                top_f: 1.0,
                right_f: 0.62,
                front_f: 0.46,
                shadow_alpha: 0.28,
                shadow_off: 0.35,
                shadow_expand: 3.0,
                stroke: "#0e1116",
            },
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = PathBuf::from(args.next().unwrap_or_else(|| {
        eprintln!("usage: iso_svg <path> [out.svg] [dusk|kowloon]");
        std::process::exit(2);
    }));
    let out_path = args.next().unwrap_or_else(|| "iso.svg".to_string());
    let mode = args.next().unwrap_or_else(|| "dusk".to_string());
    let style = Style::for_mode(&mode);

    let cancel = AtomicBool::new(false);
    let (tree, stats) = scan(&path, &ScanOptions { threads: 32 }, &cancel, None);
    let dom = tree.dominant_categories();

    let opts = LayoutOptions {
        max_depth: 12,
        min_tile: style.min_tile,
        padding: style.padding,
    };
    let tiles = layout(&tree, Tree::ROOT, Rect::new(0.0, 0.0, 1000.0, 1000.0), &opts);

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

    let size_of = |t: &Tile| (tree.node(t.node).subtree_size + 1) as f64;
    let max_ln = leaves.iter().map(|t| size_of(t).ln()).fold(1.0, f64::max);
    let height = |t: &Tile| style.height_base + style.height_scale * (size_of(t).ln() / max_ln);

    let mut order = leaves.clone();
    order.sort_by(|a, b| {
        (a.rect.x + a.rect.y)
            .partial_cmp(&(b.rect.x + b.rect.y))
            .unwrap()
    });

    // Bounds (blocks + plinth).
    let plinth_th = 26.0;
    let (mut minx, mut miny, mut maxx, mut maxy) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    let mut extend = |x: f64, y: f64, z: f64| {
        let (sx, sy) = project(x, y, z);
        minx = minx.min(sx);
        maxx = maxx.max(sx);
        miny = miny.min(sy);
        maxy = maxy.max(sy);
    };
    for t in &order {
        let (x, y, w, h) = (t.rect.x as f64, t.rect.y as f64, t.rect.w as f64, t.rect.h as f64);
        let hz = height(t);
        extend(x, y, hz);
        extend(x + w, y + h, hz);
        extend(x + w, y + h, 0.0);
    }
    for &(cx, cy) in &[(0.0, 0.0), (1000.0, 1000.0), (1000.0, 0.0), (0.0, 1000.0)] {
        extend(cx, cy, 0.0);
        extend(cx, cy, -plinth_th);
    }

    let margin = 30.0;
    let (vw, vh) = (maxx - minx + 2.0 * margin, maxy - miny + 2.0 * margin);
    let (tx, ty) = (-minx + margin, -miny + margin);

    let pts = |p: &[(f64, f64)]| -> String {
        let mut s = String::new();
        for (px, py) in p {
            let _ = write!(s, "{:.1},{:.1} ", px + tx, py + ty);
        }
        s
    };
    let poly = |p: &[(f64, f64)], fill: &str| {
        format!(
            "<polygon points=\"{}\" fill=\"{fill}\" stroke=\"{}\" stroke-width=\"0.4\"/>",
            pts(p),
            style.stroke
        )
    };

    let mut svg = String::new();
    let _ = writeln!(
        svg,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{vw:.0}\" height=\"{vh:.0}\" viewBox=\"0 0 {vw:.0} {vh:.0}\">"
    );
    let _ = writeln!(
        svg,
        "<defs><linearGradient id=\"sky\" x1=\"0\" y1=\"0\" x2=\"0\" y2=\"1\"><stop offset=\"0\" stop-color=\"{}\"/><stop offset=\"1\" stop-color=\"{}\"/></linearGradient></defs>",
        style.bg_top, style.bg_bot
    );
    let _ = writeln!(svg, "<rect width=\"{vw:.0}\" height=\"{vh:.0}\" fill=\"url(#sky)\"/>");

    // Plinth (behind the city).
    {
        let top = [project(0.0, 0.0, 0.0), project(1000.0, 0.0, 0.0), project(1000.0, 1000.0, 0.0), project(0.0, 1000.0, 0.0)];
        let right = [project(1000.0, 0.0, 0.0), project(1000.0, 1000.0, 0.0), project(1000.0, 1000.0, -plinth_th), project(1000.0, 0.0, -plinth_th)];
        let front = [project(0.0, 1000.0, 0.0), project(1000.0, 1000.0, 0.0), project(1000.0, 1000.0, -plinth_th), project(0.0, 1000.0, -plinth_th)];
        let _ = writeln!(svg, "{}", poly(&right, style.plinth_r));
        let _ = writeln!(svg, "{}", poly(&front, style.plinth_f));
        let _ = writeln!(svg, "{}", poly(&top, style.plinth_top));
    }

    for t in &order {
        let (x, y, w, h) = (t.rect.x as f64, t.rect.y as f64, t.rect.w as f64, t.rect.h as f64);
        let hz = height(t);
        let rgb = cat_rgb(dom[t.node.index()]);

        // Ground shadow: footprint expanded and offset by height (tall → long).
        let off = hz * style.shadow_off;
        let e = style.shadow_expand;
        let sh = [
            project(x - e + off, y - e + off, 0.0),
            project(x + w + e + off, y - e + off, 0.0),
            project(x + w + e + off, y + h + e + off, 0.0),
            project(x - e + off, y + h + e + off, 0.0),
        ];
        let _ = writeln!(
            svg,
            "<polygon points=\"{}\" fill=\"black\" fill-opacity=\"{:.2}\"/>",
            pts(&sh),
            style.shadow_alpha
        );

        let right = [project(x + w, y, 0.0), project(x + w, y + h, 0.0), project(x + w, y + h, hz), project(x + w, y, hz)];
        let front = [project(x, y + h, 0.0), project(x + w, y + h, 0.0), project(x + w, y + h, hz), project(x, y + h, hz)];
        let top = [project(x, y, hz), project(x + w, y, hz), project(x + w, y + h, hz), project(x, y + h, hz)];
        let _ = writeln!(svg, "{}", poly(&right, &hex(rgb, style.right_f)));
        let _ = writeln!(svg, "{}", poly(&front, &hex(rgb, style.front_f)));
        let _ = writeln!(svg, "{}", poly(&top, &hex(rgb, style.top_f)));
    }
    let _ = writeln!(svg, "</svg>");
    std::fs::write(&out_path, svg).unwrap();
    println!(
        "[{mode}] {} files / {}; {} blocks; wrote {out_path}",
        stats.files,
        sector_core::human_size(stats.bytes),
        order.len()
    );
}
