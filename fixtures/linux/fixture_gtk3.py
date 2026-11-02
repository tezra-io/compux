#!/usr/bin/env python3
"""A GTK3 application with known controls, so computer use on Linux can be
qualified per control family instead of per screenshot.

It is the GTK3 member of a set (`fixture_gtk4.py`, `fixture_qt.py`, and the macOS
`fixtures/macos/FixtureApp.swift`), and it exists for the reason the macOS one
does: every real application is a moving target, and none of them says whether a
control was really pressed. This one has one window, a known set of controls, and
APPLICATION-OWNED STATE: every handler records what happened into a JSON file, in
the same shape the macOS fixture writes, so "did the press land" has an answer
that does not come from a picture of the screen.

  python3 fixtures/linux/fixture_gtk3.py --state-file /tmp/gtk3.json
  python3 fixtures/linux/fixture_gtk3.py --self-test --state-file /tmp/gtk3.json

Run it only inside a private session (`private-session.sh`, beside this file),
never on someone's desktop: it is a target for evals and live checks in a
container, and it lives here so the corpus and compux read the same application.

The state file is `{"version": 1, "events": [...]}`, each event `{seq, control,
action}` plus `value` where publishing one is harmless, or `length` for the secure
field, whose text is never written anywhere. It is rewritten whole on every event
through a rename, so a reader never sees half a document.

How a change gets recorded: from the toolkit's own change signal, which GTK emits
however the change arrived (a click, a key, or an accessibility write such as
`EditableText.SetTextContents`), so nothing here samples on a timer the way the
AppKit half must. Every signal is connected AFTER the control's initial state is
set, so building and showing the window records nothing; `--self-test` checks it.

The controls, and the verb each one is here for:

  * a BUTTON, a CHECKBOX, a TEXT FIELD, a SECURE FIELD, a DISABLED BUTTON and a
    POP-UP BUTTON, labelled "GTK3 ..." as the macOS halves label theirs "AppKit
    ..." and "SwiftUI ...": press, toggle, set_value, the secure field that can
    never verify, the press that must be refused, and select;
  * a SEARCH field that also records FOCUS: the focus verb and typing into a field;
  * a FRUIT list (select), a FOLDERS outline whose rows expand and collapse
    (Projects starts expanded, so collapse has a target), TABS, and a VOLUME slider;
  * a HISTORY list whose last row starts far out of view: scroll_to, recorded as
    `scroll` / `bottom` when the list reaches its end;
  * "Clear All..." opening a MODAL DIALOG with OK and Cancel;
  * an ACTIONS menu in the menu bar with "Export Report" and "Print Preview";
  * TWO GROUPS, Document and Sidebar, each with a button labelled "Save", so only
    the ancestry tells them apart.
"""

import argparse
import json
import os
import sys

import gi

gi.require_version("Gtk", "3.0")
from gi.repository import GLib, Gtk  # noqa: E402

TOOLKIT = "gtk3"
LABEL = "GTK3"
STATE_VERSION = 1

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
MENU_ITEMS = ["Export Report", "Print Preview"]


def control(family):
    """The name a control records under: `gtk3.button`, `gtk3.list`, ..."""
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


def framed(title, child):
    """A titled frame, which is what gives a control inside it an ancestor LABEL:
    the thing that tells the two "Save" buttons apart."""
    frame = Gtk.Frame(label=title)
    frame.get_accessible().set_name(title)
    frame.add(child)
    return frame


def column(*children):
    box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)
    box.set_border_width(6)
    for child in children:
        box.pack_start(child, False, False, 0)
    return box


def named(widget, name):
    widget.get_accessible().set_name(name)
    return widget


class FixtureWindow(Gtk.Window):
    """One window of known controls. Each `_build_*` makes one family and wires
    it to the recorder once its initial state is set."""

    def __init__(self, recorder):
        super().__init__(title=f"Compux Fixture {LABEL}")
        self.recorder = recorder
        self.dialog = None
        self.set_default_size(980, 720)

        basics = framed(LABEL, column(*self._build_basics()))
        lists = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        lists.pack_start(framed("Fruit", self._build_list()), True, True, 0)
        lists.pack_start(framed("Folders", self._build_outline()), True, True, 0)
        lists.pack_start(framed("History", self._build_history()), True, True, 0)
        self.document_save = self._save_button("document.save")
        self.sidebar_save = self._save_button("sidebar.save")
        saves = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        saves.pack_start(framed("Document", column(self.document_save)), False, False, 0)
        saves.pack_start(framed("Sidebar", column(self.sidebar_save)), False, False, 0)

        root = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
        root.pack_start(self._build_menu(), False, False, 0)
        body = column(basics, lists, self._build_tabs(), saves)
        root.pack_start(body, True, True, 0)
        self.add(root)
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
        self.secure_field = named(Gtk.Entry(placeholder_text=f"{LABEL} secure field",
                                            visibility=False), f"{LABEL} secure field")
        self.secure_field.connect("changed", lambda e: rec.record(
            control("secure_field"), "set_value", length=len(e.get_text())))

        # Insensitive, and its handler still records: a press that somehow
        # reached a disabled control is then visible rather than silent.
        self.disabled_button = Gtk.Button(label=f"{LABEL} disabled button", sensitive=False)
        self.disabled_button.connect(
            "clicked", lambda _b: rec.record(control("disabled_button"), "press"))

        self.popup = named(Gtk.ComboBoxText(), f"{LABEL} pop-up button")
        for item in POPUP_ITEMS:
            self.popup.append_text(item)
        self.popup.set_active(0)
        self.popup.connect("changed", lambda c: rec.record(
            control("popup"), "select", value=c.get_active_text()))

        self.search = named(Gtk.SearchEntry(placeholder_text="Search"), "Search")
        self.search.connect("changed", lambda e: rec.record(
            control("search"), "set_value", value=e.get_text()))
        # `is-focus` is the window's focus widget, whatever the window manager
        # did with the toplevel: a focus moved by an accessibility call and one
        # moved by a click read the same.
        self.search.connect("notify::is-focus", self._search_focus)

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

    def _search_focus(self, entry, _param):
        if entry.is_focus():
            self.recorder.record(control("search"), "focus")

    def _build_list(self):
        self.fruit = named(Gtk.ListBox(selection_mode=Gtk.SelectionMode.SINGLE), "Fruit")
        for name in FRUIT:
            self.fruit.add(Gtk.Label(label=name, xalign=0))
        self.fruit.connect("row-selected", self._row_selected, control("list"))
        return self.fruit

    def _row_selected(self, _listbox, row, name):
        # Unselecting answers `None`; only a row that became selected is an event.
        if row is not None:
            self.recorder.record(name, "select", value=row.get_child().get_label())

    def _build_outline(self):
        store = Gtk.TreeStore(str)
        for parent, children in FOLDERS:
            node = store.append(None, [parent])
            for child in children:
                store.append(node, [child])
        self.outline = named(Gtk.TreeView(model=store, headers_visible=False), "Folders")
        self.outline.append_column(
            Gtk.TreeViewColumn("Folder", Gtk.CellRendererText(), text=0))
        self.outline.expand_row(self._folder_path(INITIALLY_EXPANDED), False)
        self.outline.connect("row-expanded", self._row_toggled, "expand")
        self.outline.connect("row-collapsed", self._row_toggled, "collapse")
        self.outline.get_selection().connect("changed", self._outline_selected)
        self.outline.set_size_request(-1, 180)
        return self.outline

    def _folder_path(self, name):
        index = [parent for parent, _children in FOLDERS].index(name)
        return Gtk.TreePath.new_from_indices([index])

    def _row_toggled(self, tree, treeiter, _path, action):
        self.recorder.record(control("outline"), action, value=tree.get_model()[treeiter][0])

    def _outline_selected(self, selection):
        model, treeiter = selection.get_selected()
        if treeiter is not None:
            self.recorder.record(control("outline"), "select", value=model[treeiter][0])

    def _build_history(self):
        self.history = named(Gtk.ListBox(selection_mode=Gtk.SelectionMode.SINGLE), "History")
        for index in range(1, HISTORY_ROWS + 1):
            self.history.add(Gtk.Label(label=f"Entry {index}", xalign=0))
        self.history.connect("row-selected", self._row_selected, control("far_list"))
        scroller = Gtk.ScrolledWindow(hscrollbar_policy=Gtk.PolicyType.NEVER)
        scroller.set_min_content_height(180)
        scroller.add(self.history)
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
        # GTK3 emits `switch-page` for the page already showing when the notebook
        # is mapped, so only a CHANGE of tab is an event.
        self._tab = TABS[0]
        self.tabs.connect("switch-page", self._tab_switched)
        return self.tabs

    def _tab_switched(self, notebook, page, _index):
        title = notebook.get_tab_label_text(page)
        if title != self._tab:
            self._tab = title
            self.recorder.record(control("tabs"), "select", value=title)

    def _build_menu(self):
        menubar = Gtk.MenuBar()
        actions = Gtk.MenuItem(label="Actions")
        menu = Gtk.Menu()
        self.menu_items = {}
        for title in MENU_ITEMS:
            item = Gtk.MenuItem(label=title)
            item.connect("activate", lambda i: self.recorder.record(
                control("menu"), "activate", value=i.get_label()))
            menu.append(item)
            self.menu_items[title] = item
        actions.set_submenu(menu)
        menubar.append(actions)
        return menubar

    def _save_button(self, name):
        button = Gtk.Button(label="Save")
        button.connect("clicked", lambda _b: self.recorder.record(name, "press"))
        return button

    def _open_dialog(self, _button):
        """A modal question with OK and Cancel. Shown, never `run()`: a nested
        main loop would hide the response from everything but this call."""
        self.dialog = Gtk.MessageDialog(
            transient_for=self, modal=True, message_type=Gtk.MessageType.QUESTION,
            buttons=Gtk.ButtonsType.OK_CANCEL, text="Clear all fields?")
        self.dialog.connect("response", self._dialog_response)
        self.dialog.show_all()
        self.recorder.record(control("dialog"), "open")

    def _dialog_response(self, dialog, response):
        # Cancel, Escape and the window's close button all decline.
        action = "accept" if response == Gtk.ResponseType.OK else "cancel"
        self.recorder.record(control("dialog"), action)
        dialog.destroy()
        self.dialog = None


# --- self-test ----------------------------------------------------------------
#
# Builds and shows the window, lets the toolkit settle, and then drives every
# control through its WIDGET, the way a signal from a click or an accessibility
# call would arrive. What it proves: the window builds, showing it records
# nothing, every control's signal reaches the recorder, and the secure field's
# text never reaches the file. What it CANNOT prove: that an AT-SPI call reaches
# the widget at all. That needs an accessibility client, and it is the corpus's
# and the live check's job.

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


def drive_every_control(window):
    window.button.clicked()
    window.checkbox.set_active(True)
    window.text_field.set_text("self-test")
    window.secure_field.set_text("hunter2")
    window.disabled_button.clicked()
    window.popup.set_active(1)
    window.search.grab_focus()
    window.search.set_text("fruit")
    window.slider.set_value(75)
    window.clear_all.clicked()
    window.dialog.response(Gtk.ResponseType.OK)
    window.fruit.select_row(window.fruit.get_row_at_index(FRUIT.index("Cherry")))
    window.outline.expand_row(window._folder_path("Documents"), False)
    window.outline.get_selection().select_path(Gtk.TreePath.new_from_indices([0, 1]))
    window.outline.collapse_row(window._folder_path(INITIALLY_EXPANDED))
    window.tabs.set_current_page(TABS.index("Advanced"))
    adjustment = window.history_adjustment
    adjustment.set_value(adjustment.get_upper() - adjustment.get_page_size())
    window.history.select_row(window.history.get_row_at_index(HISTORY_ROWS - 1))
    window.menu_items["Export Report"].activate()
    window.document_save.clicked()
    window.sidebar_save.clicked()


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

    def __init__(self, window, path):
        self.window, self.path, self.status = window, path, 1

    def start(self):
        GLib.timeout_add(SETTLE_MS, self._run)

    def _run(self):
        failures = [f"showing the window recorded {e}" for e in read_state(self.path)]
        if not failures:
            drive_every_control(self.window)
            failures = check_events(read_state(self.path))
        for failure in failures:
            print(f"FAIL: {failure}")
        if not failures:
            print(f"ok: {len(EXPECTED)} events from every {LABEL} control recorded into "
                  f"{self.path}, and showing the window recorded none")
        self.status = 1 if failures else 0
        Gtk.main_quit()
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
    window = FixtureWindow(Recorder(args.state_file))
    window.connect("destroy", Gtk.main_quit)
    window.show_all()
    if not args.self_test:
        Gtk.main()
        return 0
    test = SelfTest(window, args.state_file)
    test.start()
    Gtk.main()
    return test.status


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
