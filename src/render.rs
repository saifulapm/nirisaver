//! Turning a new grid into pixels and a damage report.
//!
//! Two records, not one, and the distinction is the whole reason this module
//! exists.
//!
//! With more than one buffer in flight, the buffer we are about to draw into
//! holds the frame from *two* presentations ago while the compositor is
//! showing the one from the last. Those are different pictures, and they ask
//! different questions:
//!
//!   * **What must I repaint?** Whatever differs from what is in *this
//!     buffer*. Repainting more is wasted work.
//!   * **What must I report as damaged?** Whatever differs from what is *on
//!     the screen*. Reporting less leaves the screen wrong.
//!
//! They come apart whenever a cell changes and changes back — `X`, `Y`, `X`
//! over three frames. On the third frame the buffer still holds the original
//! `X`, so the buffer diff is empty; the screen holds `Y`, so the screen diff
//! is not. A client that damages only its buffer diff tells a compositor that
//! takes damage at its word that nothing moved, and the `Y` stays up. Effects
//! that churn glyphs hit this on essentially every frame.
//!
//! So: repaint the buffer diff, damage the union of both.
//!
//! The two are computed at different granularities on purpose. Repainting is
//! per cell, because a glyph clipped to its cell means a changed cell is
//! exactly the work a changed cell costs. Damage is per run of cells, with
//! small gaps coalesced, because every rectangle is a message the compositor
//! has to act on and forty of them to move a dozen glyphs is a worse trade
//! than one that is slightly too generous.

use crate::grid::{bounding_rect, coarsen_to_rows, merge_spans, CellRect, Grid, Rgb, RowSpan};
use crate::raster::{flood, Layout, Rasterizer};

/// What a particular set of pixels holds — the grid, and the fade level it was
/// drawn at. Alpha belongs here because it multiplies every pixel: a frame
/// with identical cells but a different alpha is a different picture, and
/// diffing the cells alone would call it unchanged.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Snapshot {
    pub grid: Grid,
    pub alpha: u8,
}

/// What is already drawn where, as this render found it.
///
/// Two records, because the buffer being drawn into and the screen being
/// looked at hold different frames — see the module header.
#[derive(Clone, Copy, Default)]
pub struct History<'a> {
    /// What these pixels currently hold.
    pub buffer: Option<&'a Snapshot>,
    /// What the compositor is currently showing.
    pub presented: Option<&'a Snapshot>,
}

impl<'a> History<'a> {
    /// Nothing known: a fresh buffer on a surface that has never presented.
    /// This is also what produces the full-frame oracle the benchmark and the
    /// damage test compare against.
    pub fn unknown() -> History<'a> {
        History::default()
    }
}

/// The result of a render: where to damage, and how much work it cost.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Rendered {
    pub rects: Vec<CellRect>,
    /// The screen holds something this render cannot describe in cell
    /// rectangles — a first frame, a fade step, a reconfigure — so the caller
    /// damages the whole surface, margins included.
    pub full_surface: bool,
    pub cells_drawn: usize,
}

/// Draw `next` into `pixels` and report the damage.
pub fn render_frame(
    pixels: &mut [u32],
    layout: &Layout,
    raster: &mut Rasterizer,
    next: &Grid,
    history: History<'_>,
    alpha: u8,
    background: Rgb,
) -> Rendered {
    debug_assert_eq!(next.cols(), layout.cols);
    debug_assert_eq!(next.rows(), layout.rows);

    let buffer_base = comparable(history.buffer, alpha, next);
    let screen_base = comparable(history.presented, alpha, next);

    let stride = layout.width as usize;
    if buffer_base.is_none() {
        // Nothing here is reusable, so the margin outside the grid needs
        // painting too. It cannot change afterwards, which is why it never
        // appears in a damage rectangle.
        flood(pixels, background, alpha);
    }

    // One pass over the grid decides both questions per cell. Two passes would
    // read the same forty thousand cells twice to learn things that fall out
    // of the same comparison.
    let mut cells_drawn = 0;
    let mut runs: Vec<RowSpan> = Vec::new();
    for row in 0..next.rows() {
        let current = next.row(row);
        let in_buffer = buffer_base.map(|g| g.row(row));
        let on_screen = screen_base.map(|g| g.row(row));
        let mut open: Option<usize> = None;
        let mut last = 0usize;

        for (col, &cell) in current.iter().enumerate() {
            let buffer_stale = in_buffer.is_none_or(|prev| prev[col] != cell);
            let screen_stale = on_screen.is_none_or(|prev| prev[col] != cell);

            if buffer_stale {
                // On a full repaint the surface is already flooded with the
                // background, so a cell that paints nothing beyond it can be
                // skipped — that is most of the screen, most of the time. The
                // pixels it would have written are identical either way, which
                // is what keeps this path byte-for-byte equal to the oracle.
                if in_buffer.is_some() || !cell.is_blank() {
                    raster.draw_cell(
                        pixels,
                        stride,
                        layout.cell_origin(col, row),
                        cell,
                        alpha,
                        background,
                    );
                    cells_drawn += 1;
                }
            }

            if !(buffer_stale || screen_stale) {
                continue;
            }
            match open {
                Some(_) if col - last <= DAMAGE_GAP + 1 => last = col,
                Some(start) => {
                    runs.push(RowSpan { row, start, end: last + 1 });
                    open = Some(col);
                    last = col;
                }
                None => {
                    open = Some(col);
                    last = col;
                }
            }
        }
        if let Some(start) = open {
            runs.push(RowSpan { row, start, end: last + 1 });
        }
    }

    Rendered { rects: bound_rects(&runs), full_surface: screen_base.is_none(), cells_drawn }
}

/// How many unchanged cells two changed runs may be separated by and still be
/// reported as one damage rectangle.
///
/// Zero would be exact and would also hand the compositor a rectangle per
/// glyph on effects that scatter them. Eight cells is a couple of hundred
/// pixels of slack in exchange for far fewer rectangles.
const DAMAGE_GAP: usize = 8;

/// The most damage rectangles worth sending.
///
/// Every rectangle is a protocol message the compositor has to act on, and at
/// thirty frames a second a few hundred of them per frame costs more than the
/// copying they save. Past this, the report gets coarser rather than longer —
/// which is always allowed: damage may overstate what changed, never
/// understate it.
const MAX_RECTS: usize = 32;

fn bound_rects(runs: &[RowSpan]) -> Vec<CellRect> {
    let rects = merge_spans(runs);
    if rects.len() <= MAX_RECTS {
        return rects;
    }
    let rects = merge_spans(&coarsen_to_rows(runs));
    if rects.len() <= MAX_RECTS {
        return rects;
    }
    bounding_rect(runs).into_iter().collect()
}

/// A record is only a usable baseline if it describes the same picture this
/// frame is being compared against: same geometry, same fade level.
fn comparable<'a>(snapshot: Option<&'a Snapshot>, alpha: u8, next: &Grid) -> Option<&'a Grid> {
    snapshot.filter(|s| s.alpha == alpha && s.grid.same_shape(next)).map(|s| &s.grid)
}

/// A cell rectangle in surface pixels.
pub fn pixel_rect(layout: &Layout, rect: CellRect) -> (i32, i32, i32, i32) {
    let (x, y) = layout.cell_origin(rect.col, rect.row);
    (
        x as i32,
        y as i32,
        (rect.cols as u32 * layout.cell.width) as i32,
        (rect.rows as u32 * layout.cell.height) as i32,
    )
}

/// Total cells covered by a run list — how much area was declared damaged, as
/// distinct from [`changed_cells`], which is how much actually moved.
pub fn run_cells(runs: &[RowSpan]) -> usize {
    runs.iter().map(RowSpan::len).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::Cell;

    const FONT: &[u8] = include_bytes!("../assets/JetBrainsMono-Regular.otf");

    struct Harness {
        raster: Rasterizer,
        layout: Layout,
        pixels: Vec<u32>,
    }

    impl Harness {
        fn new() -> Self {
            let raster = Rasterizer::new(FONT, 14.0, 1.0).unwrap();
            let layout = Layout::fit(320, 240, raster.metrics());
            let pixels = vec![0u32; (layout.width * layout.height) as usize];
            Harness { raster, layout, pixels }
        }

        fn full(&mut self, grid: &Grid, alpha: u8) -> Vec<u32> {
            let mut pixels = vec![0u32; self.pixels.len()];
            render_frame(
                &mut pixels,
                &self.layout,
                &mut self.raster,
                grid,
                History::unknown(),
                alpha,
                [0, 0, 0],
            );
            pixels
        }
    }

    fn lit(ch: char) -> Cell {
        Cell { ch, fg: [200, 220, 255], bg: None, bold: false }
    }

    #[test]
    fn an_incremental_render_matches_a_full_one() {
        let mut h = Harness::new();
        let mut grid = Grid::new(h.layout.cols, h.layout.rows);
        let first = h.full(&grid, 255);
        h.pixels.copy_from_slice(&first);
        let mut buffer = Some(Snapshot { grid: grid.clone(), alpha: 255 });

        for step in 0..8 {
            grid.set(step % grid.cols(), step % grid.rows(), lit('#'));
            grid.set((step + 3) % grid.cols(), (step + 1) % grid.rows(), lit('@'));
            let oracle = h.full(&grid, 255);
            render_frame(
                &mut h.pixels,
                &h.layout,
                &mut h.raster,
                &grid,
                History { buffer: buffer.as_ref(), presented: buffer.as_ref() },
                255,
                [0, 0, 0],
            );
            assert_eq!(h.pixels, oracle, "incremental diverged from the oracle at step {step}");
            buffer = Some(Snapshot { grid: grid.clone(), alpha: 255 });
        }
    }

    #[test]
    fn a_cell_that_changes_back_is_still_damaged() {
        // The three-frame X, Y, X sequence. The buffer being drawn into holds
        // the first X, so it needs no repaint; the screen holds Y, so it needs
        // the damage anyway.
        let mut h = Harness::new();
        let mut x = Grid::new(h.layout.cols, h.layout.rows);
        x.set(4, 2, lit('X'));
        let mut y = x.clone();
        y.set(4, 2, lit('Y'));

        let buffer = Snapshot { grid: x.clone(), alpha: 255 };
        let presented = Snapshot { grid: y, alpha: 255 };
        let out = render_frame(
            &mut h.pixels,
            &h.layout,
            &mut h.raster,
            &x,
            History { buffer: Some(&buffer), presented: Some(&presented) },
            255,
            [0, 0, 0],
        );
        assert_eq!(out.cells_drawn, 0, "the buffer already holds this frame");
        assert_eq!(out.rects, vec![CellRect { col: 4, row: 2, cols: 1, rows: 1 }]);
        assert!(!out.full_surface);
    }

    #[test]
    fn a_fade_step_repaints_and_damages_everything() {
        let mut h = Harness::new();
        let grid = Grid::new(h.layout.cols, h.layout.rows);
        let snap = Snapshot { grid: grid.clone(), alpha: 128 };
        let out = render_frame(
            &mut h.pixels,
            &h.layout,
            &mut h.raster,
            &grid,
            History { buffer: Some(&snap), presented: Some(&snap) },
            200,
            [0, 0, 0],
        );
        assert!(out.full_surface, "alpha moved, so the screen record does not apply");
    }

    #[test]
    fn an_unchanged_frame_costs_nothing() {
        let mut h = Harness::new();
        let mut grid = Grid::new(h.layout.cols, h.layout.rows);
        grid.set(1, 1, lit('o'));
        let snap = Snapshot { grid: grid.clone(), alpha: 255 };
        let out = render_frame(
            &mut h.pixels,
            &h.layout,
            &mut h.raster,
            &grid,
            History { buffer: Some(&snap), presented: Some(&snap) },
            255,
            [0, 0, 0],
        );
        assert_eq!(out, Rendered { rects: vec![], full_surface: false, cells_drawn: 0 });
    }

    #[test]
    fn the_margin_is_never_reported_as_damaged() {
        let mut h = Harness::new();
        let grid = Grid::new(h.layout.cols, h.layout.rows);
        let snap = Snapshot { grid: grid.clone(), alpha: 255 };
        let mut moved = grid.clone();
        moved.set(0, 0, lit('!'));
        let out = render_frame(
            &mut h.pixels,
            &h.layout,
            &mut h.raster,
            &moved,
            History { buffer: Some(&snap), presented: Some(&snap) },
            255,
            [0, 0, 0],
        );
        for rect in &out.rects {
            let (x, y, w, hgt) = pixel_rect(&h.layout, *rect);
            assert!(x >= h.layout.origin_x as i32);
            assert!(y >= h.layout.origin_y as i32);
            assert!(
                x + w <= (h.layout.origin_x + h.layout.cols as u32 * h.layout.cell.width) as i32
            );
            assert!(
                y + hgt <= (h.layout.origin_y + h.layout.rows as u32 * h.layout.cell.height) as i32
            );
        }
    }
}
