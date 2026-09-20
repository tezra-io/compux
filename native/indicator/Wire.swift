// The pipe the helper speaks to the indicator, and nothing else.
//
// One newline-delimited JSON object per line in each direction. The helper writes
// STATE, the indicator writes EVENTS, and neither side ever writes anything else on
// those two file descriptors: stdout carries the four event lines and diagnostics go
// to stderr, so a helper parsing our stdout never has to guess what a line is.
//
//   in   {"state":"working"|"paused"|"unavailable","app":"Safari","title":"Inbox",
//         "bounds":{"x":..,"y":..,"w":..,"h":..}|null,"occluded":true|false}
//   out  {"event":"ready"|"pause"|"resume"|"stop"}
//
// `bounds` are GLOBAL POINTS with a TOP-LEFT origin — the window server's space, which
// is what `CGWindowListCopyWindowInfo` hands the helper. AppKit's screen space has a
// bottom-left origin, so the conversion is explicit and lives in `Placement`.
//
// Nothing here imports AppKit: the whole file is pure, so `--self-test-headless` can
// prove it on a machine with no window server.

import CoreGraphics
import Foundation

// --- what the helper says ----------------------------------------------------

/// The one thing the indicator renders: what the helper last said, never what the
/// indicator guessed. A button press writes an event and changes nothing on screen;
/// the next state line does that.
struct TargetState: Equatable {
    /// The three states the badge can be in. An unknown fourth is a line from a newer
    /// helper: it is ignored and counted, never rendered as a guess.
    enum Activity: String {
        case working
        case paused
        case unavailable
    }

    /// The target window's rectangle in the window server's space (top-left origin).
    struct Bounds: Equatable {
        var x: CGFloat
        var y: CGFloat
        var w: CGFloat
        var h: CGFloat
    }

    var activity: Activity
    /// The application's name and the window's title, as ANOTHER application chose to
    /// spell them. Untrusted text: bounded and flattened by `targetLabel` before it
    /// reaches a view, and never interpreted as anything but characters.
    var app: String
    var title: String
    /// `nil` when the helper cannot place the badge at all, which hides it.
    var bounds: Bounds?
    var occluded: Bool

    /// What the indicator shows before the helper has said anything.
    static let initial = TargetState(
        activity: .working, app: "", title: "", bounds: nil, occluded: false)
}

/// Why a line was ignored. Every case is counted and reported, and none is fatal: a
/// helper that writes one bad line must not lose its indicator.
enum WireError: Error, CustomStringConvertible {
    case tooLong(Int)
    case notJSON
    case notAnObject
    case missingState
    case unknownState(String)
    case badValue(String)

    var description: String {
        switch self {
        case .tooLong(let count): return "line is \(count) bytes"
        case .notJSON: return "not valid JSON"
        case .notAnObject: return "not a JSON object"
        case .missingState: return "no \"state\""
        case .unknownState(let raw): return "unknown state \"\(raw)\""
        case .badValue(let key): return "\"\(key)\" has the wrong type"
        }
    }
}

extension TargetState {
    /// The longest line worth parsing. A window title is a few hundred characters; a
    /// megabyte of it is a bug or an attack, and either way it is not a state line.
    static let lineLimit = 64 * 1024

    /// Parse one line. Unknown keys are ignored (a newer helper may add some); a known
    /// key with the wrong type is not, because silently coercing one is how a badge
    /// ends up over the wrong window.
    static func decode(line: String) throws -> TargetState {
        guard line.utf8.count <= lineLimit else { throw WireError.tooLong(line.utf8.count) }
        guard let data = line.data(using: .utf8),
            let parsed = try? JSONSerialization.jsonObject(with: data)
        else { throw WireError.notJSON }
        guard let object = parsed as? [String: Any] else { throw WireError.notAnObject }

        guard let rawState = object["state"] else { throw WireError.missingState }
        guard let stateText = rawState as? String else { throw WireError.badValue("state") }
        guard let activity = Activity(rawValue: stateText) else {
            throw WireError.unknownState(stateText)
        }

        return TargetState(
            activity: activity,
            app: try text(object["app"], key: "app"),
            title: try text(object["title"], key: "title"),
            bounds: try bounds(object["bounds"]),
            occluded: try flag(object["occluded"], key: "occluded"))
    }

    private static func text(_ value: Any?, key: String) throws -> String {
        guard let value = value, !(value is NSNull) else { return "" }
        guard let string = value as? String else { throw WireError.badValue(key) }
        return string
    }

    private static func flag(_ value: Any?, key: String) throws -> Bool {
        guard let value = value, !(value is NSNull) else { return false }
        guard let number = value as? NSNumber, CFGetTypeID(number) == CFBooleanGetTypeID() else {
            throw WireError.badValue(key)
        }
        return number.boolValue
    }

    private static func bounds(_ value: Any?) throws -> Bounds? {
        guard let value = value, !(value is NSNull) else { return nil }
        guard let object = value as? [String: Any] else { throw WireError.badValue("bounds") }
        let x = try coordinate(object["x"], key: "bounds.x")
        let y = try coordinate(object["y"], key: "bounds.y")
        let w = try coordinate(object["w"], key: "bounds.w")
        let h = try coordinate(object["h"], key: "bounds.h")
        // A window with no area is not a window. Refusing it here keeps every later
        // rectangle finite and non-degenerate.
        guard w > 0, h > 0 else { throw WireError.badValue("bounds") }
        return Bounds(x: x, y: y, w: w, h: h)
    }

    /// A JSON number that is not a boolean. `true as? NSNumber` succeeds on Darwin, so
    /// the CoreFoundation type is what tells the two apart.
    private static func coordinate(_ value: Any?, key: String) throws -> CGFloat {
        guard let number = value as? NSNumber, CFGetTypeID(number) != CFBooleanGetTypeID() else {
            throw WireError.badValue(key)
        }
        let double = number.doubleValue
        guard double.isFinite else { throw WireError.badValue(key) }
        return CGFloat(double)
    }
}

// --- what the indicator says -------------------------------------------------

/// The four lines this process can write. There are no others, and none of them
/// carries a value, which is why they are spelled out rather than encoded: no
/// untrusted text ever reaches stdout.
enum IndicatorEvent: String {
    case ready
    case pause
    case resume
    case stop

    var line: String { "{\"event\":\"\(rawValue)\"}\n" }
}

/// Where an event goes. The one implementation that matters writes stdout; the
/// self-test records instead, so both are driven by the same button actions and the
/// same `line` above.
protocol EventSink: AnyObject {
    func send(_ event: IndicatorEvent)
}

/// stdout, one line at a time, flushed by construction: `write(2)` is unbuffered, so
/// the helper sees a press the moment it happens rather than when a buffer fills.
final class StandardOutputSink: EventSink {
    /// Called when stdout is gone, which means the helper is gone.
    private let helperGone: () -> Void

    init(helperGone: @escaping () -> Void) {
        self.helperGone = helperGone
    }

    func send(_ event: IndicatorEvent) {
        write(Array(event.line.utf8))
    }

    private func write(_ bytes: [UInt8]) {
        var offset = 0
        var interruptions = 0
        while offset < bytes.count {
            let written = bytes.withUnsafeBufferPointer { buffer -> Int in
                Darwin.write(STDOUT_FILENO, buffer.baseAddress! + offset, bytes.count - offset)
            }
            if written > 0 {
                offset += written
                continue
            }
            // Bounded: a signal can interrupt a write, a storm of them is a fault.
            if errno == EINTR, interruptions < 64 {
                interruptions += 1
                continue
            }
            helperGone()
            return
        }
    }
}

/// The self-test's sink: the same event lines, kept instead of written.
final class RecordingSink: EventSink {
    private(set) var lines: [String] = []

    func send(_ event: IndicatorEvent) {
        lines.append(event.line)
    }

    func takeLines() -> [String] {
        defer { lines = [] }
        return lines
    }
}

// --- the target's label ------------------------------------------------------

/// The badge's one piece of untrusted text, made safe to render.
///
/// `app` and `title` are chosen by another application. A window can be titled with
/// newlines, control characters, a right-to-left override that makes the rest of the
/// badge read backwards, or a megabyte of anything. None of that may change what the
/// badge looks like, so every character that is not a printable one becomes a space,
/// runs of spaces collapse, and each half is cut to a length that fits.
func targetLabel(app: String, title: String, appLimit: Int = 28, titleLimit: Int = 44) -> String {
    let application = cut(flatten(app), to: appLimit)
    let window = cut(flatten(title), to: titleLimit)
    switch (application.isEmpty, window.isEmpty) {
    case (true, true): return "No target"
    case (false, true): return application
    case (true, false): return window
    case (false, false): return "\(application) — \(window)"
    }
}

/// One line of printable characters, whatever arrived.
private func flatten(_ raw: String) -> String {
    var out = String.UnicodeScalarView()
    for scalar in raw.unicodeScalars {
        switch scalar.properties.generalCategory {
        // Control (newlines, tabs), format (the bidi overrides), and the explicit
        // line/paragraph separators: all of them become a space rather than being
        // dropped, so two words never run together.
        case .control, .format, .lineSeparator, .paragraphSeparator, .surrogate,
            .privateUse, .unassigned:
            out.append(" ")
        default:
            out.append(scalar)
        }
    }
    return String(out).split(separator: " ").joined(separator: " ")
}

/// Cut to a character count, marking that something was cut. The view truncates to
/// the pixel as well; this is the bound that keeps a hostile title from costing
/// anything to lay out in the first place.
private func cut(_ text: String, to limit: Int) -> String {
    guard text.count > limit else { return text }
    return String(text.prefix(max(limit - 1, 1))) + "…"
}
