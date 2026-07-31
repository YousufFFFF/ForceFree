//! The "Verdigris" palette.
//!
//! Oxidised copper and brass: an industrial, reclamation-yard vocabulary rather
//! than the near-black-plus-neon-accent that most terminal tools reach for.
//!
//! Three rules hold this together:
//!
//!   1. **Only accents are ever coloured.** The foreground is never set, so the
//!      output stays legible on light terminals as well as dark ones. Anything
//!      that needs to recede uses [`dim`] rather than a dark colour.
//!   2. **Roles, not colours.** Call sites ask for [`space_freed`], not "green",
//!      so re-theming touches this file alone.
//!   3. **Every role names its 16-colour stand-in.** Terminals that cannot manage
//!      truecolour get a deliberate choice rather than whatever the emulator
//!      approximates to.
//!
//! Verdigris against brass is a green–orange pair, which stays distinguishable
//! under the common colour deficiencies and separates by lightness in greyscale.
//!
//! Whether colour is emitted *at all* is `anstream`'s job — it handles
//! `NO_COLOR`, `CLICOLOR`, `CLICOLOR_FORCE`, whether stdout is a terminal, and
//! enabling virtual terminal processing on Windows consoles. What it does not do
//! is reduce colour *depth*, so that decision is made here, once.

use anstyle::{AnsiColor, Color, RgbColor, Style};
use std::sync::OnceLock;

/// Queried once. The answer cannot change during a run, and the lookup reads
/// environment variables we would otherwise re-parse for every bar drawn.
fn truecolor() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(anstyle_query::truecolor)
}

fn role(r: u8, g: u8, b: u8, fallback: AnsiColor) -> Style {
    let colour = if truecolor() {
        Color::Rgb(RgbColor(r, g, b))
    } else {
        Color::Ansi(fallback)
    };
    Style::new().fg_color(Some(colour))
}

/// Space that deletion would actually return. The reason to act.
pub fn space_freed() -> Style {
    role(0x6E, 0x98, 0x87, AnsiColor::Green)
}

/// Time you would spend rebuilding. The reason to hesitate.
pub fn time_cost() -> Style {
    role(0xC6, 0x8B, 0x3C, AnsiColor::Yellow)
}

/// Work that isn't backed up, and so will not be touched.
pub fn held_back() -> Style {
    role(0x9E, 0x4A, 0x4A, AnsiColor::Red)
}

/// Spine, rules, and anything that should recede.
pub fn structure() -> Style {
    role(0x5C, 0x63, 0x70, AnsiColor::BrightBlack)
}

/// Totals and the few figures worth landing on.
pub fn emphasis() -> Style {
    role(0xCF, 0xC8, 0xBA, AnsiColor::White)
}

/// Deliberately no colour — just weight. For secondary text that would be noise
/// in any hue.
pub fn dim() -> Style {
    Style::new().dimmed()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> [Style; 6] {
        [
            space_freed(),
            time_cost(),
            held_back(),
            structure(),
            emphasis(),
            dim(),
        ]
    }

    /// The palette is accents only. A background would fight whatever the user's
    /// terminal already is, and light themes would suffer for it.
    #[test]
    fn no_role_sets_a_background() {
        for style in all() {
            assert!(
                style.get_bg_color().is_none(),
                "{style:?} sets a background"
            );
        }
    }

    /// Space and time are the two quantities the reader compares, so they must
    /// never render as the same colour — at either depth.
    #[test]
    fn the_two_quantities_are_distinguishable() {
        assert_ne!(space_freed().get_fg_color(), time_cost().get_fg_color());
    }

    /// Whichever depth is in play, every coloured role must actually carry a
    /// colour; a role that silently resolves to nothing would be invisible.
    #[test]
    fn every_coloured_role_resolves_to_a_colour() {
        for style in [
            space_freed(),
            time_cost(),
            held_back(),
            structure(),
            emphasis(),
        ] {
            assert!(style.get_fg_color().is_some(), "{style:?} has no colour");
        }
    }
}
