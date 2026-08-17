//! What the screensaver actually says, and how it is set on the page.
//!
//! The animation engine takes a block of text and animates it in place, so
//! everything about presentation — wrapping, where the attribution goes,
//! centring — has to be decided before the engine ever sees it. That is all
//! this module does, and it does it with no I/O and no globals, which is what
//! lets the layout be tested as a pure function.

use ttfx::utils::rng::Rng;

/// How wrapped lines sit against each other.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Align {
    #[default]
    Center,
    Left,
}

impl std::str::FromStr for Align {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "center" | "centre" => Ok(Align::Center),
            "left" => Ok(Align::Left),
            other => Err(format!("unknown alignment {other:?} (expected center or left)")),
        }
    }
}

/// One entry from the quote list.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Quote {
    pub text: String,
    pub attribution: Option<String>,
}

/// The typographic settings a block is set with.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Measure {
    /// Wrap width in columns.
    pub width: usize,
    pub align: Align,
    /// What an attribution line opens with, once it has been moved off the end
    /// of the quote. Separate from the separator that *split* it: a list can
    /// use `|` as its field separator and still want a dash on screen.
    pub attribution_prefix: String,
}

/// Split a quote list into entries.
///
/// Blank lines and `#` comments are dropped, so a long list can be sectioned
/// and annotated. The separator is matched from the right: several quotes in
/// any real list contain the separator inside the quotation itself, and the
/// attribution is always the last field.
pub fn parse_quotes(contents: &str, separator: &str) -> Vec<Quote> {
    contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| match separator.is_empty() {
            true => Quote { text: line.to_string(), attribution: None },
            false => match line.rsplit_once(separator) {
                Some((text, attribution)) => Quote {
                    text: text.trim_end().to_string(),
                    attribution: Some(attribution.trim().to_string()).filter(|a| !a.is_empty()),
                },
                None => Quote { text: line.to_string(), attribution: None },
            },
        })
        .collect()
}

/// Set a quote as a block: the quotation wrapped to the measure, then a blank
/// line, then the attribution on a line of its own.
pub fn layout_quote(quote: &Quote, measure: &Measure) -> String {
    let mut lines = wrap(&quote.text, measure.width);
    if let Some(attribution) = &quote.attribution {
        let credit = format!("{}{}", measure.attribution_prefix, attribution);
        lines.push(String::new());
        lines.extend(wrap(&credit, measure.width));
    }
    align(&lines, measure.align)
}

/// Set a plain block of text: existing line breaks are honoured, and any line
/// longer than the measure is wrapped.
pub fn layout_text(text: &str, measure: &Measure) -> String {
    let mut lines = Vec::new();
    for line in text.trim_end_matches('\n').lines() {
        if line.trim().is_empty() {
            lines.push(String::new());
        } else {
            lines.extend(wrap(line, measure.width));
        }
    }
    align(&lines, measure.align)
}

/// Greedy word wrap. Words longer than the measure are broken rather than
/// allowed to widen the block — a URL in a quote should not push the whole
/// paragraph off the side of the screen.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let mut word = word;
        while word.chars().count() > width {
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
            }
            let split = word.char_indices().nth(width).map(|(i, _)| i).unwrap_or(word.len());
            let (head, tail) = word.split_at(split);
            lines.push(head.to_string());
            word = tail;
        }
        let candidate =
            current.chars().count() + usize::from(!current.is_empty()) + word.chars().count();
        if !current.is_empty() && candidate > width {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Pad lines into a block. Centring happens here rather than being left to the
/// engine's canvas anchor: the engine centres the block, and this centres the
/// lines within it, and only both together read as centred text.
fn align(lines: &[String], align: Align) -> String {
    let block = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if align == Align::Center {
            let len = line.chars().count();
            // Trailing padding is left off: the engine fills the rest of the
            // canvas itself, and real trailing spaces would widen the block.
            for _ in 0..(block - len) / 2 {
                out.push(' ');
            }
        }
        out.push_str(line);
    }
    out
}

/// Where the animated text comes from. Resolved once at startup — by the time
/// this exists, every file that was going to be read has been read.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Content {
    Quotes(Vec<Quote>),
    Text(String),
}

impl Content {
    /// The block for one cycle. Quotes draw a fresh entry each time; a custom
    /// text block is replayed as it stands.
    pub fn block(&self, rng: &mut Rng, measure: &Measure) -> String {
        match self {
            Content::Quotes(quotes) if !quotes.is_empty() => {
                layout_quote(&quotes[rng.choice_index(quotes.len())], measure)
            }
            // An empty list is not an error at this point — resolution already
            // rejected the cases where it would be — so say something rather
            // than present a blank screen.
            Content::Quotes(_) => layout_text("nirisaver", measure),
            Content::Text(text) => layout_text(text, measure),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn measure(width: usize, align: Align) -> Measure {
        Measure { width, align, attribution_prefix: "— ".to_string() }
    }

    #[test]
    fn a_quote_splits_on_its_separator() {
        let quotes = parse_quotes("Be excellent. — Bill\n", " — ");
        assert_eq!(
            quotes,
            vec![Quote { text: "Be excellent.".into(), attribution: Some("Bill".into()) }]
        );
    }

    #[test]
    fn the_separator_is_matched_from_the_right() {
        // A real list has quotations containing the separator. The attribution
        // is the last field, not the second.
        let quotes = parse_quotes("Whether you think you can — you're right. — Henry Ford", " — ");
        assert_eq!(quotes[0].text, "Whether you think you can — you're right.");
        assert_eq!(quotes[0].attribution.as_deref(), Some("Henry Ford"));
    }

    #[test]
    fn the_separator_is_configurable() {
        let quotes = parse_quotes("Words | Someone", " | ");
        assert_eq!(quotes[0].attribution.as_deref(), Some("Someone"));
        let quotes = parse_quotes("Words :: Someone", " :: ");
        assert_eq!(quotes[0].attribution.as_deref(), Some("Someone"));
    }

    #[test]
    fn comments_and_blanks_are_dropped() {
        let quotes = parse_quotes("# heading\n\n  \nOne — A\n# tail\n", " — ");
        assert_eq!(quotes.len(), 1);
    }

    #[test]
    fn a_line_without_a_separator_is_a_quote_with_no_attribution() {
        let quotes = parse_quotes("Just a line", " — ");
        assert_eq!(quotes[0].attribution, None);
    }

    #[test]
    fn the_attribution_gets_its_own_line() {
        let quote = Quote { text: "Short.".into(), attribution: Some("Someone".into()) };
        let block = layout_quote(&quote, &measure(40, Align::Left));
        assert_eq!(block, "Short.\n\n— Someone");
    }

    #[test]
    fn wrapping_respects_the_measure() {
        let text = "one two three four five six seven eight nine ten";
        for line in wrap(text, 12) {
            assert!(line.chars().count() <= 12, "{line:?} is over the measure");
        }
    }

    #[test]
    fn an_overlong_word_is_broken_rather_than_widening_the_block() {
        let lines = wrap("short verylongunbreakablewordhere", 8);
        assert!(lines.iter().all(|l| l.chars().count() <= 8), "{lines:?}");
        assert_eq!(lines.concat().replace(' ', "").len(), "shortverylongunbreakablewordhere".len());
    }

    #[test]
    fn centring_pads_only_on_the_left() {
        let block = align(&["ab".into(), "abcd".into()], Align::Center);
        assert_eq!(block, " ab\nabcd");
    }

    #[test]
    fn left_alignment_pads_nothing() {
        let block = align(&["ab".into(), "abcd".into()], Align::Left);
        assert_eq!(block, "ab\nabcd");
    }

    #[test]
    fn a_centred_block_is_no_wider_than_its_measure() {
        let quote = Quote {
            text: "It does not matter how slowly you go as long as you do not stop.".into(),
            attribution: Some("Confucius".into()),
        };
        let block = layout_quote(&quote, &measure(32, Align::Center));
        assert!(block.lines().all(|l| l.chars().count() <= 32), "{block}");
    }

    #[test]
    fn plain_text_keeps_its_own_line_breaks() {
        let block = layout_text("first\n\nthird", &measure(40, Align::Left));
        assert_eq!(block, "first\n\nthird");
    }

    #[test]
    fn quotes_rotate_with_the_seed() {
        let quotes = Content::Quotes(parse_quotes("a — x\nb — y\nc — z\nd — w", " — "));
        let m = measure(40, Align::Left);
        let mut rng = Rng::seeded(1);
        let drawn: Vec<_> = (0..8).map(|_| quotes.block(&mut rng, &m)).collect();
        let mut same_seed = Rng::seeded(1);
        let again: Vec<_> = (0..8).map(|_| quotes.block(&mut same_seed, &m)).collect();
        assert_eq!(drawn, again, "the same seed must draw the same sequence");
        assert!(drawn.iter().collect::<std::collections::HashSet<_>>().len() > 1);
    }
}
