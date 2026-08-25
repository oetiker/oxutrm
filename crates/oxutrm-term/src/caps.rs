//! What the local terminal can show, and what the child should be told.

use oxutrm_proto::TerminalCaps;

/// Detect the local terminal's capabilities from the environment.
///
/// This describes the terminal **oxutrm is running inside**, so it is the
/// client's answer about itself. The host never calls it.
pub fn detect_caps() -> TerminalCaps {
    let term = std::env::var("TERM").unwrap_or_else(|_| "dumb".to_owned());
    let colorterm = std::env::var("COLORTERM").unwrap_or_default();
    caps_from(&term, &colorterm)
}

/// The whole of the decision, as a pure function of the two variables.
///
/// Split out so it can be tested directly: `set_var` is `unsafe` in edition
/// 2024, and mutating process-global state would race every other test in the
/// binary regardless.
fn caps_from(term: &str, colorterm: &str) -> TerminalCaps {
    let truecolor = colorterm == "truecolor" || colorterm == "24bit";
    let colors = if truecolor {
        16_777_216
    } else if term.contains("256") {
        256
    } else if term == "dumb" {
        8
    } else {
        16
    };

    TerminalCaps {
        truecolor,
        colors,
        // Every terminal worth attaching from has had these for a decade, and
        // a terminal that lacks one ignores the escape rather than breaking.
        // `dumb` is the exception and is treated as such.
        bracketed_paste: term != "dumb",
        mouse_sgr: term != "dumb",
        osc52: term != "dumb",
        term_name: term.to_owned(),
    }
}

/// The `TERM` and `COLORTERM` a child of this emulator should be given.
///
/// **Takes no arguments, and that is the point.** It is derived solely from
/// what `alacritty_terminal` emulates. The client's capabilities must not
/// influence it: the child's `TERM` cannot change when a differently-capable
/// client reattaches, and down-converting here would permanently degrade the
/// host's state for every future client. All capability adaptation happens in
/// the client, against a host state that stayed full fidelity.
pub fn negotiate_term() -> (String, Option<String>) {
    // The emulator handles 256 colours and 24-bit SGR, so this is honest
    // rather than hopeful. `xterm-256color` is present on every system that
    // has a terminfo database at all, which `alacritty` itself is not.
    ("xterm-256color".to_owned(), Some("truecolor".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_child_is_told_the_same_thing_regardless_of_who_is_attached() {
        // The signature takes no arguments precisely so this cannot vary.
        // A client that can only do 16 colours reattaching must not shrink
        // the host's state - it down-converts locally instead.
        let a = negotiate_term();
        let b = negotiate_term();
        assert_eq!(a, b);
        assert_eq!(a.0, "xterm-256color");
        assert_eq!(a.1.as_deref(), Some("truecolor"));
    }

    #[test]
    fn a_term_name_that_exists_everywhere_is_used() {
        // `alacritty` is not in most terminfo databases; xterm-256color is.
        // A child that cannot find its terminfo entry degrades to something
        // close to unusable.
        let (term, _) = negotiate_term();
        assert!(!term.contains("alacritty"));
        assert!(term.contains("256color"));
    }

    #[test]
    fn truecolor_is_believed_only_when_colorterm_says_so() {
        let caps = caps_from("xterm-256color", "truecolor");
        assert!(caps.truecolor);
        assert_eq!(caps.colors, 16_777_216);
        assert_eq!(caps.term_name, "xterm-256color");

        let caps = caps_from("xterm-256color", "24bit");
        assert!(caps.truecolor, "24bit is the other spelling in the wild");

        let caps = caps_from("xterm-256color", "");
        assert!(!caps.truecolor);
        assert_eq!(caps.colors, 256, "the name still says 256");
    }

    #[test]
    fn a_dumb_terminal_is_taken_at_its_word() {
        let caps = caps_from("dumb", "");
        assert_eq!(caps.colors, 8);
        assert!(!caps.bracketed_paste);
        assert!(!caps.mouse_sgr);
        assert!(!caps.osc52);
    }

    #[test]
    fn an_ordinary_terminal_gets_the_modern_defaults() {
        // Bracketed paste, SGR mouse and OSC 52 have been universal for a
        // decade, and a terminal without one ignores the escape rather than
        // breaking. Assuming them is safer than assuming their absence.
        let caps = caps_from("screen", "");
        assert_eq!(caps.colors, 16);
        assert!(caps.bracketed_paste && caps.mouse_sgr && caps.osc52);
    }

    #[test]
    fn detect_caps_reports_this_process_environment() {
        // Whatever TERM happens to be here, the result must be self-consistent
        // and must name something.
        let caps = detect_caps();
        assert!(!caps.term_name.is_empty());
        assert!(caps.colors >= 8);
        assert_eq!(caps.truecolor, caps.colors == 16_777_216);
    }
}
