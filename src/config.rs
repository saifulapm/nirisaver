//! Configuration, resolved exactly once.
//!
//! This is the only module that reads a file or looks at an environment
//! variable. Everything else is handed a [`Settings`] and has no way to ask a
//! question the machine could answer differently tomorrow.
//!
//! That is a deliberate constraint, not tidiness. A constructor that reaches
//! for `$XDG_CONFIG_HOME` on its way to a default makes `Default` mean
//! something different on every machine — and the first thing that quietly
//! stops meaning anything is the benchmark, which is supposed to render the
//! same frames for everyone and suddenly renders whatever happens to be in the
//! developer's quote list. [`Settings::builtin`] exists for exactly that
//! reason: a complete, useful configuration that touches nothing.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

use crate::cli::Cli;
use crate::engine::{resolve_effects, EffectChoice};
use crate::grid::Rgb;
use crate::text::{parse_quotes, Align, Content, Measure};

/// The font that ships with the program. Bundling one is what lets the
/// benchmark and the tests render without reading anything outside the repo.
pub const EMBEDDED_FONT: &[u8] = include_bytes!("../assets/JetBrainsMono-Regular.otf");

pub const DEFAULT_SEPARATOR: &str = " — ";
pub const DEFAULT_ATTRIBUTION_PREFIX: &str = "— ";
pub const DEFAULT_WRAP: usize = 64;
pub const DEFAULT_HOLD_MS: u64 = 14_000;
pub const DEFAULT_FADE_IN_MS: u64 = 700;
pub const DEFAULT_FADE_OUT_MS: u64 = 250;
pub const DEFAULT_FRAME_RATE: u32 = 30;
pub const DEFAULT_FONT_SIZE: f32 = 18.0;
pub const DEFAULT_LINE_HEIGHT: f32 = 1.0;
pub const DEFAULT_BACKGROUND: Rgb = [0, 0, 0];
pub const DEFAULT_FOREGROUND: Rgb = [235, 235, 235];

/// What a run with no quote list and no text of its own animates.
pub const BUILTIN_TEXT: &str = "nirisaver";

/// The pieces of the environment this program cares about, captured once so
/// resolution is a pure function of its arguments and tests can hand it a
/// directory of their own.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Env {
    pub config_home: Option<PathBuf>,
    pub home: Option<PathBuf>,
    pub runtime_dir: Option<PathBuf>,
}

impl Env {
    pub fn from_process() -> Env {
        let var = |name: &str| {
            std::env::var_os(name).map(PathBuf::from).filter(|p| !p.as_os_str().is_empty())
        };
        Env {
            config_home: var("XDG_CONFIG_HOME"),
            home: var("HOME"),
            runtime_dir: var("XDG_RUNTIME_DIR"),
        }
    }

    /// `~/.config/nirisaver`, honouring `XDG_CONFIG_HOME`.
    pub fn config_dir(&self) -> Option<PathBuf> {
        let base = self
            .config_home
            .clone()
            .or_else(|| self.home.as_ref().map(|home| home.join(".config")))?;
        Some(base.join("nirisaver"))
    }
}

/// The config file, as written. Every key optional; unknown keys rejected, so
/// a typo is a message rather than a setting that silently does nothing.
#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct FileConfig {
    pub source: Option<String>,
    pub quotes: Option<PathBuf>,
    pub separator: Option<String>,
    pub attribution_prefix: Option<String>,
    pub text: Option<String>,
    pub text_file: Option<PathBuf>,
    pub align: Option<Align>,
    pub wrap: Option<usize>,
    pub hold: Option<u64>,
    pub fade_in: Option<u64>,
    pub fade_out: Option<u64>,
    pub frame_rate: Option<u32>,
    pub effect: Option<String>,
    pub include_effects: Option<Vec<String>>,
    pub exclude_effects: Option<Vec<String>>,
    pub font: Option<PathBuf>,
    pub font_size: Option<f32>,
    pub line_height: Option<f32>,
    pub background: Option<String>,
    pub foreground: Option<String>,
    pub seed: Option<u64>,
}

/// A complete configuration with nothing left to look up.
#[derive(Clone, PartialEq, Debug)]
pub struct Settings {
    pub content: Content,
    pub measure: Measure,
    pub hold: Duration,
    pub fade_in: Duration,
    pub fade_out: Duration,
    pub frame_rate: u32,
    pub effects: Vec<String>,
    pub font: Cow<'static, [u8]>,
    pub font_size: f32,
    pub line_height: f32,
    pub background: Rgb,
    pub foreground: Rgb,
    pub seed: u64,
    /// Where the single-instance lock lives, or `None` when there is nowhere
    /// sensible to put one.
    pub lock_path: Option<PathBuf>,
}

impl Settings {
    /// A working configuration that reads nothing: the bundled font, the
    /// built-in text, every effect. This is what the benchmark and the tests
    /// start from.
    pub fn builtin() -> Settings {
        Settings {
            content: Content::Text(BUILTIN_TEXT.to_string()),
            measure: Measure {
                width: DEFAULT_WRAP,
                align: Align::default(),
                attribution_prefix: DEFAULT_ATTRIBUTION_PREFIX.to_string(),
            },
            hold: Duration::from_millis(DEFAULT_HOLD_MS),
            fade_in: Duration::from_millis(DEFAULT_FADE_IN_MS),
            fade_out: Duration::from_millis(DEFAULT_FADE_OUT_MS),
            frame_rate: DEFAULT_FRAME_RATE,
            effects: crate::engine::available_effects(),
            font: Cow::Borrowed(EMBEDDED_FONT),
            font_size: DEFAULT_FONT_SIZE,
            line_height: DEFAULT_LINE_HEIGHT,
            background: DEFAULT_BACKGROUND,
            foreground: DEFAULT_FOREGROUND,
            seed: 0,
            lock_path: None,
        }
    }
}

/// Read the config file, fold the command line over it, and produce the one
/// [`Settings`] the rest of the program runs on.
pub fn resolve(cli: &Cli, env: &Env) -> Result<Settings> {
    // `--no-config` means the whole config directory, not just config.toml.
    // The quote list is configuration too, and a run that still picked it up
    // would not be reproducible — which is the only reason to pass the flag.
    // It matters more than it looks: a quote list makes `Content::Quotes`,
    // which draws a random quote and so consumes an RNG value that
    // `Content::Text` does not, shifting every later draw and with it the
    // effect, the engine's seed and the whole run.
    let config_dir = if cli.no_config { None } else { env.config_dir() };
    let file = load_file_config(cli, config_dir.as_deref())?;
    let mut settings = Settings::builtin();

    settings.measure.width = cli.wrap.or(file.wrap).unwrap_or(DEFAULT_WRAP).max(1);
    settings.measure.align = cli.align.or(file.align).unwrap_or_default();
    settings.measure.attribution_prefix = cli
        .attribution_prefix
        .clone()
        .or(file.attribution_prefix.clone())
        .unwrap_or_else(|| DEFAULT_ATTRIBUTION_PREFIX.to_string());

    settings.hold = Duration::from_millis(cli.hold.or(file.hold).unwrap_or(DEFAULT_HOLD_MS));
    settings.fade_in =
        Duration::from_millis(cli.fade_in.or(file.fade_in).unwrap_or(DEFAULT_FADE_IN_MS));
    settings.fade_out =
        Duration::from_millis(cli.fade_out.or(file.fade_out).unwrap_or(DEFAULT_FADE_OUT_MS));
    settings.frame_rate =
        cli.frame_rate.or(file.frame_rate).unwrap_or(DEFAULT_FRAME_RATE).clamp(1, 240);

    settings.font_size = cli.font_size.or(file.font_size).unwrap_or(DEFAULT_FONT_SIZE).max(4.0);
    settings.line_height =
        cli.line_height.or(file.line_height).unwrap_or(DEFAULT_LINE_HEIGHT).clamp(0.5, 3.0);
    settings.background = match cli.background.as_deref().or(file.background.as_deref()) {
        Some(hex) => parse_hex(hex).with_context(|| "background")?,
        None => DEFAULT_BACKGROUND,
    };
    settings.foreground = match cli.foreground.as_deref().or(file.foreground.as_deref()) {
        Some(hex) => parse_hex(hex).with_context(|| "foreground")?,
        None => DEFAULT_FOREGROUND,
    };

    settings.effects = resolve_effects(&EffectChoice {
        pinned: cli.effect.clone().or(file.effect.clone()),
        include: pick_list(&cli.include_effects, file.include_effects.as_deref()),
        exclude: pick_list(&cli.exclude_effects, file.exclude_effects.as_deref()),
    })?;

    settings.font = match cli.font.clone().or(file.font.clone()) {
        Some(path) => Cow::Owned(
            std::fs::read(&path).with_context(|| format!("reading font {}", path.display()))?,
        ),
        None => Cow::Borrowed(EMBEDDED_FONT),
    };

    settings.content = resolve_content(cli, &file, config_dir.as_deref())?;

    settings.seed = cli.seed.or(file.seed).unwrap_or_else(random_seed);
    settings.lock_path = env.runtime_dir.as_ref().map(|dir| dir.join("nirisaver.lock"));
    Ok(settings)
}

fn pick_list(cli: &[String], file: Option<&[String]>) -> Vec<String> {
    if cli.is_empty() {
        file.map(<[String]>::to_vec).unwrap_or_default()
    } else {
        cli.to_vec()
    }
}

fn load_file_config(cli: &Cli, config_dir: Option<&Path>) -> Result<FileConfig> {
    if cli.no_config {
        return Ok(FileConfig::default());
    }
    // The two paths fail differently on purpose. A file someone named is a
    // file they expect to be read, so a missing one is an error; the default
    // is a convention, and not having adopted it is not a mistake.
    let (path, required) = match &cli.config {
        Some(path) => (path.clone(), true),
        None => match config_dir {
            Some(dir) => (dir.join("config.toml"), false),
            None => return Ok(FileConfig::default()),
        },
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => {
            Ok(FileConfig::default())
        }
        Err(e) => Err(e).with_context(|| format!("reading config {}", path.display())),
    }
}

fn resolve_content(cli: &Cli, file: &FileConfig, config_dir: Option<&Path>) -> Result<Content> {
    let text = cli.text.clone().or(file.text.clone());
    let text_file = cli.text_file.clone().or(file.text_file.clone());
    let quotes_path = cli.quotes.clone().or(file.quotes.clone());

    let source = cli
        .source
        .clone()
        .or(file.source.clone())
        // Naming a text source and then not saying which source is not
        // ambiguous, so do not make the user say it twice.
        .unwrap_or_else(|| match text.is_some() || text_file.is_some() {
            true => "text".to_string(),
            false => "quotes".to_string(),
        });

    match source.as_str() {
        "text" => {
            if let Some(text) = text {
                return Ok(Content::Text(text));
            }
            if let Some(path) = text_file {
                let body = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading text file {}", path.display()))?;
                return Ok(Content::Text(body));
            }
            Err(anyhow!("source is \"text\" but neither --text nor --text-file was given"))
        }
        "quotes" => {
            let separator = cli
                .separator
                .clone()
                .or(file.separator.clone())
                .unwrap_or_else(|| DEFAULT_SEPARATOR.to_string());
            match quotes_path {
                Some(path) => {
                    let body = std::fs::read_to_string(&path)
                        .with_context(|| format!("reading quotes {}", path.display()))?;
                    let quotes = parse_quotes(&body, &separator);
                    if quotes.is_empty() {
                        return Err(anyhow!("{} contains no quotes", path.display()));
                    }
                    Ok(Content::Quotes(quotes))
                }
                None => {
                    // No list named, so the default path is a suggestion.
                    // Dropping a file there turns quotes on with no flags at
                    // all; not having one is not an error.
                    let default = config_dir.map(|dir| dir.join("quotes.txt"));
                    let body = default.as_ref().and_then(|p| std::fs::read_to_string(p).ok());
                    match body {
                        Some(body) => {
                            let quotes = parse_quotes(&body, &separator);
                            match quotes.is_empty() {
                                true => Ok(Content::Text(BUILTIN_TEXT.to_string())),
                                false => Ok(Content::Quotes(quotes)),
                            }
                        }
                        None => Ok(Content::Text(BUILTIN_TEXT.to_string())),
                    }
                }
            }
        }
        other => Err(anyhow!("unknown source {other:?} (expected \"quotes\" or \"text\")")),
    }
}

fn parse_hex(hex: &str) -> Result<Rgb> {
    let digits = hex.strip_prefix('#').unwrap_or(hex);
    if digits.len() != 6 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(anyhow!("{hex:?} is not a #rrggbb colour"));
    }
    let channel = |i: usize| u8::from_str_radix(&digits[i..i + 2], 16).unwrap();
    Ok([channel(0), channel(2), channel(4)])
}

/// A seed for a run nobody asked to be reproducible. Not cryptographic and not
/// trying to be — it decides which quote comes up.
fn random_seed() -> u64 {
    let mut rng = ttfx::utils::rng::Rng::from_entropy();
    rng.randrange(0, i64::MAX) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    /// A scratch directory that removes itself. Small enough not to be worth a
    /// dependency.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            use std::sync::atomic::{AtomicU32, Ordering};
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "nirisaver-test-{}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
                tag
            ));
            std::fs::create_dir_all(path.join("nirisaver")).unwrap();
            Scratch(path)
        }

        fn env(&self) -> Env {
            Env {
                config_home: Some(self.0.clone()),
                home: Some(self.0.clone()),
                runtime_dir: Some(self.0.clone()),
            }
        }

        fn write(&self, name: &str, body: &str) {
            std::fs::write(self.0.join("nirisaver").join(name), body).unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn cli(args: &[&str]) -> Cli {
        let mut full = vec!["nirisaver"];
        full.extend_from_slice(args);
        Cli::try_parse_from(full).unwrap()
    }

    #[test]
    fn builtin_settings_touch_nothing() {
        // The property the benchmark depends on: identical everywhere,
        // whatever is or is not in the user's config directory.
        let a = Settings::builtin();
        let b = Settings::builtin();
        assert_eq!(a, b);
        assert_eq!(a.content, Content::Text(BUILTIN_TEXT.to_string()));
        assert!(matches!(a.font, Cow::Borrowed(_)));
        assert_eq!(a.seed, 0);
        assert_eq!(a.lock_path, None);
    }

    #[test]
    fn an_absent_config_is_not_an_error() {
        let scratch = Scratch::new("absent");
        let settings = resolve(&cli(&[]), &scratch.env()).unwrap();
        assert_eq!(settings.hold, Duration::from_millis(DEFAULT_HOLD_MS));
    }

    #[test]
    fn a_named_config_that_is_missing_is_an_error() {
        let scratch = Scratch::new("named");
        let err = resolve(&cli(&["--config", "/nonexistent/nirisaver.toml"]), &scratch.env())
            .unwrap_err()
            .to_string();
        assert!(err.contains("nirisaver.toml"), "{err}");
    }

    #[test]
    fn the_config_file_sets_everything_the_flags_do() {
        let scratch = Scratch::new("full");
        scratch.write(
            "config.toml",
            r##"
                source = "text"
                text = "from the file"
                align = "left"
                wrap = 33
                hold = 1234
                fade-in = 10
                fade-out = 20
                frame-rate = 45
                effect = "matrix"
                font-size = 22.5
                line-height = 1.25
                background = "#101112"
                foreground = "#a0b0c0"
                seed = 77
            "##,
        );
        let s = resolve(&cli(&[]), &scratch.env()).unwrap();
        assert_eq!(s.content, Content::Text("from the file".into()));
        assert_eq!(s.measure.align, Align::Left);
        assert_eq!(s.measure.width, 33);
        assert_eq!(s.hold, Duration::from_millis(1234));
        assert_eq!(s.fade_in, Duration::from_millis(10));
        assert_eq!(s.fade_out, Duration::from_millis(20));
        assert_eq!(s.frame_rate, 45);
        assert_eq!(s.effects, vec!["matrix".to_string()]);
        assert_eq!(s.font_size, 22.5);
        assert_eq!(s.line_height, 1.25);
        assert_eq!(s.background, [0x10, 0x11, 0x12]);
        assert_eq!(s.foreground, [0xa0, 0xb0, 0xc0]);
        assert_eq!(s.seed, 77);
    }

    #[test]
    fn flags_override_the_file() {
        let scratch = Scratch::new("override");
        scratch.write("config.toml", "hold = 1000\nwrap = 20\n");
        let s = resolve(&cli(&["--hold", "5"]), &scratch.env()).unwrap();
        assert_eq!(s.hold, Duration::from_millis(5));
        assert_eq!(s.measure.width, 20, "unmentioned keys keep the file's value");
    }

    #[test]
    fn no_config_ignores_the_file() {
        let scratch = Scratch::new("noconfig");
        scratch.write("config.toml", "hold = 1000\n");
        let s = resolve(&cli(&["--no-config"]), &scratch.env()).unwrap();
        assert_eq!(s.hold, Duration::from_millis(DEFAULT_HOLD_MS));
    }

    #[test]
    fn no_config_ignores_the_default_quote_list_too() {
        // The flag exists to make a run reproducible, and a run that still
        // read the machine's quote list would not be: quotes draw an RNG value
        // that a plain text block does not, so the effect selection and every
        // draw after it move. CI caught this as a headless checksum that
        // differed between a laptop with a quote list and a runner without one.
        let scratch = Scratch::new("noconfig-quotes");
        scratch.write("quotes.txt", "One. — A\nTwo. — B\n");
        let s = resolve(&cli(&["--no-config"]), &scratch.env()).unwrap();
        assert_eq!(s.content, Content::Text(BUILTIN_TEXT.to_string()));
    }

    #[test]
    fn no_config_still_honours_an_explicitly_named_quote_list() {
        // Naming a file is not reading the config directory.
        let scratch = Scratch::new("noconfig-explicit");
        scratch.write("elsewhere.txt", "Named. — Someone\n");
        let path = scratch.0.join("nirisaver").join("elsewhere.txt");
        let s = resolve(&cli(&["--no-config", "--quotes", path.to_str().unwrap()]), &scratch.env())
            .unwrap();
        match s.content {
            Content::Quotes(q) => assert_eq!(q.len(), 1),
            other => panic!("expected quotes, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_key_is_reported_rather_than_ignored() {
        let scratch = Scratch::new("typo");
        scratch.write("config.toml", "holdd = 1000\n");
        let err = resolve(&cli(&[]), &scratch.env()).unwrap_err().to_string();
        assert!(err.contains("parsing config"), "{err}");
    }

    #[test]
    fn the_default_quote_list_turns_quotes_on_by_itself() {
        let scratch = Scratch::new("quotes-default");
        scratch.write("quotes.txt", "# a list\n\nOne thing. — Someone\nAnother. — Nobody\n");
        let s = resolve(&cli(&[]), &scratch.env()).unwrap();
        match s.content {
            Content::Quotes(q) => assert_eq!(q.len(), 2),
            other => panic!("expected quotes, got {other:?}"),
        }
    }

    #[test]
    fn an_absent_default_quote_list_falls_back_rather_than_failing() {
        let scratch = Scratch::new("quotes-none");
        let s = resolve(&cli(&[]), &scratch.env()).unwrap();
        assert_eq!(s.content, Content::Text(BUILTIN_TEXT.to_string()));
    }

    #[test]
    fn a_named_quote_list_that_is_missing_is_an_error() {
        let scratch = Scratch::new("quotes-named");
        let err = resolve(&cli(&["--quotes", "/nonexistent/quotes.txt"]), &scratch.env())
            .unwrap_err()
            .to_string();
        assert!(err.contains("quotes.txt"), "{err}");
    }

    #[test]
    fn the_separator_is_configurable_end_to_end() {
        let scratch = Scratch::new("separator");
        scratch.write("quotes.txt", "A thought :: Someone\n");
        scratch.write("config.toml", "separator = \" :: \"\n");
        let s = resolve(&cli(&[]), &scratch.env()).unwrap();
        match s.content {
            Content::Quotes(q) => assert_eq!(q[0].attribution.as_deref(), Some("Someone")),
            other => panic!("expected quotes, got {other:?}"),
        }
    }

    #[test]
    fn naming_a_text_implies_the_text_source() {
        let scratch = Scratch::new("implied");
        scratch.write("quotes.txt", "A — B\n");
        let s = resolve(&cli(&["--text", "just this"]), &scratch.env()).unwrap();
        assert_eq!(s.content, Content::Text("just this".into()));
    }

    #[test]
    fn a_text_source_with_no_text_is_an_error() {
        let scratch = Scratch::new("empty-text");
        let err = resolve(&cli(&["--source", "text"]), &scratch.env()).unwrap_err().to_string();
        assert!(err.contains("--text"), "{err}");
    }

    #[test]
    fn effect_filters_come_from_either_place() {
        let scratch = Scratch::new("filters");
        scratch.write("config.toml", "exclude-effects = [\"matrix\"]\n");
        let s = resolve(&cli(&[]), &scratch.env()).unwrap();
        assert!(!s.effects.contains(&"matrix".to_string()));
        let s = resolve(&cli(&["--include-effects", "beams", "rain"]), &scratch.env()).unwrap();
        assert_eq!(s.effects, vec!["beams".to_string(), "rain".to_string()]);
    }

    #[test]
    fn a_bad_colour_says_which_one() {
        let scratch = Scratch::new("colour");
        let err = resolve(&cli(&["--background", "nope"]), &scratch.env()).unwrap_err();
        assert!(format!("{err:#}").contains("background"), "{err:#}");
    }

    #[test]
    fn colours_parse_with_or_without_the_hash() {
        assert_eq!(parse_hex("#0a141e").unwrap(), [10, 20, 30]);
        assert_eq!(parse_hex("0A141E").unwrap(), [10, 20, 30]);
        assert!(parse_hex("#fff").is_err());
    }

    #[test]
    fn the_lock_lives_in_the_runtime_directory() {
        let scratch = Scratch::new("lock");
        let s = resolve(&cli(&[]), &scratch.env()).unwrap();
        assert_eq!(s.lock_path, Some(scratch.0.join("nirisaver.lock")));
    }

    #[test]
    fn an_empty_environment_resolves_to_the_builtin_content() {
        let s = resolve(&cli(&[]), &Env::default()).unwrap();
        assert_eq!(s.content, Content::Text(BUILTIN_TEXT.to_string()));
        assert_eq!(s.lock_path, None);
    }
}
