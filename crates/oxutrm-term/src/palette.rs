//! The colour table `alacritty_terminal` does not ship.
//!
//! `Term::colors()` is an **override** table for OSC 4, 10 and 11 — every
//! entry is `None` until an application sets one. The crate has no default
//! palette at all, so a naive `colors()[idx]` renders every cell as "no
//! colour" and the screen comes out blank. oxutrm supplies its own table and
//! consults `colors()` only as the override layer on top.
//!
//! # Layout
//!
//! 269 entries, all ranges half-open:
//!
//! | Range | Contents |
//! |---|---|
//! | `0..16` | the named colours, normal then bright |
//! | `16..232` | the 6x6x6 cube (216) |
//! | `232..256` | the greyscale ramp (24) |
//! | `256` | foreground |
//! | `257` | background |
//! | `258` | cursor |
//! | `259..267` | the eight dim variants |
//! | `267` | bright foreground |
//! | `268` | dim foreground |
//!
//! 16 + 216 + 24 + 3 + 8 + 2 = 269. Note there is **no dim background**:
//! `NamedColor` ends `BrightForeground, DimForeground`, and inventing a 270th
//! slot would shift every index above it.
//!
//! Promotion of DIM and BOLD to the bright variants is the **renderer's**
//! job, not this table's. The host keeps full fidelity so that a
//! differently-capable client reattaching later still gets the truth.

use alacritty_terminal::vte::ansi::{Color as VteColor, NamedColor, Rgb};

use oxutrm_proto::Color;

/// How many entries the table has. Pinned by a test, because every index
/// above a mistake would be silently wrong.
pub const PALETTE_LEN: usize = 269;

const NAMED: usize = 16;
const CUBE: usize = 216;
const GREY: usize = 24;

/// The index of each non-palette slot.
const FOREGROUND: usize = 256;
const BACKGROUND: usize = 257;
const CURSOR: usize = 258;
const DIM_BASE: usize = 259;
const BRIGHT_FOREGROUND: usize = 267;
const DIM_FOREGROUND: usize = 268;

/// The xterm 16, which every terminal starts from.
const BASE_16: [(u8, u8, u8); 16] = [
    (0x00, 0x00, 0x00), // black
    (0xcd, 0x00, 0x00), // red
    (0x00, 0xcd, 0x00), // green
    (0xcd, 0xcd, 0x00), // yellow
    (0x00, 0x00, 0xee), // blue
    (0xcd, 0x00, 0xcd), // magenta
    (0x00, 0xcd, 0xcd), // cyan
    (0xe5, 0xe5, 0xe5), // white
    (0x7f, 0x7f, 0x7f), // bright black
    (0xff, 0x00, 0x00), // bright red
    (0x00, 0xff, 0x00), // bright green
    (0xff, 0xff, 0x00), // bright yellow
    (0x5c, 0x5c, 0xff), // bright blue
    (0xff, 0x00, 0xff), // bright magenta
    (0x00, 0xff, 0xff), // bright cyan
    (0xff, 0xff, 0xff), // bright white
];

/// The full 269-entry table.
pub fn palette() -> [Rgb; PALETTE_LEN] {
    let mut table = [Rgb { r: 0, g: 0, b: 0 }; PALETTE_LEN];

    for (i, (r, g, b)) in BASE_16.iter().enumerate() {
        table[i] = Rgb {
            r: *r,
            g: *g,
            b: *b,
        };
    }

    // The 6x6x6 cube. The levels are xterm's, not evenly spaced: the jump
    // from 0 to 95 is deliberate, because a linear ramp looks wrong.
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let mut i = NAMED;
    for r in LEVELS {
        for g in LEVELS {
            for b in LEVELS {
                table[i] = Rgb { r, g, b };
                i += 1;
            }
        }
    }
    debug_assert_eq!(i, NAMED + CUBE);

    // The greyscale ramp, 8 to 238 in steps of 10.
    for step in 0..GREY {
        let v = 8 + step as u8 * 10;
        table[NAMED + CUBE + step] = Rgb { r: v, g: v, b: v };
    }

    table[FOREGROUND] = Rgb {
        r: 0xd8,
        g: 0xd8,
        b: 0xd8,
    };
    table[BACKGROUND] = Rgb {
        r: 0x00,
        g: 0x00,
        b: 0x00,
    };
    table[CURSOR] = Rgb {
        r: 0xd8,
        g: 0xd8,
        b: 0xd8,
    };

    // The eight dim variants, at roughly two thirds intensity.
    for k in 0..8 {
        let Rgb { r, g, b } = table[k];
        table[DIM_BASE + k] = Rgb {
            r: (r as u16 * 2 / 3) as u8,
            g: (g as u16 * 2 / 3) as u8,
            b: (b as u16 * 2 / 3) as u8,
        };
    }

    table[BRIGHT_FOREGROUND] = Rgb {
        r: 0xff,
        g: 0xff,
        b: 0xff,
    };
    table[DIM_FOREGROUND] = Rgb {
        r: 0x90,
        g: 0x90,
        b: 0x90,
    };

    table
}

/// Where a [`NamedColor`] sits in the table.
pub fn named_index(c: NamedColor) -> usize {
    match c {
        NamedColor::Black => 0,
        NamedColor::Red => 1,
        NamedColor::Green => 2,
        NamedColor::Yellow => 3,
        NamedColor::Blue => 4,
        NamedColor::Magenta => 5,
        NamedColor::Cyan => 6,
        NamedColor::White => 7,
        NamedColor::BrightBlack => 8,
        NamedColor::BrightRed => 9,
        NamedColor::BrightGreen => 10,
        NamedColor::BrightYellow => 11,
        NamedColor::BrightBlue => 12,
        NamedColor::BrightMagenta => 13,
        NamedColor::BrightCyan => 14,
        NamedColor::BrightWhite => 15,
        NamedColor::Foreground => FOREGROUND,
        NamedColor::Background => BACKGROUND,
        NamedColor::Cursor => CURSOR,
        NamedColor::DimBlack => DIM_BASE,
        NamedColor::DimRed => DIM_BASE + 1,
        NamedColor::DimGreen => DIM_BASE + 2,
        NamedColor::DimYellow => DIM_BASE + 3,
        NamedColor::DimBlue => DIM_BASE + 4,
        NamedColor::DimMagenta => DIM_BASE + 5,
        NamedColor::DimCyan => DIM_BASE + 6,
        NamedColor::DimWhite => DIM_BASE + 7,
        NamedColor::BrightForeground => BRIGHT_FOREGROUND,
        NamedColor::DimForeground => DIM_FOREGROUND,
    }
}

/// Convert an emulator colour into ours, consulting the OSC override table
/// first.
///
/// `Foreground` and `Background` become [`Color::Default`] rather than a
/// concrete value: the client renders into a real terminal that has its own
/// theme, and resolving them here would repaint the user's chosen background
/// as our guess at one.
pub fn to_proto_color(
    c: VteColor,
    overrides: &alacritty_terminal::term::color::Colors,
    table: &[Rgb; PALETTE_LEN],
) -> Color {
    match c {
        VteColor::Named(NamedColor::Foreground | NamedColor::Background) => Color::Default,
        VteColor::Named(named) => {
            let idx = named_index(named);
            match overrides[idx] {
                Some(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
                // A palette index below 256 travels as an index, so a client
                // with its own theme can honour it.
                None if idx < 256 => Color::Idx(idx as u8),
                None => {
                    let rgb = table[idx];
                    Color::Rgb(rgb.r, rgb.g, rgb.b)
                }
            }
        }
        VteColor::Indexed(i) => match overrides[i as usize] {
            Some(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
            None => Color::Idx(i),
        },
        VteColor::Spec(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_exactly_269_entries() {
        // 16 named + 216 cube + 24 greyscale + fg + bg + cursor + 8 dim
        // + bright fg + dim fg. Every index above a miscount would be
        // silently wrong, so the arithmetic is spelled out.
        assert_eq!(NAMED + CUBE + GREY + 3 + 8 + 2, PALETTE_LEN);
        assert_eq!(PALETTE_LEN, 269);
        assert_eq!(palette().len(), 269);
    }

    #[test]
    fn the_ranges_are_half_open_and_do_not_overlap() {
        assert_eq!(NAMED, 16, "0..16 named");
        assert_eq!(NAMED + CUBE, 232, "16..232 cube");
        assert_eq!(NAMED + CUBE + GREY, 256, "232..256 greyscale");
        assert_eq!(FOREGROUND, 256);
        assert_eq!(BACKGROUND, 257);
        assert_eq!(CURSOR, 258);
        assert_eq!(DIM_BASE, 259);
        assert_eq!(DIM_BASE + 8, BRIGHT_FOREGROUND, "259..267 dim, then 267");
        assert_eq!(DIM_FOREGROUND, 268);
    }

    #[test]
    fn there_is_no_dim_background_slot() {
        // NamedColor ends `BrightForeground, DimForeground`. Inventing a
        // 270th entry for a dim background would shift nothing today and
        // everything the moment someone indexed past it.
        assert_eq!(named_index(NamedColor::DimForeground), PALETTE_LEN - 1);
    }

    #[test]
    fn every_named_colour_lands_in_range_and_no_two_collide() {
        let all = [
            NamedColor::Black,
            NamedColor::Red,
            NamedColor::Green,
            NamedColor::Yellow,
            NamedColor::Blue,
            NamedColor::Magenta,
            NamedColor::Cyan,
            NamedColor::White,
            NamedColor::BrightBlack,
            NamedColor::BrightRed,
            NamedColor::BrightGreen,
            NamedColor::BrightYellow,
            NamedColor::BrightBlue,
            NamedColor::BrightMagenta,
            NamedColor::BrightCyan,
            NamedColor::BrightWhite,
            NamedColor::Foreground,
            NamedColor::Background,
            NamedColor::Cursor,
            NamedColor::DimBlack,
            NamedColor::DimRed,
            NamedColor::DimGreen,
            NamedColor::DimYellow,
            NamedColor::DimBlue,
            NamedColor::DimMagenta,
            NamedColor::DimCyan,
            NamedColor::DimWhite,
            NamedColor::BrightForeground,
            NamedColor::DimForeground,
        ];
        let mut seen = std::collections::HashSet::new();
        for c in all {
            let i = named_index(c);
            assert!(i < PALETTE_LEN, "{c:?} -> {i}");
            assert!(seen.insert(i), "{c:?} collides at {i}");
        }
        assert_eq!(seen.len(), 29, "NamedColor has 29 variants");
    }

    #[test]
    fn the_cube_starts_and_ends_where_xterm_says() {
        let t = palette();
        assert_eq!(t[16], Rgb { r: 0, g: 0, b: 0 }, "the cube starts at black");
        assert_eq!(
            t[231],
            Rgb {
                r: 255,
                g: 255,
                b: 255
            },
            "and ends at white"
        );
        // 16 + 36*1 + 6*0 + 0 is the second red level with no green or blue.
        assert_eq!(t[52], Rgb { r: 95, g: 0, b: 0 });
    }

    #[test]
    fn the_greyscale_ramp_is_eight_to_two_thirty_eight() {
        let t = palette();
        assert_eq!(t[232], Rgb { r: 8, g: 8, b: 8 });
        assert_eq!(
            t[255],
            Rgb {
                r: 238,
                g: 238,
                b: 238
            }
        );
    }

    #[test]
    fn foreground_and_background_stay_default_so_the_client_keeps_its_theme() {
        let t = palette();
        let none = alacritty_terminal::term::color::Colors::default();
        assert_eq!(
            to_proto_color(VteColor::Named(NamedColor::Foreground), &none, &t),
            Color::Default
        );
        assert_eq!(
            to_proto_color(VteColor::Named(NamedColor::Background), &none, &t),
            Color::Default
        );
    }

    #[test]
    fn a_palette_colour_travels_as_an_index_not_as_rgb() {
        // An index lets a client with its own theme honour it. Resolving to
        // RGB here would impose the host's palette on every client forever.
        let t = palette();
        let none = alacritty_terminal::term::color::Colors::default();
        assert_eq!(
            to_proto_color(VteColor::Indexed(42), &none, &t),
            Color::Idx(42)
        );
        assert_eq!(
            to_proto_color(VteColor::Named(NamedColor::Red), &none, &t),
            Color::Idx(1)
        );
    }

    #[test]
    fn an_osc_override_wins_over_the_default_table() {
        let t = palette();
        let mut overrides = alacritty_terminal::term::color::Colors::default();
        overrides[1] = Some(Rgb { r: 9, g: 9, b: 9 });
        assert_eq!(
            to_proto_color(VteColor::Named(NamedColor::Red), &overrides, &t),
            Color::Rgb(9, 9, 9),
            "OSC 4 must be honoured"
        );
        assert_eq!(
            to_proto_color(VteColor::Indexed(1), &overrides, &t),
            Color::Rgb(9, 9, 9)
        );
    }

    #[test]
    fn a_true_colour_spec_passes_straight_through() {
        let t = palette();
        let none = alacritty_terminal::term::color::Colors::default();
        assert_eq!(
            to_proto_color(VteColor::Spec(Rgb { r: 1, g: 2, b: 3 }), &none, &t),
            Color::Rgb(1, 2, 3)
        );
    }

    #[test]
    fn the_dim_variants_are_darker_than_what_they_dim() {
        let t = palette();
        for k in 0..8usize {
            let normal = t[k];
            let dim = t[DIM_BASE + k];
            assert!(
                dim.r <= normal.r && dim.g <= normal.g && dim.b <= normal.b,
                "dim variant {k} is not darker"
            );
        }
    }
}
