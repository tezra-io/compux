// The ownership indicator: a second, tiny process inside the same signed bundle as the
// compux helper, which shows on screen that Fermix is working in one window and offers
// Pause, Resume and Stop.
//
// Why a separate process, and not a thread of the helper: AppKit needs the main thread's
// run loop, and the helper's main thread is the input worker — the one proven path for
// real keyboard and mouse events, some of whose keyboard-layout calls are only safe
// there. A child gets its own main thread, cannot stall or crash that worker, ships in
// the SAME bundle and signature (so System Settings ▸ Privacy still shows one "Fermix",
// and a panel needs no grant of its own), and its buttons reach the same local gate:
// this stdout, the helper's control thread, the gate. No daemon is in that path, so
// Stop works while the daemon is suspended.
//
//   compux-indicator                       read state on stdin, write events on stdout
//   compux-indicator --self-test           the above, plus a panel on the screen (§6)
//   compux-indicator --self-test-headless  only what needs no window (what CI runs)
//
// It holds no secrets, reads nothing from the screen, opens no file and talks to
// nothing but its parent: it renders state and reports three button presses.
//
// Threading, which is the whole of its concurrency story: everything touching AppKit
// runs on the main thread. One background thread reads stdin and hops each line to the
// main queue; there is no other thread and no shared mutable state.

import AppKit

@main
enum CompuxIndicator {
    /// The delegate outlives `main` — `NSApplication` holds its delegate weakly.
    private static var runner: Runner?

    static func main() {
        var arguments = CommandLine.arguments.dropFirst()
        var windowed = false
        var headless = false

        while let argument = arguments.first {
            arguments = arguments.dropFirst()
            switch argument {
            case "--self-test":
                windowed = true
            case "--self-test-headless":
                headless = true
            case "--help", "-h":
                print("usage: compux-indicator [--self-test | --self-test-headless]")
                exit(0)
            default:
                fail("unknown argument: \(argument)")
            }
        }

        if headless { exit(SelfTest.headless()) }
        if windowed { exit(SelfTest.windowed()) }

        let application = NSApplication.shared
        // An accessory: no Dock tile, no menu of its own, and nothing that can make
        // this process the active application. It is never activated.
        application.setActivationPolicy(.accessory)
        let runner = Runner()
        Self.runner = runner
        application.delegate = runner
        application.run()
    }

    static func fail(_ message: String) -> Never {
        FileHandle.standardError.write(Data("compux-indicator: \(message)\n".utf8))
        exit(2)
    }
}

/// The running indicator: the pipe on one side, the badge on the other.
final class Runner: NSObject, NSApplicationDelegate {
    private var indicator: Indicator?
    private var sink: StandardOutputSink?
    private var termination: DispatchSourceSignal?
    /// Lines that could not be read. Counted rather than fatal: a helper that writes one
    /// bad line must not lose its indicator, and a helper newer than this binary may
    /// write a state this one has never heard of.
    private var ignored = 0

    func applicationDidFinishLaunching(_ notification: Notification) {
        // A write to a stdout nobody is reading must come back as an error rather than
        // as a signal that kills this process before it can take the badge off screen.
        signal(SIGPIPE, SIG_IGN)

        let sink = StandardOutputSink(helperGone: { [weak self] in self?.finish() })
        self.sink = sink
        indicator = Indicator(sink: sink)
        installTermination()
        readStandardInput()
    }

    // --- the pipe ------------------------------------------------------------

    /// One thread, blocking on stdin, handing every line to the main queue. This hop is
    /// the only place another thread reaches the badge.
    private func readStandardInput() {
        let thread = Thread { [weak self] in
            while let line = readLine(strippingNewline: true) {
                DispatchQueue.main.async { self?.accept(line) }
            }
            // End of file: the helper closed the pipe or died. Either way there is
            // nothing left to show and nobody left to tell.
            DispatchQueue.main.async { self?.finish() }
        }
        thread.name = "compux-indicator stdin"
        thread.start()
    }

    private func accept(_ line: String) {
        // A blank line is the pipe's punctuation, not a state line.
        guard !line.trimmingCharacters(in: .whitespaces).isEmpty else { return }
        do {
            indicator?.apply(try TargetState.decode(line: line))
        } catch {
            ignored += 1
            // The line itself is never echoed: it carries another application's text,
            // and the hostile one is exactly the line that gets ignored. The reason
            // separates a helper bug from a helper newer than this binary.
            if ignored <= 5 { note("ignored a state line: \(error)") }
            if ignored == 5 { note("further ignored lines are counted, not logged") }
        }
    }

    // --- leaving -------------------------------------------------------------

    private func installTermination() {
        // The default disposition has to go first: a dispatch source only sees SIGTERM
        // if the signal does not kill the process on its way in.
        signal(SIGTERM, SIG_IGN)
        let source = DispatchSource.makeSignalSource(signal: SIGTERM, queue: .main)
        source.setEventHandler { [weak self] in self?.finish() }
        source.resume()
        termination = source
    }

    /// Everything off the screen, then out — at once, and with 0, because a helper that
    /// is gone is not a failure of the indicator.
    private func finish() {
        indicator?.removeEverything()
        if ignored > 0 { note("\(ignored) state line(s) ignored") }
        exit(0)
    }

    private func note(_ message: String) {
        FileHandle.standardError.write(Data("compux-indicator: \(message)\n".utf8))
    }
}
