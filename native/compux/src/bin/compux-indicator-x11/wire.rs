//! The pipe the helper speaks to the badge, and nothing else.
//!
//! One newline-delimited JSON object per line in each direction, the same lines
//! the macOS badge reads and writes (`native/indicator/Wire.swift`):
//!
//! ```text
//!   in   {"state":"working"|"paused"|"unavailable","app":"Files","title":"Home",
//!         "bounds":{"x":..,"y":..,"w":..,"h":..}|null,"occluded":true|false}
//!   out  {"event":"ready"|"pause"|"resume"|"stop"}
//! ```
//!
//! `bounds` are root-window pixels with a top-left origin, which is also the space
//! the badge's own window is placed in, so on X11 nothing converts them.

use std::fmt;
use std::io::BufRead;

use serde_json::{Map, Value};

/// The longest line worth parsing. A window title is a few hundred characters; a
/// megabyte of it is a bug or an attack, and either way it is not a state line.
pub const LINE_LIMIT: usize = 64 * 1024;

/// How many interrupted reads in a row are retried. A signal can interrupt a read;
/// a storm of them is a fault, and the read fails with the last one.
pub const MAX_INTERRUPTIONS: u32 = 64;

/// The three states the badge can be in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
    Working,
    Paused,
    Unavailable,
}

/// The target window's rectangle, in root-window pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// What the helper last said. Every pixel the badge draws is a function of this.
#[derive(Clone, Debug, PartialEq)]
pub struct TargetState {
    pub activity: Activity,
    /// Another application's name and window title: untrusted text, flattened and
    /// bounded by `label::target_label` before it is drawn.
    pub app: String,
    pub title: String,
    /// `None` when the helper cannot place the badge at all, which hides it.
    pub bounds: Option<Bounds>,
    pub occluded: bool,
}

impl TargetState {
    /// What the badge shows before the helper has said anything: nothing, since
    /// there are no bounds.
    pub fn initial() -> TargetState {
        TargetState {
            activity: Activity::Working,
            app: String::new(),
            title: String::new(),
            bounds: None,
            occluded: false,
        }
    }
}

/// Why a line was ignored. Every case is counted and none is fatal: a helper that
/// writes one bad line must not lose its badge.
#[derive(Debug, PartialEq)]
pub enum WireError {
    TooLong(usize),
    NotText,
    NotJson,
    NotAnObject,
    MissingState,
    UnknownState(String),
    BadValue(&'static str),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::TooLong(count) => write!(f, "line is {count} bytes"),
            WireError::NotText => write!(f, "line is not UTF-8"),
            WireError::NotJson => write!(f, "not valid JSON"),
            WireError::NotAnObject => write!(f, "not a JSON object"),
            WireError::MissingState => write!(f, "no \"state\""),
            // The state word is the helper's, not another application's, and it is
            // cut so a long one cannot flood the log.
            WireError::UnknownState(raw) => {
                write!(
                    f,
                    "unknown state \"{}\"",
                    raw.chars().take(32).collect::<String>()
                )
            }
            WireError::BadValue(key) => write!(f, "\"{key}\" has the wrong type"),
        }
    }
}

/// Parse one state line. Unknown keys are ignored, since a newer helper may add
/// some; a known key with the wrong type is not, because silently coercing one is
/// how a badge ends up over the wrong window.
pub fn decode(line: &str) -> Result<TargetState, WireError> {
    if line.len() > LINE_LIMIT {
        return Err(WireError::TooLong(line.len()));
    }
    let parsed: Value = serde_json::from_str(line).map_err(|_| WireError::NotJson)?;
    let Value::Object(object) = parsed else {
        return Err(WireError::NotAnObject);
    };

    let activity = match object.get("state") {
        None => return Err(WireError::MissingState),
        Some(Value::String(word)) => activity(word)?,
        Some(_other) => return Err(WireError::BadValue("state")),
    };

    Ok(TargetState {
        activity,
        app: text(&object, "app")?,
        title: text(&object, "title")?,
        bounds: bounds(&object)?,
        occluded: flag(&object, "occluded")?,
    })
}

fn activity(word: &str) -> Result<Activity, WireError> {
    match word {
        "working" => Ok(Activity::Working),
        "paused" => Ok(Activity::Paused),
        "unavailable" => Ok(Activity::Unavailable),
        unknown => Err(WireError::UnknownState(unknown.to_string())),
    }
}

/// A string, or empty when absent or null.
fn text(object: &Map<String, Value>, key: &'static str) -> Result<String, WireError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(text)) => Ok(text.clone()),
        Some(_other) => Err(WireError::BadValue(key)),
    }
}

/// A boolean, or false when absent or null.
fn flag(object: &Map<String, Value>, key: &'static str) -> Result<bool, WireError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_other) => Err(WireError::BadValue(key)),
    }
}

/// A rectangle with area, or `None` when absent or null.
fn bounds(object: &Map<String, Value>) -> Result<Option<Bounds>, WireError> {
    let rect = match object.get("bounds") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Object(rect)) => rect,
        Some(_other) => return Err(WireError::BadValue("bounds")),
    };
    let bounds = Bounds {
        x: coordinate(rect, "x", "bounds.x")?,
        y: coordinate(rect, "y", "bounds.y")?,
        w: coordinate(rect, "w", "bounds.w")?,
        h: coordinate(rect, "h", "bounds.h")?,
    };
    // A window with no area is not a window. Refusing it here keeps every later
    // rectangle finite and non-degenerate.
    if bounds.w <= 0.0 || bounds.h <= 0.0 {
        return Err(WireError::BadValue("bounds"));
    }
    Ok(Some(bounds))
}

/// A finite JSON number; a boolean is not one.
fn coordinate(rect: &Map<String, Value>, key: &str, name: &'static str) -> Result<f64, WireError> {
    match rect.get(key).and_then(Value::as_f64) {
        Some(value) if value.is_finite() => Ok(value),
        _otherwise => Err(WireError::BadValue(name)),
    }
}

/// The four lines this process can write. None carries a value, so no untrusted
/// text ever reaches stdout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Ready,
    Pause,
    Resume,
    Stop,
}

impl Event {
    pub fn line(self) -> &'static str {
        match self {
            Event::Ready => "{\"event\":\"ready\"}\n",
            Event::Pause => "{\"event\":\"pause\"}\n",
            Event::Resume => "{\"event\":\"resume\"}\n",
            Event::Stop => "{\"event\":\"stop\"}\n",
        }
    }
}

/// One line off the pipe, read without ever holding more than the limit.
#[derive(Debug, PartialEq)]
pub enum Line {
    Text(String),
    /// Longer than the limit; its bytes were read and dropped, and the count says
    /// how many.
    TooLong(usize),
    NotText,
}

/// Read the next line, or `None` at the end of the pipe. A line over `limit`
/// bytes is consumed to its newline and reported, never kept.
pub fn read_line(input: &mut impl BufRead, limit: usize) -> std::io::Result<Option<Line>> {
    let mut kept: Vec<u8> = Vec::new();
    let mut seen = 0usize;
    let mut interruptions = 0u32;

    // Bounded by the pipe: every pass consumes at least one byte, ends the line,
    // or counts an interruption against its cap.
    loop {
        let buffer = match input.fill_buf() {
            Ok(buffer) => buffer,
            Err(e)
                if e.kind() == std::io::ErrorKind::Interrupted
                    && interruptions < MAX_INTERRUPTIONS =>
            {
                interruptions += 1;
                continue;
            }
            Err(e) => return Err(e),
        };
        if buffer.is_empty() {
            return Ok(ended(kept, seen, limit));
        }

        let newline = buffer.iter().position(|&byte| byte == b'\n');
        let take = newline.unwrap_or(buffer.len());
        if seen + take <= limit {
            kept.extend_from_slice(&buffer[..take]);
        }
        seen += take;
        input.consume(newline.map_or(take, |at| at + 1));

        if newline.is_some() {
            return Ok(Some(finished(kept, seen, limit)));
        }
    }
}

/// The pipe ended: the last line, if it had any bytes, else the end.
fn ended(kept: Vec<u8>, seen: usize, limit: usize) -> Option<Line> {
    if seen == 0 {
        return None;
    }
    Some(finished(kept, seen, limit))
}

/// A whole line's bytes, without its ending.
fn finished(mut kept: Vec<u8>, seen: usize, limit: usize) -> Line {
    if seen > limit {
        return Line::TooLong(seen);
    }
    if kept.last() == Some(&b'\r') {
        kept.pop();
    }
    match String::from_utf8(kept) {
        Ok(text) => Line::Text(text),
        Err(_not_utf8) => Line::NotText,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(line: &str) -> TargetState {
        decode(line).unwrap_or_else(|e| panic!("{line}: {e}"))
    }

    #[test]
    fn a_full_state_line_is_read_field_by_field() {
        let read = state(
            r#"{"state":"paused","app":"Files","title":"Home","bounds":{"x":10,"y":-20.5,"w":640,"h":480},"occluded":true}"#,
        );
        assert_eq!(
            read,
            TargetState {
                activity: Activity::Paused,
                app: "Files".to_string(),
                title: "Home".to_string(),
                bounds: Some(Bounds {
                    x: 10.0,
                    y: -20.5,
                    w: 640.0,
                    h: 480.0
                }),
                occluded: true,
            }
        );
    }

    #[test]
    fn missing_or_null_optional_fields_read_as_empty_and_hidden() {
        let read = state(r#"{"state":"working","app":null,"bounds":null}"#);
        assert_eq!(read.app, "");
        assert_eq!(read.title, "");
        assert_eq!(read.bounds, None);
        assert!(!read.occluded);
        assert_eq!(
            state(r#"{"state":"unavailable"}"#).activity,
            Activity::Unavailable
        );
    }

    #[test]
    fn unknown_keys_from_a_newer_helper_are_ignored() {
        let read = state(r#"{"state":"working","app":"A","shiny":[1,2,3]}"#);
        assert_eq!(read.app, "A");
    }

    #[test]
    fn a_line_that_is_not_a_state_is_refused_by_name() {
        assert_eq!(decode("not json"), Err(WireError::NotJson));
        assert_eq!(decode("[1,2]"), Err(WireError::NotAnObject));
        assert_eq!(decode(r#"{"app":"A"}"#), Err(WireError::MissingState));
        assert_eq!(
            decode(r#"{"state":"sleeping"}"#),
            Err(WireError::UnknownState("sleeping".into()))
        );
        assert_eq!(decode(r#"{"state":7}"#), Err(WireError::BadValue("state")));
    }

    #[test]
    fn a_known_key_of_the_wrong_type_is_never_coerced() {
        assert_eq!(
            decode(r#"{"state":"working","app":3}"#),
            Err(WireError::BadValue("app"))
        );
        assert_eq!(
            decode(r#"{"state":"working","title":false}"#),
            Err(WireError::BadValue("title"))
        );
        assert_eq!(
            decode(r#"{"state":"working","occluded":1}"#),
            Err(WireError::BadValue("occluded"))
        );
        assert_eq!(
            decode(r#"{"state":"working","bounds":[1,2,3,4]}"#),
            Err(WireError::BadValue("bounds"))
        );
        assert_eq!(
            decode(r#"{"state":"working","bounds":{"x":true,"y":0,"w":1,"h":1}}"#),
            Err(WireError::BadValue("bounds.x"))
        );
        assert_eq!(
            decode(r#"{"state":"working","bounds":{"x":0,"y":0,"w":1}}"#),
            Err(WireError::BadValue("bounds.h"))
        );
    }

    #[test]
    fn a_window_with_no_area_is_not_a_window() {
        for bounds in [
            r#"{"x":0,"y":0,"w":0,"h":10}"#,
            r#"{"x":0,"y":0,"w":10,"h":-1}"#,
        ] {
            let line = format!(r#"{{"state":"working","bounds":{bounds}}}"#);
            assert_eq!(decode(&line), Err(WireError::BadValue("bounds")), "{line}");
        }
    }

    #[test]
    fn a_line_over_the_limit_is_refused_before_it_is_parsed() {
        let line = format!(
            r#"{{"state":"working","title":"{}"}}"#,
            "x".repeat(LINE_LIMIT)
        );
        assert_eq!(decode(&line), Err(WireError::TooLong(line.len())));
    }

    #[test]
    fn the_four_event_lines_are_exactly_what_the_helper_parses() {
        assert_eq!(Event::Ready.line(), "{\"event\":\"ready\"}\n");
        assert_eq!(Event::Pause.line(), "{\"event\":\"pause\"}\n");
        assert_eq!(Event::Resume.line(), "{\"event\":\"resume\"}\n");
        assert_eq!(Event::Stop.line(), "{\"event\":\"stop\"}\n");
    }

    #[test]
    fn lines_are_read_one_at_a_time_without_their_line_endings() {
        let mut input = std::io::Cursor::new(b"one\r\ntwo\n\nthree".to_vec());
        let mut read = Vec::new();
        for _ in 0..10 {
            match read_line(&mut input, 16).expect("read") {
                Some(line) => read.push(line),
                None => break,
            }
        }
        assert_eq!(
            read,
            vec![
                Line::Text("one".into()),
                Line::Text("two".into()),
                Line::Text(String::new()),
                Line::Text("three".into()),
            ]
        );
    }

    #[test]
    fn a_line_over_the_limit_is_dropped_whole_and_the_next_one_still_reads() {
        let mut bytes = vec![b'x'; 100];
        bytes.extend_from_slice(b"\nnext\n");
        let mut input = std::io::BufReader::with_capacity(8, std::io::Cursor::new(bytes));
        assert_eq!(
            read_line(&mut input, 16).expect("read"),
            Some(Line::TooLong(100))
        );
        assert_eq!(
            read_line(&mut input, 16).expect("read"),
            Some(Line::Text("next".into()))
        );
        assert_eq!(read_line(&mut input, 16).expect("read"), None);
    }

    #[test]
    fn a_line_that_is_not_utf8_is_named_as_such() {
        let mut input = std::io::Cursor::new(b"\xff\xfe\nok\n".to_vec());
        assert_eq!(
            read_line(&mut input, 16).expect("read"),
            Some(Line::NotText)
        );
        assert_eq!(
            read_line(&mut input, 16).expect("read"),
            Some(Line::Text("ok".into()))
        );
    }
}
