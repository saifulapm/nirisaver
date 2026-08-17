//! The command line.
//!
//! Every flag here overrides the same-named key in the config file, and
//! nothing here reads a file or an environment variable — parsing produces a
//! bag of `Option`s and stops. Resolution happens once, in [`crate::config`],
//! and everything downstream is handed a plain struct with no questions left
//! in it.

use std::path::PathBuf;

use clap::Parser;

use crate::text::Align;

#[derive(Parser, Debug, Default)]
#[command(
    name = "nirisaver",
    version,
    about = "A Wayland screensaver for niri: animated text effects on a layer-shell overlay",
    long_about = "Draws animated text — a rotating quote, or a block of your own — across every \
output on a wlr-layer-shell overlay, and dismisses itself on any key, pointer \
motion, click or SIGTERM.\n\n\
Settings come from ~/.config/nirisaver/config.toml; every flag below overrides \
the matching key there."
)]
pub struct Cli {
    /// Config file to read (default: $XDG_CONFIG_HOME/nirisaver/config.toml)
    #[arg(short, long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Ignore the config directory entirely — config.toml and the default
    /// quote list both — and use the built-in defaults plus these flags
    #[arg(long)]
    pub no_config: bool,

    /// What to animate: a rotating quote, or one fixed block of text
    #[arg(long, value_name = "KIND", value_parser = ["quotes", "text"])]
    pub source: Option<String>,

    /// Quote list: one per line, "text <separator> author"
    #[arg(long, value_name = "PATH")]
    pub quotes: Option<PathBuf>,

    /// What splits a quote from its attribution (default: " — ")
    #[arg(long, value_name = "STR")]
    pub separator: Option<String>,

    /// What the attribution line opens with once it has its own line
    #[arg(long, value_name = "STR")]
    pub attribution_prefix: Option<String>,

    /// A block of text to animate, given inline
    #[arg(long, value_name = "STR", conflicts_with = "text_file")]
    pub text: Option<String>,

    /// A block of text to animate, read from a file
    #[arg(long, value_name = "PATH")]
    pub text_file: Option<PathBuf>,

    /// How wrapped lines sit against each other
    #[arg(long, value_name = "ALIGN")]
    pub align: Option<Align>,

    /// Wrap measure, in columns
    #[arg(long, value_name = "COLS")]
    pub wrap: Option<usize>,

    /// How long a finished frame stays readable before the next cycle, in ms
    #[arg(long, value_name = "MS")]
    pub hold: Option<u64>,

    /// Fade-in duration, in ms
    #[arg(long, value_name = "MS")]
    pub fade_in: Option<u64>,

    /// Fade-out duration, in ms
    #[arg(long, value_name = "MS")]
    pub fade_out: Option<u64>,

    /// Animation frames per second
    #[arg(long, value_name = "FPS")]
    pub frame_rate: Option<u32>,

    /// Pin one effect for every cycle
    #[arg(long, value_name = "NAME", conflicts_with_all = ["include_effects", "exclude_effects"])]
    pub effect: Option<String>,

    /// Draw each cycle's effect only from these
    #[arg(long, value_name = "NAME", num_args = 1.., conflicts_with = "exclude_effects")]
    pub include_effects: Vec<String>,

    /// Draw each cycle's effect from everything except these
    #[arg(long, value_name = "NAME", num_args = 1..)]
    pub exclude_effects: Vec<String>,

    /// Print every available effect and exit
    #[arg(long)]
    pub list_effects: bool,

    /// A TrueType or OpenType font to render with (default: the bundled JetBrains Mono)
    #[arg(long, value_name = "PATH")]
    pub font: Option<PathBuf>,

    /// Font size in logical pixels, before output scaling
    #[arg(long, value_name = "PX")]
    pub font_size: Option<f32>,

    /// Cell height as a multiple of the font's own line height
    #[arg(long, value_name = "RATIO")]
    pub line_height: Option<f32>,

    /// Surface background colour, as #rrggbb
    #[arg(long, value_name = "HEX")]
    pub background: Option<String>,

    /// Colour for text the effect leaves unstyled, as #rrggbb
    #[arg(long, value_name = "HEX")]
    pub foreground: Option<String>,

    /// Seed the animation, so a run replays exactly
    #[arg(long, value_name = "N")]
    pub seed: Option<u64>,

    /// Render without a compositor, for testing layout and content
    #[arg(long)]
    pub headless: bool,

    /// Headless: how many frames to render
    #[arg(long, value_name = "N", requires = "headless")]
    pub frames: Option<u64>,

    /// Headless: grid width in cells
    #[arg(long, value_name = "N", requires = "headless")]
    pub cols: Option<usize>,

    /// Headless: grid height in cells
    #[arg(long, value_name = "N", requires = "headless")]
    pub rows: Option<usize>,

    /// Headless: print the cell grid of the last frame as well as the summary
    #[arg(long, requires = "headless")]
    pub dump_grid: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_line_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn help_documents_every_option() {
        // --help is the documentation of record for the flags, so a flag
        // without a doc comment is a bug, not a style nit.
        for arg in Cli::command().get_arguments() {
            assert!(arg.get_help().is_some(), "{} has no help text", arg.get_id());
        }
    }

    #[test]
    fn flags_parse_into_options_and_nothing_else() {
        let cli = Cli::try_parse_from(["nirisaver", "--hold", "9000", "--align", "left"]).unwrap();
        assert_eq!(cli.hold, Some(9000));
        assert_eq!(cli.align, Some(Align::Left));
        assert_eq!(cli.wrap, None);
    }

    #[test]
    fn pinning_an_effect_excludes_the_filters() {
        assert!(Cli::try_parse_from([
            "nirisaver",
            "--effect",
            "matrix",
            "--exclude-effects",
            "beams"
        ])
        .is_err());
    }

    #[test]
    fn headless_only_options_need_headless() {
        assert!(Cli::try_parse_from(["nirisaver", "--frames", "10"]).is_err());
        assert!(Cli::try_parse_from(["nirisaver", "--headless", "--frames", "10"]).is_ok());
    }
}
