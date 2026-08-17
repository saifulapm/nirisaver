//! Rendering with no compositor at all.
//!
//! The layout and content logic — wrapping, attribution, effect selection,
//! quote rotation, how much of the grid a frame actually moves — has nothing
//! to do with Wayland, and none of it should need a running compositor to be
//! checked. This path drives the same animator the overlay does against a
//! virtual clock and prints what it saw, which is how CI gets an opinion about
//! any of it.
//!
//! Given a seed, the output is a fixed string. That is the point.

use anyhow::Result;

use crate::config::Settings;
use crate::engine::{Advance, Animation, Animator};
use crate::grid::changed_cells;

/// What a headless run saw.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Report {
    pub presented: u64,
    pub engine_frames: u64,
    pub cycles: u64,
    pub cells_changed: usize,
    pub checksum: u64,
    pub last_effect: String,
    pub cols: usize,
    pub rows: usize,
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "grid={}x{} presented={} engine-frames={} cycles={} cells-changed={} \
             last-effect={} checksum={:016x}",
            self.cols,
            self.rows,
            self.presented,
            self.engine_frames,
            self.cycles,
            self.cells_changed,
            self.last_effect,
            self.checksum
        )
    }
}

/// Render `frames` presentable frames and report. `dump` also returns the
/// final grid as text.
pub fn run(settings: &Settings, cols: usize, rows: usize, frames: u64) -> Result<(Report, String)> {
    let animation = Animation {
        cols,
        rows,
        frame_rate: settings.frame_rate,
        hold: settings.hold,
        measure: settings.measure.clone(),
        content: settings.content.clone(),
        effects: settings.effects.clone(),
        default_fg: settings.foreground,
    };
    let mut animator = Animator::new(animation, settings.seed)?;

    let interval = (1000 / settings.frame_rate.max(1)).max(1) as u64;
    let mut previous = animator.grid().clone();
    let mut now = 0u64;
    let mut presented = 0u64;
    let mut cells_changed = 0usize;

    while presented < frames {
        match animator.advance(now)? {
            Advance::Frame => {
                cells_changed += changed_cells(animator.grid(), Some(&previous));
                previous.clone_from(animator.grid());
                presented += 1;
                now += interval;
            }
            // Jumping straight to the wakeup the animator asked for is what
            // makes a fourteen-second hold cost one loop iteration here, the
            // same as it costs one timer on the overlay.
            Advance::Idle { until_ms } => now = until_ms.max(now + 1),
        }
    }

    let report = Report {
        presented,
        engine_frames: animator.frames(),
        cycles: animator.cycles(),
        cells_changed,
        checksum: animator.grid().checksum(),
        last_effect: animator.effect_name().to_string(),
        cols,
        rows,
    };
    Ok((report, animator.grid().to_text()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::Content;

    fn settings(seed: u64) -> Settings {
        let mut s = Settings::builtin();
        s.seed = seed;
        s.hold = std::time::Duration::from_millis(200);
        s.content = Content::Quotes(crate::text::parse_quotes(
            "First light. — A\nSecond thought. — B\nThird rail. — C\n",
            " — ",
        ));
        s
    }

    #[test]
    fn a_seed_pins_the_whole_run() {
        let a = run(&settings(2024), 80, 24, 120).unwrap();
        let b = run(&settings(2024), 80, 24, 120).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn different_seeds_render_differently() {
        let a = run(&settings(1), 80, 24, 120).unwrap().0;
        let b = run(&settings(2), 80, 24, 120).unwrap().0;
        assert_ne!(a.checksum, b.checksum);
    }

    #[test]
    fn the_requested_number_of_frames_is_what_comes_back() {
        let (report, _) = run(&settings(5), 60, 20, 37).unwrap();
        assert_eq!(report.presented, 37);
        assert_eq!((report.cols, report.rows), (60, 20));
    }

    #[test]
    fn a_long_run_rotates_through_cycles() {
        let (report, _) = run(&settings(7), 60, 20, 2000).unwrap();
        assert!(report.cycles > 1, "expected more than one cycle: {report}");
    }

    #[test]
    fn the_dump_is_the_grid_the_summary_checksummed() {
        let (report, dump) = run(&settings(11), 40, 10, 90).unwrap();
        assert_eq!(dump.lines().count(), 10);
        assert!(dump.lines().all(|l| l.chars().count() <= 40));
        // A finished cycle has visible text in it; a checksum of an empty grid
        // would pass the equality tests above without proving anything.
        let (again, _) = run(&settings(11), 40, 10, 90).unwrap();
        assert_eq!(report.checksum, again.checksum);
    }
}
