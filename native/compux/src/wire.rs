//! The protocol-8 frames, as types. Pure: it neither reads nor writes a pipe.
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
//! raw Port. Since protocol 7 it sends the SAME tagged `request` frames as the
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

/// The fields reserved for a later slice. A request carrying one is refused, never
/// silently run without the targeting it asked for. `observation_id` left this list
/// at protocol 8, where it became the way every coordinate names its image;
/// `element_ref` left it at protocol 9, where it became the way an action names a
/// control. `target_id` — a bound window — is slice 5's.
const RESERVED_FIELDS: [&str; 1] = ["target_id"];

/// The actions addressed INTO an observation: their target is read out of a reply
/// the caller was handed, as a pixel of that image or as one of the controls it
/// listed. Each names that observation with `observation_id`; none takes a
/// `region`, because the rectangle belongs to the image, not to the action. Drag
/// names one image for both of its points.
const ADDRESSED_ACTIONS: [&str; 9] = [
    "left_click",
    "right_click",
    "double_click",
    "mouse_move",
    "left_click_drag",
    "scroll",
    "inspect",
    "press",
    "set_value",
];

/// Addressed by a CONTROL and never by a point. Their whole promise is that they
/// cannot miss, so a coordinate on one of them is a contradiction rather than a
/// hint at what was meant.
const ELEMENT_ACTIONS: [&str; 2] = ["press", "set_value"];

/// Addressed by a point OR by a control. With a reference the helper re-reads the
/// control's bounds at the moment it acts and clicks its centre, so a control that
/// moved since the listing is hit where it is now — and one that is gone is
/// refused rather than clicked at the place it used to be.
const POINT_OR_ELEMENT_ACTIONS: [&str; 5] = [
    "left_click",
    "right_click",
    "double_click",
    "mouse_move",
    "scroll",
];

/// The actions that PRODUCE coordinates. `region` stays theirs, and an
/// `observation_id` beside it says which image the rectangle was read in; with
/// none it is read in a full-display image, which is the space `windows` answers
/// in. Two spaces, both exact — not a recovery path.
const VIEWING_ACTIONS: [&str; 3] = ["screenshot", "elements", "wait_for_change"];

/// Does this action address a target inside an observation it must name?
pub fn addresses_an_image(action: &str) -> bool {
    ADDRESSED_ACTIONS.contains(&action)
}

/// Does this action address a control, and only a control?
pub fn addresses_an_element(action: &str) -> bool {
    ELEMENT_ACTIONS.contains(&action)
}

/// Does this action take an `observation_id` at all?
pub fn takes_an_observation(action: &str) -> bool {
    addresses_an_image(action) || VIEWING_ACTIONS.contains(&action)
}

/// One request line the sidecar will read. The Elixir transport refuses to write
/// a larger one; this is the same bound on the reading side, so a line neither
/// side would accept cannot half-arrive.
pub const MAX_REQUEST_BYTES: usize = 65_536;

const MAX_REQUEST_ID_BYTES: usize = 64;

/// Actions that dispatch input. **Not** the same list as
/// [`carries_mutation_seq`], and the difference is deliberate: `mouse_move` is
/// read-only to the wire (it changes nothing a caller can read back, so it earns
/// no sequence number and no receipt) yet it moves the human's pointer, so a
/// pause must stop it. `request_permissions` is the mirror image — it is a
/// mutation on the wire but it dispatches no input.
///
/// `press` and `set_value` are here too. They post no synthetic event and move no
/// pointer, but they act on the machine in front of the person, and a pause that
/// stopped the clicks and let the accessibility actions through would be a pause
/// in name only.
const INPUT_ACTIONS: [&str; 11] = [
    "mouse_move",
    "left_click",
    "right_click",
    "double_click",
    "left_click_drag",
    "scroll",
    "type",
    "key",
    "paste",
    "press",
    "set_value",
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
    /// The image this request's coordinates were read in. Required of the actions
    /// that address a point, optional on the ones that produce coordinates, refused
    /// on the rest — all decided in `parse`, so no action function can forget it.
    pub observation_id: Option<String>,
    /// The control this request names, inside that observation. Required of
    /// `press` and `set_value`, an alternative to `x`/`y` on a pointer action, and
    /// refused everywhere else — also decided in `parse`.
    pub element_ref: Option<String>,
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
    let detail = "this wire needs a tagged frame; this line has no type".to_string();

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

    let (observation_id, element_ref) = match addressing(value, &action) {
        Ok(addressed) => addressed,
        Err((error, detail)) => {
            return Err(ParseFailure {
                error,
                detail,
                reply: Reply::Response(request_id),
                action: Some(action),
            })
        }
    };

    Ok(Inbound::Request(Request {
        request_id,
        action,
        observation_id,
        element_ref,
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

/// What this request is addressed at, decided before the action runs: the
/// observation its target was read from, and the control it names inside it.
///
/// The whole addressing rule, in order:
///
///   * an action that is ADDRESSED into an observation must name one and may not
///     describe one — a `region` there is the defect protocol 8 replaced, a
///     rectangle copied from a previous reply and applied to a transform that has
///     moved on;
///   * inside that observation it names a point or a control. `press` and
///     `set_value` name a control and nothing else; a pointer action names either
///     but never both, because there is no safe way to choose between them;
///   * an action that PRODUCES coordinates may name an observation, and its
///     `region` is then read in that image;
///   * anything else reads no coordinates and addresses no control, so naming
///     either is a request this build cannot honour and is refused, not ignored.
type Addressed = (Option<String>, Option<String>);

fn addressing(value: &Value, action: &str) -> Result<Addressed, (&'static str, String)> {
    let named = non_empty(value, "observation_id").map_err(|field| {
        (
            "observation_required",
            format!(
                "{field} must be a non-empty string naming the reply this request's target was \
                 read from"
            ),
        )
    })?;
    let reference = non_empty(value, "element_ref").map_err(|field| {
        (
            "element_required",
            format!(
                "{field} must be a non-empty string naming a control, as an elements reply \
                     spelled it"
            ),
        )
    })?;

    if !takes_an_observation(action) {
        return match (named, reference) {
            (None, None) => Ok((None, None)),
            (Some(_), _) => Err((
                "unknown_field",
                format!("{action} reads no coordinates, so it takes no observation_id"),
            )),
            (None, Some(_)) => Err((
                "unknown_field",
                format!("{action} addresses no control, so it takes no element_ref"),
            )),
        };
    }

    if !addresses_an_image(action) {
        return match reference {
            None => Ok((named, None)),
            Some(_) => Err((
                "unknown_field",
                format!("{action} produces references, it does not take an element_ref"),
            )),
        };
    }

    if value.get("region").is_some() {
        return Err((
            "unknown_field",
            format!(
                "region is not accepted on {action}: its coordinates are pixels in the image \
                 named by observation_id, which carries its own rectangle"
            ),
        ));
    }

    let Some(id) = named else {
        return Err((
            "observation_required",
            format!("{action} needs observation_id: the id of the reply its target was read from"),
        ));
    };

    target(value, action, reference).map(|reference| (Some(id), reference))
}

/// A point, a control, or the refusal that says the request named both or neither.
fn target(
    value: &Value,
    action: &str,
    reference: Option<String>,
) -> Result<Option<String>, (&'static str, String)> {
    let point = ["x", "y", "from", "to"]
        .iter()
        .any(|field| value.get(*field).is_some());

    if reference.is_some() && point {
        return Err((
            "addressing_conflict",
            format!(
                "{action} is addressed either by a point or by element_ref, never by both: send \
                 the coordinates, or the reference, not the two together"
            ),
        ));
    }

    if addresses_an_element(action) {
        return match reference {
            Some(reference) => Ok(Some(reference)),
            None => Err((
                "element_required",
                format!(
                    "{action} needs element_ref: the reference of the control, from the elements \
                     reply that observation_id names"
                ),
            )),
        };
    }

    // A drag names two points and `inspect` reports what is under one, so neither
    // has a meaning for a reference; a pointer action takes either.
    match reference {
        None => Ok(None),
        Some(_) if POINT_OR_ELEMENT_ACTIONS.contains(&action) => Ok(reference),
        Some(_) => Err((
            "unknown_field",
            format!("{action} addresses a point, so it takes no element_ref"),
        )),
    }
}

/// A string field that means nothing when empty: absent is `None`, a non-empty
/// string is itself, and anything else names the field so the caller is told which
/// one it got wrong.
fn non_empty(value: &Value, field: &'static str) -> Result<Option<String>, &'static str> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if !text.is_empty() => Ok(Some(text.clone())),
        Some(_other) => Err(field),
    }
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

// --- failures ----------------------------------------------------------------

/// What an action failed with: the wire's `error` code, and the sentence that goes
/// beside it when the code alone cannot carry the fact.
///
/// Most failures are a code and nothing else, which is why `From<String>` exists
/// and every function that only ever produces one keeps its `Result<_, String>`.
/// The ones that need more — a point and the size of the image it missed, two
/// capture dimensions that cannot both be right — carry a `detail`, because
/// "capture_geometry_mismatch" on its own tells an operator nothing and folding the
/// numbers into the code would make every failure its own code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub code: String,
    pub detail: Option<String>,
}

impl Failure {
    pub fn new(code: &str, detail: String) -> Failure {
        Failure {
            code: code.to_string(),
            detail: Some(detail),
        }
    }
}

impl From<String> for Failure {
    fn from(code: String) -> Failure {
        Failure { code, detail: None }
    }
}

impl From<&str> for Failure {
    fn from(code: &str) -> Failure {
        Failure {
            code: code.to_string(),
            detail: None,
        }
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

/// What is known about the effect.
///
/// `Verified` arrived at protocol 9 and is deliberately narrow: it means the
/// action READ BACK what it had just written and got the same thing. A
/// changed-pixel signal is not that, and an after-image is evidence for the model
/// to assess rather than a verdict, which is `not_observed`; no after-image at
/// all is `unknown`. A successful accessibility return is a dispatch result, not
/// an effect, so `press` is `not_observed` however cleanly it returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    Verified,
    NotObserved,
    Unknown,
}

impl Effect {
    fn as_str(self) -> &'static str {
        match self {
            Effect::Verified => "verified",
            Effect::NotObserved => "not_observed",
            Effect::Unknown => "unknown",
        }
    }
}

/// How the action reached the machine.
///
/// `foreground_hid` is a synthetic event posted at the input tap: it needs the
/// pointer or the focus, and whatever is in front receives it. `ax` is a message
/// to one control through the accessibility API: no pointer moves and nothing can
/// miss. A caller reads this to know which of the two it got — and a `press` that
/// reports `foreground_hid` would be exactly the silent fallback this build
/// refuses to have.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InputMethod {
    #[default]
    ForegroundHid,
    Ax,
}

impl InputMethod {
    fn as_str(self) -> &'static str {
        match self {
            InputMethod::ForegroundHid => "foreground_hid",
            InputMethod::Ax => "ax",
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    pub dispatch: Dispatch,
    pub effect: Effect,
    pub input_method: InputMethod,
    pub timings: Timings,
    /// The image the action's coordinates were read in, and the one it handed back
    /// afterwards. Together they are the audit trail of a click: what it aimed at,
    /// and what the caller may aim at next.
    pub observation_id_before: Option<String>,
    pub observation_id_after: Option<String>,
    /// Whether the action took the foreground, when it is the kind of action that
    /// can be asked. `None` is "not applicable" — a click takes the foreground by
    /// definition — and is left off the wire rather than published as `false`.
    pub foreground_changed: Option<bool>,
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
            input_method: InputMethod::ForegroundHid,
            timings,
            observation_id_before: None,
            observation_id_after: None,
            foreground_changed: None,
        }
    }

    /// Name the images this action read from and produced. Separate from `derive`
    /// because a refusal has a before and no after, and the derivation knows about
    /// neither.
    pub fn addressing(mut self, before: Option<String>, after: Option<String>) -> Receipt {
        self.observation_id_before = before;
        self.observation_id_after = after;
        self
    }

    /// What only the action itself could know, as the gate recorded it: which
    /// method carried it, the effect it could prove (`None` leaves the derived
    /// one), and whether the foreground moved while it ran.
    pub fn noted(
        mut self,
        method: InputMethod,
        effect: Option<Effect>,
        foreground_changed: Option<bool>,
    ) -> Receipt {
        self.input_method = method;
        if let Some(effect) = effect {
            self.effect = effect;
        }
        self.foreground_changed = foreground_changed;
        self
    }

    fn into_value(self) -> Value {
        let mut receipt = json!({
            "dispatch": self.dispatch.as_str(),
            "effect": self.effect.as_str(),
            "input_method": self.input_method.as_str(),
            "timings_ms": {
                "input": self.timings.input_ms,
                "settle": self.timings.settle_ms,
                "capture": self.timings.capture_ms,
            }
        });

        if let Some(object) = receipt.as_object_mut() {
            for (key, id) in [
                ("observation_id_before", self.observation_id_before),
                ("observation_id_after", self.observation_id_after),
            ] {
                if let Some(id) = id {
                    object.insert(key.to_string(), json!(id));
                }
            }
            if let Some(changed) = self.foreground_changed {
                object.insert("foreground_changed".to_string(), json!(changed));
            }
        }

        receipt
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
        frame.insert("receipt".into(), receipt.into_value());
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
        frame.insert("receipt".into(), receipt.into_value());
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
            "observation_id":"7c1e-12","deadline_ms":30000,"sidecar_generation":"boot-1",
            "session_generation":1,"authorization_generation":3,"mutation_seq":12}"#;

        let Ok(Inbound::Request(request)) = parse(line) else {
            panic!("not a request");
        };
        assert_eq!(request.request_id, "r7");
        assert_eq!(request.action, "left_click");
        assert_eq!(request.mutation_seq, Some(12));
        assert_eq!(request.authorization_generation, Some(3));
        assert_eq!(request.sidecar_generation.as_deref(), Some("boot-1"));
        assert_eq!(request.observation_id.as_deref(), Some("7c1e-12"));
        // The action functions read their arguments straight off the body.
        assert_eq!(request.body["x"], json!(4));
    }

    // Every coordinate names the image it was read in. An action that addresses a
    // point without one is refused BEFORE the worker sees it, because there is no
    // safe thing to do with a coordinate whose space is unknown.
    #[test]
    fn an_addressed_action_must_name_its_image() {
        for action in ADDRESSED_ACTIONS {
            let line = format!(
                r#"{{"type":"request","request_id":"r1","action":"{action}","x":4,"y":9}}"#
            );
            let failure = parse(&line).expect_err("no image named");
            assert_eq!(failure.error, "observation_required");
            assert_eq!(failure.reply, Reply::Response("r1".to_string()));
            assert_eq!(failure.action.as_deref(), Some(action));
            assert!(failure.detail.contains(action), "{}", failure.detail);
        }
    }

    // A rectangle on a click is the defect this protocol replaces: one copied from
    // a previous reply, applied to a transform that has since moved. It is refused
    // by name rather than ignored, so a caller that still sends one learns why.
    #[test]
    fn an_addressed_action_refuses_a_region() {
        for action in ADDRESSED_ACTIONS {
            let line = format!(
                r#"{{"type":"request","request_id":"r1","action":"{action}","x":4,"y":9,
                   "observation_id":"7c1e-1","region":{{"x":0,"y":0,"w":10,"h":10}}}}"#
            );
            let failure = parse(&line).expect_err("a region is not accepted here");
            assert_eq!(failure.error, "unknown_field");
            assert!(failure.detail.contains("region"), "{}", failure.detail);
        }
    }

    #[test]
    fn a_viewing_action_takes_a_region_with_or_without_an_image() {
        for action in VIEWING_ACTIONS {
            let with = format!(
                r#"{{"type":"request","request_id":"r1","action":"{action}",
                   "observation_id":"7c1e-1","region":{{"x":0,"y":0,"w":10,"h":10}}}}"#
            );
            let Ok(Inbound::Request(request)) = parse(&with) else {
                panic!("{action} must take both");
            };
            assert_eq!(request.observation_id.as_deref(), Some("7c1e-1"));

            let without = format!(r#"{{"type":"request","request_id":"r1","action":"{action}"}}"#);
            let Ok(Inbound::Request(request)) = parse(&without) else {
                panic!("{action} must take neither");
            };
            assert_eq!(request.observation_id, None);
        }
    }

    // `windows` answers in the full-display space and reads nothing, so an image id
    // on it is a request this build cannot honour — refused, not ignored.
    #[test]
    fn an_action_that_reads_no_coordinates_refuses_an_image() {
        let line = r#"{"type":"request","request_id":"r1","action":"windows",
            "observation_id":"7c1e-1"}"#;
        let failure = parse(line).expect_err("windows names no image");
        assert_eq!(failure.error, "unknown_field");

        let empty = r#"{"type":"request","request_id":"r1","action":"left_click","x":1,"y":2,
            "observation_id":""}"#;
        assert_eq!(
            parse(empty).expect_err("an empty id names nothing").error,
            "observation_required"
        );
    }

    #[test]
    fn hello_carries_a_protocol_version_and_no_generations() {
        let line = r#"{"type":"request","request_id":"r1","action":"hello",
            "protocol_version":8,"deadline_ms":10000}"#;

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
    fn a_reserved_later_slice_field_is_refused_by_name() {
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

        for field in ["observation_id", "element_ref"] {
            assert!(
                !RESERVED_FIELDS.contains(&field),
                "{field} is an addressing field now, not a reservation"
            );
        }
    }

    // --- protocol 9: a control has a name ------------------------------------

    /// A request line with the fields a test cares about, so each case below is
    /// the one thing it is about.
    fn line(action: &str, fields: &str) -> String {
        format!(r#"{{"type":"request","request_id":"r1","action":"{action}"{fields}}}"#)
    }

    // `press` and `set_value` name a control, in an observation, and nothing else.
    #[test]
    fn an_element_action_names_its_observation_and_its_control() {
        for action in ELEMENT_ACTIONS {
            let Ok(Inbound::Request(request)) = parse(&line(
                action,
                r#","observation_id":"7c1e-1","element_ref":"e3""#,
            )) else {
                panic!("{action} must take both");
            };
            assert_eq!(request.observation_id.as_deref(), Some("7c1e-1"));
            assert_eq!(request.element_ref.as_deref(), Some("e3"));

            let no_image = parse(&line(action, r#","element_ref":"e3""#))
                .expect_err("a reference alone means nothing");
            assert_eq!(no_image.error, "observation_required");

            let no_control = parse(&line(action, r#","observation_id":"7c1e-1""#))
                .expect_err("it must say WHICH control");
            assert_eq!(no_control.error, "element_required");
            assert!(no_control.detail.contains("element_ref"));
            assert_eq!(no_control.reply, Reply::Response("r1".to_string()));
            assert_eq!(no_control.action.as_deref(), Some(action));
        }
    }

    // A point beside a reference is two answers to one question, and nothing may
    // choose between them: the control might have moved since the point was read,
    // and clicking the point would then be the wrong thing done confidently.
    #[test]
    fn a_point_and_a_control_on_one_request_is_a_conflict() {
        let conflicting = [
            ("left_click", r#","x":4,"y":9"#),
            ("right_click", r#","x":4,"y":9"#),
            ("double_click", r#","x":4,"y":9"#),
            ("mouse_move", r#","x":4,"y":9"#),
            ("scroll", r#","x":4,"y":9,"direction":"down""#),
            ("press", r#","x":4,"y":9"#),
            ("set_value", r#","x":4,"y":9,"value":"hi""#),
        ];

        for (action, point) in conflicting {
            let fields = format!(r#","observation_id":"7c1e-1","element_ref":"e3"{point}"#);
            let failure = parse(&line(action, &fields)).expect_err("two ways at once");
            assert_eq!(failure.error, "addressing_conflict", "{action}");
            assert!(
                failure.detail.contains("never by both"),
                "{}",
                failure.detail
            );
        }
    }

    // A pointer action takes either form, and the parse layer keeps them apart so
    // no action function has to.
    #[test]
    fn a_pointer_action_takes_a_point_or_a_control() {
        for action in POINT_OR_ELEMENT_ACTIONS {
            let by_point = line(action, r#","observation_id":"7c1e-1","x":4,"y":9"#);
            let Ok(Inbound::Request(request)) = parse(&by_point) else {
                panic!("{action} by point");
            };
            assert_eq!(request.element_ref, None);

            let by_control = line(action, r#","observation_id":"7c1e-1","element_ref":"e3""#);
            let Ok(Inbound::Request(request)) = parse(&by_control) else {
                panic!("{action} by control");
            };
            assert_eq!(request.element_ref.as_deref(), Some("e3"));
        }
    }

    // A drag names two points and `inspect` reports what is under one, so a
    // reference on either is refused rather than quietly ignored — and so is one
    // on an action that produces references or reads nothing at all.
    #[test]
    fn an_action_with_no_meaning_for_a_control_refuses_a_reference() {
        let cases = [
            ("left_click_drag", r#","observation_id":"7c1e-1""#),
            ("inspect", r#","observation_id":"7c1e-1""#),
            ("elements", ""),
            ("screenshot", ""),
            ("windows", ""),
            ("type", ""),
            ("key", ""),
        ];

        for (action, extra) in cases {
            let fields = format!(r#"{extra},"element_ref":"e3""#);
            let failure =
                parse(&line(action, &fields)).expect_err("a reference means nothing here");
            assert_eq!(failure.error, "unknown_field", "{action}");
            assert!(failure.detail.contains("element_ref"), "{}", failure.detail);
        }

        let empty = line("press", r#","observation_id":"7c1e-1","element_ref":"""#);
        assert_eq!(
            parse(&empty)
                .expect_err("an empty reference names nothing")
                .error,
            "element_required"
        );
    }

    // A pause has to stop an accessibility action too. It posts no synthetic
    // event, but it acts on the machine in front of the person, and a pause that
    // stopped the clicks and let these through would be a pause in name only.
    #[test]
    fn an_accessibility_action_is_input_and_earns_a_receipt() {
        for action in ELEMENT_ACTIONS {
            assert!(touches_input(action), "{action} acts on the machine");
            assert!(carries_mutation_seq(action), "{action} is not read-only");
            assert!(addresses_an_element(action));
            assert!(addresses_an_image(action), "{action} names its observation");
        }

        assert!(!addresses_an_element("left_click"));
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

    // What a click aimed at and what it handed back. Present only when there is
    // one: a keystroke names no image, and a refusal has no after.
    #[test]
    fn a_receipt_names_the_images_on_both_sides_of_the_action() {
        let receipt = Receipt::derive(true, true, true, Timings::default())
            .addressing(Some("7c1e-12".to_string()), Some("7c1e-13".to_string()));
        let frame = response(&envelope(), "r1", json!({"ok": true}), Some(receipt));

        assert_eq!(frame["receipt"]["observation_id_before"], json!("7c1e-12"));
        assert_eq!(frame["receipt"]["observation_id_after"], json!("7c1e-13"));

        let refused = Receipt::derive(false, false, false, Timings::default())
            .addressing(Some("7c1e-12".to_string()), None);
        let frame = error_response(&envelope(), "r1", "paused", None, Some(refused));

        assert_eq!(frame["receipt"]["observation_id_before"], json!("7c1e-12"));
        assert!(frame["receipt"].get("observation_id_after").is_none());

        let plain = Receipt::derive(true, true, false, Timings::default());
        let frame = response(&envelope(), "r1", json!({"ok": true}), Some(plain));
        assert!(frame["receipt"].get("observation_id_before").is_none());
    }

    // What only the action knows. Said, never inferred: an accessibility action
    // reports its own method, the effect its read-back proved, and whether the
    // foreground moved — and a pointer action reports none of the last.
    #[test]
    fn a_receipt_reports_the_method_the_effect_and_the_foreground() {
        let ax = Receipt::derive(true, true, false, Timings::default()).noted(
            InputMethod::Ax,
            Some(Effect::Verified),
            Some(false),
        );
        let frame = response(&envelope(), "r1", json!({"ok": true}), Some(ax));

        assert_eq!(frame["receipt"]["input_method"], json!("ax"));
        assert_eq!(frame["receipt"]["effect"], json!("verified"));
        assert_eq!(frame["receipt"]["foreground_changed"], json!(false));

        // An after-image would have derived `not_observed`; a proven effect wins,
        // because it is the stronger claim and the only one that was measured.
        let verified = Receipt::derive(true, true, true, Timings::default()).noted(
            InputMethod::Ax,
            Some(Effect::Verified),
            Some(true),
        );
        let frame = response(&envelope(), "r1", json!({"ok": true}), Some(verified));
        assert_eq!(frame["receipt"]["effect"], json!("verified"));
        assert_eq!(frame["receipt"]["foreground_changed"], json!(true));

        // A click says nothing about the foreground: it takes it by definition, so
        // the field is absent rather than published as a meaningless false.
        let click = Receipt::derive(true, true, true, Timings::default()).noted(
            InputMethod::default(),
            None,
            None,
        );
        let frame = response(&envelope(), "r1", json!({"ok": true}), Some(click));
        assert_eq!(frame["receipt"]["input_method"], json!("foreground_hid"));
        assert_eq!(frame["receipt"]["effect"], json!("not_observed"));
        assert!(frame["receipt"].get("foreground_changed").is_none());
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
