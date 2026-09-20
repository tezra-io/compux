// What the indicator can prove about itself, in two halves.
//
//   --self-test-headless   the logic that needs no window: the wire, the placement
//                          maths against a SYNTHETIC list of displays, the bounding of
//                          an untrusted label, and the four event lines. This is what
//                          CI runs, because a hosted runner's GUI session is not a
//                          thing to depend on.
//
//   --self-test            all of the above, and then the half that needs a screen:
//                          the panel really appears, it is NOT key, NOT main, at the
//                          floating level, excluded from capture, and — the reason any
//                          of this is a separate process at all — the front application
//                          is the same before and after it was shown.
//
// The windowed half puts a small panel on the screen for about a second. That is the
// point of it: no assertion about a window's style mask proves that showing it did not
// steal a keystroke, and this is the cheapest thing that comes close.

import AppKit

/// A list of what went wrong, so one run reports every failure instead of the first.
struct Checks {
    private(set) var failures: [String] = []

    mutating func expect(_ passed: Bool, _ message: @autoclosure () -> String) {
        guard !passed else { return }
        failures.append(message())
    }

    mutating func equal<T: Equatable>(_ actual: T, _ expected: T, _ what: String) {
        guard actual != expected else { return }
        failures.append("\(what): got \(actual), expected \(expected)")
    }

    mutating func absorb(_ other: Checks) {
        failures.append(contentsOf: other.failures)
    }
}

enum SelfTest {
    // --- the synthetic world -------------------------------------------------
    //
    // A 1512x982 primary with a menu bar, and a second display ABOVE AND LEFT of it, so
    // every rule is proved against negative coordinates in both spaces rather than
    // against whatever happens to be plugged into the build machine.

    private static let layout = ScreenLayout(
        screens: [
            ScreenBox(
                frame: CGRect(x: 0, y: 0, width: 1512, height: 982),
                visibleFrame: CGRect(x: 0, y: 0, width: 1512, height: 945)),
            ScreenBox(
                frame: CGRect(x: -1920, y: 982, width: 1920, height: 1080),
                visibleFrame: CGRect(x: -1920, y: 982, width: 1920, height: 1080)),
        ],
        primaryHeight: 982)

    /// The widest, and the only height, the badge may ever be. Every placement below is
    /// computed at the worst case, because that is the one the contract has to hold for.
    private static let badge = CGSize(
        width: Placement.maxBadgeWidth, height: Placement.badgeHeight)

    private static func working(_ bounds: TargetState.Bounds?, occluded: Bool = false)
        -> TargetState
    {
        TargetState(
            activity: .working, app: "Safari", title: "Inbox", bounds: bounds, occluded: occluded)
    }

    // --- the two entry points ------------------------------------------------

    static func headless() -> Int32 {
        var checks = Checks()
        checkWire(&checks)
        checkPlacement(&checks)
        checkContract(&checks)
        checkLabel(&checks)
        checkEvents(&checks)
        return report(
            checks, ok: "ok: wire, placement, size contract, label and event lines (no window)")
    }

    static func windowed() -> Int32 {
        var checks = Checks()
        checkWire(&checks)
        checkPlacement(&checks)
        checkContract(&checks)
        checkLabel(&checks)
        checkEvents(&checks)
        checkOnScreen(&checks)
        return report(
            checks, ok: "ok: the panel showed, took no focus, and reported every press")
    }

    // --- the wire ------------------------------------------------------------

    private static func checkWire(_ checks: inout Checks) {
        let full = """
            {"state":"paused","app":"Safari","title":"Inbox",\
            "bounds":{"x":10,"y":20,"w":300,"h":200},"occluded":true,"future":"ignored"}
            """
        do {
            let state = try TargetState.decode(line: full)
            checks.equal(state.activity, .paused, "state")
            checks.equal(state.app, "Safari", "app")
            checks.equal(state.title, "Inbox", "title")
            checks.equal(state.bounds, TargetState.Bounds(x: 10, y: 20, w: 300, h: 200), "bounds")
            checks.equal(state.occluded, true, "occluded")
        } catch {
            checks.expect(false, "a full state line was refused: \(error)")
        }

        // The defaults: no bounds at all, and an absent `occluded` is false.
        do {
            let state = try TargetState.decode(line: #"{"state":"working"}"#)
            checks.expect(state.bounds == nil, "an absent \"bounds\" became a rectangle")
            checks.equal(state.occluded, false, "occluded default")
            checks.equal(state.app, "", "app default")
        } catch {
            checks.expect(false, "a minimal state line was refused: \(error)")
        }

        do {
            let state = try TargetState.decode(
                line: #"{"state":"working","bounds":null,"app":null}"#)
            checks.expect(state.bounds == nil, "\"bounds\":null became a rectangle")
            checks.equal(state.app, "", "\"app\":null")
        } catch {
            checks.expect(false, "a null-bounds line was refused: \(error)")
        }

        // Ignored and counted, never fatal, and never coerced into something renderable.
        let refused: [(String, String)] = [
            (#"{"state":"thinking"}"#, "an unknown state"),
            ("not json at all", "a line that is not JSON"),
            ("[1,2,3]", "a JSON array"),
            (#"{"app":"Safari"}"#, "a line with no state"),
            (#"{"state":"working","occluded":"yes"}"#, "a string occluded"),
            (#"{"state":"working","app":7}"#, "a numeric app"),
            (#"{"state":"working","bounds":{"x":1,"y":2,"w":0,"h":4}}"#, "a zero-width window"),
            (#"{"state":"working","bounds":{"x":true,"y":2,"w":3,"h":4}}"#, "a boolean coordinate"),
            (#"{"state":"working","bounds":{"y":2,"w":3,"h":4}}"#, "bounds with no x"),
        ]
        for (line, what) in refused {
            let decoded = try? TargetState.decode(line: line)
            checks.expect(decoded == nil, "\(what) was accepted")
        }

        let long = #"{"state":"working","title":""# + String(repeating: "a", count: 70 * 1024) + "\"}"
        checks.expect((try? TargetState.decode(line: long)) == nil, "a 70 KiB line was accepted")
    }

    // --- placement -----------------------------------------------------------

    private static func checkPlacement(_ checks: inout Checks) {
        // Room above the title bar: the badge sits outside the window, left-aligned.
        checks.equal(
            Placement.badgeFrame(
                for: working(.init(x: 300, y: 200, w: 600, h: 400)), size: badge, layout: layout),
            CGRect(x: 300, y: 788, width: 440, height: 28),
            "above the window")

        // A window whose top edge leaves no room: inside, below the traffic lights.
        checks.equal(
            Placement.badgeFrame(
                for: working(.init(x: 300, y: 60, w: 600, h: 850)), size: badge, layout: layout),
            CGRect(x: 308, y: 866, width: 440, height: 28),
            "inside below the title bar")

        // A window running under the menu bar: still inside, and clamped out of it —
        // downwards, which the contract has room for.
        checks.equal(
            Placement.badgeFrame(
                for: working(.init(x: 300, y: 0, w: 600, h: 900)), size: badge, layout: layout),
            CGRect(x: 308, y: 917, width: 440, height: 28),
            "clamped below the menu bar")

        // A window near the right edge, inside placement: the badge slides back to the
        // window's own left edge rather than past the contract's right edge.
        checks.equal(
            Placement.badgeFrame(
                for: working(.init(x: 1070, y: 40, w: 400, h: 860)), size: badge, layout: layout),
            CGRect(x: 1072, y: 886, width: 440, height: 28),
            "slid back from the right edge")

        // The display ABOVE AND LEFT of the primary one: negative coordinates in both
        // spaces, and the badge goes on that screen rather than on the primary.
        let far = working(.init(x: -1700, y: -900, w: 800, h: 500))
        checks.equal(
            Placement.badgeFrame(for: far, size: badge, layout: layout),
            CGRect(x: -1700, y: 1888, width: 440, height: 28),
            "on the display above and left")
        checks.equal(
            Placement.screen(
                showing: Placement.appKitRect(far.bounds!, primaryHeight: layout.primaryHeight),
                in: layout),
            layout.screens[1],
            "the display showing the target")

        // Hidden, and each for its own reason.
        let hidden: [(TargetState, String)] = [
            (working(.init(x: 300, y: 200, w: 600, h: 400), occluded: true), "occluded"),
            (working(nil), "no bounds"),
            (
                TargetState(
                    activity: .unavailable, app: "Safari", title: "Inbox",
                    bounds: .init(x: 300, y: 200, w: 600, h: 400), occluded: false),
                "unavailable"
            ),
            (working(.init(x: 9000, y: 9000, w: 100, h: 100)), "on no display"),
            // The window's left edge is 112 pt from the right of the display, so the
            // part of the contract's rectangle that is on screen cannot hold the badge.
            // Hiding is the only answer: drawing it anywhere else puts it where the
            // helper is not looking for something covering it.
            (working(.init(x: 1400, y: 300, w: 500, h: 300)), "no room inside the contract"),
        ]
        for (state, why) in hidden {
            checks.expect(
                Placement.badgeFrame(for: state, size: badge, layout: layout) == nil,
                "\(why): the badge was placed anyway")
        }
    }

    // --- the size contract with the helper -----------------------------------

    /// The badge lies inside the rectangle the helper hit-tests for occlusion.
    ///
    /// The helper's numbers are written out again here rather than read from
    /// `Placement`: a test that restates the other side of a contract is the only kind
    /// that can fail when this side drifts away from it.
    private static func checkContract(_ checks: inout Checks) {
        // native/compux/src/indicator.rs: BADGE_ABOVE, BADGE_INSIDE_DROP + BADGE_HEIGHT.
        let above: CGFloat = 34
        let below: CGFloat = 52 + 28

        checks.equal(Placement.gap + Placement.badgeHeight, above, "how far above the top edge")
        checks.expect(
            Placement.titleBarHeight + Placement.badgeHeight <= below,
            "the inside placement reaches \(Placement.titleBarHeight + Placement.badgeHeight) pt "
                + "below the top edge, past the \(below) the helper allows")
        // The one number the helper cannot derive: it must carry `inset + maxBadgeWidth`
        // as its BADGE_WIDTH, so a change here is a change there.
        checks.equal(Placement.inset + Placement.maxBadgeWidth, 448, "the contract's width")

        // Every window that produces a badge produces one inside the rectangle — the
        // two placements, both clamps, and the display with negative origins.
        let windows: [(TargetState.Bounds, String)] = [
            (.init(x: 300, y: 200, w: 600, h: 400), "room above"),
            (.init(x: 300, y: 60, w: 600, h: 850), "no room above"),
            (.init(x: 300, y: 0, w: 600, h: 900), "under the menu bar"),
            (.init(x: 1070, y: 40, w: 400, h: 860), "near the right edge"),
            (.init(x: 0, y: 200, w: 400, h: 400), "at the left edge"),
            (.init(x: -1700, y: -900, w: 800, h: 500), "above and left"),
        ]
        for (bounds, what) in windows {
            guard let frame = Placement.badgeFrame(for: working(bounds), size: badge, layout: layout)
            else {
                checks.expect(false, "\(what): no badge at all")
                continue
            }
            // Back into the window server's top-left space, which is the space the
            // helper's rectangle is expressed in.
            let drawn = CGRect(
                x: frame.minX, y: layout.primaryHeight - frame.maxY,
                width: frame.width, height: frame.height)
            let area = CGRect(
                x: bounds.x, y: bounds.y - above,
                width: Placement.inset + Placement.maxBadgeWidth, height: above + below)
            checks.expect(
                area.contains(drawn),
                "\(what): the badge at \(drawn) leaves the helper's area \(area)")
        }

        // The label is bounded in characters before it is bounded in points, so the row
        // cannot be widened without limit in the first place. That the POINTS stay
        // inside `maxBadgeWidth` is a layout fact, asserted in the windowed half.
        let longest = targetLabel(
            app: String(repeating: "W", count: 400), title: String(repeating: "W", count: 400))
        checks.expect(longest.count <= 75, "the longest label is \(longest.count) characters")
    }

    // --- the untrusted label -------------------------------------------------

    private static func checkLabel(_ checks: inout Checks) {
        checks.equal(targetLabel(app: "Safari", title: "Inbox"), "Safari — Inbox", "plain label")
        checks.equal(targetLabel(app: "", title: ""), "No target", "empty label")
        checks.equal(targetLabel(app: "Safari", title: ""), "Safari", "no title")

        let hostile = targetLabel(app: "Evil\nApp", title: "one\ntwo\u{202E}three\u{0007}")
        checks.expect(!hostile.contains("\n"), "a multi-line title stayed multi-line")
        checks.expect(
            !hostile.unicodeScalars.contains { $0.properties.generalCategory == .format },
            "a bidi override survived into the label")
        checks.equal(hostile, "Evil App — one two three", "a hostile label")

        let long = targetLabel(app: String(repeating: "a", count: 200), title: String(repeating: "b", count: 200))
        checks.expect(long.count <= 28 + 44 + 3, "a 400-character label was not cut: \(long.count)")
        checks.expect(long.contains("…"), "a cut label does not say it was cut")
    }

    // --- the four lines ------------------------------------------------------

    private static func checkEvents(_ checks: inout Checks) {
        checks.equal(IndicatorEvent.ready.line, "{\"event\":\"ready\"}\n", "ready line")
        checks.equal(IndicatorEvent.pause.line, "{\"event\":\"pause\"}\n", "pause line")
        checks.equal(IndicatorEvent.resume.line, "{\"event\":\"resume\"}\n", "resume line")
        checks.equal(IndicatorEvent.stop.line, "{\"event\":\"stop\"}\n", "stop line")

        let sink = RecordingSink()
        sink.send(.pause)
        sink.send(.stop)
        checks.equal(sink.takeLines(), ["{\"event\":\"pause\"}\n", "{\"event\":\"stop\"}\n"], "order")
        checks.equal(sink.takeLines(), [], "lines were taken twice")
    }

    // --- the half that needs a screen ----------------------------------------

    private static func checkOnScreen(_ checks: inout Checks) {
        let application = NSApplication.shared
        // An accessory: no Dock tile, no menu bar of its own, and nothing that could
        // make this process the active one. It is never activated, here or anywhere.
        application.setActivationPolicy(.accessory)
        application.finishLaunching()

        let front = NSWorkspace.shared.frontmostApplication?.processIdentifier
        let sink = RecordingSink()
        let indicator = Indicator(sink: sink)

        checks.equal(sink.takeLines(), [IndicatorEvent.ready.line], "ready on construction")
        checks.expect(indicator.statusItem.button != nil, "the status item has no button")

        // A target near the top-left of the primary display, so the badge lands
        // somewhere visible whatever is plugged in.
        let bounds = TargetState.Bounds(x: 200, y: 200, w: 700, h: 420)
        let shown = TargetState(
            activity: .working, app: "Compux", title: "Self test", bounds: bounds, occluded: false)
        indicator.apply(shown)

        let expected = Placement.badgeFrame(
            for: shown, size: indicator.panel.frame.size, layout: .current())
        checks.expect(expected != nil, "nothing on this machine could place the badge")

        // About a second on the screen. Everything below is asserted after it, because
        // the question is what a second of being visible did to the front application.
        RunLoop.current.run(until: Date().addingTimeInterval(1.0))

        checks.expect(indicator.panel.isVisible, "the panel is not on the screen")
        checks.expect(!indicator.panel.isKeyWindow, "the panel became key")
        checks.expect(!indicator.panel.isMainWindow, "the panel became main")
        checks.expect(!indicator.panel.canBecomeKey, "the panel can become key")
        checks.expect(!indicator.panel.canBecomeMain, "the panel can become main")
        checks.equal(indicator.panel.level, .floating, "window level")
        checks.equal(indicator.panel.sharingType, .none, "sharing type")
        checks.expect(
            indicator.panel.collectionBehavior.contains(.transient),
            "the panel is in Mission Control")
        checks.expect(
            indicator.panel.collectionBehavior.contains(.ignoresCycle),
            "the panel is in the window cycle")
        checks.expect(
            indicator.panel.collectionBehavior.contains(.canJoinAllSpaces),
            "the panel is on one Space only")
        checks.expect(!application.isActive, "this process activated itself")
        checks.equal(
            NSWorkspace.shared.frontmostApplication?.processIdentifier, front,
            "the front application changed while the badge was shown")
        if let expected = expected {
            checks.equal(indicator.panel.frame, expected, "the badge's frame")
        }

        checkStateLines(&checks, indicator: indicator, bounds: bounds)
        checkPresses(&checks, indicator: indicator, sink: sink, bounds: bounds)

        indicator.removeEverything()
        checks.expect(!indicator.panel.isVisible, "the panel survived removeEverything()")
    }

    /// The state lines the helper sends, and what each one must do to the badge.
    private static func checkStateLines(
        _ checks: inout Checks, indicator: Indicator, bounds: TargetState.Bounds
    ) {
        indicator.apply(
            TargetState(
                activity: .working, app: "Compux", title: "Self test", bounds: bounds,
                occluded: true))
        checks.expect(!indicator.panel.isVisible, "an occluded target kept its badge")

        indicator.apply(
            TargetState(
                activity: .working, app: "Compux", title: "Self test", bounds: nil,
                occluded: false))
        checks.expect(!indicator.panel.isVisible, "a target with no bounds kept its badge")

        indicator.apply(
            TargetState(
                activity: .unavailable, app: "Compux", title: "Self test", bounds: bounds,
                occluded: false))
        checks.expect(!indicator.panel.isVisible, "an unavailable target kept its badge")
        checks.equal(
            indicator.statusItem.menu?.item(at: 0)?.title, "Fermix — Unavailable",
            "the status menu's state")

        indicator.apply(
            TargetState(
                activity: .paused, app: "Compux", title: "Self test", bounds: bounds,
                occluded: false))
        checks.expect(indicator.panel.isVisible, "a paused target lost its badge")
        checks.equal(indicator.pauseButton.title, "Resume", "the paused button")
        checks.equal(indicator.statusItem.menu?.item(at: 3)?.title, "Resume", "the paused menu")

        // A window title from another application, spelled to break a badge.
        indicator.apply(
            TargetState(
                activity: .working, app: "Evil\nApp", title: "one\ntwo\u{202E}three",
                bounds: bounds, occluded: false))
        let label = indicator.statusItem.menu?.item(at: 1)?.title ?? ""
        checks.equal(label, "Evil App — one two three", "a hostile title")
        checks.equal(indicator.pauseButton.title, "Pause", "the working button")

        // The size contract, through the real layout engine: a title from another
        // application costs characters, never points, and never a button.
        indicator.apply(
            TargetState(
                activity: .unavailable, app: String(repeating: "W", count: 400),
                title: String(repeating: "W", count: 400), bounds: bounds, occluded: false))
        indicator.apply(
            TargetState(
                activity: .working, app: String(repeating: "W", count: 400),
                title: String(repeating: "W", count: 400), bounds: bounds, occluded: false))
        checks.expect(
            indicator.panel.frame.width <= Placement.maxBadgeWidth,
            "a 400-character title widened the badge to \(indicator.panel.frame.width)")
        checks.equal(indicator.panel.frame.height, Placement.badgeHeight, "the badge's height")
        checks.expect(
            indicator.stopButton.frame.width >= 40,
            "a long title squeezed the Stop button to \(indicator.stopButton.frame.width)")
    }

    /// Both surfaces, each writing exactly its own line and changing nothing on screen.
    private static func checkPresses(
        _ checks: inout Checks, indicator: Indicator, sink: RecordingSink,
        bounds: TargetState.Bounds
    ) {
        _ = sink.takeLines()

        indicator.pauseButton.performClick(nil)
        checks.equal(sink.takeLines(), [IndicatorEvent.pause.line], "the Pause button")
        // The badge must NOT have guessed: it still says Pause until the helper says
        // otherwise, which is the whole reason the state is the helper's to own.
        checks.equal(indicator.pauseButton.title, "Pause", "the button guessed a state")

        indicator.apply(
            TargetState(
                activity: .paused, app: "Compux", title: "Self test", bounds: bounds,
                occluded: false))
        indicator.pauseButton.performClick(nil)
        checks.equal(sink.takeLines(), [IndicatorEvent.resume.line], "the Resume button")

        indicator.stopButton.performClick(nil)
        checks.equal(sink.takeLines(), [IndicatorEvent.stop.line], "the Stop button")

        // The menu bar is the same three commands, through the same selectors.
        if let item = indicator.menu.item(at: 4), let action = item.action {
            _ = NSApp.sendAction(action, to: item.target, from: item)
            checks.equal(sink.takeLines(), [IndicatorEvent.stop.line], "the Stop menu item")
        } else {
            checks.expect(false, "the status menu has no Stop item")
        }
    }

    // --- reporting -----------------------------------------------------------

    private static func report(_ checks: Checks, ok: String) -> Int32 {
        guard checks.failures.isEmpty else {
            for failure in checks.failures {
                FileHandle.standardError.write(Data("FAIL: \(failure)\n".utf8))
            }
            return 1
        }
        print(ok)
        return 0
    }
}
