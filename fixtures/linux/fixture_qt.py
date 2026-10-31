#!/usr/bin/env python3
"""A Qt 6 (PyQt6) application with known controls, so computer use on Linux can
be qualified per control family instead of per screenshot.

The Qt member of a set (`fixture_gtk3.py` beside it explains the set, the
state-file contract and why application-owned state is the oracle). The controls
and the names they record under are the GTK fixtures', with `qt.` in front, built
from Qt's own widgets: QListWidget, QTreeWidget, QTabWidget, QSlider and a
window-modal QMessageBox.

  python3 fixtures/linux/fixture_qt.py --state-file /tmp/qt.json
  python3 fixtures/linux/fixture_qt.py --self-test --state-file /tmp/qt.json

Qt publishes no accessibility tree unless an assistive technology is running
(org.a11y.Status.IsEnabled) or QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1 is set;
QT_ACCESSIBILITY=1 alone did not register Qt 6.4.2. The application sets neither
for itself: whoever starts it decides (`private-session.sh` does, for this one
process), which is the only way a person's own Qt applications ever get it.
Run it only inside that private session, never on someone's desktop.
"""

import argparse
import json
import os
import sys

from PyQt6.QtCore import Qt, QTimer
from PyQt6.QtWidgets import (
    QApplication, QCheckBox, QComboBox, QGroupBox, QHBoxLayout, QLabel, QLineEdit,
    QListWidget, QMainWindow, QMessageBox, QPushButton, QSlider, QTabWidget, QTreeWidget,
    QTreeWidgetItem, QVBoxLayout, QWidget,
)

TOOLKIT = "qt"
LABEL = "Qt"
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
    """The name a control records under: `qt.button`, `qt.list`, ..."""
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
    widget.setAccessibleName(name)
    return widget


def boxed(title, *children, layout=QVBoxLayout):
    """A titled group box, which is what gives a control inside it an ancestor
    LABEL: the thing that tells the two "Save" buttons apart."""
    box = QGroupBox(title)
    inner = layout(box)
    for child in children:
        inner.addWidget(child)
    return box


class FixtureWindow(QMainWindow):
    """One window of known controls. Each `_build_*` makes one family and wires
    it to the recorder once its initial state is set."""

    def __init__(self, recorder):
        super().__init__()
        self.recorder = recorder
        self.dialog = None
        self.setWindowTitle(f"Compux Fixture {LABEL}")
        self.resize(980, 720)
        self._build_menu()
        self.document_save = self._save_button("document.save")
        self.sidebar_save = self._save_button("sidebar.save")

        lists = QWidget()
        lists_layout = QHBoxLayout(lists)
        for title, widget in (("Fruit", self._build_list()), ("Folders", self._build_outline()),
                              ("History", self._build_history())):
            lists_layout.addWidget(boxed(title, widget))
        saves = QWidget()
        saves_layout = QHBoxLayout(saves)
        saves_layout.addWidget(boxed("Document", self.document_save))
        saves_layout.addWidget(boxed("Sidebar", self.sidebar_save))

        root = QWidget()
        layout = QVBoxLayout(root)
        layout.addWidget(boxed(LABEL, *self._build_basics()))
        layout.addWidget(lists)
        layout.addWidget(self._build_tabs())
        layout.addWidget(saves)
        self.setCentralWidget(root)

    def _build_basics(self):
        rec = self.recorder
        self.button = QPushButton(f"{LABEL} button")
        self.button.clicked.connect(lambda: rec.record(control("button"), "press"))

        self.checkbox = QCheckBox(f"{LABEL} checkbox")
        self.checkbox.toggled.connect(lambda on: rec.record(
            control("checkbox"), "toggle", value="on" if on else "off"))

        self.text_field = named(QLineEdit(placeholderText=f"{LABEL} text field"),
                                f"{LABEL} text field")
        self.text_field.textChanged.connect(lambda text: rec.record(
            control("text_field"), "set_value", value=text))

        # The text never reaches the file: only how much of it arrived.
        self.secure_field = named(QLineEdit(placeholderText=f"{LABEL} secure field",
                                            echoMode=QLineEdit.EchoMode.Password),
                                  f"{LABEL} secure field")
        self.secure_field.textChanged.connect(lambda text: rec.record(
            control("secure_field"), "set_value", length=len(text)))

        # Disabled, and its handler still records: a press that somehow reached a
        # disabled control is then visible rather than silent.
        self.disabled_button = QPushButton(f"{LABEL} disabled button", enabled=False)
        self.disabled_button.clicked.connect(
            lambda: rec.record(control("disabled_button"), "press"))

        self.popup = named(QComboBox(), f"{LABEL} pop-up button")
        self.popup.addItems(POPUP_ITEMS)
        self.popup.currentTextChanged.connect(lambda text: rec.record(
            control("popup"), "select", value=text))

        self.search = named(QLineEdit(placeholderText="Search"), "Search")
        self.search.textChanged.connect(lambda text: rec.record(
            control("search"), "set_value", value=text))

        self.slider = named(QSlider(Qt.Orientation.Horizontal, minimum=0, maximum=100, value=50),
                            "Volume")
        self.slider.valueChanged.connect(lambda value: rec.record(
            control("slider"), "set_value", value=str(value)))

        self.clear_all = QPushButton("Clear All…")
        self.clear_all.clicked.connect(self._open_dialog)
        return [self.button, self.checkbox, self.text_field, self.secure_field,
                self.disabled_button, self.popup, self.search, self.slider, self.clear_all]

    def search_focused(self, _old, new):
        """`QApplication.focusChanged`: the field gained focus, however it moved."""
        if new is self.search:
            self.recorder.record(control("search"), "focus")

    def _build_list(self):
        self.fruit = named(QListWidget(), "Fruit")
        self.fruit.addItems(FRUIT)
        self.fruit.itemSelectionChanged.connect(
            lambda: self._list_selected(self.fruit, control("list")))
        return self.fruit

    def _list_selected(self, view, name):
        # A selection cleared answers no items; only an item that became
        # selected is an event.
        items = view.selectedItems()
        if items:
            self.recorder.record(name, "select", value=items[0].text())

    def _outline_selected(self):
        items = self.outline.selectedItems()
        if items:
            self.recorder.record(control("outline"), "select", value=items[0].text(0))

    def _build_outline(self):
        self.outline = named(QTreeWidget(headerHidden=True), "Folders")
        self.folders = {}
        for parent, children in FOLDERS:
            item = QTreeWidgetItem([parent])
            item.addChildren([QTreeWidgetItem([child]) for child in children])
            self.outline.addTopLevelItem(item)
            self.folders[parent] = item
            for index in range(item.childCount()):
                self.folders[item.child(index).text(0)] = item.child(index)
        self.folders[INITIALLY_EXPANDED].setExpanded(True)
        self.outline.itemExpanded.connect(lambda item: self.recorder.record(
            control("outline"), "expand", value=item.text(0)))
        self.outline.itemCollapsed.connect(lambda item: self.recorder.record(
            control("outline"), "collapse", value=item.text(0)))
        self.outline.itemSelectionChanged.connect(self._outline_selected)
        return self.outline

    def _build_history(self):
        self.history = named(QListWidget(), "History")
        self.history.addItems([f"Entry {index}" for index in range(1, HISTORY_ROWS + 1)])
        self.history.setMinimumHeight(180)
        self.history.itemSelectionChanged.connect(
            lambda: self._list_selected(self.history, control("far_list")))
        self._history_at_bottom = False
        self.history.verticalScrollBar().valueChanged.connect(self._history_scrolled)
        return self.history

    def _history_scrolled(self, value):
        # One event per ARRIVAL at the end, not one per step of the way there.
        bar = self.history.verticalScrollBar()
        at_bottom = bar.maximum() > 0 and value >= bar.maximum()
        if at_bottom and not self._history_at_bottom:
            self.recorder.record(control("far_list"), "scroll", value="bottom")
        self._history_at_bottom = at_bottom

    def _build_tabs(self):
        self.tabs = named(QTabWidget(), "Tabs")
        for title in TABS:
            self.tabs.addTab(QLabel(f"{title} settings"), title)
        # Adding the first tab makes it current; connected after, that is silent.
        self.tabs.currentChanged.connect(lambda index: self.recorder.record(
            control("tabs"), "select", value=self.tabs.tabText(index)))
        return self.tabs

    def _build_menu(self):
        actions = self.menuBar().addMenu("Actions")
        self.menu_actions = {}
        for title in MENU_ITEMS:
            action = actions.addAction(title)
            action.triggered.connect(lambda _checked, title=title: self.recorder.record(
                control("menu"), "activate", value=title))
            self.menu_actions[title] = action

    def _save_button(self, name):
        button = QPushButton("Save")
        button.clicked.connect(lambda: self.recorder.record(name, "press"))
        return button

    def _open_dialog(self):
        """A window-modal question with OK and Cancel. `open()`, never `exec()`: a
        nested event loop would hide the answer from everything but this call."""
        buttons = QMessageBox.StandardButton.Ok | QMessageBox.StandardButton.Cancel
        self.dialog = QMessageBox(QMessageBox.Icon.Question, "Clear all?", "Clear all fields?",
                                  buttons, self)
        self.dialog.finished.connect(self._dialog_finished)
        self.dialog.open()
        self.recorder.record(control("dialog"), "open")

    def _dialog_finished(self, _result):
        # Cancel, Escape and the window's close button all decline.
        clicked = self.dialog.clickedButton()
        accepted = (clicked is not None
                    and self.dialog.standardButton(clicked) == QMessageBox.StandardButton.Ok)
        self.recorder.record(control("dialog"), "accept" if accepted else "cancel")
        self.dialog.deleteLater()
        self.dialog = None


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


def drive_every_control(window):
    window.button.click()
    window.checkbox.setChecked(True)
    window.text_field.setText("self-test")
    window.secure_field.setText("hunter2")
    # `click()` does nothing on a disabled button, so the signal is sent as a
    # press that reached it would send it.
    window.disabled_button.clicked.emit()
    window.popup.setCurrentIndex(1)
    window.search.setFocus()
    window.search.setText("fruit")
    window.slider.setValue(75)
    window.clear_all.click()
    window.dialog.button(QMessageBox.StandardButton.Ok).click()
    window.fruit.setCurrentRow(FRUIT.index("Cherry"))
    window.folders["Documents"].setExpanded(True)
    window.outline.setCurrentItem(window.folders["Letters"])
    window.folders[INITIALLY_EXPANDED].setExpanded(False)
    window.tabs.setCurrentIndex(TABS.index("Advanced"))
    bar = window.history.verticalScrollBar()
    bar.setValue(bar.maximum())
    window.history.setCurrentRow(HISTORY_ROWS - 1)
    window.menu_actions["Export Report"].trigger()
    window.document_save.click()
    window.sidebar_save.click()


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
    """Two steps on the event loop: after the window settles, check that showing
    it recorded nothing; then drive every control and check what was recorded."""

    def __init__(self, application, window, path):
        self.application, self.window, self.path, self.status = application, window, path, 1

    def start(self):
        QTimer.singleShot(SETTLE_MS, self._run)

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
        self.application.quit()


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
    application = QApplication([sys.argv[0]])
    window = FixtureWindow(Recorder(args.state_file))
    application.focusChanged.connect(window.search_focused)
    window.show()
    # Keyboard focus starts on the plain button, so showing the window can never
    # record a focus event on the search field.
    window.button.setFocus()
    if not args.self_test:
        return application.exec()
    test = SelfTest(application, window, args.state_file)
    test.start()
    application.exec()
    return test.status


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
