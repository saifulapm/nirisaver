//! The cell grid: what a frame *is*, once the escape codes are gone.
//!
//! Everything downstream of the animation engine speaks in cells, not in bytes
//! and not in pixels. That is what makes the interesting parts testable without
//! a compositor: a frame is a `Grid`, a change between frames is a set of
//! `RowSpan`s, and the rasterizer's only job is to turn spans into pixels.

/// 24-bit colour, the only kind the engine emits.
pub type Rgb = [u8; 3];

/// One character cell. `Copy` and `Eq` on purpose — comparing two frames is
/// the hot path of the whole program, and it wants to be a memcmp-shaped
/// loop over a flat array, not a walk over indirections.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub ch: char,
    pub fg: Rgb,
    /// `None` means "the surface background", which is not the same as black:
    /// the background is configurable and the fade multiplies it.
    pub bg: Option<Rgb>,
    pub bold: bool,
}

impl Cell {
    pub const DEFAULT_FG: Rgb = [255, 255, 255];

    pub const fn blank() -> Self {
        Cell { ch: ' ', fg: Self::DEFAULT_FG, bg: None, bold: false }
    }

    /// Whether this cell paints anything beyond the surface background. Blank
    /// cells are skipped on a full redraw, which is most of the screen for
    /// most frames.
    pub fn is_blank(&self) -> bool {
        self.bg.is_none() && (self.ch == ' ' || self.ch == '\0')
    }
}

impl Default for Cell {
    fn default() -> Self {
        Cell::blank()
    }
}

/// A fixed-size grid of cells, row-major from the top-left.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Grid {
    cols: usize,
    rows: usize,
    cells: Vec<Cell>,
}

impl Grid {
    pub fn new(cols: usize, rows: usize) -> Self {
        Grid { cols, rows, cells: vec![Cell::blank(); cols * rows] }
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    pub fn row(&self, row: usize) -> &[Cell] {
        &self.cells[row * self.cols..(row + 1) * self.cols]
    }

    pub fn get(&self, col: usize, row: usize) -> Cell {
        self.cells[row * self.cols + col]
    }

    pub fn set(&mut self, col: usize, row: usize, cell: Cell) {
        self.cells[row * self.cols + col] = cell;
    }

    /// Reset every cell to blank without reallocating. Resizes if the geometry
    /// moved, which only happens on a compositor reconfigure.
    pub fn reset(&mut self, cols: usize, rows: usize) {
        if self.cols != cols || self.rows != rows {
            self.cols = cols;
            self.rows = rows;
            self.cells.clear();
            self.cells.resize(cols * rows, Cell::blank());
        } else {
            self.cells.fill(Cell::blank());
        }
    }

    pub fn same_shape(&self, other: &Grid) -> bool {
        self.cols == other.cols && self.rows == other.rows
    }

    /// A cheap order-sensitive digest, for the benchmark and the headless
    /// summary. Not a hash anyone should rely on for anything else.
    pub fn checksum(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut mix = |v: u64| {
            h ^= v;
            h = h.wrapping_mul(0x0100_0000_01b3);
        };
        mix(self.cols as u64);
        mix(self.rows as u64);
        for cell in &self.cells {
            mix(cell.ch as u64);
            mix(u64::from(cell.fg[0]) << 16 | u64::from(cell.fg[1]) << 8 | u64::from(cell.fg[2]));
            match cell.bg {
                Some(bg) => {
                    mix(1 << 24 | u64::from(bg[0]) << 16 | u64::from(bg[1]) << 8 | u64::from(bg[2]))
                }
                None => mix(0),
            }
            mix(cell.bold as u64);
        }
        h
    }

    /// The grid as plain text, one row per line, trailing blanks trimmed.
    /// This is what `--dump-grid` prints.
    pub fn to_text(&self) -> String {
        let mut out = String::with_capacity(self.cells.len() + self.rows);
        for row in 0..self.rows {
            let line: String = self.row(row).iter().map(|c| c.ch).collect();
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }
}

/// A half-open run of changed cells within one row: `start..end`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RowSpan {
    pub row: usize,
    pub start: usize,
    pub end: usize,
}

impl RowSpan {
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.start >= self.end
    }
}

/// Runs of cells that differ from `prev`, one or more per row, coalescing runs
/// separated by fewer than `gap` unchanged cells.
///
/// This is the *damage* shape, not the repaint shape. Repainting walks cells
/// directly and touches only the ones that moved; damage is a promise to a
/// compositor, and a promise made in forty tiny rectangles costs more to keep
/// than one slightly generous one. The gap is where that trade is made.
///
/// A run per row rather than one bounding box for the whole frame: effects
/// that touch a few cells at the top and a few at the bottom would otherwise
/// damage everything in between, and the compositor would re-upload a screen's
/// worth of texture to move a dozen glyphs.
///
/// `prev` of `None` — a fresh buffer, or one whose fade alpha no longer
/// matches — means every row, in full.
pub fn changed_runs(next: &Grid, prev: Option<&Grid>, gap: usize) -> Vec<RowSpan> {
    runs_where(next, gap, |col, row| match prev {
        Some(prev) if next.same_shape(prev) => next.get(col, row) != prev.get(col, row),
        _ => true,
    })
}

/// Runs of cells that differ from *either* baseline.
///
/// One pass rather than two lists merged afterwards, because the two questions
/// have the same answer shape and only the predicate differs. See
/// [`crate::render`] for why there are two baselines at all.
pub fn damaged_runs(
    next: &Grid,
    buffer: Option<&Grid>,
    screen: Option<&Grid>,
    gap: usize,
) -> Vec<RowSpan> {
    let buffer = buffer.filter(|g| next.same_shape(g));
    let screen = screen.filter(|g| next.same_shape(g));
    runs_where(next, gap, |col, row| {
        let cell = next.get(col, row);
        buffer.is_none_or(|g| g.get(col, row) != cell)
            || screen.is_none_or(|g| g.get(col, row) != cell)
    })
}

fn runs_where(
    next: &Grid,
    gap: usize,
    mut changed: impl FnMut(usize, usize) -> bool,
) -> Vec<RowSpan> {
    let mut runs = Vec::new();
    for row in 0..next.rows() {
        let mut start = None;
        let mut last = 0usize;
        for col in 0..next.cols() {
            if !changed(col, row) {
                continue;
            }
            match start {
                Some(_) if col - last <= gap + 1 => last = col,
                Some(open) => {
                    runs.push(RowSpan { row, start: open, end: last + 1 });
                    start = Some(col);
                    last = col;
                }
                None => {
                    start = Some(col);
                    last = col;
                }
            }
        }
        if let Some(open) = start {
            runs.push(RowSpan { row, start: open, end: last + 1 });
        }
    }
    runs
}

/// How many cells actually differ. The honest measure of how much a frame
/// moved, as distinct from how much area had to be declared damaged.
pub fn changed_cells(next: &Grid, prev: Option<&Grid>) -> usize {
    let Some(prev) = prev.filter(|p| next.same_shape(p)) else {
        return next.cols() * next.rows();
    };
    next.cells().iter().zip(prev.cells()).filter(|(a, b)| a != b).count()
}

/// A rectangle in cell coordinates.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CellRect {
    pub col: usize,
    pub row: usize,
    pub cols: usize,
    pub rows: usize,
}

/// Collapse vertically adjacent rows that changed over the same columns into
/// one rectangle. Effects that sweep a column band change the same span on
/// every row, so this is usually the difference between forty damage
/// rectangles and one.
///
/// Only *identical* spans merge. Merging overlapping-but-different spans would
/// mean damaging cells neither row actually touched, and the whole point of
/// the exercise is not to.
pub fn merge_spans(spans: &[RowSpan]) -> Vec<CellRect> {
    let mut out: Vec<CellRect> = Vec::with_capacity(spans.len());
    for span in spans {
        if span.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some(last)
                if last.col == span.start
                    && last.cols == span.len()
                    && last.row + last.rows == span.row =>
            {
                last.rows += 1;
            }
            _ => out.push(CellRect { col: span.start, row: span.row, cols: span.len(), rows: 1 }),
        }
    }
    out
}

/// Collapse each row's runs into a single span covering all of them.
///
/// The first fallback when a frame's changes are scattered enough that
/// declaring them precisely would cost more messages than it saves copying.
pub fn coarsen_to_rows(runs: &[RowSpan]) -> Vec<RowSpan> {
    let mut out: Vec<RowSpan> = Vec::with_capacity(runs.len());
    for run in runs {
        match out.last_mut() {
            Some(last) if last.row == run.row => {
                last.start = last.start.min(run.start);
                last.end = last.end.max(run.end);
            }
            _ => out.push(*run),
        }
    }
    out
}

/// One rectangle covering every run. The last fallback.
pub fn bounding_rect(runs: &[RowSpan]) -> Option<CellRect> {
    let first = runs.first()?;
    let mut rect = CellRect { col: first.start, row: first.row, cols: first.len(), rows: 1 };
    let (mut left, mut right) = (first.start, first.end);
    for run in runs {
        left = left.min(run.start);
        right = right.max(run.end);
        rect.rows = run.row + 1 - rect.row;
    }
    rect.col = left;
    rect.cols = right - left;
    Some(rect)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(ch: char) -> Cell {
        Cell { ch, ..Cell::blank() }
    }

    #[test]
    fn a_fresh_buffer_is_entirely_changed() {
        let grid = Grid::new(4, 3);
        let runs = changed_runs(&grid, None, 0);
        assert_eq!(runs.len(), 3);
        assert!(runs.iter().all(|s| s.start == 0 && s.end == 4));
        assert_eq!(changed_cells(&grid, None), 12);
    }

    #[test]
    fn identical_grids_have_no_runs() {
        let grid = Grid::new(4, 3);
        assert!(changed_runs(&grid, Some(&grid.clone()), 0).is_empty());
        assert_eq!(changed_cells(&grid, Some(&grid.clone())), 0);
    }

    #[test]
    fn scattered_changes_stay_separate_runs_without_a_gap() {
        let prev = Grid::new(16, 1);
        let mut next = prev.clone();
        next.set(2, 0, lit('a'));
        next.set(12, 0, lit('b'));
        assert_eq!(
            changed_runs(&next, Some(&prev), 0),
            vec![RowSpan { row: 0, start: 2, end: 3 }, RowSpan { row: 0, start: 12, end: 13 }]
        );
        assert_eq!(changed_cells(&next, Some(&prev)), 2);
    }

    #[test]
    fn a_gap_coalesces_nearby_runs() {
        let prev = Grid::new(16, 1);
        let mut next = prev.clone();
        next.set(2, 0, lit('a'));
        next.set(5, 0, lit('b'));
        next.set(12, 0, lit('c'));
        // 2 and 5 are two cells apart, 5 and 12 are six.
        assert_eq!(
            changed_runs(&next, Some(&prev), 4),
            vec![RowSpan { row: 0, start: 2, end: 6 }, RowSpan { row: 0, start: 12, end: 13 }]
        );
    }

    #[test]
    fn a_reshaped_grid_is_entirely_changed() {
        let prev = Grid::new(4, 2);
        let next = Grid::new(5, 2);
        assert_eq!(changed_runs(&next, Some(&prev), 0).len(), 2);
        assert_eq!(changed_cells(&next, Some(&prev)), 10);
    }

    #[test]
    fn damage_covers_both_baselines() {
        // The cell that moved in the buffer is at 1; the one that is stale on
        // screen is at 9. Neither baseline alone names both.
        let base = Grid::new(12, 1);
        let mut next = base.clone();
        next.set(1, 0, lit('x'));
        let mut screen = base.clone();
        screen.set(9, 0, lit('y'));
        next.set(9, 0, Cell::blank());

        let runs = damaged_runs(&next, Some(&base), Some(&screen), 0);
        assert_eq!(
            runs,
            vec![RowSpan { row: 0, start: 1, end: 2 }, RowSpan { row: 0, start: 9, end: 10 }]
        );
    }

    #[test]
    fn identical_stacked_runs_merge_into_one_rectangle() {
        let spans: Vec<_> = (0..4).map(|row| RowSpan { row, start: 3, end: 7 }).collect();
        assert_eq!(merge_spans(&spans), vec![CellRect { col: 3, row: 0, cols: 4, rows: 4 }]);
    }

    #[test]
    fn differing_runs_stay_separate() {
        let spans =
            vec![RowSpan { row: 0, start: 3, end: 7 }, RowSpan { row: 1, start: 2, end: 7 }];
        assert_eq!(merge_spans(&spans).len(), 2);
    }

    #[test]
    fn a_gap_in_rows_breaks_the_merge() {
        let spans =
            vec![RowSpan { row: 0, start: 0, end: 2 }, RowSpan { row: 2, start: 0, end: 2 }];
        assert_eq!(merge_spans(&spans).len(), 2);
    }

    #[test]
    fn coarsening_gives_one_span_per_row() {
        let runs = vec![
            RowSpan { row: 0, start: 1, end: 2 },
            RowSpan { row: 0, start: 9, end: 11 },
            RowSpan { row: 3, start: 4, end: 5 },
        ];
        assert_eq!(
            coarsen_to_rows(&runs),
            vec![RowSpan { row: 0, start: 1, end: 11 }, RowSpan { row: 3, start: 4, end: 5 }]
        );
    }

    #[test]
    fn the_bounding_rectangle_covers_every_run() {
        let runs = vec![RowSpan { row: 2, start: 5, end: 6 }, RowSpan { row: 4, start: 1, end: 9 }];
        assert_eq!(bounding_rect(&runs), Some(CellRect { col: 1, row: 2, cols: 8, rows: 3 }));
        assert_eq!(bounding_rect(&[]), None);
    }

    #[test]
    fn the_checksum_notices_colour() {
        let mut a = Grid::new(2, 1);
        a.set(0, 0, lit('x'));
        let mut b = a.clone();
        assert_eq!(a.checksum(), b.checksum());
        b.set(0, 0, Cell { ch: 'x', fg: [1, 2, 3], ..Cell::blank() });
        assert_ne!(a.checksum(), b.checksum());
    }
}
