//! compux — the native computer-use (screen-capture + input-injection) sidecar.
//!
//! Reads one JSON request line from stdin, performs the GUI action, writes one
//! JSON response line to stdout. The wire contract is `Compux.Protocol`
//! (lib/compux/protocol.ex). The Elixir `Compux.PortDriver` owns this process
//! as a Port.
//!
//! Coordinate model (the #1 "clicks land offset" risk — read carefully):
//!   * A screenshot is the target display captured at PHYSICAL pixels, then
//!     downscaled to fit the sent budgets. The model sees that downscaled image
//!     and sends click coordinates in ITS pixel space.
//!   * Every reply that hands out coordinates therefore names the image they are
//!     in (`observation_id`), and every action that sends coordinates back names
//!     one. The transform is stored with the image and used as stored; nothing
//!     re-derives it from a rectangle the caller repeated. `mod geometry` owns
//!     the arithmetic and `mod observation` owns the table.
//!   * v1 drives ONE display (the configured index, default primary).
//!
//! Runtime behavior must be verified on a real machine with the macOS TCC grants
//! (Screen Recording and Accessibility). It never panics the request loop — every
//! action answers with `{"ok": true, ...}` or `{"ok": false, "error": "..."}`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use enigo::{Axis, Button, Enigo, Key, Mouse, Settings};
use image::ImageEncoder as _;
use serde::Deserialize;
use serde_json::{json, Value};
use xcap::Monitor;

use gate::{Gate, Gated, Phase, SystemClock};
use geometry::{
    crop_rect, sent_scale, to_logical, to_sent, Geometry, Host, Measured, MonitorFacts, Region,
};
use held::Platform as _;
use observation::{Kind, Observation, Observations};
use wire::Failure;

/// Capture mode (MILESTONE_32 §8.4a): the AXObserver/CFRunLoop event-push engine +
/// the serialized `Emitter`. Isolated from the request/response core here.
mod capture;

/// Held synthetic input: the press registry and the `Platform` seam every input
/// sequence posts through, so nothing this process presses can outlive the action.
mod held;

/// The protocol-8 frames as types: line parsing, the outbound builders, receipts.
mod wire;

/// Display geometry and the one coordinate transform, pure and platform-injected.
mod geometry;

/// The images and coordinate lists handed out, and the transform each was made
/// with — so a click is mapped by the geometry its picture was taken with.
mod observation;

/// The admission gate: generations, the `mutation_seq` high-water mark, the pause
/// barrier, and the cancellation checkpoints every paced sequence reads.
mod gate;

/// The control reader thread, which owns stdin so the sidecar listens while it acts.
mod control;

/// Wire-compatibility version. MUST match `Compux.Protocol.protocol_version/0`.
/// Bumped ONLY on a wire-incompatible change; reported in the `hello` handshake so
/// a consumer can refuse a mismatched sidecar (the two-pin drift guard).
///
/// v3: added the operational idle-detection actions `idle_ms` + `wait_for_idle`
/// (coexistence — let a policy layer yield the seat to a present human).
///
/// v5: `screenshot` gained the optional grounding-integrity fields `rulers`,
/// `annotate_point`, and `marks` (M28) — additive, but a consumer advertising
/// them against an older sidecar would get silently un-annotated images, so the
/// version bumps and the handshake refuses the pairing loudly.
///
/// v6: CAPTURE MODE (MILESTONE_32 §8.4a) — the control actions `observe_start` /
/// `observe_stop` (excluded from `hello`'s model verbs) and an unsolicited
/// `{"type":"event"|"ack",…}` push wire. The push channel is wire-incompatible
/// with the strictly-positional request/response core, so the version bumps and
/// the handshake refuses an older pairing. See `mod capture`.
///
/// 0.9.0 stays on v6: the `browser.navigated` observation kind and the browser
/// context on `field.value` are ADDITIVE fields on that same push wire, and the
/// retired `sites` key of `observe_start` is accepted and ignored — no control
/// action changed, so neither side needs a new minimum.
///
/// v7 (M42 slice 2): the action wire becomes TAGGED and CORRELATED. Every line
/// carries a `type`; every request a `request_id` its response echoes, and, after
/// the handshake, the generations it belongs to. Requests and responses no longer
/// pair by ORDER, which retires the desync class where one late frame answered
/// every later question. A mutating request carries an increasing `mutation_seq`
/// and its response a `receipt` saying whether input was dispatched; `control` and
/// `control_ack` make Pause a confirmed barrier rather than a hope. The caller's
/// remaining budget rides as `deadline_ms`, NOT `timeout_ms`, which two actions
/// have used as an argument of their own since v2. Computer history keeps its
/// `ack` and `event` families byte for byte; only this integer inside the ack
/// moves. See `mod wire`.
///
/// v8 (M42 slice 3): every reply that hands out coordinates names the image they
/// are in, and every action that sends coordinates back names one. A pointer action
/// or `inspect` carries `observation_id` and no `region`; `screenshot`, `elements`
/// and `wait_for_change` keep `region` and may name the image it was read in. The
/// transform is stored with the image and used as stored, so nothing re-derives it
/// from a rectangle a caller repeated — and it is built from a ratio MEASURED on
/// the frame that was really captured rather than assumed from the display mode.
/// See `mod observation` and `mod geometry`.
const PROTOCOL_VERSION: u32 = 8;

/// The capture-stall self-reap (EX_TEMPFAIL), and NOTHING else.
///
/// Fermix reads this exact status as a clean, retryable capture wedge: it feeds
/// `CaptureHealth`, `Session.note_capture_wedge/1` and the realtime feed's
/// `wedge?/1`. Any other failure that exits 75 is therefore reported to the
/// operator as a wedged capture backend, which is why `disclaim` owns its own
/// codes and none of them is this one.
const EXIT_CAPTURE_STALLED: i32 = 75;

// --- macOS TCC responsibility disclaim ---------------------------------------

/// Make this process its OWN TCC "responsible process".
///
/// `Compux.PortDriver` spawns us via `posix_spawn` (the Elixir Port). A child that
/// does not disclaim inherits its PARENT's TCC identity, so macOS would attribute our
/// Screen-Recording (`kTCCServiceScreenCapture`) and Accessibility
/// (`kTCCServiceAccessibility`) requests to the ad-hoc, version-keyed BEAM/daemon
/// ancestor — an identity whose grant never persists (re-prompt every action, a new
/// System-Settings row per version). Disclaiming attaches the grant to compux's OWN
/// stable Developer-ID bundle identity instead.
///
/// The disclaim flag must be set on the spawn attributes BEFORE the spawn — which the
/// Port cannot do, and a running process cannot do to itself — so we re-exec ONCE via
/// `POSIX_SPAWN_SETEXEC` (replaces this image in place; the pid and the stdin/stdout
/// Port fds are preserved, so the wire protocol is untouched). A `COMPUX_DISCLAIMED`
/// sentinel bounds it to a single re-exec. The API is private/header-less (resolved
/// via `dlsym`), so we FAIL LOUD (non-zero exit) if it is absent or errors, rather
/// than silently running un-disclaimed and resurrecting the mis-attribution bug.
///
/// EXIT CODES: this module owns 70-74, 76 and 77, one per distinguishable failure.
/// It must never claim 75 — that is the capture-stall self-reap in `main`, which
/// Fermix reads as a specific, clean, retryable condition (it feeds `CaptureHealth`
/// and the realtime feed's wedge check). `posix_spawnattr_setflags` used to exit 75
/// and was therefore reported to the operator as a wedged capture backend.
#[cfg(target_os = "macos")]
mod disclaim {
    use std::env;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::ptr;

    type SetDisclaimFn =
        unsafe extern "C" fn(*mut libc::posix_spawnattr_t, libc::c_int) -> libc::c_int;

    /// One code per distinguishable failure, so an operator's exit status names
    /// which step broke. Named rather than inline so the whole set can be gated:
    /// see `no_disclaim_exit_code_collides_with_the_capture_stall`.
    const NO_SETDISCLAIM_SYMBOL: i32 = 70;
    const SETDISCLAIM_REFUSED: i32 = 71;
    const REEXEC_FAILED: i32 = 72;
    const NO_CURRENT_EXE: i32 = 73;
    const ATTR_INIT_FAILED: i32 = 74;
    const NUL_IN_ARGV: i32 = 76;
    /// 77 because 70 to 76 were all taken and 75 belongs to the capture stall.
    const SETFLAGS_FAILED: i32 = 77;

    /// Every code this module can exit with. The gate loops over this, so a code
    /// added later either joins it or fails the test.
    #[cfg(test)]
    pub const EXIT_CODES: [i32; 7] = [
        NO_SETDISCLAIM_SYMBOL,
        SETDISCLAIM_REFUSED,
        REEXEC_FAILED,
        NO_CURRENT_EXE,
        ATTR_INIT_FAILED,
        NUL_IN_ARGV,
        SETFLAGS_FAILED,
    ];

    pub fn become_responsible() {
        if env::var_os("COMPUX_DISCLAIMED").is_some() {
            return; // already the re-exec'd, disclaimed image
        }

        let set_disclaim = resolve_set_disclaim();
        let exe = env::current_exe()
            .unwrap_or_else(|e| fatal(NO_CURRENT_EXE, &format!("current_exe: {e}")));
        let exe_c = cstr(exe.as_os_str().as_bytes());

        let argv = CArray::new(env::args_os().map(|a| cstr(a.as_bytes())).collect());
        let mut env_strings: Vec<CString> = env::vars_os()
            .map(|(k, v)| {
                let mut kv = k.as_bytes().to_vec();
                kv.push(b'=');
                kv.extend_from_slice(v.as_bytes());
                cstr(&kv)
            })
            .collect();
        env_strings.push(cstr(b"COMPUX_DISCLAIMED=1"));
        let envp = CArray::new(env_strings);

        unsafe {
            let mut attr: libc::posix_spawnattr_t = std::mem::zeroed();
            if libc::posix_spawnattr_init(&mut attr) != 0 {
                fatal(ATTR_INIT_FAILED, "posix_spawnattr_init failed");
            }
            if set_disclaim(&mut attr, 1) != 0 {
                fatal(
                    SETDISCLAIM_REFUSED,
                    "responsibility_spawnattrs_setdisclaim returned nonzero",
                );
            }
            if libc::posix_spawnattr_setflags(&mut attr, libc::POSIX_SPAWN_SETEXEC as libc::c_short)
                != 0
            {
                fatal(SETFLAGS_FAILED, "posix_spawnattr_setflags failed");
            }
            // SETEXEC replaces this image; posix_spawn returns ONLY on failure.
            libc::posix_spawn(
                ptr::null_mut(),
                exe_c.as_ptr(),
                ptr::null(),
                &attr,
                argv.ptrs.as_ptr(),
                envp.ptrs.as_ptr(),
            );
            fatal(REEXEC_FAILED, "POSIX_SPAWN_SETEXEC re-exec failed");
        }
    }

    fn resolve_set_disclaim() -> SetDisclaimFn {
        // dlsym's RTLD_DEFAULT pseudo-handle on macOS is (void *)-2.
        let rtld_default = (-2isize) as *mut libc::c_void;
        let name = cstr(b"responsibility_spawnattrs_setdisclaim");
        let sym = unsafe { libc::dlsym(rtld_default, name.as_ptr()) };
        if sym.is_null() {
            fatal(
                NO_SETDISCLAIM_SYMBOL,
                "responsibility_spawnattrs_setdisclaim unavailable",
            );
        }
        unsafe { std::mem::transmute::<*mut libc::c_void, SetDisclaimFn>(sym) }
    }

    fn cstr(bytes: &[u8]) -> CString {
        CString::new(bytes).unwrap_or_else(|_| fatal(NUL_IN_ARGV, "unexpected NUL in argv/env"))
    }

    // Owns the CString backing store so the null-terminated pointer vector stays valid.
    struct CArray {
        ptrs: Vec<*mut libc::c_char>,
        _owned: Vec<CString>,
    }

    impl CArray {
        fn new(owned: Vec<CString>) -> CArray {
            let mut ptrs: Vec<*mut libc::c_char> =
                owned.iter().map(|s| s.as_ptr() as *mut _).collect();
            ptrs.push(ptr::null_mut());
            CArray {
                ptrs,
                _owned: owned,
            }
        }
    }

    fn fatal(code: i32, msg: &str) -> ! {
        eprintln!("compux: FATAL disclaim: {msg}");
        std::process::exit(code);
    }
}

fn main() {
    // macOS: become our OWN TCC responsible process before any capture/input call,
    // so Screen-Recording + Accessibility grants attribute to compux's stable code
    // identity rather than the ad-hoc BEAM ancestor that Port-spawned us. One-shot
    // self-re-exec (see the `disclaim` module); a no-op on the second entry.
    #[cfg(target_os = "macos")]
    disclaim::become_responsible();

    // One serialized writer shared by the control reader, the action worker and
    // the capture observer thread (§8.4a): every response, acknowledgement, ack
    // and event frame goes through `emitter`, so two writers can never split each
    // other's line.
    let emitter = capture::Emitter::new();

    // One gate for the process. The reader flips it; the worker is admitted
    // through it. Both hold the same handle, which is what makes a pause a
    // barrier rather than a request to stop soon.
    let gate = Gate::new(boot_generation(), Arc::new(SystemClock::new()));

    // Capacity one: a second request while one is in flight is refused `busy` by
    // the reader, never queued behind work whose screen has moved on.
    let (to_worker, jobs) = mpsc::sync_channel::<control::Job>(1);

    // The reader owns stdin on its own thread, so a control is heard and answered
    // while the worker is inside a wait, a poll or an accessibility settle.
    {
        let gate = gate.clone();
        let emitter = emitter.clone();
        thread::spawn(move || control::run(io::stdin().lock(), &gate, &emitter, to_worker));
    }

    // The action worker runs on the MAIN thread: input stays where it runs today,
    // and AppKit will want this thread in a later slice.
    run_worker(jobs, &gate, &emitter);

    // Every sender is gone, so stdin reached EOF: the owning Port closed. Tear down any live
    // capture observer, then switch OFF any accessibility attribute this process
    // switched on (B4), so one enumeration never leaves the user's browser in an
    // altered AX mode. Best-effort: a SIGKILLed sidecar skips this, and the next
    // activation is idempotent.
    capture::stop();
    #[cfg(target_os = "macos")]
    ax::clear_activations();
}

/// A fresh identity per boot, so a frame minted by an earlier sidecar can never be
/// taken for one of ours. The pid alone would not do it — pids are reused.
fn boot_generation() -> String {
    format!("boot-{}-{}", std::process::id(), capture::now_ms())
}

/// Everything the action worker owns that is neither on the wire nor in the gate:
/// the images handed out and the transform each was made with, and the pixels per
/// point measured on each display. One thread reads and writes both, one request
/// at a time, so neither is locked — and nothing else may take a reference to
/// them, which is the invariant that keeps it that way.
struct Worker {
    observations: Observations,
    measured: Measured,
}

/// The serial action worker. One request at a time, to completion, on this thread.
fn run_worker(jobs: mpsc::Receiver<control::Job>, gate: &Gate, emitter: &capture::Emitter) {
    let mut worker = Worker {
        observations: Observations::new(&gate.envelope().sidecar_generation, gate.clock()),
        measured: Measured::new(),
    };

    for job in jobs {
        let control::Job::Action(request) = job;
        let frame = serve(&request, gate, emitter, &mut worker);

        // One JSON line per reply. A write failure means the parent is gone.
        if emitter.emit_line(&frame.to_string()).is_err() {
            break;
        }

        // A capture wedged the OS backend and leaked a stuck worker we can't
        // reclaim — the reply is now flushed, so exit and let the parent respawn
        // a clean sidecar (EX_TEMPFAIL). Do this AFTER flush so the caller got
        // its `capture_stalled` answer first.
        if CAPTURE_WEDGED.load(Ordering::SeqCst) {
            capture::stop();
            #[cfg(target_os = "macos")]
            ax::clear_activations();
            std::process::exit(EXIT_CAPTURE_STALLED);
        }
    }
}

/// Admit, run, and answer one request.
fn serve(
    request: &wire::Request,
    gate: &Gate,
    emitter: &capture::Emitter,
    worker: &mut Worker,
) -> Value {
    if let Err(refusal) = gate.admit(request) {
        return wire::error_response(
            gate.envelope(),
            &request.request_id,
            refusal.code(),
            Some(refusal.detail().to_string()),
            receipt(request, false, false, false, wire::Timings::default(), None),
        );
    }

    let outcome = handle(request, gate, emitter, worker);
    let done = gate.finish();
    reply(request, gate, outcome, done)
}

/// Turn what the action did into the frame that reports it.
fn reply(
    request: &wire::Request,
    gate: &Gate,
    outcome: Result<Value, Failure>,
    done: gate::Dispatched,
) -> Value {
    // The capture verbs answer in the computer-history `ack` family, byte for byte
    // as they did at protocol 6. Their client reads that shape and nothing else.
    if wire::answers_with_ack(&request.action) {
        return match outcome {
            Ok(ack) => ack,
            Err(failure) => observe_ack(&request.action, false, Some(failure.code)),
        };
    }

    match outcome {
        Ok(payload) => {
            // An after-image is whatever the action actually produced, never what
            // it was asked to produce: `data` is present only when a capture ran.
            let after_image = payload.get("data").is_some();
            // The image this action handed back, when it handed one back — read
            // off the payload for the same reason: what was produced, not what was
            // asked for.
            let after = payload
                .get("observation_id")
                .and_then(Value::as_str)
                .map(str::to_string);
            let receipt = receipt(
                request,
                done.posted,
                done.input_complete,
                after_image,
                done.timings,
                after,
            );
            wire::response(gate.envelope(), &request.request_id, payload, receipt)
        }

        Err(failure) => {
            // `input_complete`, not `false`: the post-action check image is taken
            // INSIDE the action, so a click whose every event landed and whose own
            // screenshot then failed dispatched `sent`. Calling that `partial` would
            // have Fermix report `unknown` where the truth is performed-unverified.
            let receipt = receipt(
                request,
                done.posted,
                done.input_complete,
                false,
                done.timings,
                None,
            );
            // Every refusal that reaches here was cancelled (a pause sets both
            // flags), so a refusal code only ever lands in `detail` and never
            // stands in for the action's own error. A detail that merely repeats
            // the code is dropped: it would tell an operator nothing.
            let (code, detail) = if done.cancelled {
                (
                    "cancelled".to_string(),
                    Some(failure.code).filter(|code| code != "cancelled"),
                )
            } else {
                (failure.code, failure.detail)
            };
            wire::error_response(gate.envelope(), &request.request_id, &code, detail, receipt)
        }
    }
}

/// A receipt rides every action that is not read-only, refusals included: "no
/// input was sent" is exactly the fact a caller needs before it retries.
fn receipt(
    request: &wire::Request,
    posted: bool,
    input_complete: bool,
    after_image: bool,
    timings: wire::Timings,
    after: Option<String>,
) -> Option<wire::Receipt> {
    if wire::carries_mutation_seq(&request.action) {
        Some(
            wire::Receipt::derive(posted, input_complete, after_image, timings)
                .addressing(request.observation_id.clone(), after),
        )
    } else {
        None
    }
}

fn handle(
    request: &wire::Request,
    gate: &Gate,
    emitter: &capture::Emitter,
    worker: &mut Worker,
) -> Result<Value, Failure> {
    let req = &request.body;

    match request.action.as_str() {
        "hello" => hello(),
        "probe" => probe(),
        "idle_ms" => idle_ms(),
        "wait_for_idle" => wait_for_idle(req, gate),
        "request_permissions" => request_permissions(req),
        // Capture control verbs (MILESTONE_32 §8.4a) — NOT model actions, excluded
        // from `hello`. They return a type-discriminated `ack` frame (even on
        // refusal), never the generic `{ok:false,error}` shape, so the consumer's
        // handshake reads `ok`/`protocol_version` rather than degrading on a
        // missing frame type.
        "observe_start" => Ok(observe_start(req, emitter)),
        "observe_stop" => Ok(observe_stop()),
        "screenshot" => screenshot(request, gate, worker),
        "mouse_move" => mouse_move(request, gate, worker),
        "left_click" => click(request, gate, worker, Button::Left, 1),
        "right_click" => click(request, gate, worker, Button::Right, 1),
        "double_click" => click(request, gate, worker, Button::Left, 2),
        "left_click_drag" => drag(request, gate, worker),
        "scroll" => scroll(request, gate, worker),
        "type" => type_text(req, gate, worker),
        "key" => key_chord(req, gate, worker),
        "wait" => wait(req, gate),
        "inspect" => inspect(request, worker),
        "wait_for_change" => wait_for_change(request, gate, worker),
        "paste" => paste(req, gate, worker),
        "elements" => elements(request, gate, worker),
        "windows" => windows(req, gate, worker),
        other => Err(format!("unknown action: {other}").into()),
    }
}

// --- addressing: which image a request's coordinates are in ------------------

/// The image an action's coordinates were read in, taken out of the table.
///
/// The parse layer has already guaranteed there is an id, so what is left is the
/// two ways a stored transform can go wrong, and they are checked in this order on
/// purpose: what is pure first, what needs the OS second.
///
/// Copied rather than borrowed, so the action that follows can go on to mint its
/// own check image into the same table.
fn observed(request: &wire::Request, worker: &Worker) -> Result<Observation, Failure> {
    let id = request
        .observation_id
        .as_deref()
        .ok_or_else(|| Failure::from("observation_required"))?;

    let observation = worker.observations.resolve(id).map_err(refused)?.clone();

    Ok(observation)
}

/// A point the model read off that image, mapped to the global logical point enigo
/// takes. A point outside the image is refused and never clamped onto an edge: it
/// was read wrong, probably off a different image, and clamping turns that into a
/// click on whatever happens to sit at the boundary.
fn point_in(observation: &Observation, x: f64, y: f64) -> Result<(i32, i32), Failure> {
    observation.contains(x, y).map_err(refused)?;

    Ok(to_logical(&observation.geometry, &observation.region, x, y))
}

/// The display that image was made on, proven to still be the display in front of
/// us. Reads what the OS says — bounds, origin, mode scale, id — which needs no
/// capture, and refuses on any difference: a click computed from a geometry that
/// has since moved lands somewhere nobody chose.
fn same_display(observation: &Observation, req: &Value) -> Result<Display, Failure> {
    let display = target_display(req)?;

    if observation.matches_display(display.id, &display.facts) {
        Ok(display)
    } else {
        Err(refused(observation::Refusal::Stale))
    }
}

/// Every one of these is `dispatch: not_sent` — nothing was done — and each says
/// what to do next in its own words.
fn refused(refusal: observation::Refusal) -> Failure {
    Failure::new(refusal.code(), refusal.detail())
}

// --- capture control (MILESTONE_32 §8.4a, NOT model actions) -----------------

/// Start the capture observer and reply with the `observe_start` ack. On a refusal
/// (no Accessibility grant, already running, non-macOS) the ack carries `ok:false`
/// plus a diagnostic `error` (which the consumer ignores). The consumer degrades to
/// observe_start_refused on the `ok:false`, so it MUST be an `ack`, never `err()`.
fn observe_start(req: &Value, emitter: &capture::Emitter) -> Value {
    match capture::start(req, emitter.clone()) {
        Ok(()) => observe_ack("observe_start", true, None),
        Err(reason) => observe_ack("observe_start", false, Some(reason)),
    }
}

/// Tear down the observer and ack. `observe_stop` is best-effort and always `ok`.
fn observe_stop() -> Value {
    capture::stop();
    observe_ack("observe_stop", true, None)
}

fn observe_ack(action: &str, ok: bool, error: Option<String>) -> Value {
    let mut ack = json!({
        "type": "ack",
        "action": action,
        "ok": ok,
        "protocol_version": PROTOCOL_VERSION,
    });
    if let Some(error) = error {
        ack["error"] = json!(error);
    }
    ack
}

// --- hello (version handshake, NOT a model action) --------------------------

/// Identity + wire-version handshake performed once by `Compux.start/1`. Lets the
/// consumer refuse a sidecar whose `protocol_version` its compiled-in encoder does
/// not speak (the two-pin drift guard). `compux_version` is diagnostic; `actions`
/// is the model-facing verb set (probe/hello are operational, excluded).
fn hello() -> Result<Value, Failure> {
    Ok(json!({
        "ok": true,
        "protocol_version": PROTOCOL_VERSION,
        "compux_version": env!("CARGO_PKG_VERSION"),
        "actions": [
            "screenshot", "left_click", "right_click", "double_click", "mouse_move",
            "left_click_drag", "scroll", "type", "key", "wait", "inspect",
            "wait_for_change", "paste", "elements", "windows"
        ],
        // Listed only because this build really has them: one foreground HID input
        // method, the three controls the gate implements, and the bounds of the
        // observation table — a caller mirrors those numbers to know which ids it
        // may still address before it asks.
        "capabilities": {
            "input_methods": ["foreground_hid"],
            "controls": ["pause", "resume", "release"],
            "observations": {
                "max": observation::MAX_OBSERVATIONS,
                "ttl_ms": observation::TTL_MS,
            },
        },
    }))
}

// --- probe (operational permission check, NOT a model action) ---------------

/// Report whether screen capture and input control are actually available, plus
/// the platform and display server. NON-PROMPTING: on macOS this queries TCC grant
/// state (Accessibility + Screen Recording) WITHOUT raising a permission dialog or
/// posting an event — the only reliable way to detect the silent-drop state where
/// capture works but synthetic input is discarded. Surfaced by the consumer's
/// diagnostics (a doctor/setup surface); the model never calls this.
fn probe() -> Result<Value, Failure> {
    Ok(json!({
        "ok": true,
        "platform": std::env::consts::OS,
        "display_server": display_server(),
        "screen_capture": screen_capture_ok(),
        "input_control": input_control_ok(),
    }))
}

// --- request_permissions (operational grant PROMPT, NOT a model action) -------

/// Actively PROMPT for the macOS grants and report the resulting state. Operational,
/// like `probe` — excluded from `hello`'s model-facing verbs. The consumer's setup /
/// doctor flow calls this at enable time so the OS dialogs (Screen Recording +
/// Accessibility) appear up front instead of on the first screenshot, and the app
/// registers in System Settings. Unlike `probe`'s non-prompting preflight, these
/// variants RAISE the system dialog. The prompts are async, so the returned booleans
/// are the pre-response snapshot (typically `false` on first call) — the consumer
/// re-runs `probe` after the user approves. No-op on Linux (no TCC).
fn request_permissions(_req: &Value) -> Result<Value, Failure> {
    Ok(json!({
        "ok": true,
        "platform": std::env::consts::OS,
        "screen_capture": request_screen_capture_ok(),
        "input_control": request_input_control_ok(),
    }))
}

#[cfg(target_os = "macos")]
fn request_screen_capture_ok() -> bool {
    permissions::request_screen_capture()
}

#[cfg(target_os = "macos")]
fn request_input_control_ok() -> bool {
    permissions::request_input_control()
}

// No TCC off macOS: report the same reality `probe` does (there is nothing to prompt).
#[cfg(not(target_os = "macos"))]
fn request_screen_capture_ok() -> bool {
    screen_capture_ok()
}

#[cfg(not(target_os = "macos"))]
fn request_input_control_ok() -> bool {
    input_control_ok()
}

#[cfg(target_os = "macos")]
mod permissions {
    //! macOS TCC grant state, queried without prompting.
    //!
    //! `AXIsProcessTrusted` (ApplicationServices): is this process trusted for the
    //! Accessibility API — the gate macOS silently drops `CGEventPost` without (so a
    //! click returns ok yet nothing moves). `CGPreflightScreenCaptureAccess`
    //! (CoreGraphics, 10.15+): is screen capture permitted — without it capture
    //! returns wallpaper-only. Both are preflight checks; neither prompts.
    use core_foundation::base::TCFType;
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
    use core_foundation::string::CFString;

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrusted() -> u8;
        // Same trust check, but with `kAXTrustedCheckOptionPrompt` it RAISES the
        // Accessibility prompt (directs the user to System Settings). Returns the
        // current (pre-grant) trust state.
        fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> u8;
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
        // Prompts if the grant is undetermined and registers the app in System
        // Settings ▸ Screen Recording. Returns whether access is already granted.
        fn CGRequestScreenCaptureAccess() -> bool;
    }

    pub fn input_control() -> bool {
        unsafe { AXIsProcessTrusted() != 0 }
    }

    pub fn screen_capture() -> bool {
        unsafe { CGPreflightScreenCaptureAccess() }
    }

    // --- prompting variants (used by `request_permissions`) ---

    pub fn request_screen_capture() -> bool {
        unsafe { CGRequestScreenCaptureAccess() }
    }

    pub fn request_input_control() -> bool {
        // The exported `kAXTrustedCheckOptionPrompt` constant does not link as a
        // symbol (same as the AX attribute-name constants elsewhere here), so we
        // build the CFString from its documented value.
        let key = CFString::new("AXTrustedCheckOptionPrompt");
        let options = CFDictionary::from_CFType_pairs(&[(
            key.as_CFType(),
            CFBoolean::true_value().as_CFType(),
        )]);
        unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef()) != 0 }
    }
}

/// Make the pointer BE at the requested point before any button/scroll event is
/// posted, so the event's own destination is the point the caller asked for.
///
/// Why this exists (observed live 2026-07-26: 4 of 7 clicks landed on the PREVIOUS
/// click's point, every one reporting success): enigo's macOS `button()` opens with
/// `let (current_x, current_y) = self.location()?` — `NSEvent::mouseLocation()` — and
/// builds `dest` for BOTH the mouse-down and the mouse-up from that live read. Our
/// preceding `move_mouse` only POSTS a CGEvent, which the window server applies
/// asynchronously, so `mouseLocation()` can still report the old position when
/// `button()` reads it. The click then goes to — and warps the cursor to — wherever
/// the pointer still was. `scroll` has the same exposure: a CGEvent scroll carries no
/// destination and lands wherever the pointer actually is.
///
/// `CGWarpMouseCursorPosition` is SYNCHRONOUS: it moves the cursor in the window
/// server before returning, so the subsequent `location()` read is the truth. It is
/// deliberately called AFTER `move_mouse`, not instead of it — `move_mouse` is what
/// emits the MouseMoved/Dragged event (hover states, drag tracking) with real deltas,
/// and warping first would zero those deltas.
///
/// Takes the same top-left global point space `move_mouse(_, _, Coordinate::Abs)`
/// does, which is what `to_logical` already produces. Note macOS suppresses local
/// HID mouse events for a short interval after a warp; compux already yields the
/// cursor to a present human through the caller's courtesy arbiter, so a brief
/// suppression during an agent action is the intended trade.
#[cfg(target_os = "macos")]
mod pointer {
    use std::ffi::c_void;
    use std::ptr;

    #[repr(C)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    /// `CGEventType::kCGEventLeftMouseDragged` / `kCGHIDEventTap` /
    /// `kCGMouseButtonLeft` — numeric values from CGEventTypes.h, stable ABI.
    const LEFT_MOUSE_DRAGGED: u32 = 6;
    const HID_EVENT_TAP: u32 = 0;
    const MOUSE_BUTTON_LEFT: u32 = 0;

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGWarpMouseCursorPosition(newCursorPosition: CGPoint) -> i32;
        fn CGEventCreateMouseEvent(
            source: *const c_void,
            event_type: u32,
            point: CGPoint,
            button: u32,
        ) -> *mut c_void;
        fn CGEventPost(tap: u32, event: *mut c_void);
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(cf: *const c_void);
    }

    /// `Err` on a non-zero CGError: a pointer we could not place is a click we must
    /// not post, since it would land somewhere the caller never asked for.
    pub fn settle(x: i32, y: i32) -> Result<(), String> {
        let point = CGPoint {
            x: f64::from(x),
            y: f64::from(y),
        };

        match unsafe { CGWarpMouseCursorPosition(point) } {
            0 => Ok(()),
            error => Err(format!("warp pointer to ({x},{y}): CGError {error}")),
        }
    }

    /// One intermediate point of a drag, posted as an EXPLICIT `LeftMouseDragged`
    /// event. enigo's `move_mouse` picks its event type from a live
    /// `pressedMouseButtons()` read, which races the just-posted mouse-down — the
    /// intermediate move then goes out as `MouseMoved` (a hover, not a drag) and
    /// no drag handler ever arms. Building the event by hand removes the state
    /// read entirely. The warp afterwards keeps the visible cursor and the next
    /// `location()` read (the release's destination) at the same point.
    pub fn drag_step(x: i32, y: i32) -> Result<(), String> {
        let point = CGPoint {
            x: f64::from(x),
            y: f64::from(y),
        };

        let event = unsafe {
            CGEventCreateMouseEvent(ptr::null(), LEFT_MOUSE_DRAGGED, point, MOUSE_BUTTON_LEFT)
        };

        if event.is_null() {
            return Err(format!("create drag event at ({x},{y})"));
        }

        unsafe {
            CGEventPost(HID_EVENT_TAP, event);
            CFRelease(event);
        }

        settle(x, y)
    }
}

/// X11 injects motion synchronously (`XTestFakeMotionEvent` + flush) and
/// `XTestFakeButtonEvent` carries no coordinate, so there is nothing to settle.
#[cfg(not(target_os = "macos"))]
mod pointer {
    pub fn settle(_x: i32, _y: i32) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn display_server() -> &'static str {
    "quartz"
}

#[cfg(target_os = "macos")]
fn screen_capture_ok() -> bool {
    permissions::screen_capture()
}

#[cfg(target_os = "macos")]
fn input_control_ok() -> bool {
    permissions::input_control()
}

// Linux: X11 is permissive (no TCC — any local client may capture/inject); Wayland
// deliberately blocks global capture + injection (no uniform API). Capability tracks
// the display server; a real capture/input still fails loud per action.
#[cfg(target_os = "linux")]
fn display_server() -> &'static str {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        "wayland"
    } else if std::env::var_os("DISPLAY").is_some() {
        "x11"
    } else {
        "none"
    }
}

#[cfg(target_os = "linux")]
fn screen_capture_ok() -> bool {
    display_server() == "x11"
}

#[cfg(target_os = "linux")]
fn input_control_ok() -> bool {
    display_server() == "x11"
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn display_server() -> &'static str {
    "unknown"
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn screen_capture_ok() -> bool {
    false
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn input_control_ok() -> bool {
    false
}

// --- display selection ------------------------------------------------------

/// A display, as the OS describes it. The geometry is NOT here: it depends on the
/// pixels-per-point ratio, which is measured from a capture (see `mod geometry`),
/// so it is built per action from these facts and a measurement.
struct Display {
    facts: MonitorFacts,
    /// The monitor's stable id (CGDirectDisplayID on macOS). Capture and the
    /// asleep check both re-resolve the monitor by THIS (never by list index),
    /// so a mid-action display change fails typed instead of rebinding to a
    /// different physical monitor. The xcap `Monitor` handle isn't `Send` and
    /// isn't held past geometry read — the id is all a later capture needs.
    id: u32,
}

/// Pick the requested monitor, distinguishing "no display is capturable at all"
/// from "that index doesn't exist on a multi-monitor host".
///
/// `xcap`'s active-monitor list is EMPTY when nothing can be captured — on macOS
/// that is the screen-locked, display-asleep, or no-GUI-session state, none of
/// which a different `display` index can fix. Reporting that as `display 0 not
/// found` reads like a bad index and sends the caller hunting for another monitor;
/// the typed `no_active_display` lets the Elixir layer say what is actually wrong.
fn select_monitor(monitors: Vec<Monitor>, index: usize) -> Result<Monitor, String> {
    if monitors.is_empty() {
        return Err("no_active_display".to_string());
    }

    monitors
        .into_iter()
        .nth(index)
        .ok_or_else(|| format!("display {index} not found"))
}

/// What the OS says about the requested display, right now. Cheap — it enumerates
/// monitors and reads their numbers, and takes no capture — which is what lets the
/// staleness check run before every addressed action.
fn target_display(req: &Value) -> Result<Display, String> {
    let index = req.get("display").and_then(Value::as_u64).unwrap_or(0) as usize;
    let monitors = Monitor::all().map_err(|e| format!("enumerate displays: {e}"))?;
    let monitor = select_monitor(monitors, index)?;

    // xcap 0.4 returns the monitor geometry as `Result`s — unwrap each loudly so a
    // capture-backend hiccup surfaces as a clean action error, never a wrong click.
    let facts = MonitorFacts {
        x: monitor.x().map_err(|e| format!("display origin x: {e}"))?,
        y: monitor.y().map_err(|e| format!("display origin y: {e}"))?,
        width: monitor.width().map_err(|e| format!("display width: {e}"))?,
        height: monitor
            .height()
            .map_err(|e| format!("display height: {e}"))?,
        scale_factor: monitor
            .scale_factor()
            .map_err(|e| format!("scale_factor: {e}"))?
            .max(1.0),
    };

    Ok(Display {
        facts,
        id: monitor.id().map_err(|e| format!("display id: {e}"))?,
    })
}

/// The transform for a display that is about to hand out coordinates WITHOUT
/// capturing an image (`windows`, `elements`).
///
/// It needs a real measurement and the only honest source of one is a real capture,
/// so a display whose CURRENT configuration nothing has captured is measured here
/// with one frame that is thrown away. Guessing it from the display mode is exactly
/// the assumption this slice removed — and remembering it against the display's id
/// alone would be the same assumption one mode change later, so the memo is keyed on
/// the facts it was measured under.
fn measured_geometry(
    display: &Display,
    gate: &Gate,
    worker: &mut Worker,
) -> Result<Geometry, Failure> {
    if let Some(measured) = worker.measured.get(display.id, &display.facts) {
        return Ok(Geometry::from_facts(&display.facts, measured, Host::HERE));
    }

    ensure_display_awake(display)?;
    let image = timed_capture(display, gate)?;
    measure_geometry(display, &image, worker)
}

/// Build the display's transform from a frame that was really captured, and
/// remember it for the replies that hand out coordinates without one.
fn measure_geometry(
    display: &Display,
    image: &image::RgbaImage,
    worker: &mut Worker,
) -> Result<Geometry, Failure> {
    let measured = geometry::measure(&display.facts, image.width(), image.height(), Host::HERE)
        .map_err(|detail| Failure::new("capture_geometry_mismatch", detail))?;

    worker
        .measured
        .remember(display.id, &display.facts, measured);
    Ok(Geometry::from_facts(&display.facts, measured, Host::HERE))
}

fn parse_region(req: &Value) -> Result<Option<Region>, String> {
    match req.get("region") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let x = region_field(value, "x")?;
            let y = region_field(value, "y")?;
            let w = region_field(value, "w")?;
            let h = region_field(value, "h")?;
            if w <= 0.0 || h <= 0.0 {
                return Err("region.w and region.h must be > 0".to_string());
            }
            Ok(Some(Region { x, y, w, h }))
        }
    }
}

fn region_field(value: &Value, key: &str) -> Result<f64, String> {
    value
        .get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| format!("region.{key} is missing or not a number"))
}

/// The rectangle a VIEWING action is asking for, in the full-display sent pixels of
/// the geometry in force now.
///
/// Three cases, and none of them is a recovery path for another. With no `region`
/// it is the whole display, and an image named beside no rectangle has nothing to
/// qualify — it is accepted and unused rather than refused, because a caller that
/// names its image on every request is doing the right thing. With a `region` and
/// no image named, the rectangle is read in a full-display image, which is the
/// space `windows` answers in. With both, it is read in THAT image and mapped
/// through the transform that image was made with — so a caller can zoom into a
/// crop of a crop without ever doing arithmetic of its own.
fn viewing_region(
    request: &wire::Request,
    geom: &Geometry,
    worker: &Worker,
) -> Result<Region, Failure> {
    let Some(rect) = parse_region(&request.body)? else {
        return Ok(Region::full(geom));
    };

    let Some(id) = request.observation_id.as_deref() else {
        return Ok(rect);
    };

    let observation = worker.observations.resolve(id).map_err(refused)?;

    Ok(geometry::rect_through(
        &observation.geometry,
        &observation.region,
        &rect,
        geom,
    ))
}

// --- overlay drawing (M28 B1/B2/B3) ------------------------------------------

/// Marker / ruler / badge drawing on the SENT image, hand-rolled on the raw
/// buffer. Deliberately no imageproc/font dependency: the sidecar ships
/// size-optimized (`opt-level="z"`, lto, strip) and the only text needed is
/// digits plus three symbols, covered by a 5x7 bitmap atlas.
///
/// Everything draws in SENT-image pixel space — the space the model reads and
/// answers in — and every write is bounds-checked, so a marker near an edge
/// clips instead of panicking.
mod overlay {
    use image::RgbaImage;

    const BLACK: [u8; 4] = [0, 0, 0, 255];
    const WHITE: [u8; 4] = [255, 255, 255, 255];
    const RED: [u8; 4] = [230, 40, 40, 255];

    /// 5x7 glyphs, one row per byte (5 low bits, MSB = leftmost pixel).
    const GLYPH_W: i32 = 5;
    const GLYPH_H: i32 = 7;

    fn glyph(c: char) -> Option<[u8; 7]> {
        match c {
            '0' => Some([0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E]),
            '1' => Some([0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E]),
            '2' => Some([0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F]),
            '3' => Some([0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E]),
            '4' => Some([0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02]),
            '5' => Some([0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E]),
            '6' => Some([0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E]),
            '7' => Some([0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08]),
            '8' => Some([0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E]),
            '9' => Some([0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C]),
            '(' => Some([0x02, 0x04, 0x08, 0x08, 0x08, 0x04, 0x02]),
            ')' => Some([0x08, 0x04, 0x02, 0x02, 0x02, 0x04, 0x08]),
            ',' => Some([0x00, 0x00, 0x00, 0x00, 0x0C, 0x04, 0x08]),
            _ => None,
        }
    }

    /// Rendered width of a label in pixels (glyphs + 1px spacing).
    fn text_width(text: &str) -> i32 {
        let n = text.chars().count() as i32;
        if n == 0 {
            0
        } else {
            n * (GLYPH_W + 1) - 1
        }
    }

    fn put(img: &mut RgbaImage, x: i32, y: i32, color: [u8; 4]) {
        if x >= 0 && y >= 0 && (x as u32) < img.width() && (y as u32) < img.height() {
            img.put_pixel(x as u32, y as u32, image::Rgba(color));
        }
    }

    fn fill_rect(img: &mut RgbaImage, x0: i32, y0: i32, w: i32, h: i32, color: [u8; 4]) {
        for y in y0..y0 + h {
            for x in x0..x0 + w {
                put(img, x, y, color);
            }
        }
    }

    /// Black text on a white plate (1px padding) — readable on any background.
    fn draw_label(img: &mut RgbaImage, x0: i32, y0: i32, text: &str) {
        fill_rect(
            img,
            x0 - 1,
            y0 - 1,
            text_width(text) + 2,
            GLYPH_H + 2,
            WHITE,
        );
        let mut x = x0;
        for c in text.chars() {
            if let Some(rows) = glyph(c) {
                for (dy, row) in rows.iter().enumerate() {
                    for dx in 0..GLYPH_W {
                        if row & (0x10 >> dx) != 0 {
                            put(img, x + dx, y0 + dy as i32, BLACK);
                        }
                    }
                }
            }
            x += GLYPH_W + 1;
        }
    }

    /// Keep a `w`-wide element fully inside `0..limit` (labels/plates near edges).
    fn clamp_span(start: i32, w: i32, limit: i32) -> i32 {
        start.min(limit - w).max(0)
    }

    fn ring(img: &mut RgbaImage, cx: i32, cy: i32, r: i32, thickness: i32, color: [u8; 4]) {
        let (r_out, r_in) = (r + thickness, r);
        for dy in -r_out..=r_out {
            for dx in -r_out..=r_out {
                let d2 = dx * dx + dy * dy;
                if d2 <= r_out * r_out && d2 > r_in * r_in {
                    put(img, cx + dx, cy + dy, color);
                }
            }
        }
    }

    /// B1: mark the EXECUTED point in the check image — ring + cross + the
    /// coordinate as text — so the model SEES where its click landed relative to
    /// the target instead of only reading its own number echoed back.
    pub fn executed_point(img: &mut RgbaImage, x: i32, y: i32) {
        ring(img, x, y, 8, 2, WHITE);
        ring(img, x, y, 6, 2, RED);
        for d in 3..=14 {
            for (px, py) in [(x + d, y), (x - d, y), (x, y + d), (x, y - d)] {
                put(img, px, py, WHITE);
            }
        }
        for d in 3..=13 {
            for (px, py) in [(x + d, y), (x - d, y), (x, y + d), (x, y - d)] {
                if d % 2 == 0 {
                    put(img, px, py, BLACK);
                }
            }
        }

        let label = format!("({x},{y})");
        let lx = clamp_span(x + 12, text_width(&label), img.width() as i32);
        let ly = clamp_span(y + 10, GLYPH_H, img.height() as i32);
        draw_label(img, lx, ly, &label);
    }

    /// B2: edge rulers in THIS image's own pixel space — ticks every 100px,
    /// labels every 200px — so the answer grid is visible in the image itself
    /// and a wrong-grid answer stops being label-compatible with what the model
    /// is looking at.
    const TICK_EVERY: i32 = 100;
    const LABEL_EVERY: i32 = 200;
    const TICK_LEN: i32 = 7;

    pub fn rulers(img: &mut RgbaImage) {
        let (w, h) = (img.width() as i32, img.height() as i32);

        let mut x = TICK_EVERY;
        while x < w {
            for y in 0..TICK_LEN {
                put(img, x, y, BLACK);
                put(img, x + 1, y, WHITE);
            }
            if x % LABEL_EVERY == 0 {
                let text = x.to_string();
                draw_label(img, clamp_span(x + 3, text_width(&text), w), 9, &text);
            }
            x += TICK_EVERY;
        }

        let mut y = TICK_EVERY;
        while y < h {
            for x in 0..TICK_LEN {
                put(img, x, y, BLACK);
                put(img, x, y + 1, WHITE);
            }
            if y % LABEL_EVERY == 0 {
                let text = y.to_string();
                draw_label(img, 9, clamp_span(y + 3, GLYPH_H, h), &text);
            }
            y += TICK_EVERY;
        }
    }

    /// B3: a numbered set-of-marks badge at an element's click point. The model
    /// answers with the NUMBER; the caller resolves it to the exact point — no
    /// pixel estimation at all. Badges are only ever drawn from the macOS AX
    /// mark collection, so the fn is scoped with it.
    #[cfg(target_os = "macos")]
    pub fn badge(img: &mut RgbaImage, x: i32, y: i32, n: usize) {
        let text = n.to_string();
        let half_w = (text_width(&text) / 2 + 4).max(8);

        fill_rect(img, x - half_w - 1, y - 7, 2 * half_w + 2, 14, BLACK);
        fill_rect(img, x - half_w, y - 6, 2 * half_w, 12, RED);
        draw_label(img, x - text_width(&text) / 2, y - 3, &text);
    }
}

// --- screenshot -------------------------------------------------------------

fn screenshot(request: &wire::Request, gate: &Gate, worker: &mut Worker) -> Result<Value, Failure> {
    let req = &request.body;
    let display = target_display(req)?;
    capture_payload_encoded(
        &display,
        Requested::Region(request),
        parse_jpeg_quality(req)?,
        parse_overlays(req)?,
        gate,
        worker,
    )
}

/// What a capture is being asked to cover. An action's check image is always the
/// FULL display — the model has just changed something and needs the broader
/// result — while a `screenshot` carries the request whose `region` says which
/// rectangle of it, and whose `observation_id` says which image that rectangle was
/// read in. `wait_for_change` has resolved its rectangle already, because it needs
/// one before it starts polling.
enum Requested<'a> {
    Full,
    Region(&'a wire::Request),
    Exactly(Region),
}

/// Grounding-integrity overlays for one capture (M28), all drawn on the SENT
/// image in its own pixel space: `rulers` (B2) makes the answer grid visible,
/// `annotate_point` (B1) marks an executed click in a check image, `marks` (B3)
/// badges accessibility click points and returns their id table.
#[derive(Default, Clone, Copy)]
struct Overlays {
    rulers: bool,
    marks: bool,
    annotate: Option<Annotate>,
}

/// Where to draw the executed-point marker, in the one space its sender knows it
/// in. A `screenshot` names a point in the image it is asking for. An action's
/// check image names the point the action really executed at, which it knows as a
/// LOGICAL point — the image the model read the coordinate in may be a crop, and
/// the check is always the whole display, so the marker would otherwise be drawn
/// at the crop's numbers on the full screen.
#[derive(Clone, Copy)]
enum Annotate {
    Sent(i32, i32),
    Logical(f32, f32),
}

fn parse_overlays(req: &Value) -> Result<Overlays, String> {
    let rulers = req.get("rulers").and_then(Value::as_bool).unwrap_or(false);
    let marks = req.get("marks").and_then(Value::as_bool).unwrap_or(false);

    let annotate = match req.get("annotate_point") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let x = value
                .get("x")
                .and_then(Value::as_f64)
                .ok_or("annotate_point.x is missing or not a number")?;
            let y = value
                .get("y")
                .and_then(Value::as_f64)
                .ok_or("annotate_point.y is missing or not a number")?;
            Some(Annotate::Sent(x.round() as i32, y.round() as i32))
        }
    };

    Ok(Overlays {
        rulers,
        marks,
        annotate,
    })
}

/// Opt into JPEG for this capture (1-100). Absent = PNG, the lossless default a
/// caller reading fine UI text wants.
fn parse_jpeg_quality(req: &Value) -> Result<Option<u8>, String> {
    match req.get("jpeg_quality") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_u64() {
            Some(q) if (1..=100).contains(&q) => Ok(Some(q as u8)),
            _ => Err("jpeg_quality must be an integer 1-100".to_string()),
        },
    }
}

/// Fail FAST when the target display is asleep instead of engaging a capture.
/// ScreenCaptureKit delivers no frame from a sleeping display until an internal
/// ~30s give-up — long enough to bust a caller's action deadline, and a client
/// stuck in that wait wedges SCK for every later capture system-wide (observed
/// live, 2026-07-01). The typed error lets the caller say what is actually wrong.
#[cfg(target_os = "macos")]
fn ensure_display_awake(display: &Display) -> Result<(), String> {
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        // boolean_t CGDisplayIsAsleep(CGDirectDisplayID display)
        fn CGDisplayIsAsleep(display: u32) -> u32;
    }

    if unsafe { CGDisplayIsAsleep(display.id) } != 0 {
        return Err("display_asleep".to_string());
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn ensure_display_awake(_display: &Display) -> Result<(), String> {
    Ok(())
}

// --- bounded capture (the anti-stall watchdog) -------------------------------

/// Hard budget for one physical frame grab. A real capture takes ~0.2–0.5s; one
/// that exceeds this is stalled inside the OS capture service (the 2026-07-01
/// wedge: CGWindowListCreateImage's ScreenCaptureKit proxy waited out a ~30s XPC
/// semaphore PER CALL), and waiting would burn the caller's whole action
/// deadline. Fail fast with the typed `capture_stalled` instead.
const CAPTURE_STALL_MS: u64 = 5_000;

/// Set when a capture blew its budget: the OS capture backend is wedged and a
/// leaked worker thread is still stuck inside the syscall. There is no in-process
/// recovery (the stuck thread can't be cancelled), so after replying we EXIT and
/// let the owning parent (`Compux.PortDriver`) respawn a fresh sidecar with a
/// clean capture-service connection. Checked in `main` AFTER the response flush,
/// so the `capture_stalled` reply always reaches the caller first.
static CAPTURE_WEDGED: AtomicBool = AtomicBool::new(false);

/// Grab a display's physical frame on a worker thread, bounded by
/// `CAPTURE_STALL_MS`. Bounding (not moving to a different capture API) is the
/// fix: the newest xcap still captures via `CGWindowListCreateImage`, so ANY
/// backend can wedge — only a hard deadline is robust.
///
/// The worker re-resolves the monitor by its stable id (an xcap `Monitor` is not
/// `Send`; the id is a plain `u32`). Re-resolving by ID — not by list index —
/// means a display unplugged mid-action fails typed (`display_disconnected`)
/// instead of silently rebinding to whatever now occupies that index. On a hard
/// stall we set `CAPTURE_WEDGED` (→ process exit + respawn) so a leaked worker
/// can never pile up or wedge the process forever.
fn capture_display_image(monitor_id: u32) -> Result<image::RgbaImage, String> {
    let (tx, rx) = mpsc::channel();

    let spawned = thread::Builder::new()
        .name("compux-capture".to_string())
        .spawn(move || {
            let _ = tx.send(capture_by_id(monitor_id));
        });

    if spawned.is_err() {
        return Err("capture failed: could not spawn capture worker".to_string());
    }

    match rx.recv_timeout(Duration::from_millis(CAPTURE_STALL_MS)) {
        Ok(result) => result,

        Err(mpsc::RecvTimeoutError::Timeout) => {
            // The worker is still stuck in the OS call; it holds a capture-service
            // resource this process can't reclaim. Flag for exit-after-reply.
            CAPTURE_WEDGED.store(true, Ordering::SeqCst);
            Err("capture_stalled".to_string())
        }

        Err(mpsc::RecvTimeoutError::Disconnected) => {
            // Worker died without sending (a panic inside xcap). It leaked no stuck
            // resource, so keep serving — a retry may well succeed.
            Err("capture failed: capture worker exited without a result".to_string())
        }
    }
}

fn capture_by_id(monitor_id: u32) -> Result<image::RgbaImage, String> {
    let monitors = Monitor::all().map_err(|e| format!("enumerate displays: {e}"))?;

    let monitor = monitors
        .into_iter()
        .find(|m| m.id().map(|id| id == monitor_id).unwrap_or(false))
        .ok_or_else(|| "display_disconnected".to_string())?;

    monitor.capture_image().map_err(|e| format!("capture: {e}"))
}

/// Encode the sent image. PNG by default — lossless, which is what a caller reading
/// fine UI text wants. `jpeg_quality` opts into JPEG for a BULK, periodic caller
/// (a continuous screen feed): a full-desktop PNG runs to hundreds of KB, and at a
/// frame every couple of seconds that saturates the consumer's uplink; the same
/// frame as JPEG is roughly an order of magnitude smaller.
///
/// Deliberately NOT paired with a dimension cap. The sent width/height feed
/// `sent_scale`, which is the inverse used to map a click back to the desktop, so
/// shrinking them here would silently move every coordinate. Compression is the one
/// axis that shrinks the payload while leaving the coordinate space identical.
fn encode_image(
    image: &image::RgbaImage,
    w: u32,
    h: u32,
    jpeg_quality: Option<u8>,
) -> Result<(Vec<u8>, &'static str), String> {
    let mut out: Vec<u8> = Vec::new();

    match jpeg_quality {
        Some(quality) => {
            // The JPEG encoder takes no alpha channel; the capture is opaque, so
            // dropping it costs nothing.
            let rgb = image::DynamicImage::ImageRgba8(image.clone()).into_rgb8();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality)
                .write_image(rgb.as_raw(), w, h, image::ExtendedColorType::Rgb8)
                .map_err(|e| format!("encode jpeg: {e}"))?;
            Ok((out, "image/jpeg"))
        }
        None => {
            image::codecs::png::PngEncoder::new(&mut out)
                .write_image(image.as_raw(), w, h, image::ExtendedColorType::Rgba8)
                .map_err(|e| format!("encode png: {e}"))?;
            Ok((out, "image/png"))
        }
    }
}

/// One capture, timed the way the receipt reports it: measured, never estimated.
fn timed_capture(display: &Display, gate: &Gate) -> Result<image::RgbaImage, String> {
    let started = gate.now_ms();
    let image = capture_display_image(display.id)?;
    gate.record(Phase::Capture, gate.now_ms().saturating_sub(started));
    Ok(image)
}

fn capture_payload_encoded(
    display: &Display,
    requested: Requested<'_>,
    jpeg_quality: Option<u8>,
    overlays: Overlays,
    gate: &Gate,
    worker: &mut Worker,
) -> Result<Value, Failure> {
    ensure_display_awake(display)?;

    // The frame comes FIRST, because the transform is built from it: how many
    // pixels the OS answers per point is a fact about the image, not about the
    // display mode. A frame that cannot be explained that way fails the action
    // here, before any of it is handed out as coordinates.
    let image = timed_capture(display, gate)?;
    let geom = measure_geometry(display, &image, worker)?;

    let region = match requested {
        Requested::Full => Region::full(&geom),
        Requested::Exactly(region) => region,
        Requested::Region(request) => viewing_region(request, &geom, worker)?,
    };
    let crop = crop_rect(&geom, &region);
    let (sent_w, sent_h) = crop.sent_dims();

    // Crop to the region's physical rect, then downscale to the sent size. The
    // model's coordinates live in this (sent) space; `to_logical` inverts it.
    let cropped = image::imageops::crop_imm(
        &image,
        crop.left_phys.round() as u32,
        crop.top_phys.round() as u32,
        crop.w_phys.round().max(1.0) as u32,
        crop.h_phys.round().max(1.0) as u32,
    )
    .to_image();

    let mut resized = image::imageops::resize(
        &cropped,
        sent_w,
        sent_h,
        image::imageops::FilterType::Triangle,
    );

    // Overlays draw on the sent image, bottom to top: grid, badges, then the
    // executed-point marker so it is never covered.
    if overlays.rulers {
        overlay::rulers(&mut resized);
    }
    let marks = if overlays.marks {
        Some(collect_marks(&geom, &region, &mut resized, gate))
    } else {
        None
    };
    let annotate = match overlays.annotate {
        None => None,
        Some(Annotate::Sent(x, y)) => Some((x, y)),
        // Placed through THIS image's own transform, so the marker lands where the
        // action landed even when the coordinate was read on a crop.
        Some(Annotate::Logical(lx, ly)) => {
            to_sent(&geom, &region, lx as f64, ly as f64).map(|(x, y)| (x as i32, y as i32))
        }
    };
    if let Some((ax, ay)) = annotate {
        overlay::executed_point(&mut resized, ax, ay);
    }

    let (encoded, mime) = encode_image(&resized, sent_w, sent_h, jpeg_quality)?;
    let data = base64::engine::general_purpose::STANDARD.encode(&encoded);

    let mut payload = json!({
        "ok": true,
        "mime": mime,
        "width": sent_w,
        "height": sent_h,
        "scale": geom.scale_factor,
        "origin": { "x": geom.origin_x.round() as i32, "y": geom.origin_y.round() as i32 },
        "physical": { "width": geom.phys_w, "height": geom.phys_h },
        "region": {
            "x": region.x.round() as i64,
            "y": region.y.round() as i64,
            "w": region.w.round() as i64,
            "h": region.h.round() as i64
        },
        "data": data
    });

    // The cursor's position in this image's coordinates, when it falls inside the
    // captured region — useful for drag/hover reasoning. Absent if off-region.
    if let (Some((cursor_x, cursor_y)), Some(object)) =
        (cursor_point(&geom, &region), payload.as_object_mut())
    {
        object.insert(
            "cursor".to_string(),
            json!({ "x": cursor_x, "y": cursor_y }),
        );
    }

    // B3: the mark table, present (possibly empty) whenever marks were requested,
    // so the caller can tell "zero accessibility marks" from "none asked for".
    if let (Some(info), Some(object)) = (marks, payload.as_object_mut()) {
        object.insert("marks".to_string(), Value::Array(info.entries));
        if let Some(note) = info.ax_activation {
            object.insert("ax_activation".to_string(), json!(note));
        }
        if info.truncated > 0 {
            object.insert("marks_truncated".to_string(), json!(info.truncated));
        }
    }

    // This image is now the space the caller's next coordinates are in, marks
    // included, so it is minted and named before it leaves.
    let observation = worker.observations.mint(
        Kind::Image,
        display.id,
        display.facts,
        geom,
        region,
        (sent_w, sent_h),
    );
    name_observation(&mut payload, &observation);

    Ok(payload)
}

/// Say which image a reply's coordinates are in, the same three fields on every
/// reply that hands any out (plus the frame counter, for the ones that are a
/// picture). `width`, `height`, `region`, `scale`, `origin` and `physical` keep
/// their own meanings beside these.
fn name_observation(payload: &mut Value, observation: &Observation) {
    let Some(object) = payload.as_object_mut() else {
        return;
    };

    object.insert("observation_id".to_string(), json!(observation.id));
    object.insert(
        "observation_kind".to_string(),
        json!(observation.kind.as_str()),
    );
    object.insert(
        "captured_at_monotonic_ns".to_string(),
        json!(observation.captured_at_monotonic_ns),
    );
    if let Some(frame_seq) = observation.frame_seq {
        object.insert("frame_seq".to_string(), json!(frame_seq));
    }
}

/// The badge cap keeps a marked image readable — a dense tree can expose
/// hundreds of interactive nodes, and a badge soup grounds worse than pixels.
/// Tree-walk order is roughly top-down, so the cap drops the least prominent.
#[cfg(target_os = "macos")]
const MAX_MARKS: usize = 60;

struct MarksInfo {
    entries: Vec<Value>,
    ax_activation: Option<String>,
    truncated: usize,
}

#[cfg(target_os = "macos")]
fn collect_marks(
    geom: &Geometry,
    region: &Region,
    img: &mut image::RgbaImage,
    gate: &Gate,
) -> MarksInfo {
    let (nodes, ax_activation) = interactive_in_view(geom, region, gate);
    let truncated = nodes.len().saturating_sub(MAX_MARKS);

    let mut entries = Vec::new();
    for (index, (node, (sx, sy))) in nodes.into_iter().take(MAX_MARKS).enumerate() {
        let id = index + 1;
        overlay::badge(img, sx as i32, sy as i32, id);
        entries.push(json!({ "id": id, "role": node.role, "title": node.title, "x": sx, "y": sy }));
    }

    MarksInfo {
        entries,
        ax_activation,
        truncated,
    }
}

#[cfg(not(target_os = "macos"))]
fn collect_marks(
    _geom: &Geometry,
    _region: &Region,
    _img: &mut image::RgbaImage,
    _gate: &Gate,
) -> MarksInfo {
    MarksInfo {
        entries: Vec::new(),
        ax_activation: Some("marks are only supported on macOS".to_string()),
        truncated: 0,
    }
}

// --- input ------------------------------------------------------------------

#[derive(Deserialize)]
struct Point {
    x: f64,
    y: f64,
}

fn enigo() -> Result<Enigo, String> {
    // Never let library INIT raise the macOS Accessibility dialog mid-action:
    // prompting is an operator flow that belongs exclusively to the
    // `request_permissions` action (the setup card's button). Without the grant,
    // actions fail/no-op and the consumer's probe reports the state loudly.
    let settings = Settings {
        open_prompt_to_get_permissions: false,
        ..Settings::default()
    };

    Enigo::new(&settings).map_err(|e| format!("init input: {e}"))
}

// Best-effort cursor position in sent-image coords (None if input can't be read or
// the cursor lies outside the region) — a screenshot never fails on the cursor read.
fn cursor_point(geom: &Geometry, region: &Region) -> Option<(i64, i64)> {
    let input = enigo().ok()?;
    let (lx, ly) = input.location().ok()?;
    to_sent(geom, region, lx as f64, ly as f64)
}

fn coords(req: &Value) -> Result<(f64, f64), String> {
    let x = req.get("x").and_then(Value::as_f64).ok_or("missing x")?;
    let y = req.get("y").and_then(Value::as_f64).ok_or("missing y")?;
    Ok((x, y))
}

fn modifiers(req: &Value) -> Vec<Key> {
    req.get("modifiers")
        .and_then(Value::as_array)
        .map(|m| {
            m.iter()
                .filter_map(|v| v.as_str().and_then(modifier_key))
                .collect()
        })
        .unwrap_or_default()
}

fn mouse_move(request: &wire::Request, gate: &Gate, worker: &mut Worker) -> Result<Value, Failure> {
    let req = &request.body;
    let (x, y) = coords(req)?;
    let observation = observed(request, worker)?;
    let (lx, ly) = point_in(&observation, x, y)?;
    same_display(&observation, req)?;
    // Read-only on the wire and still the human's pointer, so it goes through the
    // gate like every other input and a pause stops it.
    let mut platform = Gated::new(gate, held::Real::default());
    platform.move_mouse(lx, ly)?;
    // Same settle as the acting verbs: this action's entire promise is "the pointer
    // is now here", and a posted move alone leaves that pending (hover would land on
    // whatever the pointer had not left yet).
    platform.settle(lx, ly)?;
    gate.input_complete();
    // read-only: no post-action screenshot
    Ok(json!({ "ok": true }))
}

fn click(
    request: &wire::Request,
    gate: &Gate,
    worker: &mut Worker,
    button: Button,
    count: u32,
) -> Result<Value, Failure> {
    let req = &request.body;
    let (x, y) = coords(req)?;
    let observation = observed(request, worker)?;
    let (lx, ly) = point_in(&observation, x, y)?;
    let display = same_display(&observation, req)?;
    let mods = modifiers(req);

    // A local, not a temporary: enigo's own `Drop` paces the events it posted, and
    // it ran after the check image before. Inlining this into the call below would
    // move that pacing sleep in front of the screenshot.
    let mut platform = Gated::new(gate, held::Real::default());
    click_seq(&mut platform, lx, ly, button, count, &mods)?;
    gate.input_complete();

    post(req, &display, gate, worker, Some((lx, ly)))
}

/// The click itself, over the injected platform: warp, settle, hold the modifiers,
/// post the button. The guard releases the modifiers after the last click AND on
/// every failure in between — a `?` out of the repeat loop used to return with them
/// still down.
fn click_seq<P: held::Platform>(
    platform: &mut P,
    lx: i32,
    ly: i32,
    button: Button,
    count: u32,
    mods: &[Key],
) -> Result<(), String> {
    held::guarded(platform, |input| {
        input.move_mouse(lx, ly)?;
        input.settle(lx, ly)?;
        input.press_keys(mods)?;
        for _ in 0..count {
            input.click_button(button)?;
        }
        Ok(())
    })
}

/// Drag pacing. A zero-dwell teleport drag lands inside one render frame, which
/// rAF-gated drag handlers (chessground boards, HTML5 drag-and-drop, sliders,
/// maps) never observe as a drag at all — they demote it to a click. Every
/// mature driver interpolates with dwell (Playwright `mouse.move(steps)`,
/// pyautogui `dragTo(duration)`); these are that, as internal constants.
const DRAG_STEPS: u32 = 10;
const DRAG_STEP_MS: u64 = 20;
const DRAG_GRAB_MS: u64 = 60;
const DRAG_DROP_MS: u64 = 50;

fn drag(request: &wire::Request, gate: &Gate, worker: &mut Worker) -> Result<Value, Failure> {
    let req = &request.body;
    let from: Point = parse_point(req, "from")?;
    let to: Point = parse_point(req, "to")?;
    // One image for both points: a drag whose ends were read off different
    // screenshots is a drag across two coordinate spaces.
    let observation = observed(request, worker)?;
    let (fx, fy) = point_in(&observation, from.x, from.y)?;
    let (tx, ty) = point_in(&observation, to.x, to.y)?;
    let display = same_display(&observation, req)?;

    // A local for the same reason as `click`: enigo's pacing runs on its drop.
    let mut platform = Gated::new(gate, held::Real::default());
    drag_seq(&mut platform, fx, fy, tx, ty)?;
    gate.input_complete();

    // The drag DESTINATION is what the check image marks: that is where the action
    // ended and the place the model has to judge.
    post(req, &display, gate, worker, Some((tx, ty)))
}

/// The drag itself, over the injected platform. Everything from the press to the
/// drop runs under the guard, so a failed step, a failed settle or a panic ends
/// with the left button UP — it used to end with the desktop still dragging.
fn drag_seq<P: held::Platform>(
    platform: &mut P,
    fx: i32,
    fy: i32,
    tx: i32,
    ty: i32,
) -> Result<(), String> {
    held::guarded(platform, |input| {
        input.move_mouse(fx, fy)?;
        input.settle(fx, fy)?;
        input.press_button(Button::Left)?;
        // Let the press register (and the target arm its drag) before moving.
        input.sleep(DRAG_GRAB_MS)?;
        drag_through(input, &drag_path(fx, fy, tx, ty, DRAG_STEPS))?;
        input.settle(tx, ty)?;
        // Dwell at the destination so the drop is observed where it happens.
        input.sleep(DRAG_DROP_MS)?;
        Ok(())
    })
}

/// The interpolated pointer path from start to end: `steps` evenly spaced
/// points, endpoints exact (the last point IS the destination), each axis
/// monotonic. Pure, so the geometry is unit-testable without posting events.
fn drag_path(fx: i32, fy: i32, tx: i32, ty: i32, steps: u32) -> Vec<(i32, i32)> {
    (1..=steps)
        .map(|i| {
            let t = i as f32 / steps as f32;
            (
                (fx as f32 + (tx - fx) as f32 * t).round() as i32,
                (fy as f32 + (ty - fy) as f32 * t).round() as i32,
            )
        })
        .collect()
}

/// Walk the interpolated path with a dwell at each point. What ONE step is differs
/// per platform (`held::Real::drag_step` holds that split); the pacing does not.
fn drag_through<P: held::Platform>(
    input: &mut held::Guard<'_, P>,
    path: &[(i32, i32)],
) -> Result<(), String> {
    for &(x, y) in path {
        input.drag_step(x, y)?;
        input.sleep(DRAG_STEP_MS)?;
    }

    Ok(())
}

fn scroll(request: &wire::Request, gate: &Gate, worker: &mut Worker) -> Result<Value, Failure> {
    let req = &request.body;
    let (x, y) = coords(req)?;
    let observation = observed(request, worker)?;
    let (lx, ly) = point_in(&observation, x, y)?;
    let display = same_display(&observation, req)?;
    let amount = req.get("amount").and_then(Value::as_i64).unwrap_or(3) as i32;
    let (axis, length) = match req.get("direction").and_then(Value::as_str) {
        Some("up") => (Axis::Vertical, -amount),
        Some("down") => (Axis::Vertical, amount),
        Some("left") => (Axis::Horizontal, -amount),
        Some("right") => (Axis::Horizontal, amount),
        other => return Err(format!("bad scroll direction: {other:?}").into()),
    };

    // One call with the repeat count inside it, so there is no loop of ours to
    // check: it is admitted at the gate and is not interruptible after that.
    let mut platform = Gated::new(gate, held::Real::default());
    platform.move_mouse(lx, ly)?;
    platform.settle(lx, ly)?;
    platform.scroll(length, axis)?;
    gate.input_complete();

    post(req, &display, gate, worker, Some((lx, ly)))
}

fn type_text(req: &Value, gate: &Gate, worker: &mut Worker) -> Result<Value, Failure> {
    let text = req
        .get("text")
        .and_then(Value::as_str)
        .ok_or("missing text")?;
    // A single `text` call with no loop of ours, so it cannot honour a 25 ms
    // checkpoint and this slice does not pretend it can: it is checked at the gate
    // before dispatch and not inside. Chunking the string would change typing
    // timing in ways only a live check could qualify.
    let mut platform = Gated::new(gate, held::Real::default());
    platform.text(text)?;
    gate.input_complete();
    // Typing executes at the focus, not at a coordinate: nothing to mark.
    post(req, &target_display(req)?, gate, worker, None)
}

fn key_chord(req: &Value, gate: &Gate, worker: &mut Worker) -> Result<Value, Failure> {
    let chord = req
        .get("chord")
        .and_then(Value::as_str)
        .ok_or("missing chord")?;
    let parts: Vec<&str> = chord.split('+').map(str::trim).collect();
    let (mod_parts, key_part) = parts.split_at(parts.len().saturating_sub(1));
    let key_name = key_part.first().copied().ok_or("empty chord")?;

    let mods: Vec<Key> = mod_parts.iter().filter_map(|m| modifier_key(m)).collect();
    let main = named_key(key_name).ok_or_else(|| format!("unknown key: {key_name}"))?;

    // A local for the same reason as `click`: enigo's pacing runs on its drop.
    let mut platform = Gated::new(gate, held::Real::default());
    key_chord_seq(&mut platform, &mods, main)?;
    gate.input_complete();

    post(req, &target_display(req)?, gate, worker, None)
}

/// The chord itself, over the injected platform. This was the one sequence that
/// already released its modifiers after a failed key — but `hold`'s own loop could
/// still fail part way through PRESSING them and strand the earlier ones, which the
/// guard's record-before-post closes.
fn key_chord_seq<P: held::Platform>(
    platform: &mut P,
    mods: &[Key],
    main: Key,
) -> Result<(), String> {
    held::guarded(platform, |input| {
        input.press_keys(mods)?;
        input.click_key(main)
    })
}

fn wait(req: &Value, gate: &Gate) -> Result<Value, Failure> {
    // Clamped like every other blocking verb: the wire is one-request-one-response,
    // so an unbounded sleep would hold the whole session hostage to one argument.
    let ms = req
        .get("ms")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(MAX_WAIT_FOR_IDLE_MS);
    // Sliced at the checkpoint cadence, so a pause during a long wait returns at
    // once instead of running the argument out.
    gate.sleep(ms)
        .map_err(|refusal| refusal.code().to_string())?;
    Ok(json!({ "ok": true }))
}

// --- v2 actions: wait_for_change / paste / elements -------------------------

/// Block until the region's pixels change (or `timeout_ms` elapses), then return
/// the resulting screenshot plus a `changed` flag. Each poll captures the frame
/// (xcap has no sub-region capture) and diffs an AVERAGED thumbnail hash of the
/// region; the poll budget is bounded by the caller's protocol.
fn wait_for_change(
    request: &wire::Request,
    gate: &Gate,
    worker: &mut Worker,
) -> Result<Value, Failure> {
    let req = &request.body;
    let display = target_display(req)?;
    // The rectangle is resolved once, before the first baseline, so every poll and
    // the frame that ends the wait all watch the same patch of screen.
    let geom = measured_geometry(&display, gate, worker)?;
    let region = viewing_region(request, &geom, worker)?;
    let timeout_ms = req
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(10_000)
        .min(MAX_WAIT_FOR_IDLE_MS);
    let poll_ms = req
        .get("poll_ms")
        .and_then(Value::as_u64)
        .unwrap_or(250)
        .max(1);

    let baseline = region_hash(&display, &geom, &region)?;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);

    loop {
        gate.sleep(poll_ms)
            .map_err(|refusal| refusal.code().to_string())?;
        let changed = region_hash(&display, &geom, &region)? != baseline;
        if changed || Instant::now() >= deadline {
            // The returned frame becomes the caller's coordinate view, so it
            // carries the same `rulers` grid an explicit screenshot would.
            let overlays = Overlays {
                rulers: req.get("rulers").and_then(Value::as_bool).unwrap_or(false),
                marks: false,
                annotate: None,
            };
            let mut payload = capture_payload_encoded(
                &display,
                Requested::Exactly(region),
                None,
                overlays,
                gate,
                worker,
            )?;
            if let Some(object) = payload.as_object_mut() {
                object.insert("changed".to_string(), json!(changed));
            }
            return Ok(payload);
        }
    }
}

/// A change-detector: capture the region and hash an AVERAGED thumbnail of it.
/// Triangle (not Nearest) folds every source pixel into a cell, so a small change
/// still perturbs the hash instead of landing between sample points and being
/// missed — a miss would block `wait_for_change` to its full timeout.
fn region_hash(display: &Display, geom: &Geometry, region: &Region) -> Result<u64, String> {
    ensure_display_awake(display)?;
    let crop = crop_rect(geom, region);
    let image = capture_display_image(display.id)?;
    let cropped = image::imageops::crop_imm(
        &image,
        crop.left_phys.round() as u32,
        crop.top_phys.round() as u32,
        crop.w_phys.round().max(1.0) as u32,
        crop.h_phys.round().max(1.0) as u32,
    )
    .to_image();
    let thumb = image::imageops::resize(&cropped, 256, 256, image::imageops::FilterType::Triangle);
    let mut hasher = DefaultHasher::new();
    thumb.as_raw().hash(&mut hasher);
    Ok(hasher.finish())
}

// --- idle detection (coexistence: yield the seat to a present human) --------

/// Upper bound on ANY blocking verb (`wait`, `wait_for_change`, `wait_for_idle`),
/// so one argument can never hang the sidecar past its per-action deadline (30s
/// in Fermix). Cross-platform since the `wait`/`wait_for_change` clamps adopted
/// it — no longer macOS-gated.
const MAX_WAIT_FOR_IDLE_MS: u64 = 25_000;

/// Milliseconds since the last input event the OS saw, via CoreGraphics' session
/// idle clock. NOTE: this counts ANY input, INCLUDING this sidecar's own synthetic
/// events (`enigo` posts through the HID tap) — so a caller that also drives input
/// must disambiguate "the human vs my own last action" itself (that is policy, and
/// lives in the consumer; compux only reports the raw number). macOS only; mirrors
/// the `CGDisplayIsAsleep` FFI shape.
#[cfg(target_os = "macos")]
fn human_idle_ms() -> Result<u64, String> {
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        // CFTimeInterval CGEventSourceSecondsSinceLastEventType(
        //     CGEventSourceStateID stateID, CGEventType eventType)
        fn CGEventSourceSecondsSinceLastEventType(state_id: u32, event_type: u32) -> f64;
    }

    // kCGEventSourceStateHIDSystemState = 1; kCGAnyInputEventType = 0xFFFF_FFFF.
    const HID_SYSTEM_STATE: u32 = 1;
    const ANY_INPUT_EVENT: u32 = 0xFFFF_FFFF;

    let seconds =
        unsafe { CGEventSourceSecondsSinceLastEventType(HID_SYSTEM_STATE, ANY_INPUT_EVENT) };

    if seconds.is_finite() && seconds >= 0.0 {
        Ok((seconds * 1000.0).round() as u64)
    } else {
        Err("idle query returned an invalid interval".to_string())
    }
}

/// Report ms since the last input event. Operational (a policy-support probe), NOT a
/// model action — excluded from `hello`'s advertised verbs like `probe`.
#[cfg(target_os = "macos")]
fn idle_ms() -> Result<Value, Failure> {
    Ok(json!({ "ok": true, "idle_ms": human_idle_ms()? }))
}

#[cfg(not(target_os = "macos"))]
fn idle_ms() -> Result<Value, Failure> {
    Err("idle detection is only supported on macOS".into())
}

/// Block until the human has been idle for `idle_ms` (default 1000), bounded by
/// `timeout_ms` (default 3000, capped at `MAX_WAIT_FOR_IDLE_MS`). Returns
/// `idle: true` if the quiet window was reached, `idle: false` if it timed out with
/// the human still active. Reuses the `wait_for_change` bounded-poll idiom so a
/// consumer can schedule input into a human-idle gap.
#[cfg(target_os = "macos")]
fn wait_for_idle(req: &Value, gate: &Gate) -> Result<Value, Failure> {
    let idle_target = req.get("idle_ms").and_then(Value::as_u64).unwrap_or(1_000);
    let timeout_ms = req
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(3_000)
        .min(MAX_WAIT_FOR_IDLE_MS);
    let poll_ms = req
        .get("poll_ms")
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .max(1);
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);

    loop {
        let idle = human_idle_ms()?;
        if idle >= idle_target {
            return Ok(json!({ "ok": true, "idle": true, "idle_ms": idle }));
        }
        if Instant::now() >= deadline {
            return Ok(json!({ "ok": true, "idle": false, "idle_ms": idle }));
        }
        gate.sleep(poll_ms)
            .map_err(|refusal| refusal.code().to_string())?;
    }
}

#[cfg(not(target_os = "macos"))]
fn wait_for_idle(_req: &Value, _gate: &Gate) -> Result<Value, Failure> {
    Err("idle detection is only supported on macOS".into())
}

/// Paste `text` via the clipboard + the platform paste chord — fast and
/// unicode-safe for long strings that char-by-char typing would stall on.
fn paste(req: &Value, gate: &Gate, worker: &mut Worker) -> Result<Value, Failure> {
    let text = req
        .get("text")
        .and_then(Value::as_str)
        .ok_or("missing text")?;

    // A local for the same reason as `click`: enigo's pacing runs on its drop, and
    // the clipboard handle lived this long too.
    let mut platform = Gated::new(gate, held::Real::default());
    paste_seq(&mut platform, text)?;
    gate.input_complete();

    post(req, &target_display(req)?, gate, worker, None)
}

/// Let the pasteboard write settle before the paste keystroke.
const PASTE_SETTLE_MS: u64 = 50;

/// The paste itself, over the injected platform. `take_clipboard` saves the user's
/// text and arms its restore in one call, and the guard runs that restore — and
/// lifts the modifier — on every exit. A failure around the keystroke used to
/// return with the modifier down and the user's clipboard still overwritten.
fn paste_seq<P: held::Platform>(platform: &mut P, text: &str) -> Result<(), String> {
    held::guarded(platform, |input| {
        input.take_clipboard(text)?;
        input.sleep(PASTE_SETTLE_MS)?;
        input.press_keys(&[paste_modifier()])?;
        input.click_key(Key::Unicode('v'))
    })
}

#[cfg(target_os = "macos")]
fn paste_modifier() -> Key {
    Key::Meta
}

#[cfg(not(target_os = "macos"))]
fn paste_modifier() -> Key {
    Key::Control
}

// --- windows ----------------------------------------------------------------

/// Hard cap on the reported window list. A desktop can carry a hundred windows
/// (helpers, panels, offscreen shells); an unbounded list would bloat the reply and
/// bury the two or three that matter. Front-most first, so the cap drops the least
/// relevant.
const MAX_WINDOWS: usize = 40;

/// Enumerate the on-screen windows of a display, each with its bounds ALREADY
/// EXPRESSED AS A `region` in that display's screenshot space.
///
/// This is the precision lever on a large or ultrawide display. A full-screen
/// capture is downscaled to fit the sent budget, so the app the caller cares about
/// arrives at a fraction of its real size; a `region` crop is rescaled to that same
/// budget on its own, so cropping to one window recovers the lost resolution
/// (up to native 1:1 — on a 3840x1080 display, ~1.7x over the full view for a
/// typical browser window).
///
/// Returning a ready-made `region` — rather than raw window geometry — is the whole
/// point: the caller pastes it straight into `screenshot` and rides the EXISTING,
/// proven region transform. No second coordinate system is introduced, so this
/// cannot reintroduce the region-offset class of bug.
///
/// READ-ONLY: pure metadata and no input — but it hands out coordinates, so it
/// mints an observation, and on a display nothing has captured yet that costs one
/// frame to measure the transform those coordinates are in.
fn windows(req: &Value, gate: &Gate, worker: &mut Worker) -> Result<Value, Failure> {
    let display = target_display(req)?;
    let geom = measured_geometry(&display, gate, worker)?;
    let full = Region::full(&geom);
    let mut listed = window_entries(&geom, &full)?;
    listed.truncate(MAX_WINDOWS);

    let mut payload = json!({ "ok": true, "windows": listed });
    let observation = worker.observations.mint(
        Kind::Semantic,
        display.id,
        display.facts,
        geom,
        full,
        crop_rect(&geom, &full).sent_dims(),
    );
    name_observation(&mut payload, &observation);

    Ok(payload)
}

fn window_entries(geom: &Geometry, full: &Region) -> Result<Vec<Value>, String> {
    let mut windows = xcap::Window::all().map_err(|e| format!("enumerate windows: {e}"))?;

    // Front-most first: xcap's `z` grows toward the front, so the caller reads the
    // window it most likely means at the top of the list.
    windows.sort_by_cached_key(|w| std::cmp::Reverse(w.z().unwrap_or(0)));

    // Shell-layer windows (the Dock, the menu bar, floating overlays) are not
    // windows a caller can work IN — listing them invites zooming into a
    // phantom region (observed live: "Dock — region {0,0,1931,543}").
    let normal = shell_window_filter()?;

    let mut entries = Vec::new();
    for window in windows {
        if window.is_minimized().unwrap_or(false) {
            continue;
        }
        if let (Some(normal), Ok(id)) = (&normal, window.id()) {
            if !normal.contains(&id) {
                continue;
            }
        }
        if let Some(region) = window_region(geom, full, &window) {
            entries.push(json!({
                "id": window.id().unwrap_or_default(),
                "app": window.app_name().unwrap_or_default(),
                "title": window.title().unwrap_or_default(),
                "focused": window.is_focused().unwrap_or(false),
                "region": {
                    "x": region.x.round() as i64,
                    "y": region.y.round() as i64,
                    "w": region.w.round() as i64,
                    "h": region.h.round() as i64
                }
            }));
        }
    }
    Ok(entries)
}

/// A window's LOGICAL bounds (`kCGWindowBounds` on macOS — the same unit `Geometry`
/// uses) converted into this display's sent-image space. Thin effectful shell; the
/// arithmetic is `logical_bounds_to_region` so it can be tested without a desktop.
fn window_region(geom: &Geometry, full: &Region, window: &xcap::Window) -> Option<Region> {
    let (x, y) = (window.x().ok()? as f64, window.y().ok()? as f64);
    let (w, h) = (window.width().ok()? as f64, window.height().ok()? as f64);
    logical_bounds_to_region(geom, full, x, y, w, h)
}

/// Convert logical bounds to a sent-space `region`, clipped to the display's own
/// sent image. `None` for a degenerate window, or one that does not overlap this
/// display, so the listing never offers a region that cannot be captured.
fn logical_bounds_to_region(
    geom: &Geometry,
    full: &Region,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) -> Option<Region> {
    if w <= 0.0 || h <= 0.0 {
        return None;
    }

    let k = sent_scale(geom) as f64;
    let left = (x - geom.origin_x as f64) * k;
    let top = (y - geom.origin_y as f64) * k;

    let x0 = left.max(full.x);
    let y0 = top.max(full.y);
    let x1 = (left + w * k).min(full.x + full.w);
    let y1 = (top + h * k).min(full.y + full.h);

    if x1 - x0 < 1.0 || y1 - y0 < 1.0 {
        return None;
    }

    Some(Region {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    })
}

/// Enumerate interactive accessibility elements (role + label + a click point in
/// screenshot coordinates) so the model can target by element, not raw pixels.
///
/// The click points are pixels in an image that is never sent, so this mints a
/// `semantic` observation and names it: the coordinates are in the same space a
/// `screenshot` of that region would be, and the model addresses them the same way.
fn elements(request: &wire::Request, gate: &Gate, worker: &mut Worker) -> Result<Value, Failure> {
    let display = target_display(&request.body)?;
    let geom = measured_geometry(&display, gate, worker)?;
    let region = viewing_region(request, &geom, worker)?;
    let mut payload = elements_for(&geom, &region, gate)?;

    let (sent_w, sent_h) = crop_rect(&geom, &region).sent_dims();
    let observation = worker.observations.mint(
        Kind::Semantic,
        display.id,
        display.facts,
        geom,
        region,
        (sent_w, sent_h),
    );
    name_observation(&mut payload, &observation);

    Ok(payload)
}

#[cfg(target_os = "macos")]
fn elements_for(geom: &Geometry, region: &Region, gate: &Gate) -> Result<Value, Failure> {
    let (nodes, ax_activation) = interactive_in_view(geom, region, gate);

    let items: Vec<Value> = nodes
        .iter()
        .map(|(node, (x, y))| json!({ "role": node.role, "title": node.title, "x": x, "y": y }))
        .collect();

    let mut payload = json!({ "ok": true, "elements": items });
    if let (Some(note), Some(object)) = (ax_activation, payload.as_object_mut()) {
        object.insert("ax_activation".to_string(), json!(note));
    }
    Ok(payload)
}

/// An interactive AX node paired with its click point in sent-image space.
#[cfg(target_os = "macos")]
type ViewNode = (ax::Node, (i64, i64));

/// Interactive AX elements of ONE application: the ones whose centers fall
/// inside this view (as sent-space points) plus the TOTAL interactive count the
/// walk saw before the view filter. The total is what distinguishes "the app's
/// tree is gated/empty" (activation territory) from "the app has elements, just
/// none inside this region" (nothing to activate). Shared by `elements` and the
/// `marks` overlay so the two can never disagree about what is clickable.
#[cfg(target_os = "macos")]
fn in_view_nodes(pid: i32, geom: &Geometry, region: &Region) -> (Vec<ViewNode>, usize) {
    let nodes = ax::interactive_elements_of(pid);
    let total = nodes.len();

    let in_view = nodes
        .into_iter()
        .filter_map(|node| {
            let center_x = node.x + node.w / 2.0;
            let center_y = node.y + node.h / 2.0;
            to_sent(geom, region, center_x, center_y).map(|point| (node, point))
        })
        .collect();

    (in_view, total)
}

/// The application to read accessibility from: the one OWNING the window the
/// caller is looking at, resolved from the window list. Deliberately NOT
/// `AXFocusedApplication`: that query proved flaky from this spawned process
/// (observed live 2026-07-29: "no focused application" three seconds after
/// `windows` listed Chrome focused), and during a voice call it can resolve to
/// the floating voice companion instead of the app on screen — which is how an
/// activation attempt hit a non-Chromium app and died with AXError -25208
/// while Chrome sat frontmost. Known limitation, accepted: an app with no
/// named on-screen window (a menu-bar app with an open popover) cannot be
/// resolved this way — the old query never reached those reliably either, and
/// the typed no-window note says what happened.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, PartialEq)]
struct TargetApp {
    pid: i32,
    app: String,
}

/// The CGWindowIDs of NORMAL-layer windows (kCGWindowLayer == 0) on screen.
/// The Dock, menu bar, Control Center, and floating shell panels live on
/// non-zero layers while reporting large bounds at high z — observed live
/// 2026-07-30: the Dock's full-screen window (layer 20) won target selection
/// for BOTH a full-screen and a browser-window region, so `marks`/`elements`
/// activated accessibility on the Dock and walked an empty tree. Layer 0 is
/// the OS's own definition of "an app window a user works in", so filtering by
/// it kills the class instead of chasing shell apps by name. (Measured here:
/// Dock 20, menu bar 24, Control Center 25, floating companion panels 3.)
#[cfg(target_os = "macos")]
mod window_layer {
    use core_foundation::base::{CFType, CFTypeRef, TCFType};
    use core_foundation::string::CFString;
    use std::collections::HashSet;
    use std::ffi::c_void;

    /// kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements.
    const ON_SCREEN_ONLY: u32 = 1 << 0;
    const EXCLUDE_DESKTOP: u32 = 1 << 4;
    /// kCFNumberSInt64Type.
    const SINT64: isize = 4;

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGWindowListCopyWindowInfo(option: u32, relative_to_window: u32) -> CFTypeRef;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFArrayGetCount(array: CFTypeRef) -> isize;
        fn CFArrayGetValueAtIndex(array: CFTypeRef, index: isize) -> CFTypeRef;
        fn CFDictionaryGetValue(dict: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
        fn CFNumberGetValue(number: CFTypeRef, the_type: isize, out: *mut c_void) -> bool;
    }

    /// `Err` when the OS refuses the listing — the same underlying call the
    /// window enumeration rides, so the two fail together and loudly.
    pub fn normal_window_ids() -> Result<HashSet<u32>, String> {
        unsafe {
            let list_ref = CGWindowListCopyWindowInfo(ON_SCREEN_ONLY | EXCLUDE_DESKTOP, 0);
            if list_ref.is_null() {
                return Err("window layer listing failed".to_string());
            }
            let list = CFType::wrap_under_create_rule(list_ref);
            let layer_key = CFString::new("kCGWindowLayer");
            let number_key = CFString::new("kCGWindowNumber");

            let mut ids = HashSet::new();
            for index in 0..CFArrayGetCount(list.as_CFTypeRef()) {
                let dict = CFArrayGetValueAtIndex(list.as_CFTypeRef(), index);
                if dict.is_null() {
                    continue;
                }
                if let (Some(0), Some(id)) =
                    (read_i64(dict, &layer_key), read_i64(dict, &number_key))
                {
                    ids.insert(id as u32);
                }
            }
            Ok(ids)
        }
    }

    // CFDictionaryGetValue returns a borrowed (Get-rule) ref — no release here.
    unsafe fn read_i64(dict: CFTypeRef, key: &CFString) -> Option<i64> {
        let value = CFDictionaryGetValue(dict, key.as_CFTypeRef());
        if value.is_null() {
            return None;
        }
        let mut out: i64 = 0;
        if CFNumberGetValue(value, SINT64, &mut out as *mut _ as *mut c_void) {
            Some(out)
        } else {
            None
        }
    }
}

/// The normal-layer window-id set on macOS; `None` where layers do not exist
/// (X11 has no shell-overlay class to exclude).
#[cfg(target_os = "macos")]
fn shell_window_filter() -> Result<Option<std::collections::HashSet<u32>>, String> {
    window_layer::normal_window_ids().map(Some)
}

#[cfg(not(target_os = "macos"))]
fn shell_window_filter() -> Result<Option<std::collections::HashSet<u32>>, String> {
    Ok(None)
}

/// The window list as selection candidates, FRONT TO BACK: minimized windows,
/// shell-layer windows (`window_layer` — the Dock/menu-bar/overlay class that
/// reports huge bounds at high z and shadowed real targets), off-display
/// windows, and windows with no readable pid are out. An enumeration failure
/// is a typed error, not an empty list — the two mean different things to the
/// caller.
#[cfg(target_os = "macos")]
fn window_candidates(geom: &Geometry) -> Result<Vec<(Region, TargetApp)>, String> {
    let full = Region::full(geom);
    let mut windows = xcap::Window::all().map_err(|e| format!("window enumeration failed: {e}"))?;
    windows.sort_by_cached_key(|w| std::cmp::Reverse(w.z().unwrap_or(0)));

    let normal = shell_window_filter()?;

    let mut candidates = Vec::new();
    for window in windows {
        if window.is_minimized().unwrap_or(false) {
            continue;
        }
        if let (Some(normal), Ok(id)) = (&normal, window.id()) {
            if !normal.contains(&id) {
                continue;
            }
        }
        let app = window.app_name().unwrap_or_default();
        let Some(bounds) = window_region(geom, &full, &window) else {
            continue;
        };
        let Ok(pid) = window.pid() else {
            continue;
        };
        candidates.push((
            bounds,
            TargetApp {
                pid: pid as i32,
                app,
            },
        ));
    }
    Ok(candidates)
}

/// Substantial-overlap floor: the fraction of the REQUEST region a window must
/// cover to win on stacking order alone. High enough that a small always-on-top
/// panel (the voice companion, ~1-3% of a window region) can never shadow the
/// window being read; low enough that a normal app window over a larger
/// background one clears it easily.
#[cfg(target_os = "macos")]
const AX_TARGET_MIN_OVERLAP: f64 = 0.10;

/// Pick the app the view is ABOUT from front-to-back candidates: the frontmost
/// window with SUBSTANTIAL overlap of the request region wins — stacking order
/// is the tiebreak the screen actually shows, so a maximized background window
/// can never beat the smaller window in front of it. Only when nothing is
/// substantial (a sparse desktop of small windows) does raw maximum overlap
/// decide. Pure, so the selection semantics are unit-tested.
#[cfg(target_os = "macos")]
fn select_target(candidates: Vec<(Region, TargetApp)>, region: &Region) -> Option<TargetApp> {
    let region_area = (region.w * region.h).max(1.0);

    let mut best: Option<(f64, TargetApp)> = None;
    for (bounds, target) in candidates {
        let area = overlap_area(&bounds, region);
        if area <= 0.0 {
            continue;
        }
        if area >= AX_TARGET_MIN_OVERLAP * region_area {
            return Some(target);
        }
        let better = best.as_ref().map(|(b, _)| area > *b).unwrap_or(true);
        if better {
            best = Some((area, target));
        }
    }
    best.map(|(_, target)| target)
}

/// Overlap area of two sent-space rectangles; 0 when disjoint.
#[cfg(target_os = "macos")]
fn overlap_area(a: &Region, b: &Region) -> f64 {
    let w = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
    let h = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
    if w <= 0.0 || h <= 0.0 {
        0.0
    } else {
        w * h
    }
}

/// Bounded settle for a lazily-built accessibility tree: Chromium switches web
/// accessibility on when an AX client starts querying it, then needs a moment
/// to build the tree — so an empty first walk re-queries on a short cadence
/// instead of concluding emptiness from one look.
#[cfg(target_os = "macos")]
const AX_SETTLE_POLL_MS: u64 = 300;
#[cfg(target_os = "macos")]
const AX_SETTLE_POLLS: u32 = 5;

/// B4, revised on live evidence: enumerate the TARGET app's tree rooted at its
/// own application element. Activation (one typed attempt —
/// `AXManualAccessibility`, then `AXEnhancedUserInterface` on an
/// attribute-unsupported / not-implemented answer — current Chrome refuses
/// BOTH yet serves its full tree to a querying client anyway) plus the bounded
/// settle poll fire ONLY when the app's tree walked to zero interactive nodes
/// overall: an app whose elements merely fall outside the view has nothing
/// gated, and flipping enhanced-UI mode on it would be a pure side effect.
/// Every outcome lands in the note — which app was read, what activation did,
/// how long the tree took — so no result is silent about its cause.
#[cfg(target_os = "macos")]
fn interactive_in_view(
    geom: &Geometry,
    region: &Region,
    gate: &Gate,
) -> (Vec<ViewNode>, Option<String>) {
    let candidates = match window_candidates(geom) {
        Ok(candidates) => candidates,
        Err(reason) => return (Vec::new(), Some(reason)),
    };
    let Some(target) = select_target(candidates, region) else {
        return (
            Vec::new(),
            Some("no application window to target for accessibility".to_string()),
        );
    };

    let (found, total) = in_view_nodes(target.pid, geom, region);
    if !found.is_empty() {
        return (found, Some(format!("read {}", target.app)));
    }
    if total > 0 {
        let note = format!(
            "{}: {total} interactive element(s) in the app, none inside this view",
            target.app
        );
        return (Vec::new(), Some(note));
    }

    let attempt = ax::activate_accessibility(target.pid);
    for poll in 1..=AX_SETTLE_POLLS {
        // Up to 1.5 seconds, reached from `elements` AND from any `marks: true`
        // screenshot, so it is a checkpoint site: a pause during a check image
        // stops waiting for a tree the caller no longer wants. Read-only, so it
        // answers with an empty set and a note rather than a cancelled action.
        if gate.sleep(AX_SETTLE_POLL_MS).is_err() {
            let note = format!("{}: cancelled while the tree settled", target.app);
            return (Vec::new(), Some(note));
        }
        let (again, again_total) = in_view_nodes(target.pid, geom, region);
        let waited = u64::from(poll) * AX_SETTLE_POLL_MS;
        if !again.is_empty() {
            let note = format!(
                "{}: tree appeared after {waited}ms ({})",
                target.app,
                attempt_note(&attempt)
            );
            return (again, Some(note));
        }
        if again_total > 0 {
            // The tree came alive; this view just contains none of it — more
            // polling cannot change that.
            let note = format!(
                "{}: tree appeared after {waited}ms ({}), but its {again_total} element(s) \
                 are outside this view",
                target.app,
                attempt_note(&attempt)
            );
            return (Vec::new(), Some(note));
        }
    }

    let waited = u64::from(AX_SETTLE_POLLS) * AX_SETTLE_POLL_MS;
    let note = format!(
        "{}: no accessibility elements — {}; tree still empty after {waited}ms",
        target.app,
        attempt_note(&attempt)
    );
    (Vec::new(), Some(note))
}

#[cfg(target_os = "macos")]
fn attempt_note(attempt: &Result<&'static str, String>) -> String {
    match attempt {
        Ok(attribute) => format!("{attribute} activated"),
        Err(reason) => format!("activation refused: {reason}"),
    }
}

#[cfg(not(target_os = "macos"))]
fn elements_for(_geom: &Geometry, _region: &Region, _gate: &Gate) -> Result<Value, Failure> {
    Err("element enumeration is only supported on macOS".into())
}

// --- accessibility (inspect) ------------------------------------------------

/// Report the accessibility element under a point of the image it names: its role
/// and label. READ-ONLY — a grounding/judgment aid (confirm what control is there
/// before a consequential click), not a gate. It addresses a point exactly as a
/// click does, through the transform its image was made with, so what it reports
/// is what a click at that coordinate would reach.
fn inspect(request: &wire::Request, worker: &Worker) -> Result<Value, Failure> {
    let (x, y) = coords(&request.body)?;
    let observation = observed(request, worker)?;
    let (lx, ly) = point_in(&observation, x, y)?;
    same_display(&observation, &request.body)?;
    Ok(inspect_at(lx as f32, ly as f32)?)
}

#[cfg(target_os = "macos")]
fn inspect_at(x: f32, y: f32) -> Result<Value, String> {
    match ax::element_at(x, y) {
        Some(el) => Ok(json!({
            "ok": true,
            "found": true,
            "role": el.role,
            "title": el.title,
            "description": el.description,
            "value": el.value,
        })),
        None => Ok(json!({ "ok": true, "found": false })),
    }
}

#[cfg(not(target_os = "macos"))]
fn inspect_at(_x: f32, _y: f32) -> Result<Value, String> {
    Err("element inspection is only supported on macOS".to_string())
}

/// macOS Accessibility FFI for `inspect`. Reads the element under a global LOGICAL
/// point via the system-wide AX element; core-foundation owns CFType memory (drop =
/// release), and every AX call's error code is checked before the out-param is read.
/// NON-PROMPTING and read-only. Like the rest of the native driver, the runtime
/// behavior needs a real Mac with the Accessibility grant to verify.
#[cfg(target_os = "macos")]
mod ax {
    use core_foundation::base::{CFType, CFTypeRef, TCFType};
    use core_foundation::boolean::CFBoolean;
    use core_foundation::string::{CFString, CFStringRef};
    use std::ffi::c_void;
    use std::sync::Mutex;

    type AXUIElementRef = CFTypeRef;

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXUIElementCreateSystemWide() -> AXUIElementRef;
        // AXError; 0 == success. On success `element` is set to a +1 (Copy-rule) ref.
        fn AXUIElementCopyElementAtPosition(
            application: AXUIElementRef,
            x: f32,
            y: f32,
            element: *mut AXUIElementRef,
        ) -> i32;
        fn AXUIElementCopyAttributeValue(
            element: AXUIElementRef,
            attribute: CFStringRef,
            value: *mut CFTypeRef,
        ) -> i32;
        // Extract the concrete value (CGPoint/CGSize) an AXValue wraps; false if the
        // requested type doesn't match.
        fn AXValueGetValue(value: CFTypeRef, the_type: u32, out: *mut c_void) -> bool;
        // B4 activation: set an attribute on an application element, created
        // from the pid the caller resolved off the window list.
        fn AXUIElementSetAttributeValue(
            element: AXUIElementRef,
            attribute: CFStringRef,
            value: CFTypeRef,
        ) -> i32;
        fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFArrayGetCount(array: CFTypeRef) -> isize;
        fn CFArrayGetValueAtIndex(array: CFTypeRef, index: isize) -> CFTypeRef;
        fn CFGetTypeID(cf: CFTypeRef) -> usize;
        fn CFArrayGetTypeID() -> usize;
    }

    // The AX attribute-name constants (`kAXRoleAttribute`, …) are header `extern
    // const`s that don't link as symbols; their string VALUES are stable + documented,
    // so we build the CFStrings from those instead.
    const ROLE: &str = "AXRole";
    const TITLE: &str = "AXTitle";
    const DESCRIPTION: &str = "AXDescription";
    const VALUE: &str = "AXValue";
    const CHILDREN: &str = "AXChildren";
    const POSITION: &str = "AXPosition";
    const SIZE: &str = "AXSize";

    // AXValueType tags for AXValueGetValue.
    const AXVALUE_CGPOINT: u32 = 1;
    const AXVALUE_CGSIZE: u32 = 2;

    // Bound the tree walk so a deep/huge hierarchy can't stall the request:
    // MAX_NODES caps elements COLLECTED, MAX_VISITED caps nodes TRAVERSED (a large
    // sparse subtree has few interactive nodes but many to walk), MAX_DEPTH the depth.
    const MAX_DEPTH: usize = 14;
    const MAX_NODES: usize = 250;
    const MAX_VISITED: usize = 3000;

    // Roles worth surfacing as clickable targets (set-of-marks).
    const INTERACTIVE: &[&str] = &[
        "AXButton",
        "AXMenuItem",
        "AXMenuButton",
        "AXPopUpButton",
        "AXCheckBox",
        "AXRadioButton",
        "AXTextField",
        "AXTextArea",
        "AXComboBox",
        "AXLink",
        "AXTabButton",
        "AXSlider",
        "AXDisclosureTriangle",
        "AXCell",
    ];

    #[repr(C)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    #[repr(C)]
    struct CGSize {
        width: f64,
        height: f64,
    }

    pub struct Element {
        pub role: Option<String>,
        pub title: Option<String>,
        pub description: Option<String>,
        pub value: Option<String>,
    }

    pub fn element_at(x: f32, y: f32) -> Option<Element> {
        unsafe {
            let system_ref = AXUIElementCreateSystemWide();
            if system_ref.is_null() {
                return None;
            }
            let system = CFType::wrap_under_create_rule(system_ref);

            let mut element_ref: AXUIElementRef = std::ptr::null();
            let err =
                AXUIElementCopyElementAtPosition(system.as_CFTypeRef(), x, y, &mut element_ref);
            if err != 0 || element_ref.is_null() {
                return None;
            }
            let element = CFType::wrap_under_create_rule(element_ref);

            Some(Element {
                role: copy_string_attr(&element, ROLE),
                title: copy_string_attr(&element, TITLE),
                description: copy_string_attr(&element, DESCRIPTION),
                value: copy_string_attr(&element, VALUE),
            })
        }
    }

    // Read any CFType-valued AX attribute (a +1 Copy-rule ref, released on drop).
    unsafe fn copy_element_attr(element: &CFType, attribute: &str) -> Option<CFType> {
        let attr = CFString::new(attribute);
        let mut value_ref: CFTypeRef = std::ptr::null();
        let err = AXUIElementCopyAttributeValue(
            element.as_CFTypeRef(),
            attr.as_concrete_TypeRef(),
            &mut value_ref,
        );
        if err != 0 || value_ref.is_null() {
            return None;
        }
        Some(CFType::wrap_under_create_rule(value_ref))
    }

    // Read a string-valued AX attribute. Non-string values (e.g. a slider's number)
    // downcast to None — we only surface text labels.
    unsafe fn copy_string_attr(element: &CFType, attribute: &str) -> Option<String> {
        copy_element_attr(element, attribute)?
            .downcast::<CFString>()
            .map(|s| s.to_string())
    }

    /// An interactive element with its global-logical frame.
    pub struct Node {
        pub role: Option<String>,
        pub title: Option<String>,
        pub x: f64,
        pub y: f64,
        pub w: f64,
        pub h: f64,
    }

    // --- accessibility activation (M28 B4) -----------------------------------

    /// Chromium/Electron's opt-in switch: the family builds its AX tree only for
    /// detected assistive clients, and this per-app attribute is the documented
    /// way to request it manually. Current Chrome refuses it (and serves its
    /// tree to a querying client regardless); Electron-family builds honor it.
    pub const MANUAL_ACCESSIBILITY: &str = "AXManualAccessibility";
    /// Second choice ONLY on a typed rejection: it also flips apps into an
    /// enhanced-UI mode window managers react to (layout side effects), which
    /// is why it is never tried first.
    pub const ENHANCED_UI: &str = "AXEnhancedUserInterface";
    /// AXError `kAXErrorAttributeUnsupported`.
    const ATTRIBUTE_UNSUPPORTED: i32 = -25205;
    /// AXError `kAXErrorNotImplemented` — what a process that does not
    /// implement the attribute's setter answers (observed live from Chrome and
    /// from non-Chromium apps).
    const NOT_IMPLEMENTED: i32 = -25208;

    /// The typed rejections that mean "this attribute is not a thing here" —
    /// the deterministic criterion for trying the second attribute.
    pub fn attribute_rejected(code: i32) -> bool {
        code == ATTRIBUTE_UNSUPPORTED || code == NOT_IMPLEMENTED
    }

    /// `(pid, attribute)` pairs this process switched ON — cleared on the exit
    /// paths so one enumeration never leaves an app in an altered AX mode.
    static ACTIVATED: Mutex<Vec<(i32, &'static str)>> = Mutex::new(Vec::new());

    /// Ask ONE application — the pid the caller resolved off the window list —
    /// to expose its accessibility tree. Returns the attribute that activated,
    /// or a typed reason. Setting the attribute on an app that does not gate
    /// its tree is a harmless typed error — callers attempt unconditionally on
    /// an empty enumeration, no app-family sniffing.
    pub fn activate_accessibility(pid: i32) -> Result<&'static str, String> {
        unsafe {
            let app_ref = AXUIElementCreateApplication(pid);
            if app_ref.is_null() {
                return Err(format!("no accessibility connection to pid {pid}"));
            }
            let app = CFType::wrap_under_create_rule(app_ref);

            match set_bool_attr(&app, MANUAL_ACCESSIBILITY, true) {
                0 => {
                    record(pid, MANUAL_ACCESSIBILITY);
                    Ok(MANUAL_ACCESSIBILITY)
                }
                code if attribute_rejected(code) => match set_bool_attr(&app, ENHANCED_UI, true) {
                    0 => {
                        record(pid, ENHANCED_UI);
                        Ok(ENHANCED_UI)
                    }
                    code => Err(format!("AXError {code}")),
                },
                code => Err(format!("AXError {code}")),
            }
        }
    }

    /// Best-effort teardown: switch OFF every attribute this process switched on.
    pub fn clear_activations() {
        let entries: Vec<(i32, &'static str)> = match ACTIVATED.lock() {
            Ok(mut list) => list.drain(..).collect(),
            Err(_poisoned) => return,
        };

        for (pid, attribute) in entries {
            unsafe {
                let app_ref = AXUIElementCreateApplication(pid);
                if app_ref.is_null() {
                    continue;
                }
                let app = CFType::wrap_under_create_rule(app_ref);
                let _ = set_bool_attr(&app, attribute, false);
            }
        }
    }

    /// Record an activation the CAPTURE engine performed. Capture drives its own
    /// set (it reads the prior value first and sequences two attributes), but the
    /// teardown ledger is shared: one list, so the process-exit `clear_activations`
    /// also undoes capture's switches on a path that never reaches its detach (a
    /// panicked observer thread, a failed join).
    pub fn record_activation(pid: i32, attribute: &'static str) {
        record(pid, attribute);
    }

    /// Switch OFF every attribute this process turned on for ONE app (capture's
    /// detach). Returns one message per attribute that could NOT be cleared, so the
    /// caller logs the failure rather than hiding it; an app with nothing recorded
    /// makes no AX call at all.
    pub fn clear_activation(pid: i32) -> Vec<String> {
        let mut failures = Vec::new();
        for attribute in take_recorded(pid) {
            unsafe {
                let app_ref = AXUIElementCreateApplication(pid);
                if app_ref.is_null() {
                    failures.push(format!(
                        "no accessibility connection to pid {pid} to clear {attribute}"
                    ));
                    continue;
                }
                let app = CFType::wrap_under_create_rule(app_ref);
                let code = set_bool_attr(&app, attribute, false);
                if code != 0 {
                    failures.push(format!("could not clear {attribute} (AXError {code})"));
                }
            }
        }
        failures
    }

    /// Remove and return the attributes recorded for one pid, leaving every other
    /// app's records in place.
    fn take_recorded(pid: i32) -> Vec<&'static str> {
        let mut mine = Vec::new();
        if let Ok(mut list) = ACTIVATED.lock() {
            list.retain(|(recorded_pid, attribute)| {
                if *recorded_pid == pid {
                    mine.push(*attribute);
                    return false;
                }
                true
            });
        }
        mine
    }

    fn record(pid: i32, attribute: &'static str) {
        if let Ok(mut list) = ACTIVATED.lock() {
            if !list.contains(&(pid, attribute)) {
                list.push((pid, attribute));
            }
        }
    }

    unsafe fn set_bool_attr(element: &CFType, attribute: &str, value: bool) -> i32 {
        let attr = CFString::new(attribute);
        let flag = if value {
            CFBoolean::true_value()
        } else {
            CFBoolean::false_value()
        };
        AXUIElementSetAttributeValue(
            element.as_CFTypeRef(),
            attr.as_concrete_TypeRef(),
            flag.as_CFTypeRef(),
        )
    }

    /// Walk ONE application's accessibility tree — rooted at its own
    /// application element, never at `AXFocusedApplication` (a query that
    /// proved flaky from this spawned process and, during a voice call, can
    /// name the floating companion instead of the app on screen) — and collect
    /// interactive elements (bounded depth + count) with global-logical frames.
    /// NON-PROMPTING, read-only; runtime behavior needs a real Mac with the
    /// Accessibility grant.
    pub fn interactive_elements_of(pid: i32) -> Vec<Node> {
        unsafe {
            let app_ref = AXUIElementCreateApplication(pid);
            if app_ref.is_null() {
                return Vec::new();
            }
            let root = CFType::wrap_under_create_rule(app_ref);
            let mut out = Vec::new();
            let mut visited = 0;
            walk(&root, 0, &mut visited, &mut out);
            out
        }
    }

    unsafe fn walk(element: &CFType, depth: usize, visited: &mut usize, out: &mut Vec<Node>) {
        if depth > MAX_DEPTH || out.len() >= MAX_NODES || *visited >= MAX_VISITED {
            return;
        }
        *visited += 1;
        if let Some(node) = interactive_node(element) {
            out.push(node);
        }
        for child in copy_children(element) {
            walk(&child, depth + 1, visited, out);
        }
    }

    unsafe fn interactive_node(element: &CFType) -> Option<Node> {
        let role = copy_string_attr(element, ROLE)?;
        if !INTERACTIVE.contains(&role.as_str()) {
            return None;
        }
        let (x, y, w, h) = element_frame(element)?;
        Some(Node {
            title: copy_string_attr(element, TITLE)
                .or_else(|| copy_string_attr(element, DESCRIPTION))
                .or_else(|| copy_string_attr(element, VALUE)),
            role: Some(role),
            x,
            y,
            w,
            h,
        })
    }

    unsafe fn copy_children(element: &CFType) -> Vec<CFType> {
        let Some(children) = copy_element_attr(element, CHILDREN) else {
            return Vec::new();
        };
        let array = children.as_CFTypeRef();
        // AXChildren SHOULD be a CFArray, but an app with a custom/broken AX impl can
        // return another CFType; the CFArray getters would then type-confuse and read
        // garbage. Verify the concrete type before treating it as an array.
        if CFGetTypeID(array) != CFArrayGetTypeID() {
            return Vec::new();
        }
        let count = CFArrayGetCount(array);
        let mut out = Vec::new();
        let mut index = 0;
        while index < count && out.len() < MAX_NODES {
            let child_ref = CFArrayGetValueAtIndex(array, index);
            if !child_ref.is_null() {
                out.push(CFType::wrap_under_get_rule(child_ref));
            }
            index += 1;
        }
        out
    }

    unsafe fn element_frame(element: &CFType) -> Option<(f64, f64, f64, f64)> {
        let position = copy_element_attr(element, POSITION)?;
        let size = copy_element_attr(element, SIZE)?;
        let mut point = CGPoint { x: 0.0, y: 0.0 };
        let mut dims = CGSize {
            width: 0.0,
            height: 0.0,
        };
        let got_point = AXValueGetValue(
            position.as_CFTypeRef(),
            AXVALUE_CGPOINT,
            &mut point as *mut _ as *mut c_void,
        );
        let got_size = AXValueGetValue(
            size.as_CFTypeRef(),
            AXVALUE_CGSIZE,
            &mut dims as *mut _ as *mut c_void,
        );
        if got_point && got_size && dims.width > 0.0 && dims.height > 0.0 {
            Some((point.x, point.y, dims.width, dims.height))
        } else {
            None
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        // The teardown ledger is shared with the capture engine (one list, so a
        // panicked observer thread still has its switches undone at process exit).
        // Clearing ONE app must therefore drain that app's records and nobody else's.
        // Pure bookkeeping only — no AX call is made for an app with no records.
        #[test]
        fn take_recorded_drains_only_the_requested_app() {
            let (app_a, app_b) = (0x7f00_0001, 0x7f00_0002);
            record(app_a, MANUAL_ACCESSIBILITY);
            record(app_a, ENHANCED_UI);
            record(app_b, MANUAL_ACCESSIBILITY);

            assert!(
                take_recorded(0x7f00_0003).is_empty(),
                "an app with nothing recorded yields nothing to clear"
            );

            let mut drained = take_recorded(app_a);
            drained.sort_unstable();
            assert_eq!(drained, vec![ENHANCED_UI, MANUAL_ACCESSIBILITY]);
            assert!(
                take_recorded(app_a).is_empty(),
                "a drained app is not cleared twice"
            );
            assert_eq!(
                take_recorded(app_b),
                vec![MANUAL_ACCESSIBILITY],
                "another app's record survived"
            );
        }
    }
}

// --- helpers ----------------------------------------------------------------

/// After a mutating action, include the post-action screen state when the request
/// asked for it (`screenshot_after`). Always the FULL display (region: None) so the
/// model sees the broader result of a zoomed action; it can re-zoom with an explicit
/// `screenshot` if it needs detail.
///
/// B1: the check draws the EXECUTED point (the action's own coordinates — the drag
/// destination for drags) into the image, so the caller SEES where the click landed
/// relative to its target instead of only reading its own number echoed back. An
/// unregioned action's coordinates are full-screen sent space — the same space this
/// full-display check captures in. `rulers` is honored from the action request.
fn post(
    req: &Value,
    display: &Display,
    gate: &Gate,
    worker: &mut Worker,
    executed: Option<(i32, i32)>,
) -> Result<Value, Failure> {
    if req
        .get("screenshot_after")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let overlays = Overlays {
            rulers: req.get("rulers").and_then(Value::as_bool).unwrap_or(false),
            marks: false,
            annotate: executed.map(|(lx, ly)| Annotate::Logical(lx as f32, ly as f32)),
        };
        capture_payload_encoded(display, Requested::Full, None, overlays, gate, worker)
    } else {
        Ok(json!({ "ok": true }))
    }
}

fn parse_point(req: &Value, field: &str) -> Result<Point, String> {
    serde_json::from_value(req.get(field).cloned().unwrap_or(Value::Null))
        .map_err(|_| format!("bad {field} point"))
}

fn modifier_key(name: &str) -> Option<Key> {
    match name {
        "cmd" | "meta" | "super" => Some(Key::Meta),
        "ctrl" | "control" => Some(Key::Control),
        "alt" | "option" => Some(Key::Alt),
        "shift" => Some(Key::Shift),
        _ => None,
    }
}

/// Map a chord key token to an enigo Key. Single printable chars become a
/// Unicode key; common named keys are mapped explicitly. Extend as needed.
fn named_key(name: &str) -> Option<Key> {
    let lower = name.to_lowercase();
    match lower.as_str() {
        "enter" | "return" => Some(Key::Return),
        "tab" => Some(Key::Tab),
        "esc" | "escape" => Some(Key::Escape),
        "space" => Some(Key::Space),
        "backspace" => Some(Key::Backspace),
        "delete" | "del" => Some(Key::Delete),
        "up" => Some(Key::UpArrow),
        "down" => Some(Key::DownArrow),
        "left" => Some(Key::LeftArrow),
        "right" => Some(Key::RightArrow),
        "home" => Some(Key::Home),
        "end" => Some(Key::End),
        "pageup" => Some(Key::PageUp),
        "pagedown" => Some(Key::PageDown),
        "f1" => Some(Key::F1),
        "f2" => Some(Key::F2),
        "f3" => Some(Key::F3),
        "f4" => Some(Key::F4),
        "f5" => Some(Key::F5),
        "f6" => Some(Key::F6),
        "f7" => Some(Key::F7),
        "f8" => Some(Key::F8),
        "f9" => Some(Key::F9),
        "f10" => Some(Key::F10),
        "f11" => Some(Key::F11),
        "f12" => Some(Key::F12),
        _ => {
            let mut chars = name.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Some(Key::Unicode(c)),
                _ => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::held::{sweep_injected_failures, Call, Recorder, CLIPBOARD_RESTORE_DWELL_MS};
    use enigo::Direction;

    /// A gate with nothing in flight, for a test that drives one function directly.
    fn idle_gate() -> Gate {
        Gate::new("boot-test".to_string(), Arc::new(SystemClock::new()))
    }

    /// A failed action as its payload, so a dispatch test can assert on one shape
    /// whether the host had a display or not.
    fn err_payload(failure: Failure) -> Value {
        json!({ "ok": false, "error": failure.code })
    }

    /// A worker with an empty table, for a test that drives one action directly.
    fn idle_worker(gate: &Gate) -> Worker {
        Worker {
            observations: Observations::new(&gate.envelope().sidecar_generation, gate.clock()),
            measured: Measured::new(),
        }
    }

    /// Run one action through the real dispatch table, as the worker does.
    fn dispatch(body: Value) -> Result<Value, Failure> {
        let gate = Gate::new("boot-test".to_string(), Arc::new(SystemClock::new()));
        let mut worker = idle_worker(&gate);
        let request = request_for(&body);

        handle(&request, &gate, &capture::Emitter::new(), &mut worker)
    }

    /// The frame the reader would have parsed for this body, addressing included —
    /// so a dispatch test exercises the same shape a real request arrives in.
    fn request_for(body: &Value) -> wire::Request {
        wire::Request {
            request_id: "r1".to_string(),
            action: body["action"].as_str().unwrap_or_default().to_string(),
            body: body.clone(),
            sidecar_generation: None,
            session_generation: None,
            authorization_generation: None,
            mutation_seq: None,
            observation_id: body["observation_id"].as_str().map(str::to_string),
        }
    }

    // The drag path's geometry is what makes an interpolated drag land exactly:
    // the LAST point must BE the destination (a rounded near-miss would drop the
    // piece a pixel off), the step count fixed, and each axis monotonic so the
    // pointer never doubles back mid-drag.
    #[test]
    fn drag_path_ends_exactly_at_the_destination() {
        let path = drag_path(10, 20, 313, 207, DRAG_STEPS);
        assert_eq!(path.len(), DRAG_STEPS as usize);
        assert_eq!(*path.last().unwrap(), (313, 207));
    }

    #[test]
    fn drag_path_is_monotonic_on_both_axes() {
        let path = drag_path(300, 400, 20, 40, DRAG_STEPS);
        let mut prev = (300, 400);
        for &(x, y) in &path {
            assert!(x <= prev.0, "x doubled back: {x} after {}", prev.0);
            assert!(y <= prev.1, "y doubled back: {y} after {}", prev.1);
            prev = (x, y);
        }
        assert_eq!(prev, (20, 40));
    }

    #[test]
    fn drag_path_handles_a_zero_length_drag() {
        let path = drag_path(50, 60, 50, 60, DRAG_STEPS);
        assert!(path.iter().all(|&p| p == (50, 60)));
    }

    // An empty monitor list (locked / asleep / no GUI session) maps to the typed
    // `no_active_display` for ANY requested index — never the bad-index message,
    // which would wrongly suggest another monitor could work. The non-empty path
    // needs a real `Monitor` (an OS handle) and is covered by on-device runs.
    #[test]
    fn empty_monitor_list_is_no_active_display_for_any_index() {
        assert_eq!(
            select_monitor(Vec::new(), 0).err(),
            Some("no_active_display".to_string())
        );
        assert_eq!(
            select_monitor(Vec::new(), 4).err(),
            Some("no_active_display".to_string())
        );
    }

    /// The display the window listing exists for: 3840x1080 at 1x, where a full
    /// capture is squeezed to 1366x384 and a window crop wins most of it back.
    fn ultrawide_geom() -> Geometry {
        Geometry {
            phys_w: 3840,
            phys_h: 1080,
            logical_w: 3840.0,
            logical_h: 1080.0,
            origin_x: 0.0,
            origin_y: 0.0,
            scale_factor: 1.0,
        }
    }

    #[test]
    fn window_bounds_become_a_region_in_sent_space() {
        let g = ultrawide_geom();
        let full = Region::full(&g);
        // A 1600x1000 window at logical (400,60): k = 1366/3840 = 0.3557.
        let r =
            logical_bounds_to_region(&g, &full, 400.0, 60.0, 1600.0, 1000.0).expect("on screen");

        let k = sent_scale(&g) as f64;
        assert!((r.x - 400.0 * k).abs() < 1.0, "x: {}", r.x);
        assert!((r.y - 60.0 * k).abs() < 1.0, "y: {}", r.y);
        assert!((r.w - 1600.0 * k).abs() < 1.0, "w: {}", r.w);
        assert!((r.h - 1000.0 * k).abs() < 1.0, "h: {}", r.h);
    }

    /// The whole point of the action: cropping to the window recovers resolution a
    /// full capture spends on empty desktop. The margin is 1.5x (not the old 2x):
    /// the area budget already recovered part of the gap for the FULL view on this
    /// display (0.356 → 0.503), which narrows the crop's relative win.
    #[test]
    fn a_window_crop_is_sharper_than_the_full_screen() {
        let g = ultrawide_geom();
        let full = Region::full(&g);
        let r =
            logical_bounds_to_region(&g, &full, 400.0, 60.0, 1600.0, 1000.0).expect("on screen");

        let full_k = sent_scale(&g) as f64;
        let crop = crop_rect(&g, &r);
        let (sent_w, _) = crop.sent_dims();
        let window_k = sent_w as f64 / 1600.0;

        assert!(
            window_k > full_k * 1.5,
            "a window crop must be clearly sharper: full={full_k} window={window_k}"
        );
    }

    // --- M28 B4 rev: AX target selection + activation codes ------------------

    #[cfg(target_os = "macos")]
    fn candidate(x: f64, y: f64, w: f64, h: f64, pid: i32, app: &str) -> (Region, TargetApp) {
        (
            Region { x, y, w, h },
            TargetApp {
                pid,
                app: app.to_string(),
            },
        )
    }

    /// The frontmost SUBSTANTIAL window wins on stacking order — a maximized
    /// background window must never beat the smaller window in front of it
    /// (occlusion-blind max-overlap was the reviewed-out bug), and a small
    /// always-on-top panel is never substantial, so it can never shadow the
    /// window being read.
    #[cfg(target_os = "macos")]
    #[test]
    fn select_target_prefers_the_frontmost_substantial_window() {
        let full = Region {
            x: 0.0,
            y: 0.0,
            w: 1931.0,
            h: 543.0,
        };
        // Front to back: tiny voice panel, focused Chrome, maximized Slack.
        let candidates = vec![
            candidate(1700.0, 400.0, 120.0, 90.0, 10, "FermixPet"),
            candidate(0.0, 16.0, 823.0, 481.0, 20, "Google Chrome"),
            candidate(0.0, 0.0, 1931.0, 543.0, 30, "Slack"),
        ];

        let chosen = select_target(candidates, &full).expect("a target");
        assert_eq!(chosen.app, "Google Chrome");
    }

    /// A region call scoped to a window picks that window even when the panel
    /// floats ABOVE it inside the region.
    #[cfg(target_os = "macos")]
    #[test]
    fn select_target_ignores_a_small_panel_over_the_region() {
        let request = Region {
            x: 0.0,
            y: 16.0,
            w: 823.0,
            h: 481.0,
        };
        let candidates = vec![
            candidate(600.0, 300.0, 120.0, 90.0, 10, "FermixPet"),
            candidate(0.0, 16.0, 823.0, 481.0, 20, "Google Chrome"),
        ];

        let chosen = select_target(candidates, &request).expect("a target");
        assert_eq!(chosen.app, "Google Chrome");
    }

    /// With nothing substantial, raw maximum overlap decides — and a LATER,
    /// larger-overlap candidate beats an earlier smaller one (kills a
    /// first-hit-wins regression), while an exact tie keeps the frontmost.
    #[cfg(target_os = "macos")]
    #[test]
    fn select_target_falls_to_max_overlap_below_the_floor() {
        let full = Region {
            x: 0.0,
            y: 0.0,
            w: 1931.0,
            h: 543.0,
        };
        let candidates = vec![
            candidate(100.0, 100.0, 80.0, 60.0, 10, "Tiny"),
            candidate(400.0, 200.0, 300.0, 150.0, 20, "MiniPlayer"),
        ];
        let chosen = select_target(candidates, &full).expect("a target");
        assert_eq!(chosen.app, "MiniPlayer");

        let tied = vec![
            candidate(0.0, 0.0, 100.0, 100.0, 1, "Front"),
            candidate(500.0, 0.0, 100.0, 100.0, 2, "Back"),
        ];
        let chosen = select_target(tied, &full).expect("a target");
        assert_eq!(chosen.app, "Front", "an exact tie keeps the frontmost");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn select_target_is_none_without_overlap() {
        let request = Region {
            x: 0.0,
            y: 0.0,
            w: 400.0,
            h: 300.0,
        };
        let candidates = vec![candidate(1500.0, 400.0, 200.0, 100.0, 10, "Elsewhere")];
        assert_eq!(select_target(candidates, &request), None);
        assert_eq!(select_target(Vec::new(), &request), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn overlap_area_geometry() {
        let a = Region {
            x: 0.0,
            y: 0.0,
            w: 10.0,
            h: 10.0,
        };
        let inside = Region {
            x: 2.0,
            y: 2.0,
            w: 4.0,
            h: 4.0,
        };
        let disjoint = Region {
            x: 20.0,
            y: 0.0,
            w: 5.0,
            h: 5.0,
        };
        let partial = Region {
            x: 5.0,
            y: 5.0,
            w: 10.0,
            h: 10.0,
        };

        assert_eq!(overlap_area(&a, &inside), 16.0);
        assert_eq!(overlap_area(&a, &disjoint), 0.0);
        assert_eq!(overlap_area(&a, &partial), 25.0);
    }

    /// The two typed rejections fall through to the second attribute; every
    /// other AXError is terminal.
    #[cfg(target_os = "macos")]
    #[test]
    fn attribute_rejection_codes() {
        assert!(ax::attribute_rejected(-25205));
        assert!(ax::attribute_rejected(-25208));
        assert!(!ax::attribute_rejected(-25204));
        assert!(!ax::attribute_rejected(0));
    }

    #[test]
    fn a_window_is_clipped_to_the_display_it_overlaps() {
        let g = ultrawide_geom();
        let full = Region::full(&g);
        // Straddles the left edge: half of it lies off this display.
        let r = logical_bounds_to_region(&g, &full, -500.0, 0.0, 1000.0, 400.0).expect("overlaps");

        assert!(r.x >= full.x, "clipped left edge: {}", r.x);
        assert!(r.x + r.w <= full.x + full.w + 1.0, "within the sent image");
        assert!(
            (r.w - 500.0 * sent_scale(&g) as f64).abs() < 1.0,
            "w: {}",
            r.w
        );
    }

    #[test]
    fn an_offscreen_or_degenerate_window_is_not_listed() {
        let g = ultrawide_geom();
        let full = Region::full(&g);

        assert!(
            logical_bounds_to_region(&g, &full, 0.0, 0.0, 0.0, 500.0).is_none(),
            "zero-width window"
        );
        assert!(
            logical_bounds_to_region(&g, &full, 9000.0, 0.0, 800.0, 600.0).is_none(),
            "entirely to the right of this display"
        );
    }

    /// Regression guard for the region-offset class of bug: a window's region must
    /// survive the round trip through the very transform clicks use.
    #[test]
    fn a_window_region_round_trips_through_the_click_transform() {
        let g = ultrawide_geom();
        let full = Region::full(&g);
        let r =
            logical_bounds_to_region(&g, &full, 400.0, 60.0, 1600.0, 1000.0).expect("on screen");

        // The window's own top-left, read as the origin of the magnified crop.
        let (lx, ly) = to_logical(&g, &r, 0.0, 0.0);
        assert!((lx - 400).abs() <= 2, "logical x: {lx}");
        assert!((ly - 60).abs() <= 2, "logical y: {ly}");
    }

    // The DISPATCH is what this pins; whether a display is attached is the host's
    // business (CI has none, and a locked Mac reports `no_active_display`). So a
    // window list OR a typed display error both pass — "unknown action" never does.
    #[test]
    fn handle_dispatches_windows() {
        let response = dispatch(json!({ "action": "windows" })).unwrap_or_else(err_payload);

        let unknown = response["error"]
            .as_str()
            .map(|e| e.contains("unknown action"))
            .unwrap_or(false);

        assert!(!unknown, "windows must reach its handler: {response}");
        assert!(
            response["windows"].is_array() || response["ok"] == json!(false),
            "expected a window list or a typed error: {response}"
        );
    }

    // --- M28 B1/B2/B3: overlay placement ------------------------------------

    fn blank(w: u32, h: u32) -> image::RgbaImage {
        image::RgbaImage::from_pixel(w, h, image::Rgba([20, 20, 20, 255]))
    }

    fn changed_pixels(img: &image::RgbaImage) -> usize {
        img.pixels().filter(|p| p.0 != [20, 20, 20, 255]).count()
    }

    /// The executed-point marker must draw AT the point (ring pixels near it),
    /// clip safely at edges, and leave a blank image otherwise untouched.
    #[test]
    fn executed_point_marker_draws_at_the_point_and_clips_at_edges() {
        let mut img = blank(200, 120);
        overlay::executed_point(&mut img, 60, 40);
        assert!(changed_pixels(&img) > 50, "marker must be visible");
        // A ring pixel at radius ~7 on the horizontal axis.
        assert_ne!(img.get_pixel(67, 40).0, [20, 20, 20, 255]);

        // Clipping: a marker at the corner must not panic and still draws.
        let mut corner = blank(40, 30);
        overlay::executed_point(&mut corner, 0, 0);
        assert!(changed_pixels(&corner) > 0);
    }

    /// Rulers tick every 100px on the top and left edges; a small image gets no
    /// ticks at all (nothing at or past its size).
    #[test]
    fn rulers_tick_every_100_pixels() {
        let mut img = blank(350, 250);
        overlay::rulers(&mut img);
        // Vertical ticks at x=100/200/300 on the top edge; horizontal at y=100/200.
        for x in [100u32, 200, 300] {
            assert_eq!(img.get_pixel(x, 0).0, [0, 0, 0, 255], "tick at x={x}");
        }
        for y in [100u32, 200] {
            assert_eq!(img.get_pixel(0, y).0, [0, 0, 0, 255], "tick at y={y}");
        }

        let mut small = blank(90, 60);
        overlay::rulers(&mut small);
        assert_eq!(
            changed_pixels(&small),
            0,
            "no tick fits an image under 100px"
        );
    }

    /// A badge paints its red disc/pill centered on the mark point.
    #[cfg(target_os = "macos")]
    #[test]
    fn badge_is_centered_on_the_mark_point() {
        let mut img = blank(120, 80);
        overlay::badge(&mut img, 60, 40, 7);
        assert_eq!(img.get_pixel(52, 40).0, [230, 40, 40, 255], "red pill body");
        assert!(changed_pixels(&img) > 80, "badge must be prominent");
    }

    #[test]
    fn parse_region_rejects_nonpositive_dimensions() {
        let req = json!({"region": {"x": 0, "y": 0, "w": 0, "h": 10}});
        assert!(parse_region(&req).is_err());
    }

    #[test]
    fn hello_reports_the_protocol_version_and_verbs() {
        let v = hello().unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["protocol_version"], json!(PROTOCOL_VERSION));
        assert!(v["compux_version"].is_string());
        assert!(v["actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "screenshot"));

        // The bounds of the observation table are capabilities, so a caller mirrors
        // the same numbers instead of hard-coding a guess at them.
        assert_eq!(
            v["capabilities"]["observations"],
            json!({ "max": observation::MAX_OBSERVATIONS, "ttl_ms": observation::TTL_MS })
        );
    }

    // --- M42 slice 3: a coordinate names the image it was read in ------------

    /// A worker holding one image of a display that is not this machine's, so the
    /// refusals that come BEFORE any display read are testable on a host with no
    /// screen at all.
    fn worker_holding_an_image(gate: &Gate) -> (Worker, Observation) {
        let mut worker = idle_worker(gate);
        let facts = MonitorFacts {
            x: 0,
            y: 0,
            width: 1512,
            height: 982,
            scale_factor: 2.0,
        };
        let measured = geometry::Measurement {
            frame_w: 3024,
            frame_h: 1964,
            pixels_per_point: 2.0,
        };
        let geom = Geometry::from_facts(&facts, measured, Host::MacOs);
        let region = Region::full(&geom);
        let sent = crop_rect(&geom, &region).sent_dims();
        let observation = worker
            .observations
            .mint(Kind::Image, 424_242, facts, geom, region, sent);

        (worker, observation)
    }

    // Every one of these is the same promise: nothing was sent. The model is told
    // which of them happened because the next move differs — look again, or read
    // the coordinate again.
    #[test]
    fn a_click_that_names_no_image_this_process_holds_is_refused_and_sends_nothing() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let (mut worker, observation) = worker_holding_an_image(&gate);

        let mut request = running("left_click", Some(1));
        request.body = json!({"action": "left_click", "x": 4, "y": 9});
        request.observation_id = Some("nobody-1".to_string());

        let frame = serve(&request, &gate, &capture::Emitter::new(), &mut worker);
        assert_eq!(frame["error"], json!("unknown_observation"));
        assert_eq!(frame["receipt"]["dispatch"], json!("not_sent"));
        assert_eq!(frame["receipt"]["observation_id_before"], json!("nobody-1"));
        assert!(frame["detail"]
            .as_str()
            .unwrap()
            .contains("fresh screenshot"));

        // A point past the edge of the image it DOES hold: refused, never clamped
        // onto the edge, and the sentence says how big that image was.
        let outside = point_in(&observation, observation.sent_w as f64, 10.0)
            .expect_err("one pixel past the right edge");
        assert_eq!(outside.code, "point_outside_observation");
        assert!(outside
            .detail
            .unwrap()
            .contains(&format!("{}x{}", observation.sent_w, observation.sent_h)));

        // And the last pixel inside it maps, so the bound is exclusive at the top
        // and inclusive at the bottom, once.
        assert!(point_in(&observation, (observation.sent_w - 1) as f64, 0.0).is_ok());
    }

    // A frame that cannot be explained fails the action rather than being clicked
    // through, and it carries both sizes: an operator reading the trace has to be
    // able to see which two numbers disagreed.
    #[test]
    fn a_capture_that_does_not_match_its_display_fails_with_both_sizes() {
        let facts = MonitorFacts {
            x: 0,
            y: 0,
            width: 1512,
            height: 982,
            scale_factor: 2.0,
        };

        let detail = geometry::measure(&facts, 2268, 1473, Host::MacOs)
            .expect_err("1.5 pixels per point is neither the mode's 2 nor 1");
        let failure = Failure::new("capture_geometry_mismatch", detail);

        assert_eq!(failure.code, "capture_geometry_mismatch");
        let detail = failure.detail.unwrap();
        for number in [
            "frame 2268x1473",
            "display 1512x982 points",
            "mode scale 2.0000",
            "1.5000 across",
            "1.5000 down",
        ] {
            assert!(detail.contains(number), "{number} missing from: {detail}");
        }
    }

    // idle_ms / wait_for_idle are OPERATIONAL (policy-support), never advertised as
    // model verbs — same posture as `probe`. Lock that so they aren't offered to a model.
    #[test]
    fn idle_verbs_are_not_advertised_model_actions() {
        let actions = hello().unwrap()["actions"].as_array().unwrap().clone();
        assert!(!actions.iter().any(|a| a == "idle_ms"));
        assert!(!actions.iter().any(|a| a == "wait_for_idle"));
    }

    // The capture control verbs are operational (policy-driven), NOT model verbs —
    // no model tool call must reach observe_start/observe_stop (MILESTONE_32 §8.4a).
    #[test]
    fn observe_verbs_are_not_advertised_model_actions() {
        let actions = hello().unwrap()["actions"].as_array().unwrap().clone();
        assert!(!actions.iter().any(|a| a == "observe_start"));
        assert!(!actions.iter().any(|a| a == "observe_stop"));
    }

    // observe_start/observe_stop reply with a type-discriminated ack carrying
    // protocol_version — the frame the consumer's handshake reads (never err()).
    #[test]
    fn observe_stop_returns_an_ack_frame() {
        let ack = dispatch(json!({"action": "observe_stop"})).expect("an ack is always ok");
        assert_eq!(ack["type"], json!("ack"));
        assert_eq!(ack["action"], json!("observe_stop"));
        assert_eq!(ack["ok"], json!(true));
        assert_eq!(ack["protocol_version"], json!(PROTOCOL_VERSION));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn idle_ms_reports_a_nonnegative_value() {
        let v = idle_ms().unwrap();
        assert_eq!(v["ok"], json!(true));
        assert!(v["idle_ms"].as_u64().is_some());
    }

    // idle_ms:0 means "idle for >= 0ms", which is always true, so the poll returns
    // immediately with idle:true — a deterministic check of the loop's success path.
    #[cfg(target_os = "macos")]
    #[test]
    fn wait_for_idle_zero_target_returns_immediately_idle() {
        let v = wait_for_idle(&json!({"idle_ms": 0, "timeout_ms": 500}), &idle_gate()).unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["idle"], json!(true));
    }

    // A target longer than any machine uptime is unreachable, so the poll must time
    // out cleanly as idle:false rather than block — the step-aside path. The target
    // must exceed real idle even on a CI runner that has been idle for hours (an
    // hours-scale target is NOT safe — it can be satisfied immediately there).
    #[cfg(target_os = "macos")]
    #[test]
    fn wait_for_idle_times_out_when_target_unreachable() {
        let v = wait_for_idle(
            &json!({"idle_ms": 9_000_000_000_000u64, "timeout_ms": 50, "poll_ms": 10}),
            &idle_gate(),
        )
        .unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["idle"], json!(false));
    }

    // The dispatch table wires the operational verbs through `handle`.
    #[cfg(target_os = "macos")]
    #[test]
    fn handle_dispatches_idle_ms() {
        let v = dispatch(json!({"action": "idle_ms"})).expect("idle_ms answers");
        assert_eq!(v["ok"], json!(true));
        assert!(v["idle_ms"].as_u64().is_some());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn idle_detection_is_macos_only_off_macos() {
        assert!(idle_ms().is_err());
        assert!(wait_for_idle(&json!({}), &idle_gate()).is_err());
    }

    // 75 is a MEANING, not a number: Fermix reads it as a clean capture-stall
    // self-reap. `posix_spawnattr_setflags` used to exit 75 too, so a disclaim
    // failure reached the operator as a wedged capture backend. Stated as "NO
    // disclaim code is 75" over the whole set, so a code added later either joins
    // the list or fails here — asserting only the one code that moved would pass
    // for the next collision as happily as this one.
    #[cfg(target_os = "macos")]
    #[test]
    fn no_disclaim_exit_code_collides_with_the_capture_stall() {
        let codes = disclaim::EXIT_CODES;
        assert!(
            !codes.contains(&EXIT_CAPTURE_STALLED),
            "a disclaim failure exits {EXIT_CAPTURE_STALLED}, which Fermix reads as a capture stall"
        );

        for (i, code) in codes.iter().enumerate() {
            assert!(
                !codes[i + 1..].contains(code),
                "two disclaim failures share exit code {code}, so neither can be diagnosed"
            );
        }
    }

    // --- held input: the four sequences that can leave a press behind ---------
    //
    // Two assertions per sequence, and both matter. The CLEAN one pins the exact
    // event list the desktop used to see, so the guard cannot have quietly changed
    // what a click or a drag does. The SWEEP refuses every one of those calls in
    // turn and demands that nothing is left held and the clipboard reads as the
    // user left it — the defect class itself, asserted at every step rather than at
    // the one step someone thought of.

    #[test]
    fn click_posts_the_event_sequence_it_always_has() {
        let mut platform = Recorder::new(None);
        click_seq(
            &mut platform,
            120,
            240,
            Button::Left,
            2,
            &[Key::Meta, Key::Shift],
        )
        .unwrap();

        assert_eq!(
            platform.calls,
            vec![
                Call::MoveMouse(120, 240),
                Call::Settle(120, 240),
                Call::Key(Key::Meta, Direction::Press),
                Call::Key(Key::Shift, Direction::Press),
                Call::Button(Button::Left, Direction::Click),
                Call::Button(Button::Left, Direction::Click),
                // Reverse of the press order (it used to release Meta first); the
                // modifier flag state a target reads is identical either way.
                Call::Key(Key::Shift, Direction::Release),
                Call::Key(Key::Meta, Direction::Release),
            ]
        );
    }

    // The leak: a `?` out of the repeat loop returned with the modifiers down.
    #[test]
    fn click_releases_its_modifiers_however_it_fails() {
        sweep_injected_failures(None, |platform| {
            click_seq(platform, 12, 34, Button::Right, 2, &[Key::Meta, Key::Shift])
        });
    }

    #[test]
    fn drag_posts_the_event_sequence_it_always_has() {
        let mut platform = Recorder::new(None);
        drag_seq(&mut platform, 10, 20, 50, 60).unwrap();

        let mut expected = vec![
            Call::MoveMouse(10, 20),
            Call::Settle(10, 20),
            Call::Button(Button::Left, Direction::Press),
            Call::Sleep(DRAG_GRAB_MS),
        ];
        for &(x, y) in &drag_path(10, 20, 50, 60, DRAG_STEPS) {
            expected.push(Call::DragStep(x, y));
            expected.push(Call::Sleep(DRAG_STEP_MS));
        }
        expected.push(Call::Settle(50, 60));
        expected.push(Call::Sleep(DRAG_DROP_MS));
        expected.push(Call::Button(Button::Left, Direction::Release));

        assert_eq!(platform.calls, expected);
    }

    // The leak: anything between the press and the release left the desktop
    // dragging — a held left button, which takes a physical click to clear.
    #[test]
    fn drag_releases_the_button_however_it_fails() {
        sweep_injected_failures(None, |platform| drag_seq(platform, 10, 20, 50, 60));
    }

    #[test]
    fn paste_posts_the_event_sequence_it_always_has() {
        let mut platform = Recorder::new(Some("the user's own text"));
        paste_seq(&mut platform, "pasted").unwrap();

        assert_eq!(
            platform.calls,
            vec![
                Call::ClipboardRead,
                Call::ClipboardWrite("pasted".to_string()),
                Call::Sleep(PASTE_SETTLE_MS),
                Call::Key(paste_modifier(), Direction::Press),
                Call::Key(Key::Unicode('v'), Direction::Click),
                Call::Key(paste_modifier(), Direction::Release),
                Call::Sleep(CLIPBOARD_RESTORE_DWELL_MS),
                Call::ClipboardWrite("the user's own text".to_string()),
            ]
        );
        assert_eq!(platform.clipboard.as_deref(), Some("the user's own text"));
    }

    // The leak: a failure around the keystroke returned with the modifier down AND
    // the user's clipboard still holding our text.
    #[test]
    fn paste_releases_and_restores_the_clipboard_however_it_fails() {
        sweep_injected_failures(Some("the user's own text"), |platform| {
            paste_seq(platform, "pasted")
        });
    }

    #[test]
    fn key_chord_posts_the_event_sequence_it_always_has() {
        let mut platform = Recorder::new(None);
        key_chord_seq(&mut platform, &[Key::Meta], Key::Unicode('c')).unwrap();

        assert_eq!(
            platform.calls,
            vec![
                Call::Key(Key::Meta, Direction::Press),
                Call::Key(Key::Unicode('c'), Direction::Click),
                Call::Key(Key::Meta, Direction::Release),
            ]
        );
    }

    // This one already released after a failed key; what it could not survive was a
    // failure part way through PRESSING several modifiers.
    #[test]
    fn key_chord_releases_its_modifiers_however_it_fails() {
        sweep_injected_failures(None, |platform| {
            key_chord_seq(platform, &[Key::Meta, Key::Shift, Key::Alt], Key::Tab)
        });
    }

    // --- cancellation: a pause that lands INSIDE a running sequence -----------
    //
    // The gate's own tests prove the barrier; these prove what the barrier does to
    // a sequence that is already part way through one — which is the half a caller
    // feels, because it is the half that decides whether the left button is still
    // down when the drag stops.

    /// A clock that installs a pause on the Nth sleep, so a cancellation lands at a
    /// known point inside a sequence rather than at a hopeful wall-clock moment.
    struct PauseOnSleep {
        at: u64,
        sleeps: std::sync::atomic::AtomicU64,
        gate: std::sync::Mutex<Option<Gate>>,
    }

    impl PauseOnSleep {
        fn new(at: u64) -> Arc<PauseOnSleep> {
            Arc::new(PauseOnSleep {
                at,
                sleeps: std::sync::atomic::AtomicU64::new(0),
                gate: std::sync::Mutex::new(None),
            })
        }

        fn arm(self: &Arc<Self>, gate: &Gate) {
            *self.gate.lock().unwrap() = Some(gate.clone());
        }

        /// Breaks the clock <-> gate cycle the arming makes, so the test leaks nothing.
        fn disarm(self: &Arc<Self>) {
            *self.gate.lock().unwrap() = None;
        }
    }

    impl gate::Clock for PauseOnSleep {
        fn now_ms(&self) -> u64 {
            0
        }

        fn now_ns(&self) -> u128 {
            0
        }

        fn sleep(&self, _ms: u64) {
            let nth = self
                .sleeps
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            if nth == self.at {
                if let Some(gate) = self.gate.lock().unwrap().as_ref() {
                    gate.control(wire::ControlAction::Pause, None);
                }
            }
        }
    }

    fn running(action: &str, mutation_seq: Option<u64>) -> wire::Request {
        wire::Request {
            request_id: "r1".to_string(),
            action: action.to_string(),
            body: json!({}),
            sidecar_generation: Some("boot-1".to_string()),
            session_generation: Some(1),
            authorization_generation: Some(1),
            mutation_seq,
            observation_id: wire::addresses_an_image(action).then(|| "7c1e-1".to_string()),
        }
    }

    // The live check the owner cannot be asked to trust to luck: a pause during a
    // long drag stops it PART WAY and the left button comes back up. Before the
    // gate there was no way to stop it at all; the danger of stopping it badly is
    // a desktop left dragging.
    #[test]
    fn a_pause_part_way_through_a_drag_stops_it_and_releases_the_button() {
        // Sleeps: three slices of the 60 ms grab, then one per path step. The
        // fifth lands inside the second step, so the drag is genuinely mid-path.
        let clock = PauseOnSleep::new(5);
        let gate = Gate::new("boot-1".to_string(), clock.clone());
        clock.arm(&gate);
        gate.admit(&running("left_click_drag", Some(1))).unwrap();

        let mut platform = Gated::new(&gate, Recorder::new(None));
        let outcome = drag_seq(&mut platform, 10, 20, 50, 60);
        let done = gate.finish();

        assert_eq!(outcome, Err("cancelled".to_string()));
        assert!(done.posted, "input did reach the screen before the pause");
        assert!(done.cancelled);

        let calls = &platform.inner().calls;
        let presses = calls
            .iter()
            .filter(|call| matches!(call, Call::Button(_, Direction::Press)))
            .count();
        let releases = calls
            .iter()
            .filter(|call| matches!(call, Call::Button(_, Direction::Release)))
            .count();
        let steps = calls
            .iter()
            .filter(|call| matches!(call, Call::DragStep(..)))
            .count();

        assert_eq!(presses, 1);
        assert_eq!(releases, 1, "the left button must not be left down");
        assert!(
            steps > 0 && steps < DRAG_STEPS as usize,
            "the drag must stop part way, not before it started or after it ended: {steps}"
        );

        // What the caller is told: input was dispatched and the action did not finish.
        let receipt = wire::Receipt::derive(done.posted, false, false, done.timings);
        assert_eq!(receipt.dispatch, wire::Dispatch::Partial);

        clock.disarm();
    }

    // A cancelled paste has two things to put back, and the clipboard is the one
    // the operator notices: their own text must survive being interrupted.
    #[test]
    fn a_pause_during_a_paste_releases_the_modifier_and_restores_the_clipboard() {
        let clock = PauseOnSleep::new(1); // the pasteboard settle, before the keystroke
        let gate = Gate::new("boot-1".to_string(), clock.clone());
        clock.arm(&gate);
        gate.admit(&running("paste", Some(1))).unwrap();

        let mut platform = Gated::new(&gate, Recorder::new(Some("the owner clipboard")));
        let outcome = paste_seq(&mut platform, "pasted by compux");
        let done = gate.finish();

        assert_eq!(outcome, Err("cancelled".to_string()));
        assert!(done.cancelled);
        assert_eq!(
            platform.inner().clipboard.as_deref(),
            Some("the owner clipboard"),
            "a cancelled paste must not keep the user's clipboard"
        );
        assert!(platform.inner().down.is_empty(), "nothing may be left held");

        clock.disarm();
    }

    // Every wait this sidecar owns returns at its next checkpoint rather than
    // running its argument out, and says which one it was.
    #[test]
    fn a_pause_during_a_wait_returns_at_once() {
        let clock = PauseOnSleep::new(1);
        let gate = Gate::new("boot-1".to_string(), clock.clone());
        clock.arm(&gate);
        gate.admit(&running("wait", None)).unwrap();

        let outcome = wait(&json!({"ms": 20_000}), &gate);

        assert_eq!(outcome, Err(Failure::from("cancelled")));
        clock.disarm();
    }

    // The worker's own answer, end to end: admit, run, refuse, report. A paused
    // click never reaches the screen and says so in one word the caller can act on.
    #[test]
    fn a_paused_click_is_refused_with_a_receipt_that_says_nothing_was_sent() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let paused = gate.control(wire::ControlAction::Pause, None);

        let mut request = running("left_click", Some(1));
        request.authorization_generation = Some(paused.authorization_generation);

        let frame = serve(
            &request,
            &gate,
            &capture::Emitter::new(),
            &mut idle_worker(&gate),
        );

        assert_eq!(frame["type"], json!("response"));
        assert_eq!(frame["request_id"], json!("r1"));
        assert_eq!(frame["ok"], json!(false));
        assert_eq!(frame["error"], json!("paused"));
        assert_eq!(frame["receipt"]["dispatch"], json!("not_sent"));
        assert_eq!(frame["sidecar_generation"], json!("boot-1"));
        assert_eq!(frame["session_generation"], json!(1));
    }

    // The handshake, through the worker: the one response that names the version,
    // carries the boot identity, and never carries an authorization generation.
    #[test]
    fn the_handshake_answers_with_this_boot_and_this_version() {
        let gate = Gate::new("boot-xyz".to_string(), Arc::new(SystemClock::new()));
        let mut request = running("hello", None);
        request.sidecar_generation = None;
        request.session_generation = None;
        request.authorization_generation = None;

        let frame = serve(
            &request,
            &gate,
            &capture::Emitter::new(),
            &mut idle_worker(&gate),
        );

        assert_eq!(frame["ok"], json!(true));
        assert_eq!(frame["protocol_version"], json!(PROTOCOL_VERSION));
        assert_eq!(frame["sidecar_generation"], json!("boot-xyz"));
        assert_eq!(frame["session_generation"], json!(1));
        assert_eq!(frame["compux_version"], json!(env!("CARGO_PKG_VERSION")));
        assert!(frame["actions"].is_array());
        assert_eq!(
            frame["capabilities"]["input_methods"],
            json!(["foreground_hid"])
        );
        assert_eq!(
            frame["capabilities"]["controls"],
            json!(["pause", "resume", "release"])
        );
        assert!(
            frame.get("authorization_generation").is_none(),
            "the gate publishes that in an acknowledgement, never here"
        );
        assert!(
            frame.get("receipt").is_none(),
            "hello is read-only and earns no receipt"
        );
    }

    // F2: the post-action check image is taken INSIDE the action, so an action can
    // land every one of its events and then fail on its own screenshot. Reporting
    // that as `partial` would have Fermix call it `unknown`, when the truth is that
    // the click happened and only the verification did not.
    #[test]
    fn a_click_whose_own_check_image_failed_still_reports_the_input_as_sent() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let request = running("left_click", Some(1));
        gate.admit(&request).unwrap();

        let mut platform = Gated::new(&gate, Recorder::new(None));
        click_seq(&mut platform, 10, 20, Button::Left, 1, &[]).unwrap();
        gate.input_complete(); // exactly where `click` latches it, before `post`
        let done = gate.finish();

        // ... and then `post` failed on its own capture.
        let frame = reply(&request, &gate, Err(Failure::from("capture_stalled")), done);

        assert_eq!(frame["ok"], json!(false));
        assert_eq!(frame["error"], json!("capture_stalled"));
        assert_eq!(
            frame["receipt"]["dispatch"],
            json!("sent"),
            "the input landed; only the check did not"
        );
    }

    // The same shape for the one refusal that can only arise AFTER the click: the
    // check image is captured inside the action, after `gate.input_complete()`, so
    // a frame that cannot be explained fails an action whose input has already
    // landed. `dispatch: sent` with `error: capture_geometry_mismatch` is therefore
    // a real combination, and a caller's sentence for that code may not claim
    // nothing was sent. The numbers ride in `detail`, because that refusal has to
    // be diagnosable from a bug report with nothing else in it.
    #[test]
    fn a_geometry_mismatch_on_the_check_image_still_reports_the_click_as_sent() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let request = running("left_click", Some(1));
        gate.admit(&request).unwrap();

        let mut platform = Gated::new(&gate, Recorder::new(None));
        click_seq(&mut platform, 10, 20, Button::Left, 1, &[]).unwrap();
        gate.input_complete();
        let done = gate.finish();

        let facts = MonitorFacts {
            x: 0,
            y: 0,
            width: 1512,
            height: 982,
            scale_factor: 2.0,
        };
        let detail = geometry::measure(&facts, 2268, 1473, Host::MacOs)
            .expect_err("1.5 pixels per point is neither the mode's 2 nor 1");
        let frame = reply(
            &request,
            &gate,
            Err(Failure::new("capture_geometry_mismatch", detail)),
            done,
        );

        assert_eq!(frame["ok"], json!(false));
        assert_eq!(frame["error"], json!("capture_geometry_mismatch"));
        assert_eq!(
            frame["receipt"]["dispatch"],
            json!("sent"),
            "the click landed before the check image was taken"
        );

        let detail = frame["detail"].as_str().expect("the numbers");
        assert!(detail.contains("frame 2268x1473"), "{detail}");
        assert!(detail.contains("display 1512x982 points"), "{detail}");
        assert!(detail.contains("mode scale 2.0000"), "{detail}");
        assert!(detail.contains("1.5000 across"), "{detail}");
    }

    // The complement, so the latch cannot simply be "always sent": a sequence that
    // stopped part way is still `partial`.
    #[test]
    fn a_click_that_stopped_part_way_still_reports_partial() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let request = running("left_click", Some(1));
        gate.admit(&request).unwrap();

        let mut platform = Gated::new(&gate, Recorder::new(None));
        platform.move_mouse(10, 20).unwrap();
        // the sequence failed here, so nothing latched
        let done = gate.finish();

        let frame = reply(&request, &gate, Err(Failure::from("click: refused")), done);

        assert_eq!(frame["receipt"]["dispatch"], json!("partial"));
    }

    // The capture rail answers in its own family, with no envelope and no id, so
    // the one client that reads it keeps working unchanged.
    #[test]
    fn a_capture_verb_answers_in_the_ack_family_at_this_version() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let mut request = running("observe_stop", None);
        request.sidecar_generation = None;
        request.session_generation = None;
        request.authorization_generation = None;

        let frame = serve(
            &request,
            &gate,
            &capture::Emitter::new(),
            &mut idle_worker(&gate),
        );

        assert_eq!(frame["type"], json!("ack"));
        assert_eq!(frame["action"], json!("observe_stop"));
        assert_eq!(frame["ok"], json!(true));
        assert_eq!(frame["protocol_version"], json!(PROTOCOL_VERSION));
        assert!(frame.get("request_id").is_none());
        assert!(frame.get("sidecar_generation").is_none());
        assert!(frame.get("receipt").is_none());
    }
}
