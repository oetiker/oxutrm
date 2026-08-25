//! Reducing a colour to what the terminal in front of the user can show.
//!
//! This happens **here, on the client**, and never on the host. The host's
//! [`ScreenState`](oxutrm_proto::ScreenState) always carries whatever the
//! emulator produced, at full fidelity, so a better terminal attaching tomorrow
//! gets the original. Down-converting on the host would bake the loss into the
//! authoritative state forever.

use oxutrm_proto::{Color, TerminalCaps};

/// The six levels of the xterm 6x6x6 colour cube.
pub const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// The sixteen ANSI system colours as most terminals actually render them.
///
/// These are approximations by nature — the low sixteen are theme-dependent,
/// which is exactly why [`rgb_to_256`] refuses to target them.
const ANSI16: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (170, 0, 0),
    (0, 170, 0),
    (170, 85, 0),
    (0, 0, 170),
    (170, 0, 170),
    (0, 170, 170),
    (170, 170, 170),
    (85, 85, 85),
    (255, 85, 85),
    (85, 255, 85),
    (255, 255, 85),
    (85, 85, 255),
    (255, 85, 255),
    (85, 255, 255),
    (255, 255, 255),
];

fn dist(a: (u8, u8, u8), b: (u8, u8, u8)) -> u32 {
    let d = |x: u8, y: u8| {
        let d = i32::from(x) - i32::from(y);
        (d * d) as u32
    };
    d(a.0, b.0) + d(a.1, b.1) + d(a.2, b.2)
}

fn nearest_level(v: u8) -> usize {
    let mut best = 0usize;
    let mut best_d = u32::MAX;
    for (i, &l) in CUBE_LEVELS.iter().enumerate() {
        let d = u32::from(v.abs_diff(l));
        let d = d * d;
        if d < best_d {
            best_d = d;
            best = i;
        }
    }
    best
}

/// The RGB an xterm palette index renders as.
pub fn idx_to_rgb(i: u8) -> (u8, u8, u8) {
    match i {
        0..=15 => ANSI16[i as usize],
        16..=231 => {
            let n = i - 16;
            (
                CUBE_LEVELS[(n / 36) as usize],
                CUBE_LEVELS[((n / 6) % 6) as usize],
                CUBE_LEVELS[(n % 6) as usize],
            )
        }
        232..=255 => {
            let v = 8 + 10 * (i - 232);
            (v, v, v)
        }
    }
}

/// Nearest entry of the xterm 256-colour palette, considering both the cube
/// and the 24-step grey ramp.
///
/// Indices 0-15 are deliberately excluded as targets: their rendering depends
/// on the user's theme, so a colour the application chose precisely should not
/// land on one and become something else entirely.
pub fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    let target = (r, g, b);

    let cube_idx =
        16 + 36 * nearest_level(r) as u16 + 6 * nearest_level(g) as u16 + nearest_level(b) as u16;
    let cube_idx = cube_idx as u8;
    let cube_d = dist(target, idx_to_rgb(cube_idx));

    // The grey ramp runs 8, 18, ... 238.
    let avg = (u32::from(r) + u32::from(g) + u32::from(b)) / 3;
    let step = (((avg as i32 - 8) as f32 / 10.0).round()).clamp(0.0, 23.0) as u8;
    let grey_idx = 232 + step;
    let grey_d = dist(target, idx_to_rgb(grey_idx));

    if grey_d < cube_d { grey_idx } else { cube_idx }
}

/// Nearest of the sixteen ANSI system colours.
pub fn rgb_to_16(r: u8, g: u8, b: u8) -> u8 {
    let target = (r, g, b);
    let mut best = 0u8;
    let mut best_d = u32::MAX;
    for (i, &c) in ANSI16.iter().enumerate() {
        let d = dist(target, c);
        if d < best_d {
            best_d = d;
            best = i as u8;
        }
    }
    best
}

/// Map a colour onto what `caps.colors` can display.
///
/// [`Color::Default`] always survives: it means "the terminal's own default",
/// which every terminal has, and resolving it here would replace a themed
/// background with a guess.
pub fn down_convert(c: Color, caps: &TerminalCaps) -> Color {
    if caps.colors >= 16_777_216 {
        return c;
    }
    match c {
        Color::Default => Color::Default,
        Color::Rgb(r, g, b) => match caps.colors {
            n if n >= 256 => Color::Idx(rgb_to_256(r, g, b)),
            n if n >= 16 => Color::Idx(rgb_to_16(r, g, b)),
            // An 8-colour terminal has no bright half. Folding the high bit
            // off here rather than emitting `9x` is what the SGR writer then
            // compensates for with a bold promotion.
            _ => Color::Idx(rgb_to_16(r, g, b) & 0x07),
        },
        Color::Idx(i) => match caps.colors {
            n if n >= 256 => Color::Idx(i),
            n if n >= 16 => {
                if i < 16 {
                    Color::Idx(i)
                } else {
                    let (r, g, b) = idx_to_rgb(i);
                    Color::Idx(rgb_to_16(r, g, b))
                }
            }
            _ => {
                if i < 8 {
                    Color::Idx(i)
                } else if i < 16 {
                    // Keep the brightness signal: the SGR writer turns a bright
                    // index into bold + the base colour, which is how an
                    // 8-colour terminal has always shown bright.
                    Color::Idx(i)
                } else {
                    // A cube or grey index is a colour, not a brightness: it
                    // says no more than the RGB it is defined as. So the high
                    // bit comes off here exactly as it does for `Rgb` above —
                    // without the mask, a dark teal lands on 8 and the SGR
                    // writer promotes it to bold, putting text in a heavier
                    // font than the application ever asked for.
                    let (r, g, b) = idx_to_rgb(i);
                    Color::Idx(rgb_to_16(r, g, b) & 0x07)
                }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(colors: u32) -> TerminalCaps {
        TerminalCaps {
            truecolor: colors >= 16_777_216,
            colors,
            bracketed_paste: true,
            mouse_sgr: true,
            osc52: true,
            term_name: "test".to_string(),
        }
    }

    #[test]
    fn a_truecolor_terminal_changes_nothing() {
        let c = caps(16_777_216);
        assert_eq!(down_convert(Color::Rgb(1, 2, 3), &c), Color::Rgb(1, 2, 3));
        assert_eq!(down_convert(Color::Idx(200), &c), Color::Idx(200));
        assert_eq!(down_convert(Color::Default, &c), Color::Default);
    }

    #[test]
    fn the_cube_corners_map_to_their_own_indices() {
        assert_eq!(rgb_to_256(0, 0, 0), 16);
        assert_eq!(rgb_to_256(255, 255, 255), 231);
        assert_eq!(rgb_to_256(255, 0, 0), 196);
        assert_eq!(rgb_to_256(0, 255, 0), 46);
        assert_eq!(rgb_to_256(0, 0, 255), 21);
        assert_eq!(rgb_to_256(95, 135, 175), 16 + 36 + 12 + 3);
    }

    #[test]
    fn near_greys_prefer_the_grey_ramp_over_the_cube() {
        assert_eq!(rgb_to_256(0x80, 0x80, 0x80), 244);
        assert_eq!(rgb_to_256(8, 8, 8), 232);
        assert_eq!(rgb_to_256(238, 238, 238), 255);
    }

    #[test]
    fn every_palette_index_round_trips_through_rgb() {
        for i in 16u8..=231 {
            let (r, g, b) = idx_to_rgb(i);
            assert_eq!(rgb_to_256(r, g, b), i, "cube index {i}");
        }
        for i in 232u8..=255 {
            let (r, g, b) = idx_to_rgb(i);
            assert_eq!(rgb_to_256(r, g, b), i, "grey index {i}");
        }
    }

    #[test]
    fn a_256_colour_terminal_folds_rgb_and_keeps_indices() {
        let c = caps(256);
        assert_eq!(down_convert(Color::Rgb(255, 0, 0), &c), Color::Idx(196));
        assert_eq!(down_convert(Color::Idx(200), &c), Color::Idx(200));
    }

    #[test]
    fn a_16_colour_terminal_folds_both_rgb_and_high_indices() {
        let c = caps(16);
        // Pure red folds to 1, not to bright red 9. xterm renders bright red as
        // (255,85,85) — a salmon further from (255,0,0) than (170,0,0) is — so
        // nearest-in-sRGB lands on the dark half. Recording the surprise beats
        // special-casing it: the rule stays one sentence long and predictable.
        assert_eq!(down_convert(Color::Rgb(255, 0, 0), &c), Color::Idx(1));
        assert_eq!(down_convert(Color::Rgb(0, 0, 0), &c), Color::Idx(0));
        assert_eq!(down_convert(Color::Rgb(255, 255, 255), &c), Color::Idx(15));
        // The salmon itself does land on bright red.
        assert_eq!(down_convert(Color::Rgb(255, 85, 85), &c), Color::Idx(9));
        // An index below 16 is already displayable and passes through.
        assert_eq!(down_convert(Color::Idx(9), &c), Color::Idx(9));
        // 196 is pure red in the cube, and folds the same way as pure red RGB.
        assert_eq!(down_convert(Color::Idx(196), &c), Color::Idx(1));
    }

    #[test]
    fn an_8_colour_terminal_keeps_rgb_out_of_the_bright_half() {
        let c = caps(8);
        // RGB has no brightness signal worth preserving, so it folds to 0..8.
        for (r, g, b) in [(255, 85, 85), (255, 255, 255), (0, 255, 0)] {
            match down_convert(Color::Rgb(r, g, b), &c) {
                Color::Idx(n) => assert!(n < 8, "rgb({r},{g},{b}) gave bright index {n}"),
                other => panic!("expected an index, got {other:?}"),
            }
        }
        // A palette index that IS bright keeps its brightness for the SGR
        // writer to turn into bold.
        assert_eq!(down_convert(Color::Idx(9), &c), Color::Idx(9));
        // A high cube index has no brightness signal, so it folds plainly.
        assert_eq!(down_convert(Color::Idx(196), &c), Color::Idx(1));
    }

    /// The bright half is reserved for colours that asked for it.
    ///
    /// An 8-colour terminal has no bright half, so the SGR writer renders one
    /// as **bold plus the base colour**. That is the right answer for
    /// `Idx(8..16)`, where the application said "bright red" — and the wrong
    /// one for everything else, which would come out in a heavier font than
    /// the application ever asked for. A cube or grey index is just a colour:
    /// it carries no more brightness signal than the RGB it is defined as, and
    /// the RGB arm has always masked the high bit off for exactly that reason.
    ///
    /// This replaces an `n < 16` assertion that held for every possible
    /// implementation, including one returning a constant.
    #[test]
    fn no_index_above_the_ansi_sixteen_reaches_the_bright_half_on_8_colours() {
        let c = caps(8);
        for i in 16u8..=255 {
            match down_convert(Color::Idx(i), &c) {
                Color::Idx(n) => assert!(
                    n < 8,
                    "index {i} folded to {n}, in the bright half this terminal does not have"
                ),
                other => panic!("expected an index, got {other:?}"),
            }
        }
    }

    /// The other half of the same rule, and the reason it is not simply "mask
    /// everything": an explicitly bright ANSI index keeps its brightness, for
    /// the renderer to spend on a bold promotion.
    #[test]
    fn the_ansi_bright_half_survives_on_8_colours() {
        let c = caps(8);
        for i in 8u8..16 {
            assert_eq!(down_convert(Color::Idx(i), &c), Color::Idx(i));
        }
    }
}
