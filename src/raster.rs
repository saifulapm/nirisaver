//! Software glyph rasterization, one cell at a time.
//!
//! Every glyph is clipped to its own cell. That is not a cosmetic choice: it
//! is what makes an incremental redraw byte-for-byte identical to a full
//! redraw. If a glyph could bleed into a neighbour, repainting only the cells
//! that changed would leave the neighbour's leftovers behind, and the whole
//! damage-tracking scheme downstream would be quietly wrong. The benchmark
//! asserts that equality on every frame, so a regression here fails loudly.

use std::collections::HashMap;

use fontdue::{Font, FontSettings};

use crate::grid::{Cell, Rgb};

/// The pixel geometry of one cell, derived from the font's own metrics.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CellMetrics {
    pub width: u32,
    pub height: u32,
    /// Distance from the top of the cell down to the baseline.
    pub baseline: i32,
}

/// Where the character grid sits inside a surface.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Layout {
    pub width: u32,
    pub height: u32,
    pub cols: usize,
    pub rows: usize,
    pub cell: CellMetrics,
    pub origin_x: u32,
    pub origin_y: u32,
}

impl Layout {
    /// The largest whole grid that fits, centred. The leftover margin is
    /// painted once with the background and never touched again — it cannot
    /// change, so it must never appear in a damage rectangle.
    pub fn fit(width: u32, height: u32, cell: CellMetrics) -> Layout {
        let cols = (width / cell.width.max(1)) as usize;
        let rows = (height / cell.height.max(1)) as usize;
        Layout {
            width,
            height,
            cols,
            rows,
            cell,
            origin_x: (width - cols as u32 * cell.width) / 2,
            origin_y: (height - rows as u32 * cell.height) / 2,
        }
    }

    /// A grid of a given size, centred. Outputs of different sizes show the
    /// *same* grid, so the animation is one picture across the desk rather
    /// than a separate one per screen; only the margin around it differs.
    pub fn centred(width: u32, height: u32, cols: usize, rows: usize, cell: CellMetrics) -> Layout {
        let used_w = (cols as u32 * cell.width).min(width);
        let used_h = (rows as u32 * cell.height).min(height);
        Layout {
            width,
            height,
            cols,
            rows,
            cell,
            origin_x: (width - used_w) / 2,
            origin_y: (height - used_h) / 2,
        }
    }

    pub fn cell_origin(&self, col: usize, row: usize) -> (u32, u32) {
        (
            self.origin_x + col as u32 * self.cell.width,
            self.origin_y + row as u32 * self.cell.height,
        )
    }
}

#[derive(Clone)]
struct Glyph {
    width: usize,
    height: usize,
    /// Offset of the bitmap's top-left from the cell's top-left.
    left: i32,
    top: i32,
    coverage: Vec<u8>,
}

/// Rasterizes cells into a premultiplied ARGB8888 surface.
pub struct Rasterizer {
    font: Font,
    size_px: f32,
    metrics: CellMetrics,
    cache: HashMap<(char, bool), Glyph>,
}

impl Rasterizer {
    /// `line_height` scales the font's own line height; 1.0 keeps the design
    /// value, which is what makes the block-drawing symbols several effects
    /// are built from tile without seams.
    pub fn new(font_bytes: &[u8], size_px: f32, line_height: f32) -> Result<Self, String> {
        let font = Font::from_bytes(font_bytes, FontSettings::default())?;
        let advance = font.metrics('M', size_px).advance_width;
        let line = font
            .horizontal_line_metrics(size_px)
            .ok_or_else(|| "font has no horizontal line metrics".to_string())?;
        let natural = line.ascent - line.descent + line.line_gap;
        let height = (natural * line_height).round().max(1.0);
        // Keep the baseline where the extra leading puts it: split the growth
        // above and below so a taller line height does not push the text off
        // the bottom of its cell.
        let baseline = (line.ascent + (height - natural) / 2.0).round();
        let metrics = CellMetrics {
            width: advance.round().max(1.0) as u32,
            height: height as u32,
            baseline: baseline as i32,
        };
        Ok(Rasterizer { font, size_px, metrics, cache: HashMap::new() })
    }

    pub fn metrics(&self) -> CellMetrics {
        self.metrics
    }

    /// Pre-rasterize a set of characters so the first frame does not pay for
    /// every glyph it touches. Purely a latency smoother; correctness does not
    /// depend on it.
    pub fn warm(&mut self, chars: impl IntoIterator<Item = char>) {
        for ch in chars {
            self.glyph(ch, false);
            self.glyph(ch, true);
        }
    }

    fn glyph(&mut self, ch: char, bold: bool) -> &Glyph {
        self.cache.entry((ch, bold)).or_insert_with(|| {
            let (m, coverage) = self.font.rasterize(ch, self.size_px);
            let mut glyph = Glyph {
                width: m.width,
                height: m.height,
                left: m.xmin,
                // fontdue reports ymin as the baseline-relative bottom edge,
                // positive upwards; the bitmap's first row is its top.
                top: -(m.ymin + m.height as i32),
                coverage,
            };
            if bold && glyph.width > 0 {
                // No bold face is loaded, so emboldening is a one-pixel smear:
                // deterministic, cheap, and enough to read as heavier.
                let mut smeared = glyph.coverage.clone();
                for row in 0..glyph.height {
                    for col in 1..glyph.width {
                        let i = row * glyph.width + col;
                        smeared[i] = smeared[i].max(glyph.coverage[i - 1]);
                    }
                }
                glyph.coverage = smeared;
            }
            glyph
        })
    }

    /// Paint one cell. `origin` is the cell's top-left pixel, `alpha` the fade
    /// level, `surface_bg` the colour a cell with no background of its own
    /// sits on.
    ///
    /// Every pixel in the cell is written exactly once. Filling the cell and
    /// then blitting the glyph over it would be simpler to read and would
    /// write the glyph's own footprint twice, which at a few hundred cells a
    /// frame is a third of the memory traffic of the whole redraw.
    pub fn draw_cell(
        &mut self,
        pixels: &mut [u32],
        stride: usize,
        origin: (u32, u32),
        cell: Cell,
        alpha: u8,
        surface_bg: Rgb,
    ) {
        let (x0, y0) = (origin.0 as usize, origin.1 as usize);
        let (cw, ch) = (self.metrics.width as usize, self.metrics.height as usize);
        let bg = cell.bg.unwrap_or(surface_bg);
        let background = premultiplied(bg, alpha);

        if cell.ch == ' ' || cell.ch == '\0' {
            for y in y0..y0 + ch {
                pixels[y * stride + x0..y * stride + x0 + cw].fill(background);
            }
            return;
        }

        let baseline = self.metrics.baseline;
        let glyph = self.glyph(cell.ch, cell.bold);
        let (gw, gh) = (glyph.width, glyph.height);
        // The glyph's footprint, clipped to the cell. Clipping is what makes an
        // incremental redraw byte-for-byte identical to a full one: a glyph
        // that could bleed into a neighbour would leave that neighbour's
        // leftovers behind when only the changed cell is repainted.
        let gx = glyph.left;
        let gy = baseline + glyph.top;
        let left = gx.clamp(0, cw as i32) as usize;
        let right = (gx + gw as i32).clamp(0, cw as i32) as usize;
        let top = gy.clamp(0, ch as i32) as usize;
        let bottom = (gy + gh as i32).clamp(0, ch as i32) as usize;

        for y in 0..ch {
            let row = (y0 + y) * stride + x0;
            if gw == 0 || y < top || y >= bottom || left >= right {
                pixels[row..row + cw].fill(background);
                continue;
            }
            pixels[row..row + left].fill(background);
            let coverage_row = (y as i32 - gy) as usize * gw;
            for x in left..right {
                let cov = glyph.coverage[coverage_row + (x as i32 - gx) as usize];
                pixels[row + x] = if cov == 0 {
                    background
                } else {
                    premultiplied(
                        [
                            blend(bg[0], cell.fg[0], cov),
                            blend(bg[1], cell.fg[1], cov),
                            blend(bg[2], cell.fg[2], cov),
                        ],
                        alpha,
                    )
                };
            }
            pixels[row + right..row + cw].fill(background);
        }
    }
}

/// `from` towards `to` by `cov/255`, rounded half-up. Integer throughout: a
/// float here would make "byte-for-byte identical to the oracle" depend on the
/// order the compiler happened to contract the multiply-adds in.
#[inline]
fn blend(from: u8, to: u8, cov: u8) -> u8 {
    let from = u32::from(from);
    let to = u32::from(to);
    let cov = u32::from(cov);
    ((from * (255 - cov) + to * cov + 127) / 255) as u8
}

/// ARGB8888 as Wayland wants it: one native-endian `u32` per pixel, colour
/// channels already multiplied by alpha.
#[inline]
pub fn premultiplied(color: Rgb, alpha: u8) -> u32 {
    let a = u32::from(alpha);
    let scale = |c: u8| (u32::from(c) * a + 127) / 255;
    (a << 24) | (scale(color[0]) << 16) | (scale(color[1]) << 8) | scale(color[2])
}

/// Flood a whole surface with one premultiplied colour. Used when a buffer
/// holds nothing reusable and the margin around the grid needs painting too.
pub fn flood(pixels: &mut [u32], color: Rgb, alpha: u8) {
    pixels.fill(premultiplied(color, alpha));
}

#[cfg(test)]
mod tests {
    use super::*;

    const FONT: &[u8] = include_bytes!("../assets/JetBrainsMono-Regular.otf");

    fn rasterizer() -> Rasterizer {
        Rasterizer::new(FONT, 32.0, 1.0).unwrap()
    }

    #[test]
    fn cell_metrics_are_positive_and_taller_than_wide() {
        let m = rasterizer().metrics();
        assert!(m.width > 0 && m.height > 0);
        assert!(m.height > m.width, "a monospace cell is taller than it is wide: {m:?}");
        assert!(m.baseline > 0 && m.baseline < m.height as i32);
    }

    #[test]
    fn the_full_block_fills_its_cell() {
        // Several effects draw with block-drawing symbols. If the cell height
        // did not follow the font's line height they would tile with seams,
        // which is visible as a grid of dark lines across a solid fill.
        let mut r = rasterizer();
        let m = r.metrics();
        let (w, h) = (m.width as usize, m.height as usize);
        let mut pixels = vec![0u32; w * h];
        r.draw_cell(
            &mut pixels,
            w,
            (0, 0),
            Cell { ch: '█', fg: [255, 255, 255], bg: None, bold: false },
            255,
            [0, 0, 0],
        );
        let lit = pixels.iter().filter(|p| **p & 0x00ff_ffff != 0).count();
        let ratio = lit as f32 / (w * h) as f32;
        assert!(ratio > 0.97, "full block covered only {:.1}% of its cell", ratio * 100.0);
    }

    #[test]
    fn a_glyph_never_escapes_its_cell() {
        // The guarantee the incremental redraw is built on.
        let mut r = rasterizer();
        let m = r.metrics();
        let (w, h) = (m.width as usize, m.height as usize);
        let stride = w * 3;
        let mut pixels = vec![0u32; stride * h * 3];
        for ch in ['W', '█', '@', 'g', '│', 'M'] {
            pixels.fill(0);
            r.draw_cell(
                &mut pixels,
                stride,
                (w as u32, h as u32),
                Cell { ch, fg: [255, 255, 255], bg: None, bold: true },
                255,
                [0, 0, 0],
            );
            for y in 0..h * 3 {
                for x in 0..stride {
                    let inside = (w..w * 2).contains(&x) && (h..h * 2).contains(&y);
                    if !inside {
                        assert_eq!(pixels[y * stride + x], 0, "{ch:?} escaped its cell at {x},{y}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_blank_cell_is_exactly_the_background() {
        let mut r = rasterizer();
        let m = r.metrics();
        let (w, h) = (m.width as usize, m.height as usize);
        let mut pixels = vec![0xdead_beef_u32; w * h];
        r.draw_cell(&mut pixels, w, (0, 0), Cell::blank(), 255, [17, 34, 51]);
        assert!(pixels.iter().all(|p| *p == premultiplied([17, 34, 51], 255)));
    }

    #[test]
    fn premultiplication_scales_colour_with_alpha() {
        assert_eq!(premultiplied([255, 255, 255], 255), 0xffff_ffff);
        assert_eq!(premultiplied([255, 255, 255], 0), 0x0000_0000);
        assert_eq!(premultiplied([200, 100, 50], 128), 0x8064_3219);
    }

    #[test]
    fn blending_hits_both_endpoints_exactly() {
        assert_eq!(blend(10, 200, 0), 10);
        assert_eq!(blend(10, 200, 255), 200);
    }
}
