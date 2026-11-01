#!/usr/bin/env python3
"""A GTK4 application with known controls, so computer use on Linux can be
qualified per control family instead of per screenshot.

The GTK4 member of a set (`fixture_gtk3.py` beside it explains the set, the
state-file contract and why application-owned state is the oracle). The controls
and the names they record under are the same as the GTK3 fixture's, with `gtk4.`
in front; the widgets are GTK4's own, because a fixture that used GTK3-style
widgets in GTK4 would qualify something nobody ships:

  * the outline is a ListView over a TreeListModel with TreeExpander rows, not the
    deprecated TreeView, which GTK4 exposes as one opaque node;
  * the pop-up button is a DropDown and the secure field a PasswordEntry;
  * the menu bar is the application's, built from a menu model and actions;
  * the dialog is a modal, transient window whose accessible role is DIALOG, since
    MessageDialog is deprecated from GTK 4.10 and AlertDialog is missing before it.

  python3 fixtures/linux/fixture_gtk4.py --state-file /tmp/gtk4.json
  python3 fixtures/linux/fixture_gtk4.py --self-test --state-file /tmp/gtk4.json

GTK4 registers on the accessibility bus only if the bus is up when it starts, as
agent-desktop found, which is why `private-session.sh` starts the bus first.
Run it only inside that private session, never on someone's desktop.
"""

import argparse
import json
import os
import sys

import gi

gi.require_version("Gtk", "4.0")
from gi.repository import Gio, GLib, Gtk  # noqa: E402

TOOLKIT = "gtk4"
LABEL = "GTK4"
STATE_VERSION = 1
APPLICATION_ID = "io.tezra.compux.FixtureGtk4"

FRUIT = ["Apple", "Banana", "Cherry", "Damson", "Elderberry"]
FOLDERS = [
    ("Documents", ["Invoices", "Letters"]),
    ("Pictures", ["Holidays", "Family"]),
    ("Projects", ["Alpha", "Beta"]),
    ("Music", []),
]
INITIALLY_EXPANDED = "Projects"
TABS = ["General", "Advanced", "About"]
HISTORY_ROWS = 200
POPUP_ITEMS = ["First", "Second", "Third"]
# Menu label -> the application action behind it.
MENU_ITEMS = {"Export Report": "export-report", "Print Preview": "print-preview"}


def control(family):
    """The name a control records under: `gtk4.button`, `gtk4.list`, ..."""
    return f"{TOOLKIT}.{family}"


# --- what the application records -------------------------------------------


class Recorder:
    """The state file, rewritten in full on every event."""

    def __init__(self, path):
        self._path = path
        self._events = []
        self._write()

    def record(self, name, action, value=None, length=None):
        event = {"seq": len(self._events) + 1, "control": name, "action": action}
        if value is not None:
            event["value"] = value
        if length is not None:
            event["length"] = length
        self._events.append(event)
        self._write()

    def events(self):
        return list(self._events)

    def _write(self):
        # A scratch file beside the target, then a rename over it: a reader
        # polling the file while the app runs never sees a partial document.
        directory, name = os.path.split(os.path.abspath(self._path))
        scratch = os.path.join(directory, f".{name}.writing")
        with open(scratch, "w", encoding="utf-8") as handle:
            json.dump({"version": STATE_VERSION, "events": self._events}, handle,
                      indent=2, sort_keys=True)
        os.replace(scratch, self._path)


# --- the window ---------------------------------------------------------------


def named(widget, name):
    widget.update_property([Gtk.AccessibleProperty.LABEL], [name])
    return widget


def framed(title, child):
    """A titled frame, which is what gives a control inside it an ancestor LABEL:
    the thing that tells the two "Save" buttons apart."""
    return named(Gtk.Frame(label=title, child=child), title)


def column(*children):
    box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)
    for margin in ("margin_top", "margin_bottom", "margin_start", "margin_end"):
        box.set_property(margin, 6)
    for child in children:
        box.append(child)
    return box


def row(*children):
    box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
    for child in children:
        box.append(child)
    return box


class FixtureWindow(Gtk.ApplicationWindow):
    """One window of known controls. Each `_build_*` makes one family and wires
    it to the recorder once its initial state is set."""

    def __init__(self, application, recorder):
        super().__init__(application=application, title=f"Compux Fixture {LABEL}",
                         show_menubar=True, default_width=980, default_height=720)
        self.recorder = recorder
        self.dialog = None
        self.dialog_ok = None
        self.document_save = self._save_button("document.save")
        self.sidebar_save = self._save_button("sidebar.save")

        lists = row(framed("Fruit", self._build_list()), framed("Folders", self._build_outline()),
                    framed("History", self._build_history()))
        saves = row(framed("Document", column(self.document_save)),
                    framed("Sidebar", column(self.sidebar_save)))
        self.set_child(column(framed(LABEL, column(*self._build_basics())), lists,
                              self._build_tabs(), saves))
        # Keyboard focus starts on the plain button, so showing the window can
        # never record a focus event on the search field.
        self.set_focus(self.button)

    def _build_basics(self):
        rec = self.recorder
        self.button = Gtk.Button(label=f"{LABEL} button")
        self.button.connect("clicked", lambda _b: rec.record(control("button"), "press"))

        self.checkbox = Gtk.CheckButton(label=f"{LABEL} checkbox")
        self.checkbox.connect("toggled", lambda b: rec.record(
            control("checkbox"), "toggle", value="on" if b.get_active() else "off"))

        self.text_field = named(Gtk.Entry(placeholder_text=f"{LABEL} text field"),
                                f"{LABEL} text field")
        self.text_field.connect("changed", lambda e: rec.record(
            control("text_field"), "set_value", value=e.get_text()))

        # The text never reaches the file: only how much of it arrived.
        self.secure_field = named(Gtk.PasswordEntry(placeholder_text=f"{LABEL} secure field"),
                                  f"{LABEL} secure field")
        self.secure_field.connect("changed", lambda e: rec.record(
            control("secure_field"), "set_value", length=len(e.get_text())))

        # Insensitive, and its handler still records: a press that somehow
        # reached a disabled control is then visible rather than silent.
        self.disabled_button = Gtk.Button(label=f"{LABEL} disabled button", sensitive=False)
        self.disabled_button.connect(
            "clicked", lambda _b: rec.record(control("disabled_button"), "press"))

        self.popup = named(Gtk.DropDown.new_from_strings(POPUP_ITEMS), f"{LABEL} pop-up button")
        self.popup.connect("notify::selected-item", lambda d, _p: rec.record(
            control("popup"), "select", value=d.get_selected_item().get_string()))

        self.search = named(Gtk.SearchEntry(placeholder_text="Search"), "Search")
        self.search.connect("changed", lambda e: rec.record(
            control("search"), "set_value", value=e.get_text()))
        # Focus moving INTO the field (its text child takes the actual focus),
        # however it was moved: a click, a key, or an accessibility call.
        focus = Gtk.EventControllerFocus()
        focus.connect("enter", lambda _c: rec.record(control("search"), "focus"))
        self.search.add_controller(focus)

        self.slider = named(Gtk.Scale.new_with_range(Gtk.Orientation.HORIZONTAL, 0, 100, 1),
                            "Volume")
        self.slider.set_digits(0)
        self.slider.set_round_digits(0)
        self.slider.set_value(50)
        self.slider.connect("value-changed", lambda s: rec.record(
            control("slider"), "set_value", value=str(int(round(s.get_value())))))

        self.clear_all = Gtk.Button(label="Clear All…")
        self.clear_all.connect("clicked", self._open_dialog)
        return [self.button, self.checkbox, self.text_field, self.secure_field,
                self.disabled_button, self.popup, self.search, self.slider, self.clear_all]

    def _build_list(self):
        self.fruit = named(Gtk.ListBox(selection_mode=Gtk.SelectionMode.SINGLE), "Fruit")
        for name in FRUIT:
            self.fruit.append(Gtk.Label(label=name, xalign=0))
        self.fruit.connect("row-selected", self._row_selected, control("list"))
        return self.fruit

    def _row_selected(self, _listbox, selected, name):
        # Unselecting answers `None`; only a row that became selected is an event.
        if selected is not None:
            self.recorder.record(name, "select", value=selected.get_child().get_label())

    def _build_outline(self):
        children = dict(FOLDERS)
        roots = Gtk.StringList.new([parent for parent, _kids in FOLDERS])
        self.tree = Gtk.TreeListModel.new(
            roots, False, False,
            lambda item: Gtk.StringList.new(children[item.get_string()])
            if children.get(item.get_string()) else None)
        self.tree.get_row(self.folder_position(INITIALLY_EXPANDED)).set_expanded(True)
        self.tree.connect("items-changed", self._outline_changed)

        self.outline_selection = Gtk.SingleSelection(model=self.tree, autoselect=False,
                                                     can_unselect=True)
        self.outline_selection.connect("notify::selected-item", self._outline_selected)
        factory = Gtk.SignalListItemFactory()
        factory.connect("setup", lambda _f, item: item.set_child(
            Gtk.TreeExpander(child=Gtk.Label(xalign=0))))
        factory.connect("bind", self._bind_folder)
        view = named(Gtk.ListView(model=self.outline_selection, factory=factory), "Folders")
        return Gtk.ScrolledWindow(child=view, min_content_height=180,
                                  hscrollbar_policy=Gtk.PolicyType.NEVER)

    @staticmethod
    def _bind_folder(_factory, item):
        expander = item.get_child()
        expander.set_list_row(item.get_item())
        expander.get_child().set_label(item.get_item().get_item().get_string())

    def folder_position(self, name):
        """Where a folder's row is now; rows above it that are expanded move it."""
        for position in range(self.tree.get_n_items()):
            if self.tree.get_row(position).get_item().get_string() == name:
                return position
        raise KeyError(name)

    def _outline_changed(self, _model, position, removed, added):
        # Expanding a folder inserts its children right below it, and collapsing
        # removes them, so the folder is the row just above the change.
        folder = self.tree.get_row(position - 1).get_item().get_string()
        self.recorder.record(control("outline"), "expand" if added else "collapse", value=folder)

    def _outline_selected(self, selection, _param):
        selected = selection.get_selected_item()
        if selected is not None:
            self.recorder.record(control("outline"), "select",
                                 value=selected.get_item().get_string())

    def _build_history(self):
        self.history = named(Gtk.ListBox(selection_mode=Gtk.SelectionMode.SINGLE), "History")
        for index in range(1, HISTORY_ROWS + 1):
            self.history.append(Gtk.Label(label=f"Entry {index}", xalign=0))
        self.history.connect("row-selected", self._row_selected, control("far_list"))
        scroller = Gtk.ScrolledWindow(child=self.history, min_content_height=180,
                                      hscrollbar_policy=Gtk.PolicyType.NEVER)
        self.history_adjustment = scroller.get_vadjustment()
        self._history_at_bottom = False
        self.history_adjustment.connect("value-changed", self._history_scrolled)
        return scroller

    def _history_scrolled(self, adjustment):
        # One event per ARRIVAL at the end, not one per pixel of the way there.
        bottom = adjustment.get_value() + adjustment.get_page_size()
        at_bottom = adjustment.get_page_size() > 0 and bottom >= adjustment.get_upper() - 1
        if at_bottom and not self._history_at_bottom:
            self.recorder.record(control("far_list"), "scroll", value="bottom")
        self._history_at_bottom = at_bottom

    def _build_tabs(self):
        self.tabs = named(Gtk.Notebook(), "Tabs")
        for title in TABS:
            self.tabs.append_page(Gtk.Label(label=f"{title} settings"), Gtk.Label(label=title))
        # Appending the first page switches to it; connected after, that is silent.
        self.tabs.connect("switch-page", lambda nb, page, _i: self.recorder.record(
            control("tabs"), "select", value=nb.get_tab_label_text(page)))
        return self.tabs

    def _save_button(self, name):
        button = Gtk.Button(label="Save")
        button.connect("clicked", lambda _b: self.recorder.record(name, "press"))
        return button

    def _open_dialog(self, _button):
        """A modal question with OK and Cancel, transient for this window."""
        self.dialog_ok = Gtk.Button(label="OK")
        cancel = Gtk.Button(label="Cancel")
        self.dialog = Gtk.Window(accessible_role=Gtk.AccessibleRole.DIALOG, title="Clear all?",
                                 transient_for=self, modal=True)
        self.dialog.set_child(column(Gtk.Label(label="Clear all fields?"),
                                     row(cancel, self.dialog_ok)))
        self.dialog_ok.connect("clicked", lambda _b: self._close_dialog("accept"))
        cancel.connect("clicked", lambda _b: self._close_dialog("cancel"))
        # The window manager's close button and Escape decline, as Cancel does.
        self.dialog.connect("close-request", lambda _w: self._dialog_declined())
        self.dialog.present()
        self.recorder.record(control("dialog"), "open")

    def _close_dialog(self, action):
        self.recorder.record(control("dialog"), action)
        self.dialog.destroy()
        self.dialog = None

    def _dialog_declined(self):
        self.recorder.record(control("dialog"), "cancel")
        self.dialog = None
        return False


def menu_bar(application, recorder):
    """The Actions menu: one application action per item, each recording its label."""
    actions = Gio.Menu()
    for label, name in MENU_ITEMS.items():
        action = Gio.SimpleAction.new(name, None)
        action.connect("activate", lambda _a, _p, label=label: recorder.record(
            control("menu"), "activate", value=label))
        application.add_action(action)
        actions.append(label, f"app.{name}")
    bar = Gio.Menu()
    bar.append_submenu("Actions", actions)
    return bar


# --- self-test ----------------------------------------------------------------
#
# Builds and shows the window, lets the toolkit settle, then drives every control
# through its WIDGET, the way a signal from a click or an accessibility call
# would arrive. It proves the window builds, showing it records nothing, every
# control's signal reaches the recorder, and the secure field's text never reaches
# the file. It cannot prove that an AT-SPI call reaches a widget: that needs an
# accessibility client, and is the corpus's and the live check's job.

SETTLE_MS = 600

EXPECTED = [
    (control("button"), "press", None, None),
    (control("checkbox"), "toggle", "on", None),
    (control("text_field"), "set_value", "self-test", None),
    (control("secure_field"), "set_value", None, 7),
    (control("disabled_button"), "press", None, None),
    (control("popup"), "select", "Second", None),
    (control("search"), "focus", None, None),
    (control("search"), "set_value", "fruit", None),
    (control("slider"), "set_value", "75", None),
    (control("dialog"), "open", None, None),
    (control("dialog"), "accept", None, None),
    (control("list"), "select", "Cherry", None),
    (control("outline"), "expand", "Documents", None),
    (control("outline"), "select", "Letters", None),
    (control("outline"), "collapse", "Projects", None),
    (control("tabs"), "select", "Advanced", None),
    (control("far_list"), "scroll", "bottom", None),
    (control("far_list"), "select", f"Entry {HISTORY_ROWS}", None),
    (control("menu"), "activate", "Export Report", None),
    ("document.save", "press", None, None),
    ("sidebar.save", "press", None, None),
]


def drive_every_control(window, application):
    window.button.emit("clicked")
    window.checkbox.set_active(True)
    window.text_field.set_text("self-test")
    window.secure_field.set_text("hunter2")
    window.disabled_button.emit("clicked")
    window.popup.set_selected(1)
    window.search.grab_focus()
    window.search.set_text("fruit")
    window.slider.set_value(75)
    window.clear_all.emit("clicked")
    window.dialog_ok.emit("clicked")
    window.fruit.select_row(window.fruit.get_row_at_index(FRUIT.index("Cherry")))
    window.tree.get_row(window.folder_position("Documents")).set_expanded(True)
    window.outline_selection.set_selected(window.folder_position("Letters"))
    window.tree.get_row(window.folder_position(INITIALLY_EXPANDED)).set_expanded(False)
    window.tabs.set_current_page(TABS.index("Advanced"))
    adjustment = window.history_adjustment
    adjustment.set_value(adjustment.get_upper() - adjustment.get_page_size())
    window.history.select_row(window.history.get_row_at_index(HISTORY_ROWS - 1))
    application.activate_action(MENU_ITEMS["Export Report"], None)
    window.document_save.emit("clicked")
    window.sidebar_save.emit("clicked")


def check_events(events):
    failures = []
    observed = [(e["control"], e["action"], e.get("value"), e.get("length")) for e in events]
    if observed != EXPECTED:
        failures.append(f"expected {len(EXPECTED)} events in order, the file has {observed}")
    for index, event in enumerate(events, start=1):
        if event["seq"] != index:
            failures.append(f"event {index} is numbered {event['seq']}")
        if event["control"] == control("secure_field") and "value" in event:
            failures.append("the secure field wrote its text into the state file")
    return failures


def read_state(path):
    with open(path, encoding="utf-8") as handle:
        state = json.load(handle)
    if state.get("version") != STATE_VERSION or not isinstance(state.get("events"), list):
        raise ValueError(f"{path} is not a version {STATE_VERSION} fixture state file")
    return state["events"]


class SelfTest:
    """Two steps on the main loop: after the window settles, check that showing
    it recorded nothing; then drive every control and check what was recorded."""

    def __init__(self, application, path):
        self.application, self.path, self.status = application, path, 1

    def start(self, window):
        GLib.timeout_add(SETTLE_MS, self._run, window)

    def _run(self, window):
        failures = [f"showing the window recorded {e}" for e in read_state(self.path)]
        if not failures:
            drive_every_control(window, self.application)
            failures = check_events(read_state(self.path))
        for failure in failures:
            print(f"FAIL: {failure}")
        if not failures:
            print(f"ok: {len(EXPECTED)} events from every {LABEL} control recorded into "
                  f"{self.path}, and showing the window recorded none")
        self.status = 1 if failures else 0
        self.application.quit()
        return GLib.SOURCE_REMOVE


# --- entry point --------------------------------------------------------------


def parse_args(argv):
    parser = argparse.ArgumentParser(description=f"Compux {LABEL} fixture application")
    parser.add_argument("--state-file", default=f"/tmp/compux-fixture-{TOOLKIT}.json",
                        help="where the application records what happened to it")
    parser.add_argument("--self-test", action="store_true",
                        help="build and show the window, drive every control, check the file")
    return parser.parse_args(argv)


def main(argv):
    args = parse_args(argv)
    recorder = Recorder(args.state_file)
    # NON_UNIQUE: every launch is its own instance, never a message to an older one.
    application = Gtk.Application(application_id=APPLICATION_ID,
                                  flags=Gio.ApplicationFlags.NON_UNIQUE)
    test = SelfTest(application, args.state_file) if args.self_test else None

    def activate(app):
        app.set_menubar(menu_bar(app, recorder))
        window = FixtureWindow(app, recorder)
        window.present()
        if test is not None:
            test.start(window)

    application.connect("activate", activate)
    # GTK parses its own options from argv; ours are already consumed.
    status = application.run([sys.argv[0]])
    return test.status if test is not None else status


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
