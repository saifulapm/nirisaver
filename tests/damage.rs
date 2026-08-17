//! A compositor that takes damage at its word.
//!
//! Reporting damage is a promise: everything outside the rectangles you name
//! is unchanged, and the compositor is entitled to copy nothing else. Most
//! compositors are more forgiving than that in practice, which is exactly why
//! a bug here is so easy to ship — it shows up as a few stale glyphs on one
//! machine and nothing at all on the next.
//!
//! So this test builds the strict compositor: it keeps a screen, and on every
//! commit it copies *only* the damaged rectangles out of the attached buffer.
//! Anything the client failed to declare stays stale, and the comparison
//! against a full-frame oracle catches it immediately.

use nirisaver::config::{Settings, EMBEDDED_FONT};
use nirisaver::engine::{Advance, Animation, Animator};
use nirisaver::grid::Grid;
use nirisaver::raster::{Layout, Rasterizer};
use nirisaver::render::{pixel_rect, render_frame, History, Snapshot};
use nirisaver::text::{parse_quotes, Content};

const QUOTES: &str = "\
It always seems impossible until it's done. — Nelson Mandela
The secret of getting ahead is getting started. — Mark Twain
Simplicity is the ultimate sophistication. — Leonardo da Vinci
";

/// A compositor that copies exactly what it was told changed, and not a pixel
/// more.
struct StrictCompositor {
    screen: Vec<u32>,
    width: usize,
}

impl StrictCompositor {
    fn new(width: usize, height: usize) -> Self {
        StrictCompositor { screen: vec![0; width * height], width }
    }

    fn commit(&mut self, buffer: &[u32], rects: &[(i32, i32, i32, i32)]) {
        for &(x, y, w, h) in rects {
            for row in y..y + h {
                let start = row as usize * self.width + x as usize;
                let end = start + w as usize;
                self.screen[start..end].copy_from_slice(&buffer[start..end]);
            }
        }
    }

    fn stale_pixels(&self, oracle: &[u32]) -> usize {
        self.screen.iter().zip(oracle).filter(|(a, b)| a != b).count()
    }
}

struct Fixture {
    raster: Rasterizer,
    layout: Layout,
    animator: Animator,
    buffers: [Vec<u32>; 2],
    contents: [Option<Snapshot>; 2],
    presented: Option<Snapshot>,
    compositor: StrictCompositor,
    oracle: Vec<u32>,
    now: u64,
}

impl Fixture {
    fn new(seed: u64) -> Fixture {
        // Small enough to run quickly, large enough that an effect has room to
        // move glyphs around. Everything is in-repo: the bundled font and the
        // quotes above.
        let raster = Rasterizer::new(EMBEDDED_FONT, 13.0, 1.0).unwrap();
        let layout = Layout::fit(480, 320, raster.metrics());
        let pixels = (layout.width * layout.height) as usize;

        let mut settings = Settings::builtin();
        settings.content = Content::Quotes(parse_quotes(QUOTES, " — "));
        settings.measure.width = 34;

        let animator = Animator::new(
            Animation {
                cols: layout.cols,
                rows: layout.rows,
                frame_rate: settings.frame_rate,
                hold: std::time::Duration::from_millis(100),
                measure: settings.measure.clone(),
                content: settings.content.clone(),
                // Effects that churn glyphs are the ones that expose the
                // screen-versus-buffer distinction: a cell that changes and
                // changes back is absent from the buffer's own delta.
                effects: vec!["decrypt".into(), "binarypath".into(), "matrix".into()],
                default_fg: settings.foreground,
            },
            seed,
        )
        .unwrap();

        Fixture {
            raster,
            layout,
            animator,
            buffers: [vec![0; pixels], vec![0; pixels]],
            contents: [None, None],
            presented: None,
            compositor: StrictCompositor::new(layout.width as usize, layout.height as usize),
            oracle: vec![0; pixels],
            now: 0,
        }
    }

    fn next_grid(&mut self) -> Grid {
        let mut now = self.now;
        loop {
            match self.animator.advance(now).unwrap() {
                Advance::Frame => {
                    self.now = now + 33;
                    return self.animator.grid().clone();
                }
                Advance::Idle { until_ms } => now = until_ms.max(now + 1),
            }
        }
    }

    fn render_oracle(&mut self, grid: &Grid) {
        render_frame(
            &mut self.oracle,
            &self.layout,
            &mut self.raster,
            grid,
            History::unknown(),
            255,
            [0, 0, 0],
        );
    }
}

/// Run `frames` frames through the strict compositor and return the worst
/// number of stale pixels seen.
///
/// `honest` picks which record the damage report is computed against: the
/// screen (correct) or the buffer being drawn into (the bug).
fn run(seed: u64, frames: usize, honest: bool) -> usize {
    let mut f = Fixture::new(seed);
    let mut worst = 0;

    for frame in 0..frames {
        let grid = f.next_grid();
        // The compositor holds the buffer it was last given and releases the
        // other, so the free one alternates. That is the situation the whole
        // two-record scheme exists for.
        let slot = frame % 2;

        let screen_record = if honest { f.presented.clone() } else { f.contents[slot].clone() };
        let rendered = render_frame(
            &mut f.buffers[slot],
            &f.layout,
            &mut f.raster,
            &grid,
            History { buffer: f.contents[slot].as_ref(), presented: screen_record.as_ref() },
            255,
            [0, 0, 0],
        );

        let rects: Vec<_> = if rendered.full_surface {
            vec![(0, 0, f.layout.width as i32, f.layout.height as i32)]
        } else {
            rendered.rects.iter().map(|r| pixel_rect(&f.layout, *r)).collect()
        };
        f.compositor.commit(&f.buffers[slot], &rects);

        let snapshot = Snapshot { grid: grid.clone(), alpha: 255 };
        f.contents[slot] = Some(snapshot.clone());
        f.presented = Some(snapshot);

        f.render_oracle(&grid);
        worst = worst.max(f.compositor.stale_pixels(&f.oracle));
    }
    worst
}

#[test]
fn damage_against_the_screen_leaves_nothing_stale() {
    for seed in [1u64, 7, 12345] {
        assert_eq!(run(seed, 240, true), 0, "seed {seed} left stale pixels on screen");
    }
}

#[test]
fn damage_against_only_the_buffer_leaves_the_screen_wrong() {
    // The negative control. Without it the test above proves only that the
    // renderer works, not that reporting damage against the screen is what
    // makes it work.
    let stale: usize = [1u64, 7, 12345].iter().map(|seed| run(*seed, 240, false)).sum();
    assert!(
        stale > 0,
        "the buffer-only damage report should have left the screen stale; \
         if this passes, the test no longer distinguishes the two records"
    );
}
