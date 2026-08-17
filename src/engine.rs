//! Driving ttfx directly, and the cycle it lives in.
//!
//! ttfx already owns effect selection, seeded randomness, canvas sizing,
//! anchoring and the per-frame simulation clock, and it hands back a complete
//! frame per call. Linking it as a crate and calling `build` then `next_frame`
//! keeps all of that; spawning it as a process and scraping a pty would
//! reinterpret a frame it had already finished computing, through a terminal
//! emulator that has to guess at what it meant.
//!
//! The clock is always ttfx's virtual one, never its real one. Two reasons:
//! the real clock makes `frame()` *sleep* off the remainder of the frame
//! interval, which in an event-driven client would block the Wayland queue;
//! and a simulation driven by frames rather than wall time replays identically
//! in the headless path and the benchmark, which is what makes either of them
//! worth running.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::CommandFactory;
use ttfx::engine::canvas::Anchor;
use ttfx::engine::ctx::{Clock, EngineCtx};
use ttfx::engine::effect::Effect;
use ttfx::utils::rng::Rng;

use crate::grid::{Grid, Rgb};
use crate::text::{Content, Measure};
use crate::vt::Parser;

/// Which effects a run may draw from.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct EffectChoice {
    /// Pin exactly one effect for every cycle.
    pub pinned: Option<String>,
    /// Draw only from these.
    pub include: Vec<String>,
    /// Draw from everything except these.
    pub exclude: Vec<String>,
}

/// Every effect ttfx exposes, in the order it lists them.
pub fn available_effects() -> Vec<String> {
    ttfx::cli::Cli::command().get_subcommands().map(|c| c.get_name().to_string()).collect()
}

/// Turn a choice into the pool a run draws from, rejecting names ttfx does not
/// know rather than silently animating something else.
pub fn resolve_effects(choice: &EffectChoice) -> Result<Vec<String>> {
    let all = available_effects();
    let check = |names: &[String], what: &str| -> Result<()> {
        for name in names {
            if !all.contains(name) {
                return Err(anyhow!(
                    "unknown effect {name:?} in {what}\navailable effects: {}",
                    all.join(", ")
                ));
            }
        }
        Ok(())
    };

    if let Some(pinned) = &choice.pinned {
        check(std::slice::from_ref(pinned), "--effect")?;
        return Ok(vec![pinned.clone()]);
    }
    check(&choice.include, "--include-effects")?;
    check(&choice.exclude, "--exclude-effects")?;

    let mut pool = if choice.include.is_empty() { all } else { choice.include.clone() };
    pool.retain(|name| !choice.exclude.contains(name));
    if pool.is_empty() {
        return Err(anyhow!("no effects left after filtering"));
    }
    Ok(pool)
}

/// Everything the animator needs, already resolved. No paths, no environment,
/// no defaults that depend on the machine.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Animation {
    pub cols: usize,
    pub rows: usize,
    pub frame_rate: u32,
    /// How long a finished frame stays up before the next cycle begins.
    pub hold: Duration,
    pub measure: Measure,
    pub content: Content,
    pub effects: Vec<String>,
    pub default_fg: Rgb,
}

/// What the caller should do next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Advance {
    /// The grid moved; present it.
    Frame,
    /// Nothing will change until this many milliseconds since the start.
    /// Until then there is no frame to draw, no parse to run and no commit to
    /// make — a hold has to be genuinely free, or a fourteen-second quote
    /// costs as much as fourteen seconds of animation.
    Idle { until_ms: u64 },
}

enum State {
    Animating,
    Holding { until_ms: u64 },
}

/// Runs effect after effect over the same grid.
pub struct Animator {
    settings: Animation,
    rng: Rng,
    parser: Parser,
    grid: Grid,
    interval_ms: u64,
    next_due_ms: u64,
    state: State,
    effect: Box<dyn Effect>,
    ctx: EngineCtx,
    effect_name: String,
    cycles: u64,
    frames: u64,
}

/// A hard cap on how many engine frames one `advance` may burn catching up.
/// A stall long enough to exceed this is better served by dropping the
/// backlog than by spending a whole event-loop turn simulating it.
const MAX_CATCHUP_FRAMES: u32 = 8;

impl Animator {
    pub fn new(settings: Animation, seed: u64) -> Result<Self> {
        let mut rng = Rng::seeded(seed);
        let parser = Parser::new(settings.default_fg);
        let grid = Grid::new(settings.cols, settings.rows);
        let interval_ms = (1000 / settings.frame_rate.max(1)).max(1) as u64;
        let (effect, ctx, effect_name) = build_cycle(&settings, &mut rng)?;
        Ok(Animator {
            settings,
            rng,
            parser,
            grid,
            interval_ms,
            next_due_ms: 0,
            state: State::Animating,
            effect,
            ctx,
            effect_name,
            cycles: 1,
            frames: 0,
        })
    }

    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    pub fn effect_name(&self) -> &str {
        &self.effect_name
    }

    pub fn cycles(&self) -> u64 {
        self.cycles
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Step the simulation up to `now_ms` and report whether there is
    /// something to present.
    pub fn advance(&mut self, now_ms: u64) -> Result<Advance> {
        // A cycle that produces nothing at all — an effect with an empty
        // canvas, a zero hold — must not be able to spin here. Two starts in
        // one call is already more than any real configuration needs.
        let mut starts = 0;
        loop {
            match self.state {
                State::Holding { until_ms } => {
                    if now_ms < until_ms {
                        return Ok(Advance::Idle { until_ms });
                    }
                    if starts >= 2 {
                        return Ok(Advance::Idle { until_ms: now_ms + self.interval_ms });
                    }
                    starts += 1;
                    let (effect, ctx, name) = build_cycle(&self.settings, &mut self.rng)?;
                    self.effect = effect;
                    self.ctx = ctx;
                    self.effect_name = name;
                    self.cycles += 1;
                    self.next_due_ms = now_ms;
                    self.state = State::Animating;
                }
                State::Animating => {
                    if now_ms < self.next_due_ms {
                        return Ok(Advance::Idle { until_ms: self.next_due_ms });
                    }
                    let mut changed = false;
                    let mut pulled = 0;
                    let finished = loop {
                        let Some(frame) = self.effect.next_frame(&mut self.ctx) else {
                            break true;
                        };
                        self.frames += 1;
                        // Only the newest frame of a catch-up burst is ever
                        // presented, but each one still has to be parsed:
                        // effects build the next frame from the state the last
                        // one left behind.
                        changed |= self.parser.parse_into(&frame, &mut self.grid);
                        self.next_due_ms += self.interval_ms;
                        pulled += 1;
                        if now_ms < self.next_due_ms || pulled >= MAX_CATCHUP_FRAMES {
                            break false;
                        }
                    };
                    if finished {
                        let hold = self.settings.hold.as_millis() as u64;
                        self.state = State::Holding { until_ms: now_ms.saturating_add(hold) };
                    }
                    if changed {
                        return Ok(Advance::Frame);
                    }
                    if !finished {
                        // The engine ran but nothing on screen moved. Wait for
                        // the next frame rather than presenting a duplicate.
                        return Ok(Advance::Idle { until_ms: self.next_due_ms });
                    }
                }
            }
        }
    }
}

fn build_cycle(
    settings: &Animation,
    rng: &mut Rng,
) -> Result<(Box<dyn Effect>, EngineCtx, String)> {
    let block = settings.content.block(rng, &settings.measure);
    let name = settings.effects[rng.choice_index(settings.effects.len())].clone();

    // Effect configuration comes from ttfx's own defaults, reached by parsing
    // the bare subcommand. Reimplementing the defaults here would mean
    // re-deriving them on every ttfx bump.
    let cli: ttfx::cli::Cli = clap::Parser::try_parse_from(["ttfx", name.as_str()])
        .with_context(|| format!("building effect {name:?}"))?;
    let mut config = cli.terminal_config();
    let effect_command = cli.effect.ok_or_else(|| anyhow!("effect {name:?} has no subcommand"))?;
    // The canvas is the surface, not a terminal. Saying so explicitly keeps
    // ttfx from consulting COLUMNS/LINES or an ioctl that has no meaning here.
    config.ignore_terminal_dimensions = true;
    config.canvas_width = settings.cols as i64;
    config.canvas_height = settings.rows as i64;
    config.anchor_canvas = Anchor::C;
    config.anchor_text = Anchor::C;
    config.frame_rate = settings.frame_rate as i64;

    // The engine gets its own stream, drawn from ours, so effect randomness
    // and content selection stay reproducible together without sharing state.
    let engine_rng = Rng::seeded(rng.randrange(0, i64::MAX) as u64);
    let clock = Clock::virtual_with_frame_rate(config.frame_rate);
    let mut ctx = EngineCtx::new(&block, config, engine_rng, clock)
        .map_err(|e| anyhow!("{e}"))
        .with_context(|| format!("preparing effect {name:?}"))?;
    let mut effect = effect_command.build_effect();
    effect
        .build(&mut ctx)
        .map_err(|e| anyhow!("{e}"))
        .with_context(|| format!("effect {name:?}"))?;
    Ok((effect, ctx, name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::Align;

    fn animation(effects: Vec<String>, hold_ms: u64) -> Animation {
        Animation {
            cols: 40,
            rows: 12,
            frame_rate: 30,
            hold: Duration::from_millis(hold_ms),
            measure: Measure {
                width: 30,
                align: Align::Center,
                attribution_prefix: "— ".to_string(),
            },
            content: Content::Text("hello there".to_string()),
            effects,
            default_fg: [255, 255, 255],
        }
    }

    #[test]
    fn every_advertised_effect_builds_and_runs() {
        // ttfx effect names are a published part of this program's interface —
        // they go in --help and the README — so a bump that renames or drops
        // one must not be discovered on screen.
        for name in available_effects() {
            let mut animator = Animator::new(animation(vec![name.clone()], 0), 4)
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            let mut now = 0;
            for _ in 0..40 {
                match animator.advance(now).unwrap_or_else(|e| panic!("{name}: {e}")) {
                    Advance::Frame => now += 33,
                    Advance::Idle { until_ms } => now = until_ms.max(now + 1),
                }
            }
            assert!(animator.frames() > 0, "{name} produced no frames");
        }
    }

    #[test]
    fn an_effect_pool_is_validated_against_ttfx() {
        assert!(resolve_effects(&EffectChoice {
            pinned: Some("matrix".into()),
            ..Default::default()
        })
        .is_ok());
        let err = resolve_effects(&EffectChoice {
            pinned: Some("nonesuch".into()),
            ..Default::default()
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("nonesuch") && err.contains("matrix"), "{err}");
    }

    #[test]
    fn exclusion_leaves_everything_else() {
        let pool = resolve_effects(&EffectChoice {
            exclude: vec!["matrix".into(), "beams".into()],
            ..Default::default()
        })
        .unwrap();
        assert_eq!(pool.len(), available_effects().len() - 2);
        assert!(!pool.contains(&"matrix".to_string()));
    }

    #[test]
    fn excluding_everything_is_an_error_not_a_blank_screen() {
        assert!(resolve_effects(&EffectChoice {
            exclude: available_effects(),
            ..Default::default()
        })
        .is_err());
    }

    #[test]
    fn a_hold_asks_for_no_frames_at_all() {
        let mut animator = Animator::new(animation(vec!["print".into()], 5_000), 1).unwrap();
        let mut now = 0u64;
        // Run the effect out.
        let mut holding_at = None;
        for _ in 0..4_000 {
            match animator.advance(now).unwrap() {
                Advance::Frame => now += 33,
                Advance::Idle { until_ms } if until_ms > now + 1_000 => {
                    holding_at = Some((now, until_ms));
                    break;
                }
                Advance::Idle { until_ms } => now = until_ms.max(now + 1),
            }
        }
        let (now, until) = holding_at.expect("the effect never finished");
        assert!(until - now > 4_000, "the hold should run the configured five seconds");
        let frames_before = animator.frames();
        // Every wakeup inside the hold must be told to go back to sleep, and
        // must not have run the engine to find that out.
        for t in (now..until).step_by(250) {
            assert_eq!(animator.advance(t).unwrap(), Advance::Idle { until_ms: until });
        }
        assert_eq!(animator.frames(), frames_before, "the hold burned engine frames");
    }

    #[test]
    fn a_new_cycle_starts_when_the_hold_expires() {
        let mut animator = Animator::new(animation(vec!["print".into()], 100), 2).unwrap();
        let mut now = 0u64;
        for _ in 0..8_000 {
            if animator.cycles() > 1 {
                break;
            }
            match animator.advance(now).unwrap() {
                Advance::Frame => now += 33,
                Advance::Idle { until_ms } => now = until_ms.max(now + 1),
            }
        }
        assert!(animator.cycles() > 1, "the second cycle never began");
    }

    #[test]
    fn the_same_seed_replays_the_same_animation() {
        let run = |seed: u64| {
            let mut animator = Animator::new(animation(available_effects(), 10), seed).unwrap();
            let mut now = 0u64;
            let mut sums = Vec::new();
            for _ in 0..300 {
                match animator.advance(now).unwrap() {
                    Advance::Frame => {
                        sums.push(animator.grid().checksum());
                        now += 33;
                    }
                    Advance::Idle { until_ms } => now = until_ms.max(now + 1),
                }
            }
            (sums, animator.effect_name().to_string())
        };
        assert_eq!(run(99), run(99));
        assert_ne!(run(99).0, run(100).0);
    }
}
