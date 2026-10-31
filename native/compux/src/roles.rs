//! Names (protocol 12, M86 P2): the normalised roles, the surface kinds, and which
//! verbs a control lists. Pure tables, so every platform names a control the same
//! way and a caller reads one vocabulary (PROTOCOL.md §6).
//!
//! A platform's own role always rides beside the normalised one as `native_role`:
//! the table is how a caller FINDS a control, never a replacement for what the
//! platform said. A role the table does not know is `unknown`, and its native role
//! says which it was.

use crate::ax::{Routes, States, PRESS, SCROLL_TO_VISIBLE};

/// Every normalised role, in the order `Compux.Protocol.roles/0` lists them. A test
/// pins the two lists together.
pub const ROLES: [&str; 40] = [
    "button",
    "menubutton",
    "checkbox",
    "switch",
    "radio",
    "tab",
    "textfield",
    "textarea",
    "combobox",
    "menuitem",
    "link",
    "slider",
    "stepper",
    "disclosure",
    "row",
    "cell",
    "column",
    "list",
    "table",
    "tree",
    "scrollarea",
    "scrollbar",
    "splitter",
    "toolbar",
    "tablist",
    "group",
    "menu",
    "menubar",
    "window",
    "dialog",
    "alert",
    "popup",
    "text",
    "image",
    "heading",
    "progress",
    "datefield",
    "document",
    "application",
    "unknown",
];

/// What a receipt's `surfaces` may name, as `Compux.Protocol.surface_kinds/0`: a
/// test pins the two lists together, and every kind [`surface`] answers to this one.
#[cfg(test)]
pub const SURFACE_KINDS: [&str; 4] = ["dialog", "alert", "menu", "popup"];

/// The verbs on a reference, in the order a listing names them.
pub const VERBS: [&str; 9] = [
    "press",
    "set_value",
    "type",
    "toggle",
    "expand",
    "collapse",
    "select",
    "focus",
    "scroll_to",
];

/// The subroles that make any window or element an alert.
const ALERT_SUBROLES: [&str; 2] = ["AXApplicationAlert", "AXApplicationAlertDialog"];

/// The subroles that make a window a dialog. A sheet is one by its role.
const DIALOG_SUBROLES: [&str; 2] = ["AXDialog", "AXSystemDialog"];

/// The normalised role for a platform role and subrole (PROTOCOL.md §6.1).
pub fn normalise(role: &str, subrole: Option<&str>) -> &'static str {
    if subrole.is_some_and(|subrole| ALERT_SUBROLES.contains(&subrole)) {
        return "alert";
    }
    match (role, subrole) {
        ("AXWindow", Some(subrole)) if DIALOG_SUBROLES.contains(&subrole) => "dialog",
        ("AXCheckBox", Some("AXSwitch" | "AXToggle")) => "switch",
        ("AXRadioButton", Some("AXTabButton")) => "tab",
        _ => by_role(role),
    }
}

/// The normalised role a platform role names on its own.
fn by_role(role: &str) -> &'static str {
    match role {
        "AXButton" => "button",
        "AXMenuButton" => "menubutton",
        "AXCheckBox" => "checkbox",
        "AXRadioButton" => "radio",
        "AXTabButton" => "tab",
        "AXTextField" | "AXSecureTextField" => "textfield",
        "AXTextArea" => "textarea",
        "AXComboBox" | "AXPopUpButton" => "combobox",
        "AXMenuItem" | "AXMenuBarItem" => "menuitem",
        "AXLink" => "link",
        "AXSlider" => "slider",
        "AXIncrementor" | "AXStepper" => "stepper",
        "AXDisclosureTriangle" => "disclosure",
        "AXRow" => "row",
        "AXCell" => "cell",
        "AXColumn" => "column",
        "AXList" => "list",
        "AXTable" => "table",
        "AXOutline" => "tree",
        "AXScrollArea" => "scrollarea",
        "AXScrollBar" => "scrollbar",
        "AXSplitter" => "splitter",
        "AXToolbar" => "toolbar",
        "AXTabGroup" => "tablist",
        "AXGroup" | "AXSplitGroup" | "AXRadioGroup" => "group",
        "AXMenu" => "menu",
        "AXMenuBar" => "menubar",
        "AXWindow" => "window",
        "AXSheet" => "dialog",
        "AXPopover" => "popup",
        "AXStaticText" => "text",
        "AXImage" => "image",
        "AXHeading" => "heading",
        "AXProgressIndicator" | "AXBusyIndicator" => "progress",
        "AXDateField" | "AXTimeField" => "datefield",
        "AXWebArea" => "document",
        "AXApplication" => "application",
        _ => "unknown",
    }
}

/// The surface a node is, when it is one: its kind and the platform's own name for
/// it (PROTOCOL.md §6.2). An ordinary window is not a surface.
pub fn surface(role: &str, subrole: Option<&str>) -> Option<(&'static str, String)> {
    let kind = match normalise(role, subrole) {
        kind @ ("dialog" | "alert" | "menu" | "popup") => kind,
        _ => return None,
    };
    // The platform's own name: the subrole where it is what made it one, else the
    // role.
    let native = match subrole {
        Some(subrole)
            if ALERT_SUBROLES.contains(&subrole) || DIALOG_SUBROLES.contains(&subrole) =>
        {
            subrole
        }
        _ => role,
    };
    Some((kind, native.to_string()))
}

/// Whether a normalised role carries a check state: its value, read as 0 or 1.
pub fn has_check_state(role: &str) -> bool {
    matches!(role, "checkbox" | "switch" | "radio")
}

/// Exactly the verbs a control has a route for (PROTOCOL.md §6.3), in [`VERBS`]
/// order. `role` is the normalised role; everything else is what the control said
/// about itself.
pub fn verbs(
    role: &str,
    actions: &[String],
    routes: &Routes,
    states: &States,
) -> Vec<&'static str> {
    let press = advertises(actions, PRESS);
    let toggle = has_check_state(role) && press && states.on.is_some();
    let expand = expandable(role, actions, routes, states);

    let listed = [
        press,
        routes.value,
        routes.text,
        toggle,
        expand,
        expand,
        routes.select,
        routes.focus,
        advertises(actions, SCROLL_TO_VISIBLE),
    ];
    VERBS
        .iter()
        .zip(listed)
        .filter_map(|(verb, has)| has.then_some(*verb))
        .collect()
}

/// Whether a control can be expanded and collapsed: an attribute that says so may
/// be written, or it is a disclosure that is pressed and reads its state.
pub fn expandable(role: &str, actions: &[String], routes: &Routes, states: &States) -> bool {
    routes.expand.is_some()
        || (role == "disclosure" && advertises(actions, PRESS) && states.on.is_some())
}

fn advertises(actions: &[String], name: &str) -> bool {
    actions.iter().any(|action| action == name)
}

/// One `~w(...)` attribute of `Compux.Protocol`, as the words it lists: how a test
/// pins a list here to the library's.
#[cfg(test)]
pub fn elixir_list(attribute: &str) -> Vec<String> {
    let source = include_str!("../../../lib/compux/protocol.ex");
    let start = source
        .find(&format!("{attribute} ~w("))
        .unwrap_or_else(|| panic!("{attribute} in protocol.ex"));
    let body = &source[start + attribute.len() + 4..];
    let end = body.find(')').expect("the list closes");
    body[..end].split_whitespace().map(str::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ax::Expander;

    // The two sides spell one vocabulary: a query names a role the library lets
    // through and the helper can match, and a surface kind the library knows.
    #[test]
    fn the_role_and_surface_tables_are_the_librarys() {
        assert_eq!(elixir_list("@roles"), ROLES.to_vec());
        assert_eq!(elixir_list("@surface_kinds"), SURFACE_KINDS.to_vec());
    }

    // Every macOS role the contract maps, with the subroles that change it.
    #[test]
    fn every_mapped_macos_role_normalises_as_the_contract_says() {
        let table: [(&str, Option<&str>, &str); 48] = [
            ("AXButton", None, "button"),
            ("AXMenuButton", None, "menubutton"),
            ("AXCheckBox", None, "checkbox"),
            ("AXCheckBox", Some("AXSwitch"), "switch"),
            ("AXCheckBox", Some("AXToggle"), "switch"),
            ("AXRadioButton", None, "radio"),
            ("AXRadioButton", Some("AXTabButton"), "tab"),
            ("AXTabButton", None, "tab"),
            ("AXTextField", None, "textfield"),
            ("AXTextField", Some("AXSecureTextField"), "textfield"),
            ("AXTextField", Some("AXSearchField"), "textfield"),
            ("AXSecureTextField", None, "textfield"),
            ("AXTextArea", None, "textarea"),
            ("AXComboBox", None, "combobox"),
            ("AXPopUpButton", None, "combobox"),
            ("AXMenuItem", None, "menuitem"),
            ("AXMenuBarItem", None, "menuitem"),
            ("AXLink", None, "link"),
            ("AXSlider", None, "slider"),
            ("AXIncrementor", None, "stepper"),
            ("AXStepper", None, "stepper"),
            ("AXDisclosureTriangle", None, "disclosure"),
            ("AXRow", None, "row"),
            ("AXRow", Some("AXOutlineRow"), "row"),
            ("AXRow", Some("AXTableRow"), "row"),
            ("AXCell", None, "cell"),
            ("AXColumn", None, "column"),
            ("AXList", None, "list"),
            ("AXTable", None, "table"),
            ("AXOutline", None, "tree"),
            ("AXScrollArea", None, "scrollarea"),
            ("AXScrollBar", None, "scrollbar"),
            ("AXSplitter", None, "splitter"),
            ("AXToolbar", None, "toolbar"),
            ("AXTabGroup", None, "tablist"),
            ("AXGroup", None, "group"),
            ("AXSplitGroup", None, "group"),
            ("AXRadioGroup", None, "group"),
            ("AXMenu", None, "menu"),
            ("AXMenuBar", None, "menubar"),
            ("AXWindow", None, "window"),
            ("AXWindow", Some("AXStandardWindow"), "window"),
            ("AXSheet", None, "dialog"),
            ("AXWindow", Some("AXDialog"), "dialog"),
            ("AXWindow", Some("AXSystemDialog"), "dialog"),
            ("AXGroup", Some("AXApplicationAlert"), "alert"),
            ("AXWindow", Some("AXApplicationAlertDialog"), "alert"),
            ("AXPopover", None, "popup"),
        ];
        for (role, subrole, normalised) in table {
            assert_eq!(normalise(role, subrole), normalised, "{role} {subrole:?}");
        }

        let rest: [(&str, &str); 10] = [
            ("AXStaticText", "text"),
            ("AXImage", "image"),
            ("AXHeading", "heading"),
            ("AXProgressIndicator", "progress"),
            ("AXBusyIndicator", "progress"),
            ("AXDateField", "datefield"),
            ("AXTimeField", "datefield"),
            ("AXWebArea", "document"),
            ("AXApplication", "application"),
            ("AXLevelIndicator", "unknown"),
        ];
        for (role, normalised) in rest {
            assert_eq!(normalise(role, None), normalised, "{role}");
        }
    }

    // A role nobody mapped is `unknown`, never a guess, and every answer is a role
    // the library lets a query name.
    #[test]
    fn an_unmapped_role_is_unknown_and_every_answer_is_in_the_table() {
        assert_eq!(normalise("AXSomethingNew", None), "unknown");
        assert_eq!(normalise("", None), "unknown");
        assert_eq!(normalise("AXGroup", Some("AXSomethingNew")), "group");

        for role in ["AXButton", "AXSheet", "AXRuler", "AXGrowArea", "AXMatte"] {
            assert!(ROLES.contains(&normalise(role, None)), "{role}");
        }
    }

    // A sheet is a dialog; an alert is one by its subrole whatever its role; a menu
    // and a popover by their roles; an ordinary window is not a surface.
    #[test]
    fn surfaces_are_named_by_kind_and_by_the_platforms_own_name() {
        assert_eq!(
            surface("AXSheet", None),
            Some(("dialog", "AXSheet".to_string()))
        );
        assert_eq!(
            surface("AXWindow", Some("AXDialog")),
            Some(("dialog", "AXDialog".to_string()))
        );
        assert_eq!(
            surface("AXWindow", Some("AXSystemDialog")),
            Some(("dialog", "AXSystemDialog".to_string()))
        );
        assert_eq!(
            surface("AXWindow", Some("AXApplicationAlertDialog")),
            Some(("alert", "AXApplicationAlertDialog".to_string()))
        );
        assert_eq!(
            surface("AXGroup", Some("AXApplicationAlert")),
            Some(("alert", "AXApplicationAlert".to_string()))
        );
        assert_eq!(
            surface("AXMenu", None),
            Some(("menu", "AXMenu".to_string()))
        );
        assert_eq!(
            surface("AXPopover", None),
            Some(("popup", "AXPopover".to_string()))
        );

        assert_eq!(surface("AXWindow", Some("AXStandardWindow")), None);
        assert_eq!(surface("AXMenuBar", None), None);
        assert_eq!(surface("AXButton", None), None);
        for (kind, _) in [
            surface("AXSheet", None),
            surface("AXMenu", None),
            surface("AXPopover", None),
            surface("AXGroup", Some("AXApplicationAlert")),
        ]
        .into_iter()
        .flatten()
        {
            assert!(SURFACE_KINDS.contains(&kind));
        }
    }

    fn names(actions: &[&str]) -> Vec<String> {
        actions.iter().map(|action| action.to_string()).collect()
    }

    // Each verb is listed exactly where its route is, and nowhere else.
    #[test]
    fn a_control_lists_exactly_the_verbs_it_has_a_route_for() {
        let none = Routes::default();
        let blank = States::default();

        assert!(
            verbs("group", &[], &none, &blank).is_empty(),
            "no route, no verb"
        );
        assert_eq!(
            verbs("button", &names(&[PRESS]), &none, &blank),
            vec!["press"]
        );
        assert_eq!(
            verbs("button", &names(&[PRESS, SCROLL_TO_VISIBLE]), &none, &blank),
            vec!["press", "scroll_to"]
        );

        let field = Routes {
            value: true,
            text: true,
            focus: true,
            ..Routes::default()
        };
        assert_eq!(
            verbs("textfield", &[], &field, &blank),
            vec!["set_value", "type", "focus"]
        );

        let row = Routes {
            select: true,
            expand: Some(Expander::Disclosing),
            ..Routes::default()
        };
        assert_eq!(
            verbs("row", &[], &row, &blank),
            vec!["expand", "collapse", "select"]
        );
    }

    // A toggle needs all three: a role with a check state, a press, and a state it
    // can read — or it could not tell whether to press at all.
    #[test]
    fn toggle_needs_a_check_state_role_a_press_and_a_readable_state() {
        let none = Routes::default();
        let on = States {
            on: Some(true),
            ..States::default()
        };
        let press = names(&[PRESS]);

        for role in ["checkbox", "switch", "radio"] {
            assert!(has_check_state(role), "{role}");
            assert_eq!(
                verbs(role, &press, &none, &on),
                vec!["press", "toggle"],
                "{role}"
            );
        }
        assert!(!has_check_state("button"));
        assert_eq!(
            verbs("checkbox", &press, &none, &States::default()),
            vec!["press"]
        );
        assert!(verbs("checkbox", &[], &none, &on).is_empty());
        assert_eq!(verbs("button", &press, &none, &on), vec!["press"]);
    }

    // A disclosure is expanded by pressing it, so it lists expand and collapse only
    // when it is pressed and its state reads.
    #[test]
    fn a_disclosure_expands_by_press_and_state_and_anything_else_by_attribute() {
        let none = Routes::default();
        let press = names(&[PRESS]);
        let open = States {
            on: Some(true),
            ..States::default()
        };

        assert!(expandable("disclosure", &press, &none, &open));
        assert!(!expandable("disclosure", &press, &none, &States::default()));
        assert!(!expandable("disclosure", &[], &none, &open));
        assert!(!expandable("button", &press, &none, &open));

        let attribute = Routes {
            expand: Some(Expander::Expanded),
            ..Routes::default()
        };
        assert!(expandable("combobox", &[], &attribute, &States::default()));
        assert_eq!(
            verbs("disclosure", &press, &none, &open),
            vec!["press", "expand", "collapse"]
        );
    }
}
