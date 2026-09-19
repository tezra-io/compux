//! The admission gate: who may dispatch input, and when it stops.
//!
//! One mutex owns the generations, the pause flag, the `mutation_seq` high-water
//! mark and the identity of the request in flight. [`Gate::admit`] takes it to
//! decide, and the dispatch primitives take it again to post.
//!
//! ## The invariant, exactly
//!
//! **For every post that is ONE EVENT — a key, a button, a pointer move, a drag
//! step, a settle — the final check and the post share the mutex** ([`Gate::dispatch`]).
//! That is the whole design: a check followed by an unguarded event post has a
//! window where a pause installs between the two and the event goes out anyway,
//! after the acknowledgement said it would not.
//!
//! **`type` and `scroll` are each a single platform call that can run for seconds**
//! — `Protocol` caps typed text at 10,000 bytes and enigo posts it in chunks — so
//! for those two the check and the mark are under the mutex and the CALL IS NOT
//! ([`Gate::dispatch_long`]). This is reported, not hidden, and it is the lesser of
//! two evils: holding the mutex across them would block the control reader past the
//! library's five-second control budget, and an unacknowledged control poisons the
//! transport and SIGKILLs this process in the middle of the typing it was trying to
//! stop.
//!
//! What a pause promises is therefore precise, and no larger: **no dispatch that
//! has not started can escape it.** A call already under way may still land, so the
//! acknowledgement names that request in `in_flight_request_id` rather than
//! pretending it stopped — and because the mark happens under the mutex BEFORE the
//! long call begins, that naming is never a guess. Everything slower than one event
//! post — a sleep, a poll, a capture, and these two calls — runs outside the mutex,
//! so a control never waits on it.
//!
//! Cleanup is not dispatch. A release of a key this process is holding is allowed
//! through a closed gate, because refusing it would be the gate itself stranding a
//! modifier on the human's keyboard. New input — a press, a click, a move, typed
//! text — is not.
//!
//! Everything here is testable with no OS call: the clock is injected, and
//! [`Gated`] wraps any [`held::Platform`], the recording one included.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use enigo::{Axis, Button, Direction, Key};

use crate::held::Platform;
use crate::wire::{ControlAction, Effect, Envelope, InputMethod, Request, Timings};

/// The coarsest a cancellation may be noticed inside a loop or a sleep of ours.
pub const CHECKPOINT_MS: u64 = 25;

/// Where the wall time of one native call is counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Input,
    Settle,
    Capture,
}

/// Why the gate said no. Each maps to one wire code; there is no general-purpose
/// refusal, because a caller that cannot tell `paused` from `stale_mutation`
/// cannot decide whether to retry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    StaleGeneration,
    StaleMutation,
    Paused,
    Cancelled,
}

impl Refusal {
    pub fn code(self) -> &'static str {
        match self {
            Refusal::StaleGeneration => "stale_generation",
            Refusal::StaleMutation => "stale_mutation",
            Refusal::Paused => "paused",
            Refusal::Cancelled => "cancelled",
        }
    }

    pub fn detail(self) -> &'static str {
        match self {
            Refusal::StaleGeneration => "this request belongs to a revoked authority",
            Refusal::StaleMutation => "this sequence number has already been seen",
            Refusal::Paused => "input is paused",
            Refusal::Cancelled => "the action was cancelled part way through",
        }
    }
}

/// Monotonic milliseconds and a sleep, injected so the gate's cadence and its
/// measured timings are asserted rather than waited on.
///
/// `now_ns` is the same clock at the resolution the wire reports an observation's
/// age in; it is one reading, not two, so an expiry and a published timestamp can
/// never disagree about when a frame was taken.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
    fn now_ns(&self) -> u128;
    fn sleep(&self, ms: u64);
}

pub struct SystemClock {
    origin: std::time::Instant,
}

impl SystemClock {
    pub fn new() -> SystemClock {
        SystemClock {
            origin: std::time::Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        SystemClock::new()
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }

    fn now_ns(&self) -> u128 {
        self.origin.elapsed().as_nanos()
    }

    fn sleep(&self, ms: u64) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

/// What a control did, for the acknowledgement that reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Acknowledged {
    pub ok: bool,
    pub authorization_generation: u64,
    pub in_flight_request_id: Option<String>,
}

/// What the action did, for the receipt that reports it.
///
/// Everything a receipt says comes from here, because the gate is the only path
/// from an action to the screen: nothing is inferred from the action's name.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dispatched {
    pub posted: bool,
    /// Whether the INPUT SEQUENCE ran to its end, which is not the same as the
    /// action succeeding: the post-action check image comes after it.
    pub input_complete: bool,
    pub cancelled: bool,
    pub timings: Timings,
    /// How the action reached the machine. The pointer and keyboard path unless
    /// an action says otherwise, because that is the only one it has.
    pub input_method: InputMethod,
    /// The effect the action could PROVE, when it could prove one. `None` leaves
    /// the derivation (an after-image, or nothing) to speak.
    pub effect: Option<Effect>,
    /// Whether the foreground moved while the action ran, for the actions that
    /// promise not to move it. `None` where the question was never asked.
    pub foreground_changed: Option<bool>,
}

struct InFlight {
    request_id: String,
    posted: bool,
    input_complete: bool,
    timings: Timings,
    input_method: InputMethod,
    effect: Option<Effect>,
    foreground_changed: Option<bool>,
}

struct State {
    authorization_generation: u64,
    paused: bool,
    mutation_high_water: u64,
    in_flight: Option<InFlight>,
}

/// A cloneable handle on one process's gate. The control reader and the action
/// worker hold the same one.
#[derive(Clone)]
pub struct Gate {
    envelope: Envelope,
    state: Arc<Mutex<State>>,
    /// Read by every checkpoint WITHOUT the mutex, so a 25 ms cadence inside a
    /// drag never contends with the control that is trying to stop it.
    cancelled: Arc<AtomicBool>,
    /// Set by a `release`, read and cleared by the action worker.
    ///
    /// Letting go of the seat lets go of what the caller could still address with
    /// it — the images it was handed, and the native element references those
    /// hold. That table belongs to the worker thread alone and takes no lock, so
    /// the control cannot clear it: it raises this, and the worker acts on it
    /// before it serves anything else.
    released: Arc<AtomicBool>,
    clock: Arc<dyn Clock>,
}

/// Both sides start here; every later value is minted here and published in an
/// acknowledgement, so the caller learns it rather than guessing it.
const INITIAL_AUTHORIZATION_GENERATION: u64 = 1;

/// One transport is one session: the process dies with it.
const SESSION_GENERATION: u64 = 1;

impl Gate {
    pub fn new(sidecar_generation: String, clock: Arc<dyn Clock>) -> Gate {
        Gate {
            envelope: Envelope {
                sidecar_generation,
                session_generation: SESSION_GENERATION,
            },
            state: Arc::new(Mutex::new(State {
                authorization_generation: INITIAL_AUTHORIZATION_GENERATION,
                paused: false,
                mutation_high_water: 0,
                in_flight: None,
            })),
            cancelled: Arc::new(AtomicBool::new(false)),
            released: Arc::new(AtomicBool::new(false)),
            clock,
        }
    }

    pub fn envelope(&self) -> &Envelope {
        &self.envelope
    }

    pub fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    /// The process's one clock, for the worker's own bounded state. Sharing it
    /// rather than taking a second reading means an observation's age and the
    /// timings on its receipt are measured against the same origin.
    pub fn clock(&self) -> Arc<dyn Clock> {
        self.clock.clone()
    }

    // A poisoned gate means a thread panicked holding it. Recovering the guard is
    // right here: the alternative is a sidecar that answers nothing at all, when
    // what it actually holds is a pause flag and a counter.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Decide whether this request may run, and become the request in flight.
    ///
    /// Order matters. Generations first: a request from a revoked authority is
    /// stale whatever else is true of it. Then the sequence number, then the
    /// pause. The high-water mark advances ONLY on admission, so a request the
    /// pause refused does not burn its own sequence number.
    pub fn admit(&self, request: &Request) -> Result<(), Refusal> {
        let mut state = self.lock();

        // Three actions are reachable on a raw Port with no handshake behind them:
        // `hello`, which is how a caller learns what this boot is, and the two
        // capture verbs, whose client never sends one. They carry no generations
        // and no sequence number, and they dispatch no input, so there is nothing
        // here for them to fail.
        if !crate::wire::UNGATED_ACTIONS.contains(&request.action.as_str()) {
            self.check_generations(&state, request)?;
            self.check_mutation_seq(&state, request)?;

            if state.paused && crate::wire::touches_input(&request.action) {
                return Err(Refusal::Paused);
            }

            // Inside the guard, so the invariant is structural: nothing ungated can
            // move a sequence number it was never checked against.
            if let Some(seq) = request.mutation_seq {
                state.mutation_high_water = seq;
            }
        }

        state.in_flight = Some(InFlight {
            request_id: request.request_id.clone(),
            posted: false,
            input_complete: false,
            timings: Timings::default(),
            input_method: InputMethod::default(),
            effect: None,
            foreground_changed: None,
        });
        self.cancelled.store(false, Ordering::SeqCst);

        Ok(())
    }

    fn check_generations(&self, state: &State, request: &Request) -> Result<(), Refusal> {
        let ours = &self.envelope;
        let matches = request.sidecar_generation.as_deref() == Some(&ours.sidecar_generation)
            && request.session_generation == Some(ours.session_generation)
            && request.authorization_generation == Some(state.authorization_generation);

        if matches {
            Ok(())
        } else {
            Err(Refusal::StaleGeneration)
        }
    }

    // Only the high-water mark is kept: a duplicate or an older sequence is
    // refused, never replayed out of a cache, so a retry can never re-post input
    // the screen already saw.
    fn check_mutation_seq(&self, state: &State, request: &Request) -> Result<(), Refusal> {
        match (
            crate::wire::carries_mutation_seq(&request.action),
            request.mutation_seq,
        ) {
            (false, _) => Ok(()),
            (true, Some(seq)) if seq > state.mutation_high_water => Ok(()),
            (true, _) => Err(Refusal::StaleMutation),
        }
    }

    /// One event: the final check and the post, under one mutex.
    ///
    /// Everything that reaches the window server comes through here or through
    /// [`Gate::dispatch_long`], which is what makes both the pause and the receipt
    /// truthful: the gate, not the action's name, is what knows whether input was
    /// posted.
    pub fn dispatch<T>(&self, phase: Phase, call: impl FnOnce() -> T) -> Result<T, Refusal> {
        let mut state = self.lock();
        Self::check(&state, &self.cancelled)?;

        let started = self.clock.now_ms();
        let outcome = call();
        let elapsed = self.clock.now_ms().saturating_sub(started);
        mark(&mut state, phase, elapsed);

        Ok(outcome)
    }

    /// One platform call that can run for seconds: the check and the mark under the
    /// mutex, the call OUTSIDE it.
    ///
    /// Only `type` and `scroll` come through here, and only because each is a
    /// single call with no loop of ours inside it. The mark is what keeps the
    /// acknowledgement honest: a pause landing after it is answered at once and
    /// names this request in `in_flight_request_id`, which is already what the
    /// design says about a call under way. Holding the mutex instead would make the
    /// control reader wait out the typing, which is how an unacknowledged control
    /// gets this process killed mid-type.
    pub fn dispatch_long<T>(&self, phase: Phase, call: impl FnOnce() -> T) -> Result<T, Refusal> {
        {
            let mut state = self.lock();
            Self::check(&state, &self.cancelled)?;
            // Marked BEFORE the call, so no window exists in which this process is
            // typing and the gate does not know it.
            mark(&mut state, phase, 0);
        }

        let started = self.clock.now_ms();
        let outcome = call();
        self.record(phase, self.clock.now_ms().saturating_sub(started));

        Ok(outcome)
    }

    /// One accessibility message: checked here, run OUTSIDE the mutex, and marked
    /// as dispatched only if it really went out.
    ///
    /// Two differences from [`Gate::dispatch`], and both are about what the
    /// platform can tell us. An EVENT post cannot: a `CGEventPost` that returned
    /// an error may still have reached the window server, so the only safe reading
    /// is that it went out, and `dispatch` marks unconditionally. An accessibility
    /// message CAN: "the accessibility API is disabled", "that element is gone"
    /// and "I gave up waiting after I had sent it" are different answers, and a
    /// receipt that called the first of those `sent` would tell a caller its press
    /// may have landed when nothing ever left this process. So `sent` decides.
    ///
    /// And the call runs outside the mutex, like [`Gate::dispatch_long`]: one
    /// message can take the whole messaging timeout, and a pause landing inside it
    /// must still be acknowledged. The acknowledgement still names this request,
    /// because `in_flight_request_id` comes from admission and not from this mark.
    pub fn dispatch_message<T>(
        &self,
        call: impl FnOnce() -> T,
        sent: impl FnOnce(&T) -> bool,
    ) -> Result<T, Refusal> {
        Self::check(&self.lock(), &self.cancelled)?;

        let started = self.clock.now_ms();
        let outcome = call();
        let elapsed = self.clock.now_ms().saturating_sub(started);

        if sent(&outcome) {
            mark(&mut self.lock(), Phase::Input, elapsed);
        } else {
            // It cost time and dispatched nothing. Both facts are true and the
            // receipt reports them separately.
            self.record(Phase::Input, elapsed);
        }

        Ok(outcome)
    }

    fn check(state: &State, cancelled: &AtomicBool) -> Result<(), Refusal> {
        if cancelled.load(Ordering::SeqCst) {
            return Err(Refusal::Cancelled);
        }
        if state.paused {
            return Err(Refusal::Paused);
        }
        Ok(())
    }

    /// Time spent in a call that is not input — a capture — recorded for the
    /// receipt without going through the gate, because it dispatches nothing.
    pub fn record(&self, phase: Phase, ms: u64) {
        let mut state = self.lock();
        if let Some(in_flight) = state.in_flight.as_mut() {
            add_timing(&mut in_flight.timings, phase, ms);
        }
    }

    /// The input sequence finished. Latched HERE rather than derived from the
    /// action's overall result, because the post-action check image is taken after
    /// it: a click whose every event landed and whose own screenshot then failed
    /// dispatched `sent`, not `partial`.
    pub fn input_complete(&self) {
        let mut state = self.lock();
        if let Some(in_flight) = state.in_flight.as_mut() {
            in_flight.input_complete = true;
        }
    }

    /// How this action reached the machine. Said, never inferred: `press` and
    /// `set_value` go through the accessibility API, everything else through the
    /// pointer and keyboard, and the receipt reports which.
    pub fn used_input_method(&self, method: InputMethod) {
        let mut state = self.lock();
        if let Some(in_flight) = state.in_flight.as_mut() {
            in_flight.input_method = method;
        }
    }

    /// The effect this action PROVED — a value read back and found equal, or a
    /// read-back that could not prove anything. Unsaid, the receipt derives what
    /// it can from whether an after-image exists.
    pub fn observed_effect(&self, effect: Effect) {
        let mut state = self.lock();
        if let Some(in_flight) = state.in_flight.as_mut() {
            in_flight.effect = Some(effect);
        }
    }

    /// Whether the foreground moved while this action ran. Only the actions that
    /// promise not to move it ask, and one that could not read the answer reports
    /// `false` rather than inventing a change.
    pub fn note_foreground(&self, changed: bool) {
        let mut state = self.lock();
        if let Some(in_flight) = state.in_flight.as_mut() {
            in_flight.foreground_changed = Some(changed);
        }
    }

    /// Is a request in flight? The reader asks before it hands over another, so a
    /// second action is refused now rather than queued behind work whose screen has
    /// moved on.
    pub fn is_busy(&self) -> bool {
        self.lock().in_flight.is_some()
    }

    /// Has a `release` been acknowledged since this was last asked? Answering it
    /// clears it, so the worker acts on one release exactly once.
    pub fn take_release(&self) -> bool {
        self.released.swap(false, Ordering::SeqCst)
    }

    /// One cancellation check. Atomics only: no mutex, so a 25 ms cadence costs
    /// nothing and can never block the control that is trying to set it.
    pub fn checkpoint(&self) -> Result<(), Refusal> {
        if self.cancelled.load(Ordering::SeqCst) {
            Err(Refusal::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Sleep, in slices no longer than the checkpoint cadence. A cancelled sleep
    /// returns at once and has NOT slept the rest of its time.
    pub fn sleep(&self, ms: u64) -> Result<(), Refusal> {
        let mut left = ms;
        while left > 0 {
            self.checkpoint()?;
            let slice = left.min(CHECKPOINT_MS);
            self.clock.sleep(slice);
            left -= slice;
        }
        self.checkpoint()
    }

    /// Finish the request in flight and hand back what it did.
    pub fn finish(&self) -> Dispatched {
        let mut state = self.lock();
        let cancelled = self.cancelled.load(Ordering::SeqCst);

        match state.in_flight.take() {
            Some(in_flight) => Dispatched {
                posted: in_flight.posted,
                input_complete: in_flight.input_complete,
                cancelled,
                timings: in_flight.timings,
                input_method: in_flight.input_method,
                effect: in_flight.effect,
                foreground_changed: in_flight.foreground_changed,
            },
            None => Dispatched {
                cancelled,
                ..Dispatched::default()
            },
        }
    }

    /// Apply a control and report what it did, under the one mutex.
    ///
    /// Every control mints a new authorization generation, which is the
    /// revocation: a request the caller minted before this control carries the old
    /// one and `admit` refuses it `stale_generation` however late it arrives.
    pub fn control(&self, action: ControlAction, expected: Option<u64>) -> Acknowledged {
        let mut state = self.lock();

        match action {
            // A pause is ALWAYS admitted. Refusing one over a generation mismatch
            // would be the gate declining to stop, which is the one answer a pause
            // may never get.
            ControlAction::Pause => {
                state.paused = true;
                state.authorization_generation += 1;
                self.cancelled.store(true, Ordering::SeqCst);
                self.acknowledge(&state, true)
            }

            // A resume names the generation it believes it is lifting. A stale one
            // is refused, so a resume in flight when a second pause installs can
            // never lift that newer pause.
            ControlAction::Resume => {
                if expected.is_some() && expected != Some(state.authorization_generation) {
                    return self.acknowledge(&state, false);
                }
                state.paused = false;
                state.authorization_generation += 1;
                self.acknowledge(&state, true)
            }

            // Let go of the seat: stop what is running and revoke, without
            // installing a barrier against what comes next. What the caller could
            // still address under the old authority goes with it; the worker does
            // that part, because the table is its alone to touch.
            ControlAction::Release => {
                state.authorization_generation += 1;
                self.cancelled.store(true, Ordering::SeqCst);
                self.released.store(true, Ordering::SeqCst);
                self.acknowledge(&state, true)
            }
        }
    }

    fn acknowledge(&self, state: &State, ok: bool) -> Acknowledged {
        Acknowledged {
            ok,
            authorization_generation: state.authorization_generation,
            in_flight_request_id: state
                .in_flight
                .as_ref()
                .map(|in_flight| in_flight.request_id.clone()),
        }
    }

    #[cfg(test)]
    fn paused(&self) -> bool {
        self.lock().paused
    }
}

/// Record that a post happened, and what it cost. Both under the caller's lock.
fn mark(state: &mut State, phase: Phase, ms: u64) {
    if let Some(in_flight) = state.in_flight.as_mut() {
        in_flight.posted = true;
        add_timing(&mut in_flight.timings, phase, ms);
    }
}

fn add_timing(timings: &mut Timings, phase: Phase, ms: u64) {
    match phase {
        Phase::Input => timings.input_ms += ms,
        Phase::Settle => timings.settle_ms += ms,
        Phase::Capture => timings.capture_ms += ms,
    }
}

// --- the gated platform -------------------------------------------------------

/// Every input call of an action sequence, routed through the gate.
///
/// This is where "cleanup is not dispatch" is written down: a `Release` goes
/// straight to the platform, because a key this process is holding must come back
/// up even through a closed gate, while a press, a click, a move or a drag step
/// has to be admitted. Wrapping [`held::Platform`] rather than editing the
/// sequences means the rule cannot be forgotten at one call site.
pub struct Gated<'a, P: Platform> {
    gate: &'a Gate,
    inner: P,
}

impl<'a, P: Platform> Gated<'a, P> {
    pub fn new(gate: &'a Gate, inner: P) -> Gated<'a, P> {
        Gated { gate, inner }
    }

    /// The wrapped platform, for a test that asserts on what actually reached it.
    /// Read-only on purpose: a mutable handle would be a way around the gate.
    #[cfg(test)]
    pub fn inner(&self) -> &P {
        &self.inner
    }
}

impl<P: Platform> Platform for Gated<'_, P> {
    fn key(&mut self, key: Key, direction: Direction) -> Result<(), String> {
        let Gated { gate, inner } = self;
        if direction == Direction::Release {
            return inner.key(key, direction);
        }
        gated(gate.dispatch(Phase::Input, || inner.key(key, direction)))
    }

    fn button(&mut self, button: Button, direction: Direction) -> Result<(), String> {
        let Gated { gate, inner } = self;
        if direction == Direction::Release {
            return inner.button(button, direction);
        }
        gated(gate.dispatch(Phase::Input, || inner.button(button, direction)))
    }

    fn move_mouse(&mut self, x: i32, y: i32) -> Result<(), String> {
        let Gated { gate, inner } = self;
        gated(gate.dispatch(Phase::Input, || inner.move_mouse(x, y)))
    }

    fn settle(&mut self, x: i32, y: i32) -> Result<(), String> {
        let Gated { gate, inner } = self;
        gated(gate.dispatch(Phase::Settle, || inner.settle(x, y)))
    }

    fn drag_step(&mut self, x: i32, y: i32) -> Result<(), String> {
        let Gated { gate, inner } = self;
        gated(gate.dispatch(Phase::Input, || inner.drag_step(x, y)))
    }

    /// One call with the repeat count inside it: admitted at the gate, and not
    /// interruptible after that, because there is no loop of ours to check. The
    /// mutex is released before the call, so a pause is answered while it runs.
    fn scroll(&mut self, length: i32, axis: Axis) -> Result<(), String> {
        let Gated { gate, inner } = self;
        gated(gate.dispatch_long(Phase::Input, || inner.scroll(length, axis)))
    }

    /// One `text` call with no loop of ours, so it too is checked here and not
    /// inside. Chunking the string would change typing timing in ways only a live
    /// check could qualify.
    fn text(&mut self, text: &str) -> Result<(), String> {
        let Gated { gate, inner } = self;
        gated(gate.dispatch_long(Phase::Input, || inner.text(text)))
    }

    /// The checkpoint site of every sequence that paces itself. A cancelled sleep
    /// is an error the sequence's `?` carries out, which is what releases the held
    /// button part way through a drag.
    fn sleep(&mut self, ms: u64) -> Result<(), String> {
        self.gate.sleep(ms).map_err(|refusal| refusal.code().into())
    }

    // The clipboard is not input and its restore is cleanup: both go straight
    // through, so a cancelled paste still puts the user's own text back.
    fn clipboard_text(&mut self) -> Result<Option<String>, String> {
        self.inner.clipboard_text()
    }

    fn set_clipboard_text(&mut self, text: &str) -> Result<(), String> {
        self.inner.set_clipboard_text(text)
    }
}

fn gated(outcome: Result<Result<(), String>, Refusal>) -> Result<(), String> {
    match outcome {
        Ok(inner) => inner,
        Err(refusal) => Err(refusal.code().into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::held::Recorder;
    use crate::wire;
    use std::sync::atomic::AtomicU64;

    /// A clock that never sleeps: time advances by exactly what was asked for, so
    /// a 25 ms cadence and a measured timing are both assertable in no time at all.
    struct FakeClock {
        now: AtomicU64,
        slept: Mutex<Vec<u64>>,
    }

    impl FakeClock {
        fn new() -> Arc<FakeClock> {
            Arc::new(FakeClock {
                now: AtomicU64::new(0),
                slept: Mutex::new(Vec::new()),
            })
        }
    }

    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.now.load(Ordering::SeqCst)
        }

        fn now_ns(&self) -> u128 {
            self.now_ms() as u128 * 1_000_000
        }

        fn sleep(&self, ms: u64) {
            self.slept.lock().unwrap().push(ms);
            self.now.fetch_add(ms, Ordering::SeqCst);
        }
    }

    fn gate() -> (Gate, Arc<FakeClock>) {
        let clock = FakeClock::new();
        (Gate::new("boot-1".to_string(), clock.clone()), clock)
    }

    fn request(action: &str) -> Request {
        Request {
            request_id: "r1".to_string(),
            action: action.to_string(),
            body: serde_json::json!({}),
            sidecar_generation: Some("boot-1".to_string()),
            session_generation: Some(1),
            authorization_generation: Some(1),
            mutation_seq: if wire::carries_mutation_seq(action) {
                Some(1)
            } else {
                None
            },
            observation_id: None,
            element_ref: None,
        }
    }

    #[test]
    fn a_request_from_this_boot_and_authority_is_admitted() {
        let (gate, _) = gate();
        assert_eq!(gate.admit(&request("left_click")), Ok(()));
    }

    #[test]
    fn a_generation_that_is_not_ours_is_refused_by_name() {
        let (gate, _) = gate();

        for mutate in [
            |r: &mut Request| r.sidecar_generation = Some("boot-somebody-else".into()),
            |r: &mut Request| r.session_generation = Some(99),
            |r: &mut Request| r.authorization_generation = Some(7),
            |r: &mut Request| r.sidecar_generation = None,
            |r: &mut Request| r.authorization_generation = None,
        ] {
            let mut stale = request("left_click");
            mutate(&mut stale);
            assert_eq!(gate.admit(&stale), Err(Refusal::StaleGeneration));
        }
    }

    // The revocation, end to end: a pause mints a new authority, and a request the
    // caller had already minted under the old one cannot run however late it lands.
    #[test]
    fn a_pause_revokes_the_requests_already_in_the_caller_s_hand() {
        let (gate, _) = gate();
        let stale = request("left_click");

        gate.control(ControlAction::Pause, None);

        assert_eq!(gate.admit(&stale), Err(Refusal::StaleGeneration));
    }

    #[test]
    fn hello_is_answered_whatever_the_gate_is_doing() {
        let (gate, _) = gate();
        gate.control(ControlAction::Pause, None);

        let mut hello = request("hello");
        hello.sidecar_generation = None;
        hello.session_generation = None;
        hello.authorization_generation = None;

        assert_eq!(gate.admit(&hello), Ok(()));
    }

    // The tension the two lists exist for: `mouse_move` carries no mutation_seq
    // and still moves the human's pointer, so a pause has to stop it.
    #[test]
    fn a_pause_refuses_mouse_move_even_though_it_is_read_only() {
        let (gate, _) = gate();
        let ack = gate.control(ControlAction::Pause, None);

        let mut moved = request("mouse_move");
        moved.authorization_generation = Some(ack.authorization_generation);
        assert_eq!(gate.admit(&moved), Err(Refusal::Paused));

        let mut looked = request("screenshot");
        looked.authorization_generation = Some(ack.authorization_generation);
        assert_eq!(gate.admit(&looked), Ok(()), "a look is not input");
    }

    #[test]
    fn a_duplicate_or_older_sequence_is_refused_and_never_replayed() {
        let (gate, _) = gate();
        let mut first = request("left_click");
        first.mutation_seq = Some(4);
        assert_eq!(gate.admit(&first), Ok(()));
        gate.finish();

        for seq in [4, 3, 1] {
            let mut again = request("left_click");
            again.mutation_seq = Some(seq);
            assert_eq!(gate.admit(&again), Err(Refusal::StaleMutation));
        }

        let mut newer = request("left_click");
        newer.mutation_seq = Some(5);
        assert_eq!(gate.admit(&newer), Ok(()));
    }

    #[test]
    fn a_mutating_request_with_no_sequence_number_is_refused() {
        let (gate, _) = gate();
        let mut naked = request("left_click");
        naked.mutation_seq = None;
        assert_eq!(gate.admit(&naked), Err(Refusal::StaleMutation));
    }

    // A refused request must not burn a sequence number: the caller's next attempt
    // would then be refused for a second, different reason it could not act on.
    #[test]
    fn a_refused_request_does_not_advance_the_high_water_mark() {
        let (gate, _) = gate();
        let ack = gate.control(ControlAction::Pause, None);

        let mut refused = request("left_click");
        refused.mutation_seq = Some(9);
        refused.authorization_generation = Some(ack.authorization_generation);
        assert_eq!(gate.admit(&refused), Err(Refusal::Paused));

        let resumed = gate.control(ControlAction::Resume, Some(ack.authorization_generation));
        let mut retry = request("left_click");
        retry.mutation_seq = Some(9);
        retry.authorization_generation = Some(resumed.authorization_generation);
        assert_eq!(gate.admit(&retry), Ok(()));
    }

    #[test]
    fn a_resume_lifts_the_pause_and_a_stale_one_does_not() {
        let (gate, _) = gate();
        let paused = gate.control(ControlAction::Pause, None);
        assert!(gate.paused());

        let refused = gate.control(
            ControlAction::Resume,
            Some(paused.authorization_generation - 1),
        );
        assert!(!refused.ok, "a stale resume must not lift a newer pause");
        assert!(gate.paused());

        let lifted = gate.control(ControlAction::Resume, Some(paused.authorization_generation));
        assert!(lifted.ok);
        assert!(!gate.paused());
        assert!(lifted.authorization_generation > paused.authorization_generation);
    }

    #[test]
    fn a_release_stops_the_work_without_installing_a_barrier() {
        let (gate, _) = gate();
        gate.admit(&request("left_click")).unwrap();

        let ack = gate.control(ControlAction::Release, None);

        assert!(ack.ok);
        assert!(!gate.paused(), "release is not a pause");
        assert_eq!(gate.checkpoint(), Err(Refusal::Cancelled));
    }

    #[test]
    fn an_acknowledgement_names_the_request_still_running() {
        let (gate, _) = gate();

        let idle = gate.control(ControlAction::Pause, None);
        assert_eq!(idle.in_flight_request_id, None);

        gate.control(ControlAction::Resume, None);
        let mut running = request("left_click_drag");
        running.request_id = "r42".to_string();
        running.authorization_generation = Some(3);
        gate.admit(&running).unwrap();

        let busy = gate.control(ControlAction::Pause, None);
        assert_eq!(busy.in_flight_request_id.as_deref(), Some("r42"));
    }

    // The window a check-then-post would leave open: the pause lands between the
    // decision and the event. Here the decision IS the post, under one mutex, so
    // the next dispatch is refused rather than posted after the acknowledgement.
    #[test]
    fn a_dispatch_after_a_pause_is_refused_not_posted() {
        let (gate, _) = gate();
        gate.admit(&request("left_click")).unwrap();

        let mut posts = 0;
        assert!(gate.dispatch(Phase::Input, || posts += 1).is_ok());

        gate.control(ControlAction::Pause, None);

        assert_eq!(
            gate.dispatch(Phase::Input, || posts += 1),
            Err(Refusal::Cancelled)
        );
        assert_eq!(posts, 1, "nothing may reach the screen after the pause");
    }

    #[test]
    fn a_sleep_is_sliced_at_the_checkpoint_cadence() {
        let (gate, clock) = gate();
        gate.admit(&request("wait")).unwrap();

        gate.sleep(70).unwrap();

        let slept = clock.slept.lock().unwrap().clone();
        assert_eq!(slept, vec![25, 25, 20]);
        assert!(slept.iter().all(|slice| *slice <= CHECKPOINT_MS));
    }

    #[test]
    fn a_cancelled_sleep_returns_without_sleeping_the_rest() {
        let (gate, clock) = gate();
        gate.admit(&request("wait")).unwrap();
        gate.control(ControlAction::Pause, None);

        assert_eq!(gate.sleep(10_000), Err(Refusal::Cancelled));
        assert!(clock.slept.lock().unwrap().is_empty());
    }

    #[test]
    fn what_the_action_did_is_measured_not_inferred() {
        let (gate, clock) = gate();
        gate.admit(&request("left_click")).unwrap();

        gate.dispatch(Phase::Input, || clock.sleep(12)).unwrap();
        gate.dispatch(Phase::Settle, || clock.sleep(80)).unwrap();
        gate.record(Phase::Capture, 140);

        let done = gate.finish();
        assert!(done.posted);
        assert!(!done.cancelled);
        assert_eq!(done.timings.input_ms, 12);
        assert_eq!(done.timings.settle_ms, 80);
        assert_eq!(done.timings.capture_ms, 140);
    }

    #[test]
    fn an_action_that_posted_nothing_says_so() {
        let (gate, _) = gate();
        gate.admit(&request("screenshot")).unwrap();
        assert!(!gate.finish().posted);
    }

    // The cleanup rule, at the seam that enforces it: a closed gate refuses new
    // input and lets a release of what this process is holding through.
    #[test]
    fn a_closed_gate_refuses_new_input_and_admits_a_release() {
        let (gate, _) = gate();
        gate.admit(&request("left_click")).unwrap();
        gate.control(ControlAction::Pause, None);

        let mut platform = Gated::new(&gate, Recorder::new(None));

        assert_eq!(
            platform.key(Key::Meta, Direction::Press),
            Err("cancelled".to_string())
        );
        assert_eq!(
            platform.button(Button::Left, Direction::Press),
            Err("cancelled".to_string())
        );
        assert_eq!(platform.move_mouse(1, 2), Err("cancelled".to_string()));
        assert_eq!(platform.settle(1, 2), Err("cancelled".to_string()));
        assert_eq!(platform.drag_step(1, 2), Err("cancelled".to_string()));

        assert_eq!(platform.key(Key::Meta, Direction::Release), Ok(()));
        assert_eq!(platform.button(Button::Left, Direction::Release), Ok(()));
        assert_eq!(platform.set_clipboard_text("restored"), Ok(()));
    }

    /// A platform whose `text` blocks until it is released, so a control can be
    /// timed against a call that is genuinely in progress rather than one that has
    /// merely been asked for.
    struct BlockingText {
        entered: Arc<(Mutex<bool>, std::sync::Condvar)>,
        release: Arc<(Mutex<bool>, std::sync::Condvar)>,
    }

    fn raise(flag: &(Mutex<bool>, std::sync::Condvar)) {
        *flag.0.lock().unwrap() = true;
        flag.1.notify_all();
    }

    fn await_flag(flag: &(Mutex<bool>, std::sync::Condvar)) {
        let mut set = flag.0.lock().unwrap();
        while !*set {
            set = flag.1.wait(set).unwrap();
        }
    }

    impl Platform for BlockingText {
        fn text(&mut self, _text: &str) -> Result<(), String> {
            raise(&self.entered);
            await_flag(&self.release);
            Ok(())
        }

        fn key(&mut self, _key: Key, _direction: Direction) -> Result<(), String> {
            Ok(())
        }
        fn button(&mut self, _button: Button, _direction: Direction) -> Result<(), String> {
            Ok(())
        }
        fn move_mouse(&mut self, _x: i32, _y: i32) -> Result<(), String> {
            Ok(())
        }
        fn settle(&mut self, _x: i32, _y: i32) -> Result<(), String> {
            Ok(())
        }
        fn drag_step(&mut self, _x: i32, _y: i32) -> Result<(), String> {
            Ok(())
        }
        fn scroll(&mut self, _length: i32, _axis: Axis) -> Result<(), String> {
            Ok(())
        }
        fn sleep(&mut self, _ms: u64) -> Result<(), String> {
            Ok(())
        }
        fn clipboard_text(&mut self) -> Result<Option<String>, String> {
            Ok(None)
        }
        fn set_clipboard_text(&mut self, _text: &str) -> Result<(), String> {
            Ok(())
        }
    }

    // The slice's headline, at the one call that could defeat it. Typing 10,000
    // bytes is seconds of a single platform call; if the gate's mutex were held
    // across it, the control reader would wait the typing out, blow the library's
    // five-second control budget, and be SIGKILLed in the middle of the very thing
    // it was trying to stop.
    #[test]
    fn a_control_is_acknowledged_while_a_long_type_call_is_running() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        gate.admit(&request("type")).unwrap();

        let entered = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));

        let typing = {
            let gate = gate.clone();
            let entered = entered.clone();
            let release = release.clone();
            std::thread::spawn(move || {
                let mut platform = Gated::new(&gate, BlockingText { entered, release });
                platform.text("a string long enough to take a while")
            })
        };

        await_flag(&entered); // the call is now genuinely in progress

        let (answered, answers) = std::sync::mpsc::channel();
        let control_gate = gate.clone();
        std::thread::spawn(move || {
            let _ = answered.send(control_gate.control(ControlAction::Pause, None));
        });
        let outcome = answers.recv_timeout(std::time::Duration::from_secs(2));

        // Release and join BEFORE asserting, so a failure reports rather than hangs.
        raise(&release);
        assert_eq!(typing.join().unwrap(), Ok(()));

        let ack = outcome.expect("a control must be answered while a long call runs");
        assert!(ack.ok);
        assert_eq!(
            ack.in_flight_request_id.as_deref(),
            Some("r1"),
            "and it must name the call that is still running"
        );
    }

    // The mark is what makes that acknowledgement honest: it happens under the
    // mutex BEFORE the long call starts, so there is no instant in which this
    // process is typing and the gate does not know it.
    #[test]
    fn a_long_call_is_marked_as_posted_before_it_begins() {
        let (gate, clock) = gate();
        gate.admit(&request("type")).unwrap();

        gate.dispatch_long(Phase::Input, || clock.sleep(400))
            .unwrap();

        let done = gate.finish();
        assert!(done.posted);
        assert_eq!(done.timings.input_ms, 400);
    }

    #[test]
    fn a_long_call_is_refused_by_a_closed_gate_like_any_other() {
        let (gate, _) = gate();
        gate.admit(&request("type")).unwrap();
        gate.control(ControlAction::Pause, None);

        let mut posts = 0;
        assert_eq!(
            gate.dispatch_long(Phase::Input, || posts += 1),
            Err(Refusal::Cancelled)
        );
        assert_eq!(posts, 0);
    }

    // The completion latch: it is the INPUT sequence finishing, not the action
    // succeeding, because the post-action check image is taken after it.
    #[test]
    fn completion_is_latched_by_the_input_sequence_not_by_the_action() {
        let (unlatched, _) = gate();
        unlatched.admit(&request("left_click")).unwrap();
        unlatched.dispatch(Phase::Input, || ()).unwrap();
        assert!(
            !unlatched.finish().input_complete,
            "posting is not finishing"
        );

        let (latched, _) = gate();
        latched.admit(&request("left_click")).unwrap();
        latched.dispatch(Phase::Input, || ()).unwrap();
        latched.input_complete();
        assert!(latched.finish().input_complete);
    }

    // What the reader asks before handing over a second request. The channel
    // cannot answer this: its slot is empty the moment the worker picks a job up.
    #[test]
    fn the_gate_knows_a_request_is_in_flight() {
        let (gate, _) = gate();
        assert!(!gate.is_busy());

        gate.admit(&request("left_click")).unwrap();
        assert!(gate.is_busy(), "the worker is running one");

        gate.finish();
        assert!(!gate.is_busy());
    }

    #[test]
    fn a_gated_platform_passes_the_open_gate_through_untouched() {
        let (gate, _) = gate();
        gate.admit(&request("left_click")).unwrap();

        let mut platform = Gated::new(&gate, Recorder::new(None));
        platform.move_mouse(4, 5).unwrap();
        platform.key(Key::Meta, Direction::Press).unwrap();
        platform.key(Key::Meta, Direction::Release).unwrap();

        assert_eq!(platform.inner().calls.len(), 3);
        assert!(gate.finish().posted);
    }
}
