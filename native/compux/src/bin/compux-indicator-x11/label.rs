//! The badge's one piece of untrusted text, made safe to draw.
//!
//! `app` and `title` are chosen by another application. A window can be titled with
//! newlines, control characters, a right-to-left override, or a megabyte of
//! anything, and none of that may change what the badge looks like. Every character
//! that is not a printable one becomes a space, runs of spaces collapse, and each
//! half is cut to a length that fits. The same rules as the macOS badge's
//! `targetLabel`, counted in characters rather than grapheme clusters.

/// The most characters of the application's name the label keeps.
pub const APP_LIMIT: usize = 28;
/// The most characters of the window's title the label keeps.
pub const TITLE_LIMIT: usize = 44;

/// "App — Title", either half alone when the other is empty, or "No target".
pub fn target_label(app: &str, title: &str) -> String {
    let application = cut(&flatten(app), APP_LIMIT);
    let window = cut(&flatten(title), TITLE_LIMIT);
    match (application.is_empty(), window.is_empty()) {
        (true, true) => "No target".to_string(),
        (false, true) => application,
        (true, false) => window,
        (false, false) => format!("{application} — {window}"),
    }
}

/// One line of printable characters, whatever arrived. Each unprintable character
/// becomes a space rather than being dropped, so two words never run together.
fn flatten(raw: &str) -> String {
    let spaced: String = raw
        .chars()
        .map(|c| if unprintable(c) { ' ' } else { c })
        .collect();
    spaced.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Controls (newlines, tabs), the format characters that steer direction or
/// joining or hide text, the line and paragraph separators, and private use.
fn unprintable(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{061C}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{E000}'..='\u{F8FF}'
                | '\u{E0000}'..='\u{E007F}'
                | '\u{F0000}'..='\u{10FFFF}'
        )
}

/// Cut to a character count, marking that something was cut.
fn cut(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit.saturating_sub(1).max(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_halves_are_joined_and_either_stands_alone() {
        assert_eq!(target_label("Files", "Home"), "Files — Home");
        assert_eq!(target_label("Files", ""), "Files");
        assert_eq!(target_label("", "Home"), "Home");
        assert_eq!(target_label("", ""), "No target");
        assert_eq!(target_label("  ", "\n"), "No target");
    }

    #[test]
    fn line_breaks_and_controls_become_single_spaces() {
        assert_eq!(
            target_label("A\tB", "one\r\ntwo\u{2028}three  four"),
            "A B — one two three four"
        );
        assert_eq!(target_label("\u{7}bell\u{0}", ""), "bell");
    }

    #[test]
    fn a_direction_override_cannot_turn_the_label_around() {
        let label = target_label("", "abc\u{202E}fed\u{2066}x\u{200F}y\u{FEFF}z");
        assert_eq!(label, "abc fed x y z");
        assert!(!label.chars().any(|c| matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')));
    }

    #[test]
    fn private_use_characters_are_not_drawn_as_whatever_a_font_put_there() {
        assert_eq!(target_label("", "a\u{E000}b\u{F0000}c"), "a b c");
    }

    #[test]
    fn each_half_is_cut_to_its_own_length_and_says_so() {
        let label = target_label(&"a".repeat(40), &"b".repeat(80));
        let (app, title) = label.split_once(" — ").expect("both halves");
        assert_eq!(app.chars().count(), APP_LIMIT);
        assert!(app.ends_with('…'));
        assert_eq!(title.chars().count(), TITLE_LIMIT);
        assert!(title.ends_with('…'));
        assert_eq!(
            target_label(&"a".repeat(APP_LIMIT), "").chars().count(),
            APP_LIMIT
        );
    }

    #[test]
    fn a_megabyte_title_costs_a_bounded_label() {
        let label = target_label("", &"é\n".repeat(500_000));
        assert!(label.chars().count() <= TITLE_LIMIT);
    }
}
