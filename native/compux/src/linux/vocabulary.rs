//! AT-SPI's numbers, as words.
//!
//! A role is read with the numeric `GetRole`, never `GetRoleName`: the name is the
//! toolkit's to spell, and GTK, Qt and Chromium spell it differently. The number is
//! at-spi2-core's `AtspiRole`, and the word this module gives it is that constant's
//! own name, lower case, one space between words — `ATSPI_ROLE_PUSH_BUTTON` is
//! `push button` — so one role reads the same from every toolkit. States arrive as
//! two 32-bit words, bit `n` in bit `n % 32` of word `n / 32`.
//!
//! Which numbers matter, and why ENABLED alone is not "enabled", come from
//! agent-desktop's measurements (ledger L3-6, `tree/vocabulary/`); no code of
//! theirs is copied here.

use crate::ax::States;

/// `AtspiRole`, in order: the word for role `n` is entry `n`.
const ROLES: [&str; 131] = [
    "invalid",
    "accelerator label",
    "alert",
    "animation",
    "arrow",
    "calendar",
    "canvas",
    "check box",
    "check menu item",
    "color chooser",
    "column header",
    "combo box",
    "date editor",
    "desktop icon",
    "desktop frame",
    "dial",
    "dialog",
    "directory pane",
    "drawing area",
    "file chooser",
    "filler",
    "focus traversable",
    "font chooser",
    "frame",
    "glass pane",
    "html container",
    "icon",
    "image",
    "internal frame",
    "label",
    "layered pane",
    "list",
    "list item",
    "menu",
    "menu bar",
    "menu item",
    "option pane",
    "page tab",
    "page tab list",
    "panel",
    "password text",
    "popup menu",
    "progress bar",
    "push button",
    "radio button",
    "radio menu item",
    "root pane",
    "row header",
    "scroll bar",
    "scroll pane",
    "separator",
    "slider",
    "spin button",
    "split pane",
    "status bar",
    "table",
    "table cell",
    "table column header",
    "table row header",
    "tearoff menu item",
    "terminal",
    "text",
    "toggle button",
    "tool bar",
    "tool tip",
    "tree",
    "tree table",
    "unknown",
    "viewport",
    "window",
    "extended",
    "header",
    "footer",
    "paragraph",
    "ruler",
    "application",
    "autocomplete",
    "editbar",
    "embedded",
    "entry",
    "chart",
    "caption",
    "document frame",
    "heading",
    "page",
    "section",
    "redundant object",
    "form",
    "link",
    "input method window",
    "table row",
    "tree item",
    "document spreadsheet",
    "document presentation",
    "document text",
    "document web",
    "document email",
    "comment",
    "list box",
    "grouping",
    "image map",
    "notification",
    "info bar",
    "level bar",
    "title bar",
    "block quote",
    "audio",
    "video",
    "definition",
    "article",
    "landmark",
    "log",
    "marquee",
    "math",
    "rating",
    "timer",
    "static",
    "math fraction",
    "math root",
    "subscript",
    "superscript",
    "description list",
    "description term",
    "description value",
    "footnote",
    "content deletion",
    "content insertion",
    "mark",
    "suggestion",
    "push button menu",
    "switch",
];

/// The role numbers this reader acts on.
pub const COMBO_BOX: u32 = 11;
pub const DIAL: u32 = 15;
pub const LABEL: u32 = 29;
pub const LIST_ITEM: u32 = 32;
pub const PASSWORD_TEXT: u32 = 40;
pub const PROGRESS_BAR: u32 = 42;
pub const SCROLL_BAR: u32 = 48;
pub const SCROLL_PANE: u32 = 49;
pub const SLIDER: u32 = 51;
pub const SPIN_BUTTON: u32 = 52;
pub const TABLE_CELL: u32 = 56;
pub const TERMINAL: u32 = 60;
pub const TEXT: u32 = 61;
pub const ENTRY: u32 = 79;
pub const TABLE_ROW: u32 = 90;
pub const TREE_ITEM: u32 = 91;
pub const LEVEL_BAR: u32 = 103;
pub const STATIC: u32 = 116;

/// The state bits this reader acts on.
pub const ACTIVE: u32 = 1;
pub const CHECKED: u32 = 4;
pub const EDITABLE: u32 = 7;
pub const ENABLED: u32 = 8;
pub const EXPANDABLE: u32 = 9;
pub const EXPANDED: u32 = 10;
pub const FOCUSABLE: u32 = 11;
pub const FOCUSED: u32 = 12;
pub const MULTI_LINE: u32 = 17;
pub const SELECTABLE: u32 = 22;
pub const SELECTED: u32 = 23;
pub const SENSITIVE: u32 = 24;
pub const SHOWING: u32 = 25;
pub const VERTICAL: u32 = 29;

/// A role number as a word. A number newer than this table is still named, by its
/// number, rather than passed off as one this table knows.
pub fn role(number: u32) -> String {
    match ROLES.get(number as usize) {
        Some(word) => (*word).to_string(),
        None => format!("role {number}"),
    }
}

/// Whether state `bit` is set in the words `GetState` answered.
pub fn has(words: &[u32], bit: u32) -> bool {
    words
        .get((bit / 32) as usize)
        .is_some_and(|word| word & (1 << (bit % 32)) != 0)
}

/// Whether a control can be operated now. ENABLED **or** SENSITIVE: GTK4 never sets
/// ENABLED, so reading it alone would grey out every GTK4 control (L3-6).
pub fn enabled(words: &[u32]) -> bool {
    has(words, ENABLED) || has(words, SENSITIVE)
}

/// A node's states as the wire names them. Each is reported only where the node
/// has such a state at all — focus where it is FOCUSABLE, selection where it is
/// SELECTABLE, expansion where it is EXPANDABLE — and `on` (CHECKED) only where the
/// caller asks, for a role that carries a check state.
pub fn states(words: &[u32], on: bool) -> States {
    let where_it_can = |can: u32, is: u32| has(words, can).then(|| has(words, is));
    States {
        focused: where_it_can(FOCUSABLE, FOCUSED),
        selected: where_it_can(SELECTABLE, SELECTED),
        expanded: where_it_can(EXPANDABLE, EXPANDED),
        on: on.then(|| has(words, CHECKED)),
    }
}

/// Whether a control's value is a number, read through `Value`.
pub fn numeric(role: u32) -> bool {
    matches!(
        role,
        SLIDER | SPIN_BUTTON | PROGRESS_BAR | SCROLL_BAR | LEVEL_BAR | DIAL
    )
}

/// Whether a control's value is text, read through `Text`. A text or terminal node
/// counts only while it is editable: a read-only one is content, not a field.
pub fn textual(role: u32, editable: bool) -> bool {
    matches!(role, ENTRY | COMBO_BOX) || (matches!(role, TEXT | TERMINAL) && editable)
}

/// Whether a node is a row of a list, a tree or a table: what GTK leaves nameless
/// and names by the label inside it.
pub fn row(role: u32) -> bool {
    matches!(role, LIST_ITEM | TREE_ITEM | TABLE_CELL | TABLE_ROW)
}

/// Whether a node is text that names what it sits in.
pub fn caption(role: u32) -> bool {
    matches!(role, LABEL | STATIC)
}

/// Whether a numeric control takes a new value. A progress or level bar only
/// reports one.
pub fn adjustable(role: u32) -> bool {
    matches!(role, SLIDER | SPIN_BUTTON | SCROLL_BAR | DIAL)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(bits: &[u32]) -> [u32; 2] {
        let mut words = [0u32; 2];
        for bit in bits {
            words[(bit / 32) as usize] |= 1 << (bit % 32);
        }
        words
    }

    // A state is said only where the node has it: a label that cannot take focus
    // is neither focused nor unfocused, and a check state is read only when asked.
    #[test]
    fn a_state_is_reported_only_where_the_node_can_have_it() {
        let plain = states(&words(&[ENABLED, SHOWING]), true);
        assert_eq!(
            plain,
            States {
                on: Some(false),
                ..States::default()
            }
        );
        let row = states(
            &words(&[
                FOCUSABLE, FOCUSED, SELECTABLE, SELECTED, EXPANDABLE, CHECKED,
            ]),
            false,
        );
        assert_eq!(
            row,
            States {
                focused: Some(true),
                selected: Some(true),
                expanded: Some(false),
                on: None,
            }
        );
        assert_eq!(states(&words(&[CHECKED]), true).on, Some(true));
    }

    // The table is at-spi2-core's enum: spot-check the numbers the ledger and the
    // fixtures name, at both ends of it.
    #[test]
    fn a_role_number_reads_as_its_constant() {
        assert_eq!(role(0), "invalid");
        assert_eq!(role(7), "check box");
        assert_eq!(role(23), "frame");
        assert_eq!(role(40), "password text");
        assert_eq!(role(43), "push button");
        assert_eq!(role(61), "text");
        assert_eq!(role(75), "application");
        assert_eq!(role(79), "entry");
        assert_eq!(role(88), "link");
        assert_eq!(role(91), "tree item");
        assert_eq!(role(116), "static");
        assert_eq!(role(129), "push button menu");
        assert_eq!(role(130), "switch");
    }

    #[test]
    fn a_role_newer_than_the_table_is_named_by_its_number() {
        assert_eq!(role(131), "role 131");
    }

    #[test]
    fn a_state_bit_is_read_from_its_own_word() {
        let words = [1 << ENABLED, 0];
        assert!(has(&words, ENABLED));
        assert!(!has(&words, SENSITIVE));
        assert!(has(&[0, 1 << 1], 33));
        assert!(!has(&[0], 33), "a word that is not there holds nothing");
    }

    // L3-6: GTK4 sets SENSITIVE and never ENABLED.
    #[test]
    fn enabled_is_either_bit() {
        assert!(enabled(&[1 << ENABLED, 0]));
        assert!(enabled(&[1 << SENSITIVE, 0]));
        assert!(!enabled(&[1 << SHOWING, 0]));
    }

    #[test]
    fn only_an_editable_text_node_carries_a_text_value() {
        assert!(textual(ENTRY, false));
        assert!(textual(TEXT, true));
        assert!(!textual(TEXT, false));
        assert!(!textual(PASSWORD_TEXT, true));
    }
}
