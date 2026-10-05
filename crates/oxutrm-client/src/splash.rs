//! The startup splash: the ox head tuning in like a badly received TV.
//!
//! Shown once, on a fresh connect, as a full-screen layer-1 [`Overlay`]: a
//! short burst of interference across the whole screen that settles into the
//! clean logo, which is then held for a moment. Pure: [`splash`] maps the
//! time since the splash began, the screen size and a seed to one frame, and
//! the same three always give the same frame. The session decides when it
//! starts and ends; nothing here knows about keys, frames or clocks.

// This runs while a client session owns the screen: nothing here may print,
// or it lands raw on the painted raw-mode terminal.
#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]

use std::time::Duration;

use oxutrm_proto::{Attrs, Cell, CellText, Color, TermSize};

use crate::overlay::Overlay;

/// The smallest screen the splash is shown on: the art and a cell either
/// side of it, and a row above and below. Anything smaller skips it.
pub const MIN_SCREEN: TermSize = TermSize { cols: 34, rows: 20 };

/// How long one frame of the interference lasts.
pub const FRAME: Duration = Duration::from_millis(40);

/// How many frames the interference takes to settle. Frame `FRAMES` is the
/// clean logo.
pub const FRAMES: u32 = 20;

/// How long the clean logo is held once the interference has settled.
pub const HOLD: Duration = Duration::from_millis(1500);

/// Whether the splash is shown at all on a screen of `size`.
pub fn fits(size: TermSize) -> bool {
    size.cols >= MIN_SCREEN.cols && size.rows >= MIN_SCREEN.rows
}

/// Which frame `elapsed` into the splash falls on: `0..=FRAMES`, and
/// `FRAMES` from the moment the interference has settled on. Two instants
/// with the same frame paint the same picture, so a caller repaints only
/// when this changes.
pub fn frame_of(elapsed: Duration) -> u32 {
    let n = elapsed.as_nanos() / FRAME.as_nanos();
    u32::try_from(n).unwrap_or(FRAMES).min(FRAMES)
}

/// Whether the splash has run its course `elapsed` after it began: the
/// interference settled and the logo held for [`HOLD`].
pub fn settled(elapsed: Duration) -> bool {
    elapsed >= FRAME * FRAMES + HOLD
}

/// The splash `elapsed` after it began, covering the whole of a `size`
/// screen, with `caption` -- which session this is, and how it is reached --
/// centred one blank row under the name.
///
/// Opaque: every cell is drawn, blank ones as spaces, so nothing of the
/// remote screen shows through. No colour is set -- the terminal's own text
/// colour is used -- and the flicker is the whole picture switching between
/// bold and dim. The caption is plain text in that same colour, tunes in
/// with the name, and is left out when the screen has no row to spare for
/// it; one wider than the screen is cut. It is shown as given, so the caller
/// makes it legible.
pub fn splash(elapsed: Duration, size: TermSize, seed: u64, caption: &str) -> Overlay {
    let frame = frame_of(elapsed);
    let t = f64::from(frame) / f64::from(FRAMES);
    // Each frame draws its own noise, independently of the one before, as a
    // badly tuned set does: seeded by the frame, so a repaint of the same
    // frame is the same picture.
    let mut rng = Rng::new(seed ^ u64::from(frame).wrapping_mul(0x9e37_79b9_7f4a_7c15));

    let attrs = if t > 0.8 || rng.chance(0.6) {
        Attrs::BOLD
    } else {
        Attrs::DIM
    };
    let p = 0.30 * (1.0 - t).powf(1.5); // snow density
    let jitter = 0.45 * (1.0 - t); // chance a row tears sideways
    let bar = (t * 2.5) % 1.0; // the rolling bar, as a fraction of the screen
    let noisy = t < 0.95;

    let (rows, cols) = (usize::from(size.rows), usize::from(size.cols));
    let art_rows = HEAD.len() + GAP_ROWS + NAME.len();
    // The caption's two rows, the blank one and its own, count towards the
    // centring only when there is room for them and a row either side.
    let captioned = !caption.is_empty() && rows >= art_rows + CAPTION_ROWS + 2;
    let block_rows = art_rows + if captioned { CAPTION_ROWS } else { 0 };
    let top = rows.saturating_sub(block_rows) / 2;
    let left = cols.saturating_sub(ART_COLS) / 2;
    let caption_row = top + art_rows + 1;
    let caption: Vec<char> = if captioned && t >= NAME_FROM {
        caption.chars().take(cols).collect()
    } else {
        Vec::new()
    };
    let caption_left = cols.saturating_sub(caption.len()) / 2;

    let mut cells = Vec::with_capacity(rows * cols);
    for r in 0..rows {
        let shift: isize = if rng.chance(jitter) {
            [-3, -2, -1, 1, 2, 3][rng.below(6)]
        } else {
            0
        };
        #[allow(clippy::cast_precision_loss)] // a screen has far fewer rows than 2^52
        let in_bar = noisy && (r as f64 / rows as f64 - bar).abs() < 0.07;
        for c in 0..cols {
            // The image moves with the tear; the snow does not.
            let ic = c
                .checked_add_signed(-shift)
                .and_then(|c| c.checked_sub(left));
            let ir = r.checked_sub(top);
            let mut v = match (ir, ic) {
                (Some(ir), Some(ic)) if ir < art_rows && ic < ART_COLS => art_cell(ir, ic, t),
                _ => 0,
            };
            if noisy {
                if in_bar {
                    v ^= rng.snow(0.5);
                }
                if p > 0.005 {
                    v ^= rng.snow(p);
                }
            }
            // The caption is text, not dots: it covers whatever the noise
            // put under it.
            let text = (r == caption_row)
                .then(|| c.checked_sub(caption_left))
                .flatten()
                .and_then(|i| caption.get(i));
            cells.push(match text {
                Some(ch) => Cell {
                    text: CellText::new(ch.encode_utf8(&mut [0; 4])),
                    fg: Color::Default,
                    bg: Color::Default,
                    attrs: Attrs::empty(),
                },
                None => Cell {
                    text: braille(v),
                    fg: Color::Default,
                    bg: Color::Default,
                    attrs,
                },
            });
        }
    }

    Overlay {
        row: 0,
        col: 0,
        rows: size.rows,
        cols: size.cols,
        cells,
    }
}

/// One cell's text: the braille pattern of `dots`, or a space for none --
/// an empty braille cell would be a glyph some fonts draw as a box.
fn braille(dots: u8) -> CellText {
    match char::from_u32(0x2800 + u32::from(dots)) {
        Some(ch) if dots != 0 => CellText::new(ch.encode_utf8(&mut [0; 4])),
        _ => CellText::const_new(" "),
    }
}

/// A tiny xorshift generator: the noise needs to look random, not to be
/// random, and has to come out the same for a test.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        // splitmix64's finaliser, so neighbouring seeds start far apart and a
        // zero seed does not lock xorshift at zero.
        let mut z = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        Rng((z ^ (z >> 31)) | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Uniform in `[0, 1)`.
    #[allow(clippy::cast_precision_loss)] // 53 bits, exactly representable
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }

    /// Uniform in `0..n`, for a small `n`.
    #[allow(clippy::cast_possible_truncation)] // n is small, the result below it
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    /// A braille cell with each of its eight dots on with probability `p`.
    fn snow(&mut self, p: f64) -> u8 {
        (0..8).fold(0, |v, bit| if self.chance(p) { v | 1 << bit } else { v })
    }
}

/// Blank rows between the head and the name.
const GAP_ROWS: usize = 2;

/// The caption's rows under the name: a blank one, then the caption.
const CAPTION_ROWS: usize = 2;

/// How far into the tuning-in (0 to 1) the name -- and with it the caption
/// -- appears.
const NAME_FROM: f64 = 0.6;

/// The art's width in cells; every line of [`HEAD`] and [`NAME`] is this wide.
const ART_COLS: usize = 32;

// The art: the ox head from the oxulnk logo's SVG paths and the name in a
// small dot font, as braille (2x4 dots a cell). GENERATED by
// `python3 tools/oxlogo.py`, whose output is pasted below unchanged; change
// the script and regenerate rather than editing these by hand. An empty cell
// is U+2800, drawn as a space.
// Generated by tools/oxlogo.py -- edit that, not this.
pub(crate) const HEAD: [&str; 13] = [
    "⠀⠀⠀⣼⡖⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢲⣧⠀⠀⠀",
    "⠀⠀⣼⣿⠇⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠸⣿⣧⠀⠀",
    "⠀⣼⣿⣿⣀⡀⠀⠀⠀⠀⠀⢠⣤⣤⣤⣤⣤⣤⣤⣤⡄⠀⠀⠀⠀⠀⢀⣀⣿⣿⣧⠀",
    "⠼⣿⣿⣿⣿⣿⣿⣿⡇⣿⣿⡌⣿⣿⣿⣿⣿⣿⣿⣿⢡⣿⣿⢸⣿⣿⣿⣿⣿⣿⣿⠧",
    "⠀⠀⠈⠉⠙⠛⠿⠿⡇⣿⣿⣧⢹⣿⣿⣿⣿⣿⣿⡏⣼⣿⣿⢸⠿⠿⠛⠋⠉⠁⠀⠀",
    "⠀⠀⠀⠀⢤⣤⣴⣶⡆⣿⢿⣿⣮⣭⣭⣭⣭⣭⣭⣵⣿⡿⣿⢰⣶⣦⣤⡤⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠉⠉⠉⠁⣿⠀⠙⢿⣿⣿⣿⣿⣿⣿⡿⠋⠀⣿⠈⠉⠉⠉⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⣿⣿⣿⡆⣿⣿⣿⣿⣿⣿⢰⣿⣿⣿⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⣿⣿⣿⡇⣿⣿⣿⣿⣿⣿⢸⣿⣿⣿⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠘⣿⣿⣇⢻⣿⣿⣿⣿⡟⣸⣿⣿⠃⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠈⢟⣥⣶⣶⣶⣶⣶⣶⣬⡻⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠙⢿⣿⣿⣿⣿⣿⣿⡿⠋⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
];

pub(crate) const NAME: [&str; 3] = [
    "⠀⠀⣾⠛⠛⠛⣷⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠛⠛⣿⠛⠛⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⣿⠀⠀⠀⣿⠀⠻⣦⣴⠟⠀⣿⠀⠀⣿⠀⠀⣿⠀⠀⣿⠞⠛⠀⣿⠛⣿⠛⣷⠀",
    "⠀⠀⢿⣤⣤⣤⡿⠀⣴⠟⠻⣦⠀⢿⣤⣤⣿⠀⠀⣿⠀⠀⣿⠀⠀⠀⣿⠀⣿⠀⣿⠀",
];

/// The art's dots at row `ir`, column `ic` of the art block, `t` into the
/// tuning-in (0 = no signal, 1 = locked): the braille bits, 0 for none.
fn art_cell(ir: usize, ic: usize, t: f64) -> u8 {
    let line = if ir < HEAD.len() {
        HEAD[ir]
    } else if let Some(n) = ir.checked_sub(HEAD.len() + GAP_ROWS) {
        // The name tunes in late.
        match NAME.get(n) {
            Some(line) if t >= NAME_FROM => line,
            _ => return 0,
        }
    } else {
        return 0;
    };
    line.chars()
        .nth(ic)
        .and_then(|ch| u8::try_from(u32::from(ch).checked_sub(0x2800)?).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: TermSize = TermSize { cols: 80, rows: 24 };

    fn at(frame: u32) -> Duration {
        FRAME * frame
    }

    fn text_of(o: &Overlay) -> String {
        o.cells
            .chunks(o.cols as usize)
            .map(|row| {
                row.iter()
                    .map(|c| c.text.as_str())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Dots that are on, over the whole frame.
    fn dots(o: &Overlay) -> u32 {
        o.cells
            .iter()
            .filter_map(|c| c.text.chars().next())
            .filter(|ch| ('\u{2800}'..='\u{28ff}').contains(ch))
            .map(|ch| (ch as u32 - 0x2800).count_ones())
            .sum()
    }

    #[test]
    fn snapshot_settled_80x24() {
        insta::assert_snapshot!(text_of(&splash(at(FRAMES), SCREEN, 7, "")));
    }

    #[test]
    fn the_splash_covers_the_whole_screen_opaquely() {
        for frame in [0, FRAMES / 2, FRAMES] {
            let o = splash(at(frame), SCREEN, 7, "");
            assert_eq!((o.row, o.col, o.rows, o.cols), (0, 0, 24, 80));
            assert_eq!(o.cells.len(), 80 * 24);
            assert!(
                o.cells.iter().all(|c| !c.text.is_empty()),
                "frame {frame}: an empty cell lets the remote screen show through"
            );
            assert!(
                o.cells.iter().all(|c| c.text != "\u{2800}"),
                "frame {frame}: an empty braille cell is drawn instead of a space"
            );
        }
    }

    #[test]
    fn no_colour_is_set() {
        for frame in [0, FRAMES / 2, FRAMES] {
            let o = splash(at(frame), SCREEN, 7, "");
            assert!(
                o.cells
                    .iter()
                    .all(|c| c.fg == Color::Default && c.bg == Color::Default),
                "frame {frame} sets a colour"
            );
        }
    }

    /// The settled logo is the art and nothing else: every dot in it is one
    /// of the art's, in the middle of the screen.
    #[test]
    fn the_settled_logo_is_the_art_centred_and_bold() {
        let o = splash(at(FRAMES), SCREEN, 7, "");
        let art: u32 = HEAD
            .iter()
            .chain(NAME.iter())
            .flat_map(|l| l.chars())
            .map(|ch| (ch as u32 - 0x2800).count_ones())
            .sum();
        assert_eq!(dots(&o), art, "the settled frame is not exactly the art");
        let text = text_of(&o);
        let lines: Vec<&str> = text.lines().collect();
        // 24 rows, 18 of art: 3 above. 80 columns, 32 of art: 24 to the left.
        assert!(lines[2].is_empty(), "the row above the art is not blank");
        assert_eq!(
            lines[3],
            format!("{}{}", " ".repeat(24), HEAD[0].replace('\u{2800}', " ")).trim_end()
        );
        assert_eq!(
            lines[18],
            format!("{}{}", " ".repeat(24), NAME[0].replace('\u{2800}', " ")).trim_end()
        );
        assert!(
            o.cells.iter().all(|c| c.attrs == Attrs::BOLD),
            "the settled logo is not bold"
        );
    }

    #[test]
    fn the_same_seed_gives_the_same_frame_and_another_seed_another() {
        assert_eq!(splash(at(3), SCREEN, 7, ""), splash(at(3), SCREEN, 7, ""));
        assert_ne!(splash(at(3), SCREEN, 7, ""), splash(at(3), SCREEN, 8, ""));
        assert_ne!(
            splash(at(3), SCREEN, 7, ""),
            splash(at(4), SCREEN, 7, ""),
            "consecutive frames of the interference are the same picture"
        );
    }

    /// Snow is densest at the start and gone by the end.
    #[test]
    fn the_interference_dies_down_as_it_tunes_in() {
        let early = dots(&splash(at(0), SCREEN, 7, ""));
        let late = dots(&splash(at(FRAMES * 3 / 4), SCREEN, 7, ""));
        let art = dots(&splash(at(FRAMES), SCREEN, 7, ""));
        assert!(
            early > late && late > art,
            "early {early}, late {late}, settled {art}"
        );
        // 0.30 of 8 dots over 1920 cells is ~4600 dots of snow alone.
        assert!(early > 3000, "too little snow at the start: {early}");
    }

    /// The name tunes in late: until t = 0.6 its rows carry no art. Checked
    /// on the art the noise is laid over, since snow lands everywhere.
    #[test]
    fn the_name_is_blank_until_late() {
        let name_row = HEAD.len() + GAP_ROWS;
        let any = |t: f64| (0..32).any(|c| art_cell(name_row + 1, c, t) != 0);
        assert!(!any(0.55), "the name showed before t = 0.6");
        assert!(any(0.6), "the name never showed");
        assert!(
            (0..32).any(|c| art_cell(0, c, 0.0) != 0),
            "the head is late too"
        );
    }

    /// The flicker: some frames bold, some dim, every cell of a frame alike.
    #[test]
    fn early_frames_flicker_between_bold_and_dim() {
        let mut seen = Vec::new();
        for seed in 0..20 {
            let o = splash(at(2), SCREEN, seed, "");
            let a = o.cells[0].attrs;
            assert!(o.cells.iter().all(|c| c.attrs == a), "a frame mixes styles");
            seen.push(a);
        }
        assert!(seen.contains(&Attrs::BOLD), "never bold: {seen:?}");
        assert!(seen.contains(&Attrs::DIM), "never dim: {seen:?}");
    }

    #[test]
    fn frames_follow_the_clock_and_stop_at_the_last() {
        assert_eq!(frame_of(Duration::ZERO), 0);
        assert_eq!(frame_of(FRAME - Duration::from_millis(1)), 0);
        assert_eq!(frame_of(FRAME), 1);
        assert_eq!(frame_of(at(FRAMES)), FRAMES);
        assert_eq!(frame_of(Duration::from_secs(60)), FRAMES);
    }

    #[test]
    fn it_settles_after_the_interference_and_the_hold() {
        let end = at(FRAMES) + HOLD;
        assert!(!settled(end - Duration::from_millis(1)));
        assert!(settled(end));
    }

    #[test]
    fn it_fits_from_34x20_up() {
        assert!(fits(TermSize { cols: 34, rows: 20 }));
        assert!(fits(SCREEN));
        assert!(!fits(TermSize { cols: 33, rows: 20 }));
        assert!(!fits(TermSize { cols: 34, rows: 19 }));
    }

    const CAPTION: &str = "resumed session 3ff1218f \u{b7} IPv4 punched";

    /// The caption sits one blank row under the name, centred on the screen
    /// on its own width, not on the art's: a caption wider than the art
    /// still reads as centred.
    #[test]
    fn the_caption_is_centred_under_the_name() {
        let o = splash(at(FRAMES), SCREEN, 7, CAPTION);
        let text = text_of(&o);
        let lines: Vec<&str> = text.lines().collect();
        // 24 rows, 18 of art + a gap + the caption = 20: 2 above.
        assert!(
            lines[1].is_empty(),
            "the row above the art is not blank: {text}"
        );
        assert_eq!(
            lines[2],
            format!("{}{}", " ".repeat(24), HEAD[0].replace('\u{2800}', " ")).trim_end(),
            "the block with its caption is not centred vertically: {text}"
        );
        // Head 2..=14, the gap, the name 17..=19, a blank row, the caption.
        assert!(
            lines[20].is_empty(),
            "no blank row between name and caption: {text}"
        );
        let width = CAPTION.chars().count();
        assert_eq!(
            lines[21],
            format!("{}{CAPTION}", " ".repeat((80 - width) / 2)),
            "{text}"
        );
        let row = &o.cells[21 * 80..22 * 80];
        let first = (80 - width) / 2;
        assert!(
            row[first..first + width].iter().all(|c| c.attrs.is_empty()),
            "the caption is not plain text"
        );
    }

    #[test]
    fn the_caption_tunes_in_with_the_name() {
        let early = text_of(&splash(at(FRAMES / 2), SCREEN, 7, CAPTION));
        assert!(!early.contains("resumed"), "{early}");
        let late = text_of(&splash(at(FRAMES * 3 / 4), SCREEN, 7, CAPTION));
        assert!(late.contains(CAPTION), "{late}");
    }

    /// A caption wider than the screen is cut, never wrapped or spilled.
    #[test]
    fn a_caption_wider_than_the_screen_is_cut() {
        let narrow = TermSize { cols: 34, rows: 22 };
        let o = splash(at(FRAMES), narrow, 7, CAPTION);
        assert_eq!(o.cells.len(), 34 * 22);
        let text = text_of(&o);
        assert!(
            text.contains("resumed session 3ff1218f \u{b7} IPv4 pu"),
            "{text}"
        );
    }

    /// At the smallest splash screen there is no row to spare: the logo is
    /// shown as before and the caption is left out.
    #[test]
    fn without_room_for_it_the_caption_is_left_out() {
        let tight = TermSize { cols: 80, rows: 20 };
        let with = splash(at(FRAMES), tight, 7, CAPTION);
        assert_eq!(with, splash(at(FRAMES), tight, 7, ""));
    }
}
