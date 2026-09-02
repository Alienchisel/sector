# SECTOR

> A fast, sleek filesystem visualizer for Windows — see your disk as a city.

![SECTOR cityscape](docs/preview.png)

SECTOR scans a drive — a local disk or a mapped NAS share — and renders it as an
interactive **2.5D isometric cityscape**, so you can see at a glance where your
space went and what kind of stuff is eating it:

- **Area = size** — bigger footprint, more bytes
- **Height = file count** — folders crammed with files rise into towers
- **Color = file type** — video, image, audio, archive, document, code, system, other

## Features

- **Fast concurrent scanner** for local drives *and* mapped NAS/SMB drives.
- **Live discovery build** — the map builds as it scans, instead of a blank spinner.
- **Instant reopen** — each scan is cached compactly to disk, so reopening an
  82 TB drive takes ~1 second instead of minutes (scan once, reopen forever).
- **Explore** — hover for path/size/type, click to drill into folders, breadcrumb
  or **Backspace** to go back, **right-click → Reveal in Explorer / Copy path**.
- **Cache-load animations** — a smooth "rise" reveal or an authentic scan "replay".

## Build & run

Windows, with the Rust MSVC toolchain:

```
cargo run -p sector-app
```

The core and layout logic is OS-agnostic and unit-tested; only the GUI and the
Windows-specific scan paths require Windows.

## Layout

| Crate | Role |
|---|---|
| `crates/sector-core` | Data model (arena tree), squarified treemap layout, file-type classification, cache serialization |
| `crates/sector-scan` | Concurrent filesystem scanner (+ benchmarking/preview examples) |
| `crates/sector-app`  | The egui / wgpu GUI |

## Docs

- [`docs/README.md`](docs/README.md) — overview, goals & non-goals
- [`docs/ROADMAP.md`](docs/ROADMAP.md) — build plan and progress
- [`docs/DECISIONS.md`](docs/DECISIONS.md) — design decisions (D1–D15)

## Status

Active development. **Windows-only by design** — it deliberately leans on Windows
filesystem specifics rather than a lowest-common-denominator portable approach.

---

Built with [Claude Code](https://claude.com/claude-code).
