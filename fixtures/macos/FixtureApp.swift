// A native macOS application with known controls, so computer use can be
// qualified per control family instead of per screenshot.
//
// It exists because every other target is a moving one. TextEdit changes between
// releases, a browser rebuilds its accessibility tree on its own schedule, and
// neither tells you whether a control was really pressed or whether something
// else happened to look the same afterwards. This app has one window, a known set
// of controls in both native toolkits, and APPLICATION-OWNED STATE: every handler
// records what happened into a JSON file, so the question "did the press land"
// has an answer that does not come from a picture of the screen.
//
//   swiftc -o /tmp/FixtureApp fixtures/macos/FixtureApp.swift    (scripts/build_fixture_app.sh)
//   /tmp/FixtureApp --state-file /tmp/fixture.json
//   /tmp/FixtureApp --self-test --state-file /tmp/fixture.json   (exits 0, shows nothing)
//
// It is never part of the release bundle and nothing in the crate links it: it is
// a target for the live check and for later evals, and it lives here so those two
// are reading the same application.
//
// The controls, and why each one is here:
//
// How a change gets recorded, which is not the same question for every control:
//
//   * a PRESS is recorded from the control's own action. `AXPress` on an AppKit
//     button or checkbox goes through `performClick:`, which sends target/action,
//     so the handler really runs;
//   * a VALUE is recorded by OBSERVING THE VALUE, not by waiting for an action.
//     `AXUIElementSetAttributeValue(AXValue)` writes the field's string and does
//     NOT send its action — a fixture that recorded on action alone would show
//     nothing after a perfectly good `set_value` and read as a product defect
//     during qualification. The AppKit half samples its controls on a low-rate
//     timer, so a value is recorded however it arrived (typed, pasted or set);
//     the SwiftUI half uses bindings whose setter records, because its state is
//     not readable from outside.
//
//   * a BUTTON            — the ordinary `press`, in both toolkits;
//   * a CHECKBOX          — a control whose value changes when it is pressed, so
//                           `press` has something to verify against;
//   * a TEXT FIELD        — `set_value` with a read-back that must match;
//   * a SECURE FIELD      — `set_value` that can never verify, because the field
//                           reads back masked. The file records the LENGTH of
//                           what arrived and never the text itself;
//   * a DISABLED BUTTON   — must be refused `element_disabled`. Its handler still
//                           records, so a press that somehow landed is visible
//                           rather than silent;
//   * a POP-UP BUTTON     — a control whose press opens a menu, which is where
//                           "the foreground did not move" gets tested;
//   * TWO GROUPS, each with a button labelled "Save" — the same label twice, so
//                           the `path` of ancestor labels is the only thing that
//                           tells them apart.
//
// And, in the AppKit half only, the controls the Linux fixtures
// (`fixtures/linux/`) also carry, so the desktop corpus asks the same things of
// every fixture. They record under the same names with `appkit.` in front:
//
//   * a SEARCH FIELD      — typing into a field, and `focus`: it records when it
//                           becomes the window's first responder;
//   * a FRUIT LIST        — `select` on a table row;
//   * a FOLDERS OUTLINE   — `expand` and `collapse` on disclosure rows (Projects
//                           starts expanded, so collapse has a target), and
//                           `select` on any row;
//   * TABS                — `select` on a tab;
//   * a VOLUME SLIDER     — `set_value` on a range;
//   * a HISTORY LIST      — two hundred rows whose last one starts far out of
//                           view: `scroll_to`, recorded as `scroll` / `bottom`
//                           when the list reaches its end, then `select`;
//   * CLEAR ALL…          — a SHEET with OK and Cancel: a modal dialog;
//   * an ACTIONS MENU     — in the menu bar, with Export Report and Print
//                           Preview: a menu item.

import AppKit
import SwiftUI

// --- what the application records --------------------------------------------

/// One thing that happened, as the state file carries it.
struct FixtureEvent: Codable {
    let seq: Int
    let control: String
    let action: String
    /// The value a control ended up with, where publishing one is harmless.
    let value: String?
    /// How many characters a secure field received. The text itself is never
    /// written anywhere: a fixture that logged passwords would be a worse problem
    /// than the one it is here to find.
    let length: Int?
}

struct FixtureState: Codable {
    var version: Int
    var events: [FixtureEvent]
}

/// The state file, rewritten in full on every event.
///
/// Written to a temporary file and renamed over the target, so a reader always
/// sees a complete JSON document rather than a half-written one — the live check
/// reads this WHILE the app is running.
final class Recorder {
    private let path: URL
    private var state = FixtureState(version: 1, events: [])

    init(path: URL) {
        self.path = path
        write()
    }

    /// Record one interaction. Returns the sequence number, so a caller can say
    /// which event it just caused.
    @discardableResult
    func record(_ control: String, _ action: String, value: String? = nil, length: Int? = nil)
        -> Int
    {
        let seq = state.events.count + 1
        state.events.append(
            FixtureEvent(seq: seq, control: control, action: action, value: value, length: length))
        write()
        return seq
    }

    func events() -> [FixtureEvent] { state.events }

    private func write() {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
        guard let data = try? encoder.encode(state) else { return }

        let scratch = path.deletingLastPathComponent()
            .appendingPathComponent(".\(path.lastPathComponent).writing")
        do {
            try data.write(to: scratch)
            _ = try FileManager.default.replaceItemAt(path, withItemAt: scratch)
        } catch {
            FileHandle.standardError.write(
                Data("FixtureApp: could not write \(path.path): \(error)\n".utf8))
        }
    }
}

// --- the handlers ------------------------------------------------------------
//
// The UI and `--self-test` call THESE, never each other's copies, so the test
// proves the recording path the live check depends on without needing a window
// server, an Accessibility grant, or a single synthetic event.

/// Every control this application publishes, by the name it records under.
enum Control: String, CaseIterable {
    case appkitButton = "appkit.button"
    case appkitCheckbox = "appkit.checkbox"
    case appkitTextField = "appkit.text_field"
    case appkitSecureField = "appkit.secure_field"
    case appkitDisabledButton = "appkit.disabled_button"
    case appkitPopUp = "appkit.popup"
    case appkitSearch = "appkit.search"
    case appkitList = "appkit.list"
    case appkitOutline = "appkit.outline"
    case appkitTabs = "appkit.tabs"
    case appkitSlider = "appkit.slider"
    case appkitFarList = "appkit.far_list"
    case appkitDialog = "appkit.dialog"
    case appkitMenu = "appkit.menu"

    case swiftuiButton = "swiftui.button"
    case swiftuiCheckbox = "swiftui.checkbox"
    case swiftuiTextField = "swiftui.text_field"
    case swiftuiSecureField = "swiftui.secure_field"
    case swiftuiDisabledButton = "swiftui.disabled_button"
    case swiftuiPopUp = "swiftui.popup"

    /// The buttons that share a label and differ only in their ancestry, one pair
    /// per toolkit: a path that works in AppKit and collapses in SwiftUI is a
    /// difference the support matrix has to be able to see.
    case documentSave = "document.save"
    case sidebarSave = "sidebar.save"
    case draftSave = "draft.save"
    case archiveSave = "archive.save"
}

/// Watches a control's value and records the change, whoever made it.
///
/// This is what makes `set_value` visible: the accessibility setter writes the
/// value and sends no action, so nothing target/action-shaped fires. Reading the
/// value on a low cadence records a change that arrived by ANY route — typed,
/// pasted, or set through the accessibility API — which is exactly the question
/// the live check is asking.
final class ValueWatcher {
    private let read: () -> String
    private let record: (String) -> Void
    private var last: String

    init(initial: String, read: @escaping () -> String, record: @escaping (String) -> Void) {
        self.read = read
        self.record = record
        self.last = initial
    }

    func sample() {
        let now = read()
        guard now != last else { return }
        last = now
        record(now)
    }
}

/// The one implementation of "what happens when a control is used".
struct Handlers {
    let recorder: Recorder

    func pressed(_ control: Control) {
        recorder.record(control.rawValue, "press")
    }

    func toggled(_ control: Control, on: Bool) {
        recorder.record(control.rawValue, "toggle", value: on ? "on" : "off")
    }

    func typed(_ control: Control, text: String) {
        recorder.record(control.rawValue, "set_value", value: text)
    }

    /// A secure field records that it received something, and how much. Never what.
    func typedSecurely(_ control: Control, text: String) {
        recorder.record(control.rawValue, "set_value", length: text.count)
    }

    func selected(_ control: Control, title: String) {
        recorder.record(control.rawValue, "select", value: title)
    }

    func focused(_ control: Control) {
        recorder.record(control.rawValue, "focus")
    }

    /// A range's value, recorded as the text of a whole number.
    func moved(_ control: Control, to value: String) {
        recorder.record(control.rawValue, "set_value", value: value)
    }

    func expanded(_ control: Control, row: String) {
        recorder.record(control.rawValue, "expand", value: row)
    }

    func collapsed(_ control: Control, row: String) {
        recorder.record(control.rawValue, "collapse", value: row)
    }

    /// A long list reached its end, however it was scrolled there.
    func scrolledToBottom(_ control: Control) {
        recorder.record(control.rawValue, "scroll", value: "bottom")
    }

    func opened(_ control: Control) {
        recorder.record(control.rawValue, "open")
    }

    /// OK accepts; Cancel and Escape decline.
    func answered(_ control: Control, accepted: Bool) {
        recorder.record(control.rawValue, accepted ? "accept" : "cancel")
    }

    func activated(_ control: Control, title: String) {
        recorder.record(control.rawValue, "activate", value: title)
    }

    /// Drive every control once, in the order they are declared. This is what
    /// `--self-test` runs and what the window's controls call one at a time, so a
    /// change to a handler is proved by the test that needs no screen.
    func driveEverything() {
        for control in Control.allCases {
            switch control {
            case .appkitCheckbox, .swiftuiCheckbox:
                toggled(control, on: true)
            case .appkitTextField, .swiftuiTextField, .appkitSearch:
                typed(control, text: "self-test")
            case .appkitSecureField, .swiftuiSecureField:
                typedSecurely(control, text: "hunter2")
            case .appkitPopUp, .swiftuiPopUp:
                selected(control, title: "Second")
            case .appkitList:
                selected(control, title: "Cherry")
            case .appkitOutline:
                expanded(control, row: "Documents")
            case .appkitTabs:
                selected(control, title: "Advanced")
            case .appkitSlider:
                moved(control, to: "75")
            case .appkitFarList:
                scrolledToBottom(control)
            case .appkitDialog:
                opened(control)
            case .appkitMenu:
                activated(control, title: "Export Report")
            default:
                pressed(control)
            }
        }
    }
}

// --- the window --------------------------------------------------------------

/// The AppKit half: one box of controls, each wired to the shared handlers.
final class AppKitGroup: NSViewController {
    private let handlers: Handlers
    /// Every control whose VALUE is watched rather than waited on. Held here so
    /// the timer that samples them lives as long as the window does.
    private(set) var watchers: [ValueWatcher] = []

    init(handlers: Handlers) {
        self.handlers = handlers
        super.init(nibName: nil, bundle: nil)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("not loaded from a nib") }

    override func loadView() {
        let stack = NSStackView()
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 8

        let button = NSButton(title: "AppKit button", target: self, action: #selector(press))
        let checkbox = NSButton(
            checkboxWithTitle: "AppKit checkbox", target: self, action: #selector(toggle))

        // No target/action on the fields: the accessibility setter writes the
        // string and sends no action, so these are WATCHED instead.
        let field = NSTextField(string: "")
        field.placeholderString = "AppKit text field"
        field.setAccessibilityLabel("AppKit text field")

        let secure = NSSecureTextField(string: "")
        secure.placeholderString = "AppKit secure field"
        secure.setAccessibilityLabel("AppKit secure field")

        let disabled = NSButton(
            title: "AppKit disabled button", target: self, action: #selector(pressDisabled))
        disabled.isEnabled = false

        let popUp = NSPopUpButton()
        popUp.addItems(withTitles: ["First", "Second", "Third"])
        popUp.setAccessibilityLabel("AppKit pop-up button")

        let search = NSSearchField(string: "")
        search.placeholderString = "Search"
        search.setAccessibilityLabel("Search")

        let slider = NSSlider(value: 50, minValue: 0, maxValue: 100, target: nil, action: nil)
        slider.setAccessibilityLabel("Volume")

        let clearAll = NSButton(title: "Clear All…", target: self, action: #selector(askToClearAll))

        for control in [button, checkbox, field, secure, disabled, popUp, search, slider, clearAll]
            as [NSView]
        {
            stack.addArrangedSubview(control)
        }

        let handlers = self.handlers
        watchers = [
            ValueWatcher(initial: "", read: { field.stringValue }) { value in
                handlers.typed(.appkitTextField, text: value)
            },
            ValueWatcher(initial: "", read: { secure.stringValue }) { value in
                handlers.typedSecurely(.appkitSecureField, text: value)
            },
            ValueWatcher(initial: "off", read: { checkbox.state == .on ? "on" : "off" }) { value in
                handlers.toggled(.appkitCheckbox, on: value == "on")
            },
            ValueWatcher(initial: "First", read: { popUp.titleOfSelectedItem ?? "" }) { value in
                handlers.selected(.appkitPopUp, title: value)
            },
            ValueWatcher(initial: "", read: { search.stringValue }) { value in
                handlers.typed(.appkitSearch, text: value)
            },
            // Focus is the window's first responder, which for a text field is the
            // shared field editor whose delegate is that field. Only gaining focus
            // is an event; losing it is the next control's business.
            ValueWatcher(initial: "", read: { isEditing(search) ? "focused" : "" }) { value in
                if value == "focused" { handlers.focused(.appkitSearch) }
            },
            ValueWatcher(initial: "50", read: { String(slider.integerValue) }) { value in
                handlers.moved(.appkitSlider, to: value)
            },
        ]

        view = boxed(title: "AppKit", content: stack)
    }

    @objc private func press() { handlers.pressed(.appkitButton) }
    @objc private func pressDisabled() { handlers.pressed(.appkitDisabledButton) }

    /// A sheet on the fixture window with OK and Cancel. A sheet is modal for its
    /// window, and compux names it a `dialog` surface.
    @objc private func askToClearAll() {
        guard let window = view.window else { return }
        let alert = NSAlert()
        alert.messageText = "Clear all fields?"
        alert.addButton(withTitle: "OK")
        alert.addButton(withTitle: "Cancel")
        let handlers = self.handlers
        handlers.opened(.appkitDialog)
        alert.beginSheetModal(for: window) { response in
            handlers.answered(.appkitDialog, accepted: response == .alertFirstButtonReturn)
        }
    }

    // The checkbox's own action is deliberately not wired: its state is watched,
    // so one change is recorded once whether it arrived by `AXPress`, by a click
    // or by a value set.
    @objc private func toggle(_ sender: NSButton) {
        _ = sender
    }
}

/// The rows of a one-column table, read through the classic (cell-based) data
/// source, so the table publishes plain rows with no views of its own.
final class Rows: NSObject, NSTableViewDataSource {
    let names: [String]

    init(_ names: [String]) { self.names = names }

    func numberOfRows(in tableView: NSTableView) -> Int { names.count }

    func tableView(_ tableView: NSTableView, objectValueFor tableColumn: NSTableColumn?, row: Int)
        -> Any?
    {
        names[row]
    }

    /// The selected row's name, or "" when nothing is selected.
    func selection(of tableView: NSTableView) -> String {
        tableView.selectedRow >= 0 ? names[tableView.selectedRow] : ""
    }
}

/// One row of the outline. An object, not a string: the outline view tells rows
/// apart by identity, and a bridged string is a new object every time.
final class Folder: NSObject {
    let name: String
    let children: [Folder]

    init(_ name: String, _ children: [String] = []) {
        self.name = name
        self.children = children.map { Folder($0) }
    }
}

final class Folders: NSObject, NSOutlineViewDataSource {
    let roots = [
        Folder("Documents", ["Invoices", "Letters"]),
        Folder("Pictures", ["Holidays", "Family"]),
        Folder("Projects", ["Alpha", "Beta"]),
        Folder("Music"),
    ]

    private func children(of item: Any?) -> [Folder] { (item as? Folder)?.children ?? roots }

    func outlineView(_ outlineView: NSOutlineView, numberOfChildrenOfItem item: Any?) -> Int {
        children(of: item).count
    }

    func outlineView(_ outlineView: NSOutlineView, child index: Int, ofItem item: Any?) -> Any {
        children(of: item)[index]
    }

    func outlineView(_ outlineView: NSOutlineView, isItemExpandable item: Any) -> Bool {
        !((item as? Folder)?.children.isEmpty ?? true)
    }

    func outlineView(
        _ outlineView: NSOutlineView, objectValueFor tableColumn: NSTableColumn?, byItem item: Any?
    ) -> Any? {
        (item as? Folder)?.name
    }

    func folder(named name: String) -> Folder? { roots.first { $0.name == name } }
}

/// A one-column table or outline in a scroll view of a fixed height, so a long
/// one keeps most of its rows out of view.
private func scrolled(_ table: NSTableView, label: String) -> NSScrollView {
    let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier(label))
    column.width = 180
    table.addTableColumn(column)
    table.headerView = nil
    table.setAccessibilityLabel(label)
    let scroll = NSScrollView()
    scroll.documentView = table
    scroll.hasVerticalScroller = true
    scroll.translatesAutoresizingMaskIntoConstraints = false
    scroll.widthAnchor.constraint(equalToConstant: 200).isActive = true
    scroll.heightAnchor.constraint(equalToConstant: 180).isActive = true
    return scroll
}

/// Whether a scroll view shows the very end of what it scrolls.
private func atBottom(_ scroll: NSScrollView) -> Bool {
    guard let document = scroll.documentView else { return false }
    let visible = scroll.contentView.documentVisibleRect
    return document.bounds.height > visible.height && visible.maxY >= document.bounds.height - 1
}

/// The AppKit collections: the fruit list, the folders outline, the history
/// list and the tabs. Selections, the tab and the scroll position are WATCHED,
/// like the AppKit fields, so a change that arrived through the accessibility
/// API is recorded as surely as a click. Expanding and collapsing post their own
/// notifications, whoever caused them, so those are observed instead.
final class AppKitCollections: NSViewController {
    private let handlers: Handlers
    private let fruit = Rows(["Apple", "Banana", "Cherry", "Damson", "Elderberry"])
    private let history = Rows((1...200).map { "Entry \($0)" })
    private let folders = Folders()
    private(set) var watchers: [ValueWatcher] = []
    private var observers: [NSObjectProtocol] = []

    init(handlers: Handlers) {
        self.handlers = handlers
        super.init(nibName: nil, bundle: nil)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("not loaded from a nib") }

    deinit { observers.forEach { NotificationCenter.default.removeObserver($0) } }

    override func loadView() {
        let fruitTable = NSTableView()
        fruitTable.dataSource = fruit
        let historyTable = NSTableView()
        historyTable.dataSource = history
        let historyScroll = scrolled(historyTable, label: "History")

        let tabs = NSTabView()
        for title in ["General", "Advanced", "About"] {
            let item = NSTabViewItem(identifier: title)
            item.label = title
            item.view = NSTextField(labelWithString: "\(title) settings")
            tabs.addTabViewItem(item)
        }
        tabs.setAccessibilityLabel("Tabs")
        tabs.translatesAutoresizingMaskIntoConstraints = false
        tabs.widthAnchor.constraint(equalToConstant: 260).isActive = true
        tabs.heightAnchor.constraint(equalToConstant: 120).isActive = true

        let row = NSStackView(views: [
            boxed(title: "Fruit", content: scrolled(fruitTable, label: "Fruit")),
            boxed(title: "Folders", content: foldersOutline()),
            boxed(title: "History", content: historyScroll),
            boxed(title: "Tabs", content: tabs),
        ])
        row.orientation = .horizontal
        row.alignment = .top
        row.spacing = 16

        let handlers = self.handlers
        let fruit = self.fruit
        let history = self.history
        let tabTitle = { tabs.selectedTabViewItem?.label ?? "" }
        watchers += [
            ValueWatcher(initial: "", read: { fruit.selection(of: fruitTable) }) { value in
                if !value.isEmpty { handlers.selected(.appkitList, title: value) }
            },
            ValueWatcher(initial: "", read: { history.selection(of: historyTable) }) { value in
                if !value.isEmpty { handlers.selected(.appkitFarList, title: value) }
            },
            // One event per ARRIVAL at the end, not one per step of the way there.
            ValueWatcher(initial: "", read: { atBottom(historyScroll) ? "bottom" : "" }) { value in
                if value == "bottom" { handlers.scrolledToBottom(.appkitFarList) }
            },
            ValueWatcher(initial: "General", read: tabTitle) { value in
                handlers.selected(.appkitTabs, title: value)
            },
        ]
        view = row
    }

    /// The outline, with Projects expanded BEFORE anything observes it, so the
    /// window opens with something to collapse and records nothing for it.
    private func foldersOutline() -> NSScrollView {
        let outline = NSOutlineView()
        let scroll = scrolled(outline, label: "Folders")
        outline.outlineTableColumn = outline.tableColumns.first
        outline.dataSource = folders
        outline.reloadData()
        outline.expandItem(folders.folder(named: "Projects"))

        let handlers = self.handlers
        let center = NotificationCenter.default
        observers = [
            center.addObserver(
                forName: NSOutlineView.itemDidExpandNotification, object: outline, queue: .main
            ) { note in
                guard let folder = note.userInfo?["NSObject"] as? Folder else { return }
                handlers.expanded(.appkitOutline, row: folder.name)
            },
            center.addObserver(
                forName: NSOutlineView.itemDidCollapseNotification, object: outline, queue: .main
            ) { note in
                guard let folder = note.userInfo?["NSObject"] as? Folder else { return }
                handlers.collapsed(.appkitOutline, row: folder.name)
            },
        ]
        watchers.append(
            ValueWatcher(
                initial: "",
                read: { (outline.item(atRow: outline.selectedRow) as? Folder)?.name ?? "" }
            ) { value in
                if !value.isEmpty { handlers.selected(.appkitOutline, title: value) }
            })
        return scroll
    }
}

/// The SwiftUI half, with the same controls: a control family can behave
/// differently per toolkit, which is exactly what the support matrix records.
struct SwiftUIGroup: View {
    let handlers: Handlers
    @State private var checked = false
    @State private var text = ""
    @State private var secret = ""
    @State private var choice = "First"

    /// A binding whose SETTER records. SwiftUI state cannot be read from outside
    /// the view, so the AppKit half's sampler has nothing to sample here; this is
    /// the same idea from the inside — whoever writes the value, including the
    /// accessibility setter when SwiftUI routes it through the binding, is
    /// recorded once as it happens.
    private func recording<T: Equatable>(
        _ state: Binding<T>, _ record: @escaping (T) -> Void
    ) -> Binding<T> {
        Binding(
            get: { state.wrappedValue },
            set: { value in
                let changed = value != state.wrappedValue
                state.wrappedValue = value
                if changed { record(value) }
            })
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Button("SwiftUI button") { handlers.pressed(.swiftuiButton) }

            Toggle(
                "SwiftUI checkbox",
                isOn: recording($checked) { handlers.toggled(.swiftuiCheckbox, on: $0) }
            )
            .toggleStyle(.checkbox)

            TextField(
                "SwiftUI text field",
                text: recording($text) { handlers.typed(.swiftuiTextField, text: $0) }
            )
            .accessibilityLabel("SwiftUI text field")

            SecureField(
                "SwiftUI secure field",
                text: recording($secret) { handlers.typedSecurely(.swiftuiSecureField, text: $0) }
            )
            .accessibilityLabel("SwiftUI secure field")

            Button("SwiftUI disabled button") { handlers.pressed(.swiftuiDisabledButton) }
                .disabled(true)

            Picker(
                "SwiftUI pop-up button",
                selection: recording($choice) { handlers.selected(.swiftuiPopUp, title: $0) }
            ) {
                ForEach(["First", "Second", "Third"], id: \.self) { Text($0) }
            }
            .accessibilityLabel("SwiftUI pop-up button")
        }
        .frame(width: 260, alignment: .leading)
        .padding(8)
    }
}

/// One SwiftUI button in its own view, so a labelled box can be wrapped around it.
struct SwiftUISave: View {
    let handlers: Handlers
    let control: Control

    var body: some View {
        Button("Save") { handlers.pressed(control) }
            .padding(8)
    }
}

/// Whether a text field is the one being edited: its window's first responder is
/// the shared field editor, and the editor's delegate is that field.
private func isEditing(_ field: NSTextField) -> Bool {
    guard let editor = field.window?.firstResponder as? NSText else { return false }
    return editor.delegate === field
}

/// A titled box, which is what gives an element inside it an ancestor LABEL —
/// the thing that tells the two "Save" buttons apart.
private func boxed(title: String, content: NSView) -> NSView {
    let box = NSBox()
    box.title = title
    box.setAccessibilityLabel(title)
    box.contentView = content
    box.translatesAutoresizingMaskIntoConstraints = false
    return box
}

final class FixtureWindow: NSObject, NSApplicationDelegate {
    private let handlers: Handlers
    private var window: NSWindow?
    private var appKit: AppKitGroup?
    private var collections: AppKitCollections?
    private var sampler: Timer?

    init(handlers: Handlers) { self.handlers = handlers }

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.mainMenu = mainMenu()
        let appKit = AppKitGroup(handlers: handlers)
        self.appKit = appKit
        let collections = AppKitCollections(handlers: handlers)
        self.collections = collections

        // The hosting view goes INSIDE a titled box, like the AppKit half. Without
        // one it publishes no group of its own, so every SwiftUI control's `path`
        // collapses to the window title and the two halves are not comparable.
        let swiftUI = boxed(
            title: "SwiftUI",
            content: NSHostingView(rootView: SwiftUIGroup(handlers: handlers)))

        // Groups whose only difference is their ancestry. Every button says
        // "Save", so a caller that cannot read a path cannot tell them apart —
        // which is the whole reason a path is published. One pair per toolkit.
        let document = boxed(title: "Document", content: saveButton(.documentSave))
        let sidebar = boxed(title: "Sidebar", content: saveButton(.sidebarSave))
        let draft = boxed(
            title: "Draft",
            content: NSHostingView(
                rootView: SwiftUISave(handlers: handlers, control: .draftSave)))
        let archive = boxed(
            title: "Archive",
            content: NSHostingView(
                rootView: SwiftUISave(handlers: handlers, control: .archiveSave)))

        let columns = NSStackView(views: [appKit.view, swiftUI])
        columns.orientation = .horizontal
        columns.alignment = .top
        columns.spacing = 16

        let saves = NSStackView(views: [document, sidebar, draft, archive])
        saves.orientation = .horizontal
        saves.spacing = 16

        let root = NSStackView(views: [columns, collections.view, saves])
        root.orientation = .vertical
        root.alignment = .leading
        root.spacing = 16
        root.edgeInsets = NSEdgeInsets(top: 16, left: 16, bottom: 16, right: 16)

        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1040, height: 880),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false)
        window.title = "Compux Fixture"
        window.contentView = root
        window.center()
        window.makeKeyAndOrderFront(nil)
        self.window = window

        NSApp.setActivationPolicy(.regular)
        NSApp.activate(ignoringOtherApps: true)

        // Four times a second: fast enough that a check reading the file right
        // after an action sees it, slow enough to cost nothing. This is what makes
        // a `set_value` visible at all — the accessibility setter sends no action.
        sampler = Timer.scheduledTimer(withTimeInterval: 0.25, repeats: true) { [weak self] _ in
            self?.appKit?.watchers.forEach { $0.sample() }
            self?.collections?.watchers.forEach { $0.sample() }
        }
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }

    /// The application menu (Quit) and an Actions menu whose items record their
    /// own titles. A menu item is the one control family that lives outside the
    /// window, in the menu bar.
    private func mainMenu() -> NSMenu {
        let appMenu = NSMenu(title: "Compux Fixture")
        appMenu.addItem(
            NSMenuItem(
                title: "Quit Compux Fixture", action: #selector(NSApplication.terminate(_:)),
                keyEquivalent: "q"))
        let appItem = NSMenuItem(title: "Compux Fixture", action: nil, keyEquivalent: "")
        appItem.submenu = appMenu

        let actions = NSMenu(title: "Actions")
        for title in ["Export Report", "Print Preview"] {
            let item = NSMenuItem(
                title: title, action: #selector(chooseMenuItem(_:)), keyEquivalent: "")
            item.target = self
            actions.addItem(item)
        }
        let actionsItem = NSMenuItem(title: "Actions", action: nil, keyEquivalent: "")
        actionsItem.submenu = actions

        let menu = NSMenu()
        menu.addItem(appItem)
        menu.addItem(actionsItem)
        return menu
    }

    @objc private func chooseMenuItem(_ sender: NSMenuItem) {
        handlers.activated(.appkitMenu, title: sender.title)
    }

    private func saveButton(_ control: Control) -> NSView {
        let button = NSButton(title: "Save", target: self, action: #selector(save(_:)))
        button.identifier = NSUserInterfaceItemIdentifier(control.rawValue)
        let stack = NSStackView(views: [button])
        stack.orientation = .vertical
        return stack
    }

    @objc private func save(_ sender: NSButton) {
        let name = sender.identifier?.rawValue ?? Control.documentSave.rawValue
        handlers.pressed(Control(rawValue: name) ?? .documentSave)
    }
}

// --- entry point -------------------------------------------------------------

@main
enum FixtureMain {
    static func main() {
        var arguments = CommandLine.arguments.dropFirst()
        var statePath = "/tmp/compux-fixture.json"
        var selfTest = false

        while let argument = arguments.first {
            arguments = arguments.dropFirst()
            switch argument {
            case "--state-file":
                guard let next = arguments.first else { fail("--state-file needs a path") }
                statePath = next
                arguments = arguments.dropFirst()
            case "--self-test":
                selfTest = true
            case "--help", "-h":
                print("usage: FixtureApp [--state-file PATH] [--self-test]")
                exit(0)
            default:
                fail("unknown argument: \(argument)")
            }
        }

        let recorder = Recorder(path: URL(fileURLWithPath: statePath))
        let handlers = Handlers(recorder: recorder)

        if selfTest {
            exit(runSelfTest(handlers: handlers, path: statePath))
        }

        let application = NSApplication.shared
        let delegate = FixtureWindow(handlers: handlers)
        application.delegate = delegate
        application.run()
    }

    /// Drive every handler in process and read the file back.
    ///
    /// No window is created and no event is posted, so this runs over SSH, in a
    /// terminal with no GUI session, and on a machine with no Accessibility grant.
    ///
    /// What it proves: that every control has a handler, that each one writes the
    /// event the live check looks for, and that a secure field's text never
    /// reaches the file. What it CANNOT prove, and the live check must: that an
    /// `AXPress` reaches a handler at all, and that a value set through the
    /// accessibility API is seen by the watcher — neither the window nor the
    /// sampling timer exists here, so a green self-test says nothing about either.
    static func runSelfTest(handlers: Handlers, path: String) -> Int32 {
        handlers.driveEverything()

        guard let data = FileManager.default.contents(atPath: path),
            let state = try? JSONDecoder().decode(FixtureState.self, from: data)
        else {
            print("FAIL: \(path) is not a readable fixture state file")
            return 1
        }

        var failures: [String] = []
        if state.events.count != Control.allCases.count {
            failures.append(
                "expected \(Control.allCases.count) events, the file has \(state.events.count)")
        }

        for (index, control) in Control.allCases.enumerated() {
            guard index < state.events.count else { break }
            let event = state.events[index]
            if event.control != control.rawValue {
                failures.append("event \(index + 1) is \(event.control), expected \(control.rawValue)")
            }
            if event.seq != index + 1 {
                failures.append("event \(index + 1) is numbered \(event.seq)")
            }
        }

        // The one thing this file must never contain.
        for event in state.events where event.control.hasSuffix("secure_field") {
            if event.value != nil {
                failures.append("\(event.control) wrote its text into the state file")
            }
            if event.length != "hunter2".count {
                failures.append("\(event.control) recorded no length")
            }
        }

        guard failures.isEmpty else {
            for failure in failures { print("FAIL: \(failure)") }
            return 1
        }

        print("ok: \(state.events.count) controls recorded into \(path)")
        print("note: this drives the handlers in process — no window, no timer, no input — so")
        print("      it proves the recording path and NOT that an AXPress reaches a handler or")
        print("      that a value set through the accessibility API is seen. That is §2 and §3")
        print("      of the live check, and it needs a screen.")
        return 0
    }

    static func fail(_ message: String) -> Never {
        FileHandle.standardError.write(Data("FixtureApp: \(message)\n".utf8))
        exit(2)
    }
}
