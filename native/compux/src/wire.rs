//! The protocol-7 frames, as types. Pure: it neither reads nor writes a pipe.
//!
//! Every line of the ACTION wire is one JSON object carrying a `type`. A request
//! carries a `request_id` its response echoes, and — after the handshake — the
//! generations it belongs to. That correlation is the whole point of the version:
//! under protocol 6 a reply was paired with a request by ORDER, so one late frame
//! shifted every answer onto the wrong question for the rest of the session.
//!
//! ## Two rails, one framing, two reply families
//!
//! Computer history is a separate client with its own sidecar process and its own
//! raw Port. Under protocol 7 it sends the SAME tagged `request` frames as the
//! action wire — `{"type":"request","request_id":"o1","action":"observe_start",…}`
//! — but it never sends `hello`, so its two requests carry no generations and no
//! `mutation_seq`, and it reads the `ack` / `event` families, byte for byte as they
//! were at protocol 6 with only the version integer inside the ack moving.
//!
//! So the framing is one mechanism and only the REPLY family differs, by action:
//! `observe_start` and `observe_stop` answer with an `ack`, everything else with a
//! `response`. An `ack` on the action wire poisons the transport by design, which
//! is the right answer — the transport must never send a capture verb.
//!
//! A TYPELESS line is the protocol-6 shape. The handshake is exact-version with no
//! legacy mode, so it is refused, never served: when it names an action we answer
//! an `ack` with `ok:false` and THIS protocol version, because that is the one
//! vocabulary a protocol-6 capturer can read — it lands in that client's
//! version-mismatch branch and names the sidecar's version, instead of leaving it
//! waiting for an acknowledgement that is never coming.

use serde_json::{json, Map, Value};

/// The reserved fields of slice 3. A request carrying one is refused, never
/// silently run without the targeting it asked for.
const RESERVED_FIELDS: [&str; 3] = ["observation_id", "target_id", "element_ref"];

/// One request line the sidecar will read. The Elixir transport refuses to write
/// a larger one; this is the same bound on the reading side, so a line neither
/// side would accept cannot half-arrive.
pub const MAX_REQUEST_BYTES: usize = 65_536;

const MAX_REQUEST_ID_BYTES: usize = 64;

/// Actions that dispatch synthetic input. **Not** the same list as
/// [`carries_mutation_seq`], and the difference is deliberate: `mouse_move` is
/// read-only to the wire (it changes nothing a caller can read back, so it earns
/// no sequence number and no receipt) yet it moves the human's pointer, so a
/// pause must stop it. `request_permissions` is the mirror image — it is a
/// mutation on the wire but it dispatches no input.
const INPUT_ACTIONS: [&str; 9] = [
    "mouse_move",
    "left_click",
    "right_click",
    "double_click",
    "left_click_drag",
    "scroll",
    "type",
    "key",
    "paste",
];

/// Read-only actions, mirroring `Compux.Protocol`'s `@read_only` exactly. The
/// complement carries a `mutation_seq` and earns a receipt. A test pins the two
/// lists together; they may not drift.
const READ_ONLY_ACTIONS: [&str; 11] = [
    "screenshot",
    "mouse_move",
    "wait",
    "inspect",
    "wait_for_change",
    "elements",
    "windows",
    "probe",
    "idle_ms",
    "wait_for_idle",
    "hello",
];

/// Does a pause have to stop this action?
pub fn touches_input(action: &str) -> bool {
    INPUT_ACTIONS.contains(&action)
}

/// Does this action carry a `mutation_seq` and earn a receipt?
pub fn carries_mutation_seq(action: &str) -> bool {
    !READ_ONLY_ACTIONS.contains(&action)
}

/// The generations every frame this process writes is stamped with. One per boot.
#[derive(Clone, Debug)]
pub struct Envelope {
    pub sidecar_generation: String,
    pub session_generation: u64,
}

impl Envelope {
    fn stamp(&self, frame: &mut Map<String, Value>) {
        frame.insert(
            "sidecar_generation".into(),
            json!(self.sidecar_generation.clone()),
        );
        frame.insert("session_generation".into(), json!(self.session_generation));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlAction {
    Pause,
    Resume,
    Release,
}

impl ControlAction {
    pub fn as_str(self) -> &'static str {
        match self {
            ControlAction::Pause => "pause",
            ControlAction::Resume => "resume",
            ControlAction::Release => "release",
        }
    }

    fn parse(name: &str) -> Option<ControlAction> {
        match name {
            "pause" => Some(ControlAction::Pause),
            "resume" => Some(ControlAction::Resume),
            "release" => Some(ControlAction::Release),
            _ => None,
        }
    }
}

/// An action request off the tagged rail. `body` is the WHOLE object, so every
/// existing action function keeps reading its own arguments out of it unchanged;
/// the envelope fields sit beside them and nothing reads them by accident.
#[derive(Clone, Debug)]
pub struct Request {
    pub request_id: String,
    pub action: String,
    pub body: Value,
    pub sidecar_generation: Option<String>,
    pub session_generation: Option<u64>,
    pub authorization_generation: Option<u64>,
    pub mutation_seq: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct Control {
    pub request_id: String,
    pub action: ControlAction,
    pub authorization_generation: Option<u64>,
}

/// What one inbound line turned out to be.
#[derive(Clone, Debug)]
pub enum Inbound {
    Request(Request),
    Control(Control),
}

/// How a refused line may be answered. A reply is only ever written against
/// something the caller actually sent — an id it minted, or an action it named —
/// never a guess, or it would answer somebody else's question.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    /// Answer a `response` frame naming this request id.
    Response(String),
    /// Answer an `ack` frame naming this action: the reply family of the only
    /// client that sends a typeless line, so it reads the refusal rather than
    /// waiting out its handshake.
    CaptureAck(String),
    /// Nothing correlatable. Stderr is the whole report.
    Silence,
}

/// A line we could not turn into a frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseFailure {
    pub error: &'static str,
    pub detail: String,
    pub reply: Reply,
    /// The action, when the line named one, so a refused mutation still earns the
    /// receipt that says no input was sent.
    pub action: Option<String>,
}

impl ParseFailure {
    fn silent(error: &'static str, detail: String) -> ParseFailure {
        ParseFailure {
            error,
            detail,
            reply: Reply::Silence,
            action: None,
        }
    }
}

/// The actions a client may send on a raw Port with no handshake behind it:
/// `hello` is how a caller learns what this boot is, and the two capture verbs
/// belong to a client that never sends one. They carry no generations and no
/// sequence number, and they dispatch no input.
pub const UNGATED_ACTIONS: [&str; 3] = ["hello", "observe_start", "observe_stop"];

/// Does this action answer with the computer-history `ack` family rather than a
/// `response`?
pub fn answers_with_ack(action: &str) -> bool {
    action == "observe_start" || action == "observe_stop"
}

/// Parse one line. The line is NOT trimmed of its meaning: an empty line is not a
/// frame and the caller skips it before getting here.
pub fn parse(line: &str) -> Result<Inbound, ParseFailure> {
    if line.len() > MAX_REQUEST_BYTES {
        return Err(ParseFailure::silent(
            "request_too_large",
            format!("{} bytes, the cap is {MAX_REQUEST_BYTES}", line.len()),
        ));
    }

    let value: Value = serde_json::from_str(line)
        .map_err(|e| ParseFailure::silent("malformed_frame", format!("invalid JSON: {e}")))?;

    let Some(object) = value.as_object() else {
        return Err(ParseFailure::silent(
            "malformed_frame",
            "a frame must be a JSON object".to_string(),
        ));
    };

    match object.get("type").and_then(Value::as_str) {
        None => Err(untagged(&value)),
        Some("request") => parse_request(&value),
        Some("control") => parse_control(&value),
        Some(other) => Err(ParseFailure::silent(
            "unknown_frame_type",
            format!("unknown frame type {other}"),
        )),
    }
}

/// A typeless line is the protocol-6 shape and there is no legacy mode, so it is
/// refused. When it names an action we answer in the `ack` family: a protocol-6
/// capturer reads the version integer in that ack and reports the mismatch
/// against this sidecar, rather than waiting out a handshake nothing will answer.
fn untagged(value: &Value) -> ParseFailure {
    let detail = "protocol 7 needs a tagged frame; this line has no type".to_string();

    match value.get("action").and_then(Value::as_str) {
        Some(action) => ParseFailure {
            error: "untagged_frame",
            detail,
            reply: Reply::CaptureAck(action.to_string()),
            action: Some(action.to_string()),
        },
        None => ParseFailure::silent("untagged_frame", detail),
    }
}

fn parse_request(value: &Value) -> Result<Inbound, ParseFailure> {
    let request_id = request_id(value)?;

    let Some(action) = value.get("action").and_then(Value::as_str) else {
        return Err(ParseFailure {
            error: "malformed_frame",
            detail: "a request needs a non-empty action".to_string(),
            reply: Reply::Response(request_id),
            action: None,
        });
    };
    let action = action.to_string();

    if let Some(field) = RESERVED_FIELDS.iter().find(|f| value.get(*f).is_some()) {
        return Err(ParseFailure {
            error: "unknown_field",
            detail: format!("{field} is reserved for a later slice"),
            reply: Reply::Response(request_id),
            action: Some(action),
        });
    }

    Ok(Inbound::Request(Request {
        request_id,
        action,
        sidecar_generation: value
            .get("sidecar_generation")
            .and_then(Value::as_str)
            .map(str::to_string),
        session_generation: value.get("session_generation").and_then(Value::as_u64),
        authorization_generation: value
            .get("authorization_generation")
            .and_then(Value::as_u64),
        mutation_seq: value.get("mutation_seq").and_then(Value::as_u64),
        body: value.clone(),
    }))
}

fn parse_control(value: &Value) -> Result<Inbound, ParseFailure> {
    let request_id = request_id(value)?;

    let name = value.get("action").and_then(Value::as_str).unwrap_or("");
    let Some(action) = ControlAction::parse(name) else {
        return Err(ParseFailure {
            error: "malformed_frame",
            detail: format!("unknown control action {name}"),
            reply: Reply::Response(request_id),
            action: None,
        });
    };

    Ok(Inbound::Control(Control {
        request_id,
        action,
        authorization_generation: value
            .get("authorization_generation")
            .and_then(Value::as_u64),
    }))
}

/// 1 to 64 printable-ASCII bytes, the same bound the Elixir side enforces. An id
/// outside it is not correlatable, so the failure carries none.
fn request_id(value: &Value) -> Result<String, ParseFailure> {
    let id = value
        .get("request_id")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let printable = !id.is_empty()
        && id.len() <= MAX_REQUEST_ID_BYTES
        && id.bytes().all(|b| (0x20..=0x7E).contains(&b));

    if printable {
        Ok(id.to_string())
    } else {
        Err(ParseFailure::silent(
            "malformed_frame",
            "request_id must be 1 to 64 printable ASCII bytes".to_string(),
        ))
    }
}

// --- receipts ----------------------------------------------------------------

/// Whether synthetic input reached the screen. Derived from what the gate
/// actually let through, never inferred from the action's name.
///
/// The wire reserves a fourth value, `unknown`, and this sidecar never sends it:
/// the gate is the ONLY path from an action to the screen, so whether a call was
/// made is a fact here rather than a guess. A build that grows a second path — a
/// background backend, an out-of-process helper — is the one that would need it,
/// and it should have to add it deliberately rather than find it lying around.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dispatch {
    NotSent,
    Sent,
    Partial,
}

impl Dispatch {
    fn as_str(self) -> &'static str {
        match self {
            Dispatch::NotSent => "not_sent",
            Dispatch::Sent => "sent",
            Dispatch::Partial => "partial",
        }
    }
}

/// What is known about the effect. `verified` is deliberately absent: it is
/// reserved for a code-derived predicate that arrives with semantic readback, and
/// a changed-pixel signal is not one. An after-image is evidence for the model to
/// assess, which is `not_observed`; no after-image at all is `unknown`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    NotObserved,
    Unknown,
}

impl Effect {
    fn as_str(self) -> &'static str {
        match self {
            Effect::NotObserved => "not_observed",
            Effect::Unknown => "unknown",
        }
    }
}

/// Measured, never estimated. A phase that did not run is 0.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timings {
    pub input_ms: u64,
    pub settle_ms: u64,
    pub capture_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Receipt {
    pub dispatch: Dispatch,
    pub effect: Effect,
    pub timings: Timings,
}

impl Receipt {
    /// The one derivation. `posted` is whether the gate let at least one input
    /// call through, `completed` whether the action ran to its own end.
    pub fn derive(posted: bool, completed: bool, after_image: bool, timings: Timings) -> Receipt {
        let dispatch = match (posted, completed) {
            (false, _) => Dispatch::NotSent,
            (true, true) => Dispatch::Sent,
            (true, false) => Dispatch::Partial,
        };

        Receipt {
            dispatch,
            effect: if after_image {
                Effect::NotObserved
            } else {
                Effect::Unknown
            },
            timings,
        }
    }

    fn to_value(self) -> Value {
        json!({
            "dispatch": self.dispatch.as_str(),
            "effect": self.effect.as_str(),
            "input_method": "foreground_hid",
            "timings_ms": {
                "input": self.timings.input_ms,
                "settle": self.timings.settle_ms,
                "capture": self.timings.capture_ms,
            }
        })
    }
}

// --- outbound frames ---------------------------------------------------------

/// A successful action response: the action's own payload, unchanged, wrapped in
/// the envelope. `protocol_version` is NOT stamped here — on the Elixir side it is
/// stripped from no response and would land in every caller's payload; it belongs
/// to `hello` alone, where it is the answer.
pub fn response(
    envelope: &Envelope,
    request_id: &str,
    payload: Value,
    receipt: Option<Receipt>,
) -> Value {
    let mut frame = match payload {
        Value::Object(map) => map,
        other => {
            let mut map = Map::new();
            map.insert("payload".into(), other);
            map
        }
    };

    frame.insert("type".into(), json!("response"));
    frame.insert("request_id".into(), json!(request_id));
    frame.insert("ok".into(), json!(true));
    envelope.stamp(&mut frame);
    if let Some(receipt) = receipt {
        frame.insert("receipt".into(), receipt.to_value());
    }

    Value::Object(frame)
}

/// A failed action response. `error` is a non-empty string code the Elixir side
/// requires; `detail` carries the sentence a human needs beside it.
pub fn error_response(
    envelope: &Envelope,
    request_id: &str,
    error: &str,
    detail: Option<String>,
    receipt: Option<Receipt>,
) -> Value {
    let mut frame = Map::new();
    frame.insert("type".into(), json!("response"));
    frame.insert("request_id".into(), json!(request_id));
    frame.insert("ok".into(), json!(false));
    frame.insert("error".into(), json!(error));
    envelope.stamp(&mut frame);

    if let Some(detail) = detail.filter(|d| !d.is_empty()) {
        frame.insert("detail".into(), json!(detail));
    }
    if let Some(receipt) = receipt {
        frame.insert("receipt".into(), receipt.to_value());
    }

    Value::Object(frame)
}

/// The answer to a control. `in_flight_request_id` is the truthful half: a pause
/// installs at once, but an OS call already under way still finishes, and this
/// names it.
pub fn control_ack(
    envelope: &Envelope,
    request_id: &str,
    action: ControlAction,
    ok: bool,
    authorization_generation: u64,
    in_flight_request_id: Option<String>,
) -> Value {
    let mut frame = Map::new();
    frame.insert("type".into(), json!("control_ack"));
    frame.insert("request_id".into(), json!(request_id));
    frame.insert("action".into(), json!(action.as_str()));
    frame.insert("ok".into(), json!(ok));
    frame.insert(
        "authorization_generation".into(),
        json!(authorization_generation),
    );
    frame.insert(
        "in_flight_request_id".into(),
        match in_flight_request_id {
            Some(id) => json!(id),
            None => Value::Null,
        },
    );
    envelope.stamp(&mut frame);

    Value::Object(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope() -> Envelope {
        Envelope {
            sidecar_generation: "boot-1".to_string(),
            session_generation: 1,
        }
    }

    #[test]
    fn a_request_keeps_its_own_arguments_in_the_body() {
        let line = r#"{"type":"request","request_id":"r7","action":"left_click","x":4,"y":9,
            "deadline_ms":30000,"sidecar_generation":"boot-1","session_generation":1,
            "authorization_generation":3,"mutation_seq":12}"#;

        let Ok(Inbound::Request(request)) = parse(line) else {
            panic!("not a request");
        };
        assert_eq!(request.request_id, "r7");
        assert_eq!(request.action, "left_click");
        assert_eq!(request.mutation_seq, Some(12));
        assert_eq!(request.authorization_generation, Some(3));
        assert_eq!(request.sidecar_generation.as_deref(), Some("boot-1"));
        // The action functions read their arguments straight off the body.
        assert_eq!(request.body["x"], json!(4));
    }

    #[test]
    fn hello_carries_a_protocol_version_and_no_generations() {
        let line = r#"{"type":"request","request_id":"r1","action":"hello",
            "protocol_version":7,"deadline_ms":10000}"#;

        let Ok(Inbound::Request(request)) = parse(line) else {
            panic!("not a request");
        };
        assert_eq!(request.sidecar_generation, None);
        assert_eq!(request.mutation_seq, None);
    }

    #[test]
    fn a_control_parses_its_three_verbs_and_nothing_else() {
        for (name, expected) in [
            ("pause", ControlAction::Pause),
            ("resume", ControlAction::Resume),
            ("release", ControlAction::Release),
        ] {
            let line = format!(r#"{{"type":"control","request_id":"c1","action":"{name}"}}"#);
            let Ok(Inbound::Control(control)) = parse(&line) else {
                panic!("not a control");
            };
            assert_eq!(control.action, expected);
        }

        let line = r#"{"type":"control","request_id":"c1","action":"stop"}"#;
        let failure = parse(line).expect_err("stop is not a control verb");
        assert_eq!(failure.reply, Reply::Response("c1".to_string()));
        assert_eq!(failure.error, "malformed_frame");
    }

    // A reserved field means the caller is asking for targeting this build cannot
    // do. Running the action anyway would act on the wrong thing while looking
    // like it obeyed.
    #[test]
    fn a_reserved_slice_three_field_is_refused_by_name() {
        for field in RESERVED_FIELDS {
            let line = format!(
                r#"{{"type":"request","request_id":"r1","action":"left_click","{field}":"x"}}"#
            );
            let failure = parse(&line).expect_err("a reserved field must be refused");
            assert_eq!(failure.error, "unknown_field");
            assert_eq!(failure.reply, Reply::Response("r1".to_string()));
            assert_eq!(failure.action.as_deref(), Some("left_click"));
            assert!(failure.detail.contains(field), "{}", failure.detail);
        }
    }

    #[test]
    fn an_unknown_tag_is_refused_and_cannot_be_correlated() {
        let failure = parse(r#"{"type":"response","request_id":"r1","ok":true}"#)
            .expect_err("a response is not an inbound frame");
        assert_eq!(failure.error, "unknown_frame_type");
        assert_eq!(
            failure.reply,
            Reply::Silence,
            "a reply must never be guessed"
        );
    }

    #[test]
    fn a_malformed_line_carries_no_request_id() {
        for line in [
            "{not json",
            "[1,2,3]",
            r#"{"type":"request","action":"wait"}"#,
        ] {
            let failure = parse(line).expect_err("must not parse");
            assert_eq!(failure.reply, Reply::Silence, "{line}");
        }
    }

    #[test]
    fn a_request_id_outside_the_bound_is_not_correlatable() {
        for id in ["", &"x".repeat(65), "no\ttabs"] {
            let line = format!(r#"{{"type":"request","request_id":"{id}","action":"wait"}}"#);
            let failure = parse(&line).expect_err("an id outside the bound");
            assert_eq!(failure.reply, Reply::Silence);
        }
        let ok = format!(
            r#"{{"type":"request","request_id":"{}","action":"wait"}}"#,
            "x".repeat(64)
        );
        assert!(parse(&ok).is_ok(), "64 bytes is the boundary, not past it");
    }

    #[test]
    fn an_over_long_line_is_refused_before_it_is_parsed() {
        let line = format!(
            r#"{{"type":"request","request_id":"r1","action":"type","text":"{}"}}"#,
            "x".repeat(MAX_REQUEST_BYTES)
        );
        let failure = parse(&line).expect_err("over the cap");
        assert_eq!(failure.error, "request_too_large");
    }

    // Computer history sends a TAGGED request like everything else; only its reply
    // family differs, and it carries no generations because it never says hello.
    #[test]
    fn a_capture_verb_is_an_ordinary_request_that_answers_with_an_ack() {
        let line = r#"{"type":"request","request_id":"o1","action":"observe_start",
            "params":{"apps":["com.apple.Notes"]}}"#;

        let Ok(Inbound::Request(request)) = parse(line) else {
            panic!("not a request");
        };
        assert_eq!(request.action, "observe_start");
        assert_eq!(request.sidecar_generation, None);
        assert_eq!(request.mutation_seq, None);
        assert!(answers_with_ack(&request.action));
        assert!(answers_with_ack("observe_stop"));
        assert!(!answers_with_ack("left_click"));
        assert!(UNGATED_ACTIONS.contains(&"observe_start"));
    }

    // A typeless line is the protocol-6 shape, and there is no legacy mode. It is
    // refused in the ONE vocabulary its only sender can read: an ack carrying this
    // sidecar's version, which lands in that client's version-mismatch branch.
    #[test]
    fn a_typeless_line_is_refused_in_the_ack_family_its_sender_reads() {
        let failure = parse(r#"{"action":"observe_start","params":{}}"#)
            .expect_err("a protocol-6 line is not served");
        assert_eq!(failure.error, "untagged_frame");
        assert_eq!(
            failure.reply,
            Reply::CaptureAck("observe_start".to_string())
        );

        let nameless =
            parse(r#"{"params":{}}"#).expect_err("nothing to answer and nothing to name");
        assert_eq!(nameless.reply, Reply::Silence);
    }

    // The two lists answer different questions and a reader will assume they are
    // the same one. `mouse_move` is the case that proves they are not.
    #[test]
    fn mouse_move_is_read_only_on_the_wire_and_still_input() {
        assert!(carries_mutation_seq("left_click"));
        assert!(touches_input("left_click"));

        // Read-only to the wire — no sequence number, no receipt — and still the
        // human's pointer, so a pause has to stop it.
        assert!(!carries_mutation_seq("mouse_move"));
        assert!(touches_input("mouse_move"));

        // The mirror image: a mutation on the wire that dispatches no input.
        assert!(carries_mutation_seq("request_permissions"));
        assert!(!touches_input("request_permissions"));

        assert!(!carries_mutation_seq("screenshot"));
        assert!(!touches_input("screenshot"));
    }

    #[test]
    fn a_response_carries_the_envelope_and_the_payload_unchanged() {
        let frame = response(
            &envelope(),
            "r7",
            json!({"ok": true, "width": 1366, "data": "abc"}),
            None,
        );

        assert_eq!(frame["type"], json!("response"));
        assert_eq!(frame["request_id"], json!("r7"));
        assert_eq!(frame["ok"], json!(true));
        assert_eq!(frame["sidecar_generation"], json!("boot-1"));
        assert_eq!(frame["session_generation"], json!(1));
        assert_eq!(frame["width"], json!(1366));
        assert_eq!(frame["data"], json!("abc"));
        assert!(frame.get("receipt").is_none());
        assert!(
            frame.get("protocol_version").is_none(),
            "only hello answers with a version"
        );
    }

    #[test]
    fn a_failure_carries_a_code_and_may_carry_a_receipt() {
        let receipt = Receipt::derive(false, false, false, Timings::default());
        let frame = error_response(
            &envelope(),
            "r7",
            "paused",
            Some("a pause is installed".to_string()),
            Some(receipt),
        );

        assert_eq!(frame["ok"], json!(false));
        assert_eq!(frame["error"], json!("paused"));
        assert_eq!(frame["detail"], json!("a pause is installed"));
        assert_eq!(frame["receipt"]["dispatch"], json!("not_sent"));
        assert_eq!(frame["receipt"]["input_method"], json!("foreground_hid"));
    }

    #[test]
    fn a_control_ack_names_what_is_still_running() {
        let frame = control_ack(
            &envelope(),
            "c3",
            ControlAction::Pause,
            true,
            4,
            Some("r9".to_string()),
        );

        assert_eq!(frame["type"], json!("control_ack"));
        assert_eq!(frame["action"], json!("pause"));
        assert_eq!(frame["authorization_generation"], json!(4));
        assert_eq!(frame["in_flight_request_id"], json!("r9"));

        let idle = control_ack(&envelope(), "c4", ControlAction::Resume, true, 5, None);
        assert_eq!(idle["in_flight_request_id"], Value::Null);
    }

    // The derivation is the whole receipt. Every combination, stated once.
    #[test]
    fn a_receipt_reports_what_the_gate_let_through() {
        let t = Timings::default();
        assert_eq!(
            Receipt::derive(false, false, false, t).dispatch,
            Dispatch::NotSent
        );
        assert_eq!(
            Receipt::derive(false, true, false, t).dispatch,
            Dispatch::NotSent,
            "a completed action that posted nothing still sent nothing"
        );
        assert_eq!(
            Receipt::derive(true, true, false, t).dispatch,
            Dispatch::Sent
        );
        assert_eq!(
            Receipt::derive(true, false, false, t).dispatch,
            Dispatch::Partial
        );
        assert_eq!(
            Receipt::derive(true, true, true, t).effect,
            Effect::NotObserved
        );
        assert_eq!(
            Receipt::derive(true, true, false, t).effect,
            Effect::Unknown
        );
    }

    #[test]
    fn timings_reach_the_frame_as_measured() {
        let receipt = Receipt::derive(
            true,
            true,
            true,
            Timings {
                input_ms: 12,
                settle_ms: 80,
                capture_ms: 140,
            },
        );
        let frame = response(&envelope(), "r1", json!({"ok": true}), Some(receipt));

        assert_eq!(frame["receipt"]["timings_ms"]["input"], json!(12));
        assert_eq!(frame["receipt"]["timings_ms"]["settle"], json!(80));
        assert_eq!(frame["receipt"]["timings_ms"]["capture"], json!(140));
    }
}
