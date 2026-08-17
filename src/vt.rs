//! A parser for exactly the frames the animation engine emits, and nothing
//! else.
//!
//! ttfx hands back a whole frame per call: every row of the canvas, top row
//! first, newline-separated, with one symbol per cell and an SGR run around
//! any symbol that is not plain. There is no cursor addressing, no scrolling,
//! no erase, no alternate screen — so there is no terminal emulator here, and
//! deliberately so. A general emulator would bring a Unicode width table with
//! it, and a width table is wrong for this input: the engine's layout
//! invariant is one symbol per cell, including the box-drawing and block
//! symbols that several effects are built out of. Consuming one `char` per
//! cell is what keeps the parsed grid aligned with the canvas the engine
//! thinks it is drawing.
//!
//! What it does understand is the SGR subset the engine can emit: reset, bold,
//! italic/underline/blink/hidden/strike (parsed and dropped — the rasterizer
//! has one weight), reverse, the sixteen legacy colours, 8-bit indexed
//! colours, and 24-bit true colour.

use crate::grid::{Cell, Grid, Rgb};

/// SGR state carried across cells within a frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Pen {
    fg: Option<Rgb>,
    bg: Option<Rgb>,
    bold: bool,
    reverse: bool,
    hidden: bool,
}

impl Pen {
    fn reset() -> Self {
        Pen { fg: None, bg: None, bold: false, reverse: false, hidden: false }
    }

    /// The cell this pen would draw `ch` into, with `default_fg` standing in
    /// for an unset foreground.
    fn cell(&self, ch: char, default_fg: Rgb) -> Cell {
        let fg = self.fg.unwrap_or(default_fg);
        let (fg, bg) = if self.reverse {
            // Reverse video with no explicit background swaps against the
            // surface background, which the rasterizer supplies; representing
            // that as `Some(bg)` here would bake in a colour the caller owns.
            (self.bg.unwrap_or(Cell::DEFAULT_FG), Some(fg))
        } else {
            (fg, self.bg)
        };
        Cell { ch: if self.hidden { ' ' } else { ch }, fg, bg, bold: self.bold }
    }
}

/// Parses engine frames into a [`Grid`].
pub struct Parser {
    default_fg: Rgb,
}

impl Parser {
    pub fn new(default_fg: Rgb) -> Self {
        Parser { default_fg }
    }

    /// Overwrite `grid` with `frame`. Returns whether anything changed, so the
    /// caller can drop an identical frame before it costs a rasterization.
    ///
    /// Cells the frame does not mention are blanked: a frame is complete by
    /// construction, so anything left over is from the frame before it.
    pub fn parse_into(&self, frame: &str, grid: &mut Grid) -> bool {
        let mut changed = false;
        let cols = grid.cols();
        let rows = grid.rows();
        let mut pen = Pen::reset();
        let (mut col, mut row) = (0usize, 0usize);
        let mut chars = frame.chars().peekable();

        // Rows the frame never reached still have to be blanked, so track how
        // far down it got rather than assuming it filled the canvas.
        let blank_from = |grid: &mut Grid, changed: &mut bool, row: usize, from: usize| {
            if row >= rows {
                return;
            }
            for col in from..cols {
                if grid.get(col, row) != Cell::blank() {
                    grid.set(col, row, Cell::blank());
                    *changed = true;
                }
            }
        };

        while let Some(ch) = chars.next() {
            match ch {
                '\x1b' => {
                    // Only CSI ... m is meaningful here. Anything else is
                    // consumed to its final byte and ignored, so a stray
                    // sequence cannot desynchronise the column counter.
                    if chars.peek() == Some(&'[') {
                        chars.next();
                        let mut params = String::new();
                        let mut final_byte = None;
                        for ch in chars.by_ref() {
                            if ch.is_ascii_digit() || ch == ';' || ch == ':' || ch == '?' {
                                params.push(ch);
                            } else {
                                final_byte = Some(ch);
                                break;
                            }
                        }
                        if final_byte == Some('m') {
                            apply_sgr(&mut pen, &params);
                        }
                    } else {
                        // Two-character escape; drop its second byte.
                        chars.next();
                    }
                }
                '\n' => {
                    blank_from(grid, &mut changed, row, col);
                    row += 1;
                    col = 0;
                }
                '\r' => col = 0,
                _ => {
                    if row < rows && col < cols {
                        let cell = pen.cell(ch, self.default_fg);
                        if grid.get(col, row) != cell {
                            grid.set(col, row, cell);
                            changed = true;
                        }
                    }
                    col += 1;
                }
            }
        }

        blank_from(grid, &mut changed, row, col);
        for row in row + 1..rows {
            blank_from(grid, &mut changed, row, 0);
        }
        changed
    }
}

fn apply_sgr(pen: &mut Pen, params: &str) {
    // A bare `ESC[m` is `ESC[0m`.
    if params.is_empty() {
        *pen = Pen::reset();
        return;
    }
    let parts: Vec<&str> = params.split(';').collect();
    let mut i = 0;
    while i < parts.len() {
        let code: u32 = parts[i].parse().unwrap_or(0);
        i += 1;
        match code {
            0 => *pen = Pen::reset(),
            1 => pen.bold = true,
            // Italic, underline, blink and strike are recognised so their
            // codes cannot be mistaken for colours, then dropped: one font
            // weight, one face, no timers.
            2 | 3 | 4 | 5 | 6 | 9 => {}
            7 => pen.reverse = true,
            8 => pen.hidden = true,
            21 | 22 => pen.bold = false,
            23 | 24 | 25 | 29 => {}
            27 => pen.reverse = false,
            28 => pen.hidden = false,
            30..=37 => pen.fg = Some(ANSI_16[(code - 30) as usize]),
            38 => pen.fg = take_extended_color(&parts, &mut i),
            39 => pen.fg = None,
            40..=47 => pen.bg = Some(ANSI_16[(code - 40) as usize]),
            48 => pen.bg = take_extended_color(&parts, &mut i),
            49 => pen.bg = None,
            90..=97 => pen.fg = Some(ANSI_16[(code - 90 + 8) as usize]),
            100..=107 => pen.bg = Some(ANSI_16[(code - 100 + 8) as usize]),
            _ => {}
        }
    }
}

/// The `38;…` / `48;…` tail: `2;r;g;b` or `5;n`. Advances `i` past whatever it
/// consumed, so a malformed tail cannot make the outer loop reinterpret colour
/// channels as SGR codes.
fn take_extended_color(parts: &[&str], i: &mut usize) -> Option<Rgb> {
    let kind: u32 = parts.get(*i)?.parse().ok()?;
    *i += 1;
    match kind {
        2 => {
            let mut channel = || -> u8 {
                let v = parts.get(*i).and_then(|p| p.parse::<u8>().ok()).unwrap_or(0);
                *i += 1;
                v
            };
            Some([channel(), channel(), channel()])
        }
        5 => {
            let index: u8 = parts.get(*i)?.parse().ok()?;
            *i += 1;
            Some(xterm_256(index))
        }
        _ => None,
    }
}

/// The standard sixteen, in the palette most terminals ship.
const ANSI_16: [Rgb; 16] = [
    [0, 0, 0],
    [205, 0, 0],
    [0, 205, 0],
    [205, 205, 0],
    [0, 0, 238],
    [205, 0, 205],
    [0, 205, 205],
    [229, 229, 229],
    [127, 127, 127],
    [255, 0, 0],
    [0, 255, 0],
    [255, 255, 0],
    [92, 92, 255],
    [255, 0, 255],
    [0, 255, 255],
    [255, 255, 255],
];

/// xterm's 256-colour palette: sixteen system colours, a 6×6×6 cube, then a
/// 24-step grey ramp.
fn xterm_256(index: u8) -> Rgb {
    match index {
        0..=15 => ANSI_16[index as usize],
        16..=231 => {
            const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
            let i = index as usize - 16;
            [LEVELS[i / 36], LEVELS[(i / 6) % 6], LEVELS[i % 6]]
        }
        _ => {
            let v = 8 + (index as u16 - 232) * 10;
            let v = v as u8;
            [v, v, v]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(frame: &str, cols: usize, rows: usize) -> Grid {
        let mut grid = Grid::new(cols, rows);
        Parser::new(Cell::DEFAULT_FG).parse_into(frame, &mut grid);
        grid
    }

    #[test]
    fn plain_rows_land_top_first() {
        let grid = parse("ab\ncd", 2, 2);
        assert_eq!(grid.get(0, 0).ch, 'a');
        assert_eq!(grid.get(1, 0).ch, 'b');
        assert_eq!(grid.get(0, 1).ch, 'c');
        assert_eq!(grid.get(1, 1).ch, 'd');
    }

    #[test]
    fn true_colour_runs_colour_exactly_one_cell() {
        let grid = parse(" \x1b[38;2;12;34;56mX\x1b[0m ", 3, 1);
        assert_eq!(grid.get(0, 0), Cell::blank());
        assert_eq!(grid.get(1, 0), Cell { ch: 'X', fg: [12, 34, 56], bg: None, bold: false });
        assert_eq!(grid.get(2, 0), Cell::blank());
    }

    #[test]
    fn a_block_symbol_occupies_one_cell() {
        // The engine's layout invariant, and the reason there is no width
        // table: a wcwidth-2 symbol here would shift the whole row.
        let grid = parse("\x1b[38;2;255;255;255m▌\x1b[0mX", 2, 1);
        assert_eq!(grid.get(0, 0).ch, '▌');
        assert_eq!(grid.get(1, 0).ch, 'X');
    }

    #[test]
    fn bold_and_background_survive_the_round_trip() {
        let grid = parse("\x1b[1m\x1b[48;2;9;9;9mZ\x1b[0m", 1, 1);
        assert_eq!(
            grid.get(0, 0),
            Cell { ch: 'Z', fg: Cell::DEFAULT_FG, bg: Some([9, 9, 9]), bold: true }
        );
    }

    #[test]
    fn indexed_colour_resolves_through_the_cube() {
        let grid = parse("\x1b[38;5;196mR\x1b[0m", 1, 1);
        assert_eq!(grid.get(0, 0).fg, [255, 0, 0]);
        let grid = parse("\x1b[38;5;244mG\x1b[0m", 1, 1);
        assert_eq!(grid.get(0, 0).fg, [128, 128, 128]);
    }

    #[test]
    fn reverse_video_swaps_the_pair() {
        let grid = parse("\x1b[7m\x1b[38;2;1;2;3mV\x1b[0m", 1, 1);
        let cell = grid.get(0, 0);
        assert_eq!(cell.bg, Some([1, 2, 3]));
        assert_eq!(cell.fg, Cell::DEFAULT_FG);
    }

    #[test]
    fn a_short_frame_blanks_what_it_does_not_reach() {
        let mut grid = Grid::new(4, 2);
        let parser = Parser::new(Cell::DEFAULT_FG);
        parser.parse_into("abcd\nefgh", &mut grid);
        assert_eq!(grid.get(3, 1).ch, 'h');
        parser.parse_into("ab", &mut grid);
        assert_eq!(grid.get(2, 0), Cell::blank());
        assert_eq!(grid.get(0, 1), Cell::blank());
    }

    #[test]
    fn an_identical_frame_reports_no_change() {
        let mut grid = Grid::new(4, 1);
        let parser = Parser::new(Cell::DEFAULT_FG);
        assert!(parser.parse_into("ab", &mut grid));
        assert!(!parser.parse_into("ab", &mut grid));
    }

    #[test]
    fn overlong_rows_are_clipped_not_wrapped() {
        let grid = parse("abcdef\nZ", 3, 2);
        assert_eq!(grid.get(2, 0).ch, 'c');
        assert_eq!(grid.get(0, 1).ch, 'Z');
    }

    #[test]
    fn a_non_sgr_sequence_does_not_shift_the_row() {
        let grid = parse("a\x1b[2Jb", 2, 1);
        assert_eq!(grid.get(0, 0).ch, 'a');
        assert_eq!(grid.get(1, 0).ch, 'b');
    }

    #[test]
    fn a_malformed_extended_colour_does_not_leak_channels_into_codes() {
        // `38;2;1` runs out of channels; the missing ones read as 0 and the
        // `1` that follows must not turn into bold.
        let grid = parse("\x1b[38;2;1mQ\x1b[0m", 1, 1);
        assert!(!grid.get(0, 0).bold);
    }
}
