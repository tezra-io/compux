//! The control reader: the thread that owns stdin, so the sidecar listens while
//! it acts.
//!
//! Under protocol 6 one thread read a line, ran the action, and only then read
//! again — so a `wait`, a `wait_for_change` poll, or the 1.5 second accessibility
//! settle poll behind an ordinary `marks: true` screenshot made the process deaf.
//! A pause could not be heard, let alone answered, until the thing it was trying
//! to stop had finished on its own.
//!
//! Here the reader does three things and none of them can block on the action:
//!
//!   * a `control` is applied to the gate **at once** — the gate's mutex is taken
//!     and released inside [`Gate::control`], never held across the write that
//!     follows — and its acknowledgement is written through the shared `Emitter`;
//!   * a `request` is handed to the action worker through a channel of capacity
//!     one, and a full channel answers `busy` rather than queueing;
//!   * a line that is not a frame is refused in whatever vocabulary its sender can
//!     read, or reported on stderr when there is nothing to answer.
//!
//! **What the `Emitter` mutex means for a pause.** One NDJSON line is written
//! under that mutex, and a screenshot response can be 16 MiB of base64, so an
//! acknowledgement CAN wait behind one such line — the technical design forbids
//! interleaving bytes inside a line, and that is the price. The bound is one line,
//! never a queue of them, because the worker holds the mutex for exactly as long
//! as one `writeln!` plus its flush. And it does not delay the barrier itself:
//! the gate has already flipped before the write is attempted, so the pause is
//! installed and no further dispatch can escape it while its report is queued.

use std::io::BufRead;
use std::sync::mpsc::{SyncSender, TrySendError};

use serde_json::Value;

use crate::capture::Emitter;
use crate::gate::Gate;
use crate::wire::{self, Control, Inbound, ParseFailure, Receipt, Reply, Request, Timings};

/// What the reader hands the action worker. One variant today; it is an enum
/// because the worker's answer depends on the family, not on the action's name.
pub enum Job {
    Action(Request),
}

/// Read frames until the pipe closes. Runs on its own thread for the whole life
/// of the process; returning means stdin reached EOF and the session is over.
pub fn run<R: BufRead>(reader: R, gate: &Gate, emitter: &Emitter, worker: SyncSender<Job>) {
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }

        match wire::parse(&line) {
            Ok(Inbound::Control(control)) => acknowledge(gate, emitter, &control),
            Ok(Inbound::Request(request)) => {
                if offer(gate, emitter, &worker, request).is_err() {
                    break; // the worker is gone; nothing can be answered any more
                }
            }
            Err(failure) => refuse(gate, emitter, &failure),
        }
    }
}

/// Apply the control and report it. The gate is flipped first and the write comes
/// after, so the barrier is installed even if the pipe is busy with a large line.
fn acknowledge(gate: &Gate, emitter: &Emitter, control: &Control) {
    let applied = gate.control(control.action, control.authorization_generation);

    emitter.emit_frame(&wire::control_ack(
        gate.envelope(),
        &control.request_id,
        control.action,
        applied.ok,
        applied.authorization_generation,
        applied.in_flight_request_id,
    ));
}

/// Hand the request to the worker, or refuse it `busy`. One in flight: a second
/// request is never queued behind the first, because a queued action would run
/// against a screen that has moved on since its caller asked for it.
fn offer(
    gate: &Gate,
    emitter: &Emitter,
    worker: &SyncSender<Job>,
    request: Request,
) -> Result<(), ()> {
    match worker.try_send(Job::Action(request)) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(Job::Action(request))) => {
            emitter.emit_frame(&busy_frame(gate, &request));
            Ok(())
        }
        Err(TrySendError::Disconnected(_)) => Err(()),
    }
}

fn busy_frame(gate: &Gate, request: &Request) -> Value {
    if wire::answers_with_ack(&request.action) {
        return crate::observe_ack(&request.action, false, Some("busy".to_string()));
    }

    wire::error_response(
        gate.envelope(),
        &request.request_id,
        "busy",
        Some("another action is already running".to_string()),
        refusal_receipt(&request.action),
    )
}

/// Answer a line that was not a frame, in the family its sender can read.
fn refuse(gate: &Gate, emitter: &Emitter, failure: &ParseFailure) {
    eprintln!(
        "compux: refused a frame: {} — {}",
        failure.error, failure.detail
    );

    match &failure.reply {
        Reply::Response(request_id) => emitter.emit_frame(&wire::error_response(
            gate.envelope(),
            request_id,
            failure.error,
            Some(failure.detail.clone()),
            failure.action.as_deref().and_then(refusal_receipt),
        )),

        // The one client that sends a typeless line reads the `ack` family and
        // keys on the version inside it, so this refusal lands in its
        // version-mismatch branch instead of leaving it waiting for a handshake.
        Reply::CaptureAck(action) => emitter.emit_frame(&crate::observe_ack(
            action,
            false,
            Some(failure.detail.clone()),
        )),

        Reply::Silence => {}
    }
}

/// A refused mutation still earns the receipt that says no input was sent — that
/// is exactly the fact the caller needs before deciding whether to retry.
fn refusal_receipt(action: &str) -> Option<Receipt> {
    if wire::carries_mutation_seq(action) {
        Some(Receipt::derive(false, false, false, Timings::default()))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::{Clock, SystemClock};
    use crate::wire::ControlAction;
    use serde_json::json;
    use std::sync::mpsc::sync_channel;
    use std::sync::Arc;

    fn gate() -> Gate {
        Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()))
    }

    fn drive(lines: &str) -> (Emitter, Vec<Job>, Gate) {
        let gate = gate();
        let emitter = Emitter::capturing();
        let (tx, rx) = sync_channel(1);

        run(lines.as_bytes(), &gate, &emitter, tx);

        (emitter.clone(), rx.try_iter().collect(), gate)
    }

    #[test]
    fn a_request_reaches_the_worker_and_answers_nothing_itself() {
        let (emitter, jobs, _) =
            drive("{\"type\":\"request\",\"request_id\":\"r1\",\"action\":\"screenshot\"}\n");

        assert_eq!(jobs.len(), 1);
        let Job::Action(request) = &jobs[0];
        assert_eq!(request.request_id, "r1");
        assert!(
            emitter.captured().is_empty(),
            "the worker answers, not the reader"
        );
    }

    #[test]
    fn a_control_is_acknowledged_by_the_reader_itself() {
        let (emitter, jobs, gate) =
            drive("{\"type\":\"control\",\"request_id\":\"c1\",\"action\":\"pause\"}\n");

        assert!(jobs.is_empty(), "a control never reaches the worker");
        let frames = emitter.captured();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["type"], json!("control_ack"));
        assert_eq!(frames[0]["action"], json!("pause"));
        assert_eq!(frames[0]["ok"], json!(true));
        assert_eq!(frames[0]["in_flight_request_id"], Value::Null);
        assert_eq!(frames[0]["sidecar_generation"], json!("boot-1"));
        assert_eq!(frames[0]["session_generation"], json!(1));
        // The barrier is installed, not merely reported.
        assert_eq!(frames[0]["authorization_generation"], json!(2));
        assert!(gate.checkpoint().is_err());
    }

    // One in flight. The second request is refused now rather than run later
    // against a screen that has moved on since its caller asked.
    #[test]
    fn a_second_request_is_refused_busy_with_a_receipt() {
        let (emitter, jobs, _) = drive(
            "{\"type\":\"request\",\"request_id\":\"r1\",\"action\":\"left_click\",\"mutation_seq\":1}\n\
             {\"type\":\"request\",\"request_id\":\"r2\",\"action\":\"left_click\",\"mutation_seq\":2}\n",
        );

        assert_eq!(jobs.len(), 1, "only one fits the channel");
        let frames = emitter.captured();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["request_id"], json!("r2"));
        assert_eq!(frames[0]["ok"], json!(false));
        assert_eq!(frames[0]["error"], json!("busy"));
        assert_eq!(frames[0]["receipt"]["dispatch"], json!("not_sent"));
    }

    #[test]
    fn a_reserved_slice_three_field_is_refused_against_its_own_request_id() {
        let (emitter, jobs, _) = drive(
            "{\"type\":\"request\",\"request_id\":\"r1\",\"action\":\"left_click\",\"target_id\":\"t1\"}\n",
        );

        assert!(jobs.is_empty(), "it must never reach the worker");
        let frames = emitter.captured();
        assert_eq!(frames[0]["request_id"], json!("r1"));
        assert_eq!(frames[0]["error"], json!("unknown_field"));
        assert_eq!(frames[0]["receipt"]["dispatch"], json!("not_sent"));
    }

    // A protocol-6 line: refused, never served, in the ack family its sender reads
    // and carrying THIS sidecar's version so the mismatch is nameable.
    #[test]
    fn a_typeless_line_is_refused_with_an_ack_naming_this_version() {
        let (emitter, jobs, _) = drive("{\"action\":\"observe_start\",\"params\":{}}\n");

        assert!(jobs.is_empty());
        let frames = emitter.captured();
        assert_eq!(frames[0]["type"], json!("ack"));
        assert_eq!(frames[0]["action"], json!("observe_start"));
        assert_eq!(frames[0]["ok"], json!(false));
        assert_eq!(
            frames[0]["protocol_version"],
            json!(crate::PROTOCOL_VERSION)
        );
        assert!(
            frames[0].get("request_id").is_none(),
            "the ack family has none"
        );
    }

    #[test]
    fn a_line_nothing_can_be_answered_against_is_reported_and_not_replied_to() {
        let (emitter, jobs, _) = drive(
            "{not json\n\
             {\"type\":\"response\",\"request_id\":\"r1\",\"ok\":true}\n\
             {\"type\":\"request\",\"action\":\"wait\"}\n",
        );

        assert!(jobs.is_empty());
        assert!(
            emitter.captured().is_empty(),
            "a reply must never be written against a guessed id"
        );
    }

    #[test]
    fn an_empty_line_is_skipped_rather_than_refused() {
        let (emitter, jobs, _) = drive("\n   \n");
        assert!(jobs.is_empty());
        assert!(emitter.captured().is_empty());
    }

    /// A clock that really sleeps, so the worker thread below is genuinely blocked
    /// while the reader answers. It sleeps in the gate's own slices, never the
    /// whole budget, which is what keeps this test's wall time bounded.
    struct RealSlices;

    impl Clock for RealSlices {
        fn now_ms(&self) -> u64 {
            0
        }

        fn sleep(&self, ms: u64) {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
    }

    // The reason the reader is its own thread: a pause must be answered while the
    // worker is inside a long wait, and must end that wait.
    #[test]
    fn a_control_is_answered_while_the_worker_is_blocked_in_a_sleep() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(RealSlices));
        let emitter = Emitter::capturing();

        let request = wire::Request {
            request_id: "r9".to_string(),
            action: "wait".to_string(),
            body: json!({}),
            sidecar_generation: Some("boot-1".to_string()),
            session_generation: Some(1),
            authorization_generation: Some(1),
            mutation_seq: None,
        };
        gate.admit(&request).unwrap();

        let sleeping = {
            let gate = gate.clone();
            std::thread::spawn(move || gate.sleep(60_000))
        };

        // The worker is now inside a one-minute wait. The reader answers anyway.
        let (tx, _rx) = sync_channel(1);
        let started = std::time::Instant::now();
        run(
            "{\"type\":\"control\",\"request_id\":\"c1\",\"action\":\"pause\"}\n".as_bytes(),
            &gate,
            &emitter,
            tx,
        );

        let outcome = sleeping.join().expect("the worker thread panicked");
        let elapsed = started.elapsed();

        assert!(outcome.is_err(), "the wait must end, not run its budget");
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "the reader waited on the worker: {elapsed:?}"
        );

        let frames = emitter.captured();
        assert_eq!(frames[0]["type"], json!("control_ack"));
        assert_eq!(
            frames[0]["in_flight_request_id"],
            json!("r9"),
            "the acknowledgement must name what is still running"
        );
    }

    #[test]
    fn every_control_verb_is_acknowledged() {
        for action in [
            ControlAction::Pause,
            ControlAction::Resume,
            ControlAction::Release,
        ] {
            let line = format!(
                "{{\"type\":\"control\",\"request_id\":\"c1\",\"action\":\"{}\"}}\n",
                action.as_str()
            );
            let (emitter, _, _) = drive(&line);
            let frames = emitter.captured();
            assert_eq!(frames[0]["action"], json!(action.as_str()));
            assert_eq!(frames[0]["ok"], json!(true));
        }
    }
}
