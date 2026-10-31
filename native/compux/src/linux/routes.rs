//! Which verbs an AT-SPI node has a route for, from what one read of it said.
//!
//! Pure: the reader reads a node once, and everything here decides from that read,
//! so a listing costs no message per route and every rule is tested without a bus.
//! Each route is the node's OWN: an action it names, an interface it lists, a state
//! it reads. Nothing is inferred from what its role suggests (agent-desktop L3-19:
//! the toolkits name their actions differently, and none names `expand`).

use super::vocabulary::{self, EDITABLE, EXPANDABLE, FOCUSABLE};
use crate::ax::{Expander, Routes, PRESS, SCROLL_TO_VISIBLE};

/// The action names that press a control, compared without case: GTK `click`,
/// Chromium `press`, `jump` (a link) and `check`, Qt `Press` (L3-20, L18-16). Never
/// `doDefault`, which Chromium advertises on plain text where it does nothing
/// (L18-16), nor `clickAncestor`, whose ancestor is the control.
const PRESS_NAMES: [&str; 6] = ["click", "press", "activate", "jump", "check", "open"];

/// Qt names a check box's press `Toggle` (L3-19). Only a control that carries a
/// check state is pressed by it: on a Qt list item the same name toggles the
/// selection.
const TOGGLE: &str = "toggle";

/// The actions that expand and collapse the node they belong to, in one toggle:
/// a GTK 3 tree cell's and a GTK 4 tree row's. A node with neither is expanded by
/// its press, which is all a GTK 3 expander has (L4-6).
const EXPANDING: [&str; 2] = ["expand or contract", "listitem.toggle-expand"];

/// The toolkit an application was made with, as far as a route depends on it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Toolkit {
    Gtk3,
    /// Has no screen positions, and answers `GrabFocus` `NotSupported` (L4-16).
    Gtk4,
    Qt,
    /// Implements `Component.ScrollTo`.
    Chromium,
    #[default]
    Other,
}

impl Toolkit {
    /// The toolkit an application's `ToolkitName` and `Version` name.
    pub fn of(name: &str, version: &str) -> Toolkit {
        let major = version.split('.').next();
        match name.to_ascii_lowercase().as_str() {
            "gtk" if major == Some("4") => Toolkit::Gtk4,
            "gtk" => Toolkit::Gtk3,
            "qt" => Toolkit::Qt,
            "chromium" => Toolkit::Chromium,
            _ => Toolkit::Other,
        }
    }
}

/// What one read of a node said, as far as its routes go.
#[derive(Clone, Copy, Debug)]
pub struct Read<'a> {
    pub role: u32,
    /// The state words `GetState` answered.
    pub states: &'a [u32],
    pub component: bool,
    pub text: bool,
    pub value: bool,
    pub editable_text: bool,
    /// Its actions that read, by index.
    pub actions: &'a [(i32, String)],
    pub toolkit: Toolkit,
    /// It sits inside a scroll pane.
    pub scrolled: bool,
}

/// The index of the action that presses this node: the first press name it
/// advertises, or for a control with a check state its own toggle.
pub fn press(read: &Read<'_>) -> Option<i32> {
    let checkable = matches!(
        vocabulary::role(read.role).as_str(),
        "check box" | "radio button" | "toggle button" | "switch" | "check menu item"
    );
    read.actions
        .iter()
        .find(|(_, name)| {
            PRESS_NAMES
                .iter()
                .any(|press| press.eq_ignore_ascii_case(name))
                || (checkable && TOGGLE.eq_ignore_ascii_case(name))
        })
        .map(|(index, _)| *index)
}

/// The index of the action that expands and collapses this node, when it reads
/// EXPANDABLE: its own expanding action, else its press.
pub fn expander(read: &Read<'_>) -> Option<i32> {
    if !vocabulary::has(read.states, EXPANDABLE) {
        return None;
    }
    let own = read.actions.iter().find(|(_, name)| {
        EXPANDING
            .iter()
            .any(|expanding| expanding.eq_ignore_ascii_case(name))
    });
    own.map(|(index, _)| *index).or_else(|| press(read))
}

/// Whether `scroll_to` has a route: `Component.ScrollTo` on Chromium, and on GTK
/// the scroll bar of the scroll pane the node sits in. Qt has neither.
pub fn scrolls(read: &Read<'_>) -> bool {
    match read.toolkit {
        Toolkit::Chromium => read.component,
        Toolkit::Gtk3 | Toolkit::Gtk4 => read.scrolled,
        Toolkit::Qt | Toolkit::Other => false,
    }
}

/// Whether the node's value may be written: editable text through `EditableText`,
/// or a number through `Value` on a control that takes one. A password field is
/// written like any field; only its value is never read.
pub fn settable(read: &Read<'_>) -> bool {
    let editable = vocabulary::has(read.states, EDITABLE);
    let text = read.editable_text && editable;
    let number = read.value && vocabulary::adjustable(read.role);
    text || number
}

/// The writes the node takes. An item never selects itself on AT-SPI: its
/// container's `Selection` does ([`super::atspi`] asks the container).
pub fn routes(read: &Read<'_>) -> Routes {
    let editable = vocabulary::has(read.states, EDITABLE);
    Routes {
        value: settable(read),
        // GTK 4 answers `GrabFocus` `NotSupported` (L4-16).
        focus: read.component
            && vocabulary::has(read.states, FOCUSABLE)
            && read.toolkit != Toolkit::Gtk4,
        select: false,
        expand: expander(read).map(|_| Expander::Action),
        text: read.editable_text && read.text && editable,
    }
}

/// The node's actions as the seam names them: its own names, and beside them
/// [`PRESS`] where one of them presses it and [`SCROLL_TO_VISIBLE`] where it can be
/// scrolled to, so the shared rules (`roles::verbs`, `semantic::plan`) read one
/// vocabulary on both systems.
pub fn seam_actions(read: &Read<'_>) -> Vec<String> {
    let mut names: Vec<String> = read.actions.iter().map(|(_, name)| name.clone()).collect();
    if press(read).is_some() {
        names.push(PRESS.to_string());
    }
    if scrolls(read) {
        names.push(SCROLL_TO_VISIBLE.to_string());
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::vocabulary::{CHECKED, EXPANDED, SENSITIVE, SHOWING};

    const PUSH_BUTTON: u32 = 43;
    const CHECK_BOX: u32 = 7;
    const LIST_ITEM: u32 = 32;
    const TABLE_CELL: u32 = 56;
    const TOGGLE_BUTTON: u32 = 62;
    const STATIC: u32 = 116;
    const TEXT: u32 = 61;
    const SLIDER: u32 = 51;
    const PROGRESS_BAR: u32 = 42;

    /// The state words `GetState` answers with these bits set.
    fn words(bits: &[u32]) -> [u32; 2] {
        let mut words = [0u32; 2];
        for bit in bits {
            words[(bit / 32) as usize] |= 1 << (bit % 32);
        }
        words
    }

    fn named(names: &[&str]) -> Vec<(i32, String)> {
        names
            .iter()
            .enumerate()
            .map(|(index, name)| (index as i32, name.to_string()))
            .collect()
    }

    fn read<'a>(role: u32, states: &'a [u32], actions: &'a [(i32, String)]) -> Read<'a> {
        Read {
            role,
            states,
            component: true,
            text: false,
            value: false,
            editable_text: false,
            actions,
            toolkit: Toolkit::Gtk3,
            scrolled: false,
        }
    }

    // L3-20, L18-16: Chromium's press is the first of its press names, read by
    // index; `doDefault` (plain text) and `clickAncestor` (text in a link) never are.
    #[test]
    fn ledger_l18_16_press_is_the_first_press_name_and_never_do_default() {
        let chromium = named(&["showContextMenu", "press"]);
        assert_eq!(press(&read(PUSH_BUTTON, &[], &chromium)), Some(1));
        let link = named(&["jump", "showContextMenu"]);
        assert_eq!(press(&read(88, &[], &link)), Some(0));
        let gtk = named(&["Click"]);
        assert_eq!(press(&read(PUSH_BUTTON, &[], &gtk)), Some(0), "any case");
        for idle in [
            &["doDefault"][..],
            &["clickAncestor", "showContextMenu"][..],
        ] {
            let idle = named(idle);
            assert_eq!(press(&read(STATIC, &[], &idle)), None, "{idle:?}");
        }
        // An index whose name did not read is not there to press: the next one is.
        let partial = vec![(1, "click".to_string())];
        assert_eq!(press(&read(PUSH_BUTTON, &[], &partial)), Some(1));
    }

    // Qt names a check box's press `Toggle`; on a list item the same name toggles
    // the selection, which is no press.
    #[test]
    fn a_check_box_is_pressed_by_its_toggle_and_a_list_item_is_not() {
        let qt = named(&["Toggle", "SetFocus"]);
        assert_eq!(press(&read(CHECK_BOX, &[], &qt)), Some(0));
        assert_eq!(press(&read(TOGGLE_BUTTON, &[], &qt)), Some(0));
        assert_eq!(press(&read(LIST_ITEM, &[], &qt)), None);
    }

    // L4-6: a GTK 3 expander's only action is `activate`, and a tree cell's own
    // `expand or contract` comes first where it has one. Neither is a route unless
    // the node reads EXPANDABLE.
    #[test]
    fn ledger_l4_6_an_expander_is_expanded_by_its_one_toggle_style_action() {
        let expander_actions = named(&["activate"]);
        let expandable = words(&[EXPANDABLE, SENSITIVE, SHOWING]);
        assert_eq!(
            expander(&read(TOGGLE_BUTTON, &expandable, &expander_actions)),
            Some(0)
        );
        let cell = named(&["edit", "expand or contract", "activate"]);
        assert_eq!(expander(&read(TABLE_CELL, &expandable, &cell)), Some(1));
        let gtk4 = named(&["listitem.collapse", "listitem.toggle-expand"]);
        assert_eq!(expander(&read(PUSH_BUTTON, &expandable, &gtk4)), Some(1));

        assert_eq!(expander(&read(TABLE_CELL, &words(&[SHOWING]), &cell)), None);
        let qt_tree_item = named(&["Toggle"]);
        assert_eq!(
            expander(&read(LIST_ITEM, &expandable, &qt_tree_item)),
            None,
            "Qt's Toggle on an item toggles the selection"
        );
        assert_eq!(
            routes(&read(TABLE_CELL, &words(&[EXPANDABLE, EXPANDED]), &cell)).expand,
            Some(Expander::Action)
        );
    }

    // L4-16: GTK 4 answers GrabFocus NotSupported, so focus is offered only on a
    // focusable Component of any other toolkit.
    #[test]
    fn ledger_l4_16_focus_is_offered_where_grab_focus_works() {
        let none = named(&[]);
        let focusable = words(&[FOCUSABLE, SHOWING]);
        assert!(routes(&read(PUSH_BUTTON, &focusable, &none)).focus);
        assert!(!routes(&read(PUSH_BUTTON, &words(&[SHOWING]), &none)).focus);
        let gtk4 = Read {
            toolkit: Toolkit::Gtk4,
            ..read(PUSH_BUTTON, &focusable, &none)
        };
        assert!(!routes(&gtk4).focus);
        let no_component = Read {
            component: false,
            ..read(PUSH_BUTTON, &focusable, &none)
        };
        assert!(!routes(&no_component).focus);
    }

    // A field takes `set_value` through EditableText and `type` through
    // EditableText with Text, only while it reads EDITABLE; a number through Value
    // only on a control that takes one.
    #[test]
    fn text_and_numbers_are_written_only_where_the_node_says_they_may_be() {
        let none = named(&[]);
        let editable = words(&[EDITABLE, SHOWING]);
        let field = Read {
            text: true,
            editable_text: true,
            ..read(TEXT, &editable, &none)
        };
        let routes_of = routes(&field);
        assert!(routes_of.value && routes_of.text);
        let showing = words(&[SHOWING]);
        let read_only = Read {
            states: &showing,
            ..field
        };
        assert!(!routes(&read_only).value && !routes(&read_only).text);

        let slider = Read {
            value: true,
            ..read(SLIDER, &showing, &none)
        };
        assert!(routes(&slider).value);
        let progress = Read {
            role: PROGRESS_BAR,
            ..slider
        };
        assert!(!routes(&progress).value, "a progress bar only reports one");
    }

    // scroll_to: Chromium's ScrollTo on a Component, GTK's scroll bar for a node
    // inside a scroll pane, and nothing on Qt.
    #[test]
    fn scroll_to_has_a_route_on_chromium_and_inside_a_gtk_scroll_pane_only() {
        let none = named(&[]);
        let at = |toolkit, scrolled| Read {
            toolkit,
            scrolled,
            ..read(LIST_ITEM, &[], &none)
        };
        assert!(scrolls(&at(Toolkit::Chromium, false)));
        assert!(scrolls(&at(Toolkit::Gtk3, true)));
        assert!(scrolls(&at(Toolkit::Gtk4, true)));
        assert!(!scrolls(&at(Toolkit::Gtk3, false)));
        assert!(!scrolls(&at(Toolkit::Qt, true)));
        assert!(!scrolls(&at(Toolkit::Other, true)));
    }

    // The seam reads one vocabulary: the node's own names, and the seam's press and
    // scroll beside them where it has those routes.
    #[test]
    fn the_seam_reads_its_own_names_for_press_and_scroll_beside_the_nodes() {
        let actions = named(&["click"]);
        let checked = words(&[CHECKED]);
        let scrolled = Read {
            scrolled: true,
            ..read(CHECK_BOX, &checked, &actions)
        };
        assert_eq!(
            seam_actions(&scrolled),
            vec![
                "click".to_string(),
                PRESS.to_string(),
                SCROLL_TO_VISIBLE.to_string()
            ]
        );
        let idle = named(&["doDefault"]);
        assert_eq!(
            seam_actions(&read(STATIC, &[], &idle)),
            vec!["doDefault".to_string()]
        );
    }

    #[test]
    fn a_toolkit_is_named_by_its_name_and_major_version() {
        assert_eq!(Toolkit::of("GTK", "4.14.2"), Toolkit::Gtk4);
        assert_eq!(Toolkit::of("GTK", "3.24.41"), Toolkit::Gtk3);
        assert_eq!(Toolkit::of("Qt", "6.4.2"), Toolkit::Qt);
        assert_eq!(Toolkit::of("Chromium", "120.0"), Toolkit::Chromium);
        assert_eq!(Toolkit::of("gecko", "115"), Toolkit::Other);
    }
}
