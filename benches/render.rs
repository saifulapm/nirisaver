//! The render benchmark.
//!
//! It measures two things and checks a third:
//!
//!   * how long an incremental frame takes — rasterizing and damaging only the
//!     cells that moved;
//!   * how long the same frame takes drawn from scratch, which is what a
//!     client without damage tracking pays every time;
//!   * that the two produce *byte-for-byte identical buffers*, on every frame.
//!
//! The third is the important one. Throughput for a pipeline nobody verified
//! is a number about nothing, and an incremental renderer that is subtly wrong
//! is exactly the kind of thing that looks fine on a developer's screen and
//! leaves stale glyphs on someone else's.
//!
//! Everything it renders comes from inside the repository: the bundled font
//! and the text below. Nothing here reads `$XDG_CONFIG_HOME`, a quote list, or
//! any other file — a benchmark whose input depends on the machine is not a
//! benchmark, and `Settings::builtin` exists so this stays true.
//!
//! Two workloads, because the answer depends enormously on how much of the
//! screen an effect is moving, and one number would hide that:
//!
//!   * **paragraph** — a wrapped quote across four effects that keep rewriting
//!     cells they have already written. This is the realistic screensaver
//!     workload and the worst case for damage tracking.
//!   * **wordmark** — a short centred word under one effect. A light frame,
//!     which is what most of a real cycle looks like once the text has
//!     settled.
//!
//! The buffers alternate, which is what the overlay does and is why the
//! redrawn-cell count runs ahead of the moved-cell count: the buffer being
//! drawn into holds the frame from two presentations ago, so it is two frames
//! of change behind, not one. That is the standing price of not deadlocking on
//! `wl_buffer.release`.

use std::alloc::{GlobalAlloc, Layout as AllocLayout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use nirisaver::config::{Settings, EMBEDDED_FONT};
use nirisaver::engine::{Advance, Animation, Animator};
use nirisaver::grid::{changed_cells, Grid};
use nirisaver::raster::{Layout, Rasterizer};
use nirisaver::render::{render_frame, History, Snapshot};
use nirisaver::text::{parse_quotes, Content};

/// 5K, the largest single surface this is likely to meet.
const WIDTH: u32 = 5120;
const HEIGHT: u32 = 2880;
const FONT_PX: f32 = 36.0;
const SEED: u64 = 0x51ce_d0d0;
const WARMUP: usize = 30;
const FRAMES: usize = 300;

const QUOTES: &str = "\
It always seems impossible until it's done. — Nelson Mandela
The secret of getting ahead is getting started. — Mark Twain
Simplicity is the ultimate sophistication. — Leonardo da Vinci
We are what we repeatedly do. Excellence, then, is not an act, but a habit. — Will Durant
";

struct Scenario {
    name: &'static str,
    content: fn() -> Content,
    measure: usize,
    effects: &'static [&'static str],
}

const SCENARIOS: [Scenario; 2] = [
    Scenario {
        name: "paragraph",
        content: || Content::Quotes(parse_quotes(QUOTES, " — ")),
        measure: 64,
        effects: &["decrypt", "binarypath", "matrix", "beams"],
    },
    Scenario {
        name: "wordmark",
        content: || Content::Text("nirisaver".into()),
        measure: 32,
        effects: &["decrypt"],
    },
];

struct Counting;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: AllocLayout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: AllocLayout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: AllocLayout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn main() {
    // `cargo test --all-targets` builds and runs this in the dev profile, where
    // the timings would be meaningless anyway and rendering 5K frames
    // unoptimised would turn the test suite into a coffee break. So an
    // unoptimised build runs a small version and reports only the part that
    // still means something there: whether the incremental path matches the
    // oracle. That comparison is an ordinary `if`, not a `debug_assert`, so it
    // runs in both builds.
    if cfg!(debug_assertions) {
        for scenario in &SCENARIOS {
            measure(scenario, 1280, 720, 5, 20);
        }
        println!("render bench smoke test: incremental matched the oracle");
        return;
    }

    println!(
        "nirisaver render benchmark\n  \
         surface   {WIDTH}x{HEIGHT} ({} MiB per buffer, two buffers per output)\n  \
         seed      {SEED:#x}, {FRAMES} frames per workload after {WARMUP} warmup",
        (WIDTH as usize * HEIGHT as usize * 4) / (1 << 20),
    );

    for scenario in &SCENARIOS {
        let stats = measure(scenario, WIDTH, HEIGHT, WARMUP, FRAMES);
        stats.print(scenario.name);
    }

    if let Some(rss) = resident_memory() {
        println!(
            "\nresident memory {rss} for the benchmark process, which holds three \
             full-size buffers\n(two plus the oracle) and the animation engine's per-character \
             state. The overlay\nitself holds two."
        );
    }
}

struct Stats {
    incremental: Vec<f64>,
    full: Vec<f64>,
    cells_drawn: usize,
    cells_moved: usize,
    rects: usize,
    allocations: usize,
    frames: usize,
    cells: usize,
    grid: (usize, usize),
    cell_px: (u32, u32),
    checksum: u64,
}

impl Stats {
    fn print(mut self, name: &str) {
        let quantiles = |samples: &mut Vec<f64>| -> (f64, f64, f64, f64) {
            samples.sort_by(f64::total_cmp);
            let mean = samples.iter().sum::<f64>() / samples.len() as f64;
            (
                mean,
                samples[samples.len() / 2],
                samples[samples.len() * 95 / 100],
                samples[samples.len() - 1],
            )
        };
        let (imean, ip50, ip95, imax) = quantiles(&mut self.incremental);
        let (fmean, fp50, fp95, fmax) = quantiles(&mut self.full);
        let per = |total: usize| total as f64 / self.frames as f64;

        println!(
            "\n{name}: grid {}x{} of {}x{} px cells\n  \
             incremental   mean {imean:6.3} ms   p50 {ip50:6.3}   p95 {ip95:6.3}   max {imax:6.3}\n  \
             full buffer   mean {fmean:6.3} ms   p50 {fp50:6.3}   p95 {fp95:6.3}   max {fmax:6.3}\n  \
             speedup       {:.1}x\n  \
             cells moved   {:.0} of {} per frame ({:.2}%)\n  \
             cells redrawn {:.0} per frame (two frames of change, one per buffer)\n  \
             damage rects  {:.1} per frame\n  \
             allocations   {:.1} per frame ({} over {} frames)\n  \
             checksum      {:016x}\n  \
             verified      incremental matched the full-frame oracle on all {} frames",
            self.grid.0,
            self.grid.1,
            self.cell_px.0,
            self.cell_px.1,
            fmean / imean,
            per(self.cells_moved),
            self.cells,
            100.0 * self.cells_moved as f64 / (self.frames * self.cells) as f64,
            per(self.cells_drawn),
            per(self.rects),
            per(self.allocations),
            self.allocations,
            self.frames,
            self.checksum,
            self.frames,
        );
    }
}

fn measure(scenario: &Scenario, width: u32, height: u32, warmup: usize, frames: usize) -> Stats {
    let font_px = FONT_PX * width as f32 / WIDTH as f32;
    let mut raster = Rasterizer::new(EMBEDDED_FONT, font_px, 1.0).expect("bundled font");
    raster.warm(' '..='~');
    let layout = Layout::fit(width, height, raster.metrics());

    let mut settings = Settings::builtin();
    settings.content = (scenario.content)();
    settings.measure.width = scenario.measure;

    let mut animator = Animator::new(
        Animation {
            cols: layout.cols,
            rows: layout.rows,
            frame_rate: settings.frame_rate,
            hold: std::time::Duration::from_millis(200),
            measure: settings.measure.clone(),
            content: settings.content.clone(),
            effects: scenario.effects.iter().map(|e| e.to_string()).collect(),
            default_fg: settings.foreground,
        },
        SEED,
    )
    .expect("animator");

    let pixels = (layout.width * layout.height) as usize;
    let mut buffers = [vec![0u32; pixels], vec![0u32; pixels]];
    let mut contents: [Option<Snapshot>; 2] = [None, None];
    let mut presented: Option<Snapshot> = None;
    let mut oracle = vec![0u32; pixels];

    let mut now = 0u64;
    let next_grid = |animator: &mut Animator, now: &mut u64| -> Grid {
        loop {
            match animator.advance(*now).expect("advance") {
                Advance::Frame => {
                    *now += 33;
                    return animator.grid().clone();
                }
                Advance::Idle { until_ms } => *now = until_ms.max(*now + 1),
            }
        }
    };

    // Warm the glyph cache and get past the first full frame, so what gets
    // timed is the steady state rather than the start-up. The last warmed
    // frame also seeds the moved-cell baseline: starting that at `None` would
    // score the first measured frame as a whole screen of change and quietly
    // inflate the average by a grid over the run.
    let mut previous: Option<Grid> = None;
    for frame in 0..warmup {
        let grid = next_grid(&mut animator, &mut now);
        let slot = frame % 2;
        render_frame(
            &mut buffers[slot],
            &layout,
            &mut raster,
            &grid,
            History { buffer: contents[slot].as_ref(), presented: presented.as_ref() },
            255,
            settings.background,
        );
        previous = Some(grid.clone());
        let snapshot = Snapshot { grid, alpha: 255 };
        contents[slot] = Some(snapshot.clone());
        presented = Some(snapshot);
    }

    let mut stats = Stats {
        incremental: Vec::with_capacity(frames),
        full: Vec::with_capacity(frames),
        cells_drawn: 0,
        cells_moved: 0,
        rects: 0,
        allocations: 0,
        frames,
        cells: layout.cols * layout.rows,
        grid: (layout.cols, layout.rows),
        cell_px: (layout.cell.width, layout.cell.height),
        checksum: 0,
    };

    for frame in 0..frames {
        let grid = next_grid(&mut animator, &mut now);
        let slot = frame % 2;

        // Allocations are counted around the render alone. The animation
        // engine allocates freely — it rebuilds a visual per character per
        // frame — and folding that in would measure ttfx rather than this.
        let allocations_before = ALLOCATIONS.load(Ordering::Relaxed);
        let start = Instant::now();
        let rendered = render_frame(
            &mut buffers[slot],
            &layout,
            &mut raster,
            &grid,
            History { buffer: contents[slot].as_ref(), presented: presented.as_ref() },
            255,
            settings.background,
        );
        stats.incremental.push(start.elapsed().as_secs_f64() * 1000.0);
        stats.allocations += ALLOCATIONS.load(Ordering::Relaxed) - allocations_before;
        stats.cells_drawn += rendered.cells_drawn;
        stats.rects += rendered.rects.len();

        // The oracle: the same frame drawn with no history at all.
        let start = Instant::now();
        render_frame(
            &mut oracle,
            &layout,
            &mut raster,
            &grid,
            History::unknown(),
            255,
            settings.background,
        );
        stats.full.push(start.elapsed().as_secs_f64() * 1000.0);

        // The check the numbers above are only worth anything with.
        if buffers[slot] != oracle {
            let differing = buffers[slot].iter().zip(&oracle).filter(|(a, b)| a != b).count();
            eprintln!(
                "FAILED: {} frame {frame} diverged from the full-frame oracle in {differing} pixels",
                scenario.name
            );
            std::process::exit(1);
        }

        stats.checksum ^= grid.checksum().rotate_left((frame % 64) as u32);
        stats.cells_moved += changed_cells(&grid, previous.as_ref());
        previous = Some(grid.clone());
        let snapshot = Snapshot { grid, alpha: 255 };
        contents[slot] = Some(snapshot.clone());
        presented = Some(snapshot);
    }
    stats
}

/// This process's own resident set. Self-inspection, not input: it cannot
/// change a single pixel of what was rendered above.
fn resident_memory() -> Option<String> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(format!("{:.1} MiB", kib as f64 / 1024.0))
}
