// The badge, the menu-bar item, and the rule that the indicator renders what the
// helper SAYS.
//
// Everything here is a consequence of one requirement: a person typing in their own
// window must not be interrupted by the thing telling them Fermix is working. So the
// panel is a non-activating one that cannot become key or main, the application is an
// accessory that is never activated, the buttons answer a click in an inactive window
// (`acceptsFirstMouse`), and there is no animation at all — which is also how Reduce
// Motion is honoured: there is nothing to reduce.
//
// The second requirement is that the badge never lies. A press writes its event and
// changes NOTHING on screen; the helper's next state line is what moves the badge,
// relabels the button and hides it. An optimistic "Resume" on a helper that never
// paused would be a badge claiming a state the gate is not in.
//
// The third is that it is never in the way: `sharingType = .none` keeps it out of every
// capture (including the helper's own), `.transient` keeps it out of Mission Control,
// `.ignoresCycle` out of the window cycle, and it is hidden outright whenever it would
// otherwise sit over a window that is not the target.
//
// The fourth is the SIZE CONTRACT in `Placement`: the helper decides "occluded" by
// hit-testing a rectangle it computes from those same numbers, so a panel wider or
// taller than them would be a badge the helper believes is clear when it is covered —
// and therefore a badge drawn on somebody else's window. `BadgeContent` is a type of
// its own for exactly that reason: a row of views lays out off screen just as well as
// on it, so the width in the contract is a MEASURED number and the cap is enforced by a
// constraint rather than by hope.

import AppKit

/// A panel that cannot take the person's focus, whatever is clicked in it.
final class BadgePanel: NSPanel {
    override var canBecomeKey: Bool { false }
    override var canBecomeMain: Bool { false }
}

/// A button that answers the first click in an inactive window, instead of spending it
/// on activating the application — which is the one thing this process must never do.
final class FirstMouseButton: NSButton {
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }
}

/// Everything the badge draws, and nothing that needs a window.
final class BadgeContent {
    let view: NSVisualEffectView
    let pauseButton: FirstMouseButton
    let stopButton: FirstMouseButton

    private let glyphField: NSTextField
    private let targetField: NSTextField
    private let stateField: NSTextField

    init(target: AnyObject?, pause: Selector, stop: Selector) {
        view = NSVisualEffectView()
        glyphField = NSTextField(labelWithString: "")
        targetField = NSTextField(labelWithString: "")
        stateField = NSTextField(labelWithString: "")
        pauseButton = FirstMouseButton(title: "Pause", target: target, action: pause)
        stopButton = FirstMouseButton(title: "Stop", target: target, action: stop)

        view.material = .hudWindow
        view.blendingMode = .behindWindow
        view.state = .active
        view.wantsLayer = true
        view.layer?.cornerRadius = 8
        view.layer?.masksToBounds = true

        glyphField.font = .systemFont(ofSize: 11)
        // The word beside it says the same thing; a screen reader reading "black
        // circle" as well would only be noise.
        glyphField.setAccessibilityElement(false)

        let name = NSTextField(labelWithString: "Fermix")
        name.font = .systemFont(ofSize: 12, weight: .semibold)
        name.setAccessibilityLabel("Fermix")

        // One line, truncated to the pixel as well as to a character count: the title
        // belongs to another application and may be anything at all. It is also the ONE
        // view allowed to give ground when the row would otherwise outgrow the width
        // contract — the state word and the buttons keep their size, so a long title
        // costs characters and never costs the person a Stop button.
        targetField.font = .systemFont(ofSize: 12)
        targetField.textColor = .secondaryLabelColor
        targetField.lineBreakMode = .byTruncatingTail
        targetField.usesSingleLineMode = true
        targetField.maximumNumberOfLines = 1
        targetField.setAccessibilityLabel("Target window")
        targetField.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        targetField.setContentHuggingPriority(.defaultLow, for: .horizontal)

        stateField.font = .systemFont(ofSize: 12)
        stateField.setAccessibilityLabel("State")
        stateField.setContentCompressionResistancePriority(.required, for: .horizontal)

        for button in [pauseButton, stopButton] {
            button.controlSize = .small
            button.font = .systemFont(ofSize: 11)
            button.setContentCompressionResistancePriority(.required, for: .horizontal)
        }
        stopButton.setAccessibilityLabel("Stop Fermix")
        stopButton.setAccessibilityHelp("End the session and release this window")
        stopButton.toolTip = "End the session and release this window"

        let stack = NSStackView(views: [
            glyphField, name, targetField, stateField, pauseButton, stopButton,
        ])
        stack.orientation = .horizontal
        stack.alignment = .centerY
        stack.spacing = 8
        stack.edgeInsets = NSEdgeInsets(top: 0, left: 10, bottom: 0, right: 8)
        stack.translatesAutoresizingMaskIntoConstraints = false

        view.addSubview(stack)
        // The height IS the contract's number, and the row is centred in it rather than
        // pinned to it, so no control's intrinsic height can ever fight the constant.
        // The width is capped by the contract and free below it.
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: view.trailingAnchor),
            stack.centerYAnchor.constraint(equalTo: view.centerYAnchor),
            view.heightAnchor.constraint(equalToConstant: Placement.badgeHeight),
            view.widthAnchor.constraint(lessThanOrEqualToConstant: Placement.maxBadgeWidth),
        ])
    }

    /// Put one state on the screen. `label` is already bounded by `targetLabel`.
    func render(_ activity: TargetState.Activity, label: String) {
        let word = Self.word(for: activity)
        let resuming = activity == .paused

        glyphField.stringValue = Self.glyph(for: activity)
        glyphField.textColor = Self.glyphColor(for: activity)
        targetField.stringValue = label
        targetField.toolTip = label
        stateField.stringValue = word
        stateField.textColor = activity == .unavailable ? .secondaryLabelColor : .labelColor

        pauseButton.title = resuming ? "Resume" : "Pause"
        pauseButton.setAccessibilityLabel(resuming ? "Resume Fermix" : "Pause Fermix")
        let help =
            resuming
            ? "Let Fermix carry on acting on this window"
            : "Hold Fermix until you resume it"
        pauseButton.setAccessibilityHelp(help)
        pauseButton.toolTip = help
    }

    /// The panel's size: as wide as the row needs, never wider than the contract, at
    /// exactly the contracted height.
    func badgeSize() -> CGSize {
        view.layoutSubtreeIfNeeded()
        let fitting = view.fittingSize
        return CGSize(
            width: min(fitting.width, Placement.maxBadgeWidth), height: Placement.badgeHeight)
    }

    static func word(for activity: TargetState.Activity) -> String {
        switch activity {
        case .working: return "Working"
        case .paused: return "Paused"
        case .unavailable: return "Unavailable"
        }
    }

    /// A static character, never a spinner: one glance says the state and nothing moves.
    static func glyph(for activity: TargetState.Activity) -> String {
        switch activity {
        case .working: return "●"
        case .paused: return "‖"
        case .unavailable: return "○"
        }
    }

    private static func glyphColor(for activity: TargetState.Activity) -> NSColor {
        switch activity {
        case .working: return .systemGreen
        case .paused: return .systemOrange
        case .unavailable: return .tertiaryLabelColor
        }
    }
}

/// The badge and the status item, kept in step with one state.
final class Indicator {
    private let sink: EventSink
    private let layout: () -> ScreenLayout

    let panel: BadgePanel
    let content: BadgeContent
    let statusItem: NSStatusItem
    let menu: NSMenu

    var pauseButton: FirstMouseButton { content.pauseButton }
    var stopButton: FirstMouseButton { content.stopButton }

    private let stateItem: NSMenuItem
    private let targetItem: NSMenuItem
    private let pauseItem: NSMenuItem
    private let stopItem: NSMenuItem
    private var screensObserver: NSObjectProtocol?

    /// The last thing the helper said. Every view is a function of this.
    private(set) var state = TargetState.initial

    /// - Parameter layout: the displays, injected so the self-test can place the badge
    ///   against a known arrangement instead of whatever is plugged in.
    init(sink: EventSink, layout: @escaping () -> ScreenLayout = { .current() }) {
        self.sink = sink
        self.layout = layout

        panel = BadgePanel(
            contentRect: CGRect(
                x: 0, y: 0, width: Placement.maxBadgeWidth, height: Placement.badgeHeight),
            styleMask: [.borderless, .nonactivatingPanel],
            backing: .buffered,
            defer: false)
        statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        menu = NSMenu(title: "Fermix")
        stateItem = NSMenuItem(title: "Fermix", action: nil, keyEquivalent: "")
        targetItem = NSMenuItem(title: "No target", action: nil, keyEquivalent: "")
        pauseItem = NSMenuItem(title: "Pause", action: nil, keyEquivalent: "")
        stopItem = NSMenuItem(title: "Stop", action: nil, keyEquivalent: "")
        content = BadgeContent(
            target: nil, pause: #selector(pausePressed), stop: #selector(stopPressed))

        buildPanel()
        buildStatusItem()
        // `self` was not available while the content was built.
        content.pauseButton.target = self
        content.stopButton.target = self
        render()
        // Both surfaces exist: the helper may start sending state, and may hold the
        // target's mutating requests until it has seen this line.
        sink.send(.ready)

        // A display added, removed or rescaled moves every window on it: re-place the
        // badge from the state we already have rather than waiting for the helper's
        // next line.
        screensObserver = NotificationCenter.default.addObserver(
            forName: NSApplication.didChangeScreenParametersNotification,
            object: nil, queue: .main
        ) { [weak self] _ in
            self?.place()
        }
    }

    deinit {
        if let observer = screensObserver {
            NotificationCenter.default.removeObserver(observer)
        }
    }

    // --- what the helper says ------------------------------------------------

    /// Render one state line. This is the only way the badge changes.
    func apply(_ state: TargetState) {
        self.state = state
        render()
    }

    /// Take everything off the screen. Called when stdin closes, on SIGTERM, and at the
    /// end of the self-test: a badge outliving the helper would claim Fermix owns a
    /// window nothing is acting on.
    func removeEverything() {
        panel.orderOut(nil)
        NSStatusBar.system.removeStatusItem(statusItem)
    }

    // --- the two commands ----------------------------------------------------

    @objc func pausePressed() {
        // What the helper last said decides which half of the toggle this is. Nothing
        // on screen changes here: `apply` does that, when the helper confirms.
        sink.send(state.activity == .paused ? .resume : .pause)
    }

    @objc func stopPressed() {
        sink.send(.stop)
    }

    // --- building ------------------------------------------------------------

    private func buildPanel() {
        panel.isFloatingPanel = true
        panel.level = .floating
        panel.hidesOnDeactivate = false
        panel.becomesKeyOnlyIfNeeded = true
        panel.isReleasedWhenClosed = false
        panel.isRestorable = false
        panel.isMovableByWindowBackground = false
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.hasShadow = true
        // No fade in, no fade out: the badge appears and disappears, and a person
        // watching their own window is not shown a small animation every time it moves.
        panel.animationBehavior = .none
        // Out of every capture — including the helper's own window stream, so the
        // badge can never end up in a screenshot the model then reasons about.
        panel.sharingType = .none
        // Every Space, above a full-screen window, out of Mission Control, out of the
        // window cycle: it belongs to the target, not to the window list.
        panel.collectionBehavior = [
            .canJoinAllSpaces, .fullScreenAuxiliary, .transient, .ignoresCycle,
        ]
        panel.setAccessibilityTitle("Fermix computer use")
        panel.contentView = content.view
    }

    private func buildStatusItem() {
        // The one surface that is always there: when the badge is hidden — occluded,
        // unplaceable, or the target is gone — this is how the person still sees that
        // Fermix holds a window, and still stops it.
        for item in [stateItem, targetItem] {
            item.isEnabled = false
        }
        pauseItem.target = self
        pauseItem.action = #selector(pausePressed)
        stopItem.target = self
        stopItem.action = #selector(stopPressed)

        menu.autoenablesItems = false
        menu.addItem(stateItem)
        menu.addItem(targetItem)
        menu.addItem(.separator())
        menu.addItem(pauseItem)
        menu.addItem(stopItem)
        statusItem.menu = menu
        statusItem.button?.font = .systemFont(ofSize: 12)
    }

    // --- rendering -----------------------------------------------------------

    private func render() {
        let label = targetLabel(app: state.app, title: state.title)
        let word = BadgeContent.word(for: state.activity)

        content.render(state.activity, label: label)

        stateItem.title = "Fermix — \(word)"
        targetItem.title = label
        pauseItem.title = state.activity == .paused ? "Resume" : "Pause"
        stopItem.title = "Stop"
        statusItem.button?.title = BadgeContent.glyph(for: state.activity)
        statusItem.button?.setAccessibilityLabel("Fermix computer use, \(word.lowercased())")

        place()
    }

    /// Size the panel to its content, then put it where the target is — or take it off
    /// the screen when there is nowhere it may go.
    private func place() {
        panel.setContentSize(content.badgeSize())

        guard let frame = Placement.badgeFrame(for: state, size: panel.frame.size, layout: layout())
        else {
            panel.orderOut(nil)
            return
        }
        panel.setFrame(frame, display: true)
        // `orderFrontRegardless` shows the panel without activating this process: an
        // ordinary `orderFront` from an inactive application does nothing.
        panel.orderFrontRegardless()
    }
}

extension ScreenLayout {
    /// The displays as AppKit sees them right now.
    ///
    /// `NSScreen.screens[0]` is the display whose origin is (0, 0) — the one the window
    /// server measures its top-left points down from. With no displays at all there is
    /// no conversion and no placement: the badge stays hidden and the status item is
    /// the whole surface.
    static func current() -> ScreenLayout {
        let screens = NSScreen.screens
        return ScreenLayout(
            screens: screens.map { ScreenBox(frame: $0.frame, visibleFrame: $0.visibleFrame) },
            primaryHeight: screens.first?.frame.height ?? 0)
    }
}
