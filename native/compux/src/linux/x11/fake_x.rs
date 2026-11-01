//! A fake X server, for the X11 route's unit tests: one socket pair, no display.
//!
//! It answers the connection setup with one screen, `QueryExtension` (XTEST is
//! there, nothing else is) and `GetInputFocus` (the round trip a checked request
//! waits on), and it refuses every XTEST `FakeInput` with the error it was given,
//! as a server that will not take the input does. Any other request is answered
//! `BadImplementation`, so a test never waits on a reply the fake does not have.
//! It ends when compux's end of the socket closes, or when no request has come
//! for [`IDLE`], which closes the socket and so fails a test that would wait.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread::JoinHandle;
use std::time::Duration;

use x11rb::protocol::xproto::{
    GetInputFocusReply, InputFocus, QueryExtensionReply, Screen, Setup, GET_INPUT_FOCUS_REQUEST,
    QUERY_EXTENSION_REQUEST,
};
use x11rb::rust_connection::{DefaultStream, RustConnection};
use x11rb::x11_utils::Serialize;

/// The root window of the fake's one screen.
pub const ROOT: u32 = 0x100;

/// X's `BadValue`: a request with a value out of range.
pub const BAD_VALUE: u8 = 2;

/// X's `BadImplementation`: what the fake answers anything it does not know.
const BAD_IMPLEMENTATION: u8 = 17;

/// XTEST's major opcode on the fake, and `FakeInput`'s minor one.
const XTEST_MAJOR: u8 = 140;
const FAKE_INPUT: u8 = 2;

/// The most requests the fake answers: far more than any test sends.
const MAX_REQUESTS: usize = 4096;

/// How long the fake waits for the next request before it hangs up.
const IDLE: Duration = Duration::from_secs(10);

/// The size of every reply's fixed part, and of every error.
const PACKET: usize = 32;

/// The fake's thread.
pub struct FakeX {
    serving: Option<JoinHandle<()>>,
}

impl FakeX {
    /// The fake, refusing every `FakeInput` with `error`, and compux's connection
    /// to it.
    pub fn start(error: u8) -> (FakeX, RustConnection) {
        let (ours, theirs) = UnixStream::pair().expect("a socket pair");
        ours.set_read_timeout(Some(IDLE)).expect("an idle bound");
        let serving = std::thread::spawn(move || serve(ours, error));
        let (stream, _) = DefaultStream::from_unix_stream(theirs).expect("compux's end");
        let connection = RustConnection::connect_to_stream(stream, 0).expect("the fake's setup");
        let fake = FakeX {
            serving: Some(serving),
        };
        (fake, connection)
    }
}

impl Drop for FakeX {
    /// The thread ends once compux's end is closed; a test drops that end first.
    fn drop(&mut self) {
        if let Some(serving) = self.serving.take() {
            if serving.join().is_err() {
                eprintln!("fake X: the serving thread panicked");
            }
        }
    }
}

fn serve(mut stream: UnixStream, error: u8) {
    if let Err(why) = set_up(&mut stream) {
        eprintln!("fake X: the setup failed: {why}");
        return;
    }
    let mut sequence: u16 = 0;
    for _ in 0..MAX_REQUESTS {
        let Some((major, minor, body)) = read_request(&mut stream) else {
            return;
        };
        sequence = sequence.wrapping_add(1);
        let answer = answer((major, minor), &body, sequence, error);
        if let Err(why) = stream.write_all(&answer) {
            eprintln!("fake X: an answer was not written: {why}");
            return;
        }
    }
}

/// Read the client's setup request and answer it with one screen.
fn set_up(stream: &mut UnixStream) -> std::io::Result<()> {
    let mut request = [0u8; 12];
    stream.read_exact(&mut request)?;
    let name = usize::from(u16::from_ne_bytes([request[6], request[7]]));
    let data = usize::from(u16::from_ne_bytes([request[8], request[9]]));
    let mut authority = vec![0u8; padded(name) + padded(data)];
    stream.read_exact(&mut authority)?;
    let screen = Screen {
        root: ROOT,
        width_in_pixels: 1280,
        height_in_pixels: 800,
        root_depth: 24,
        ..Screen::default()
    };
    let setup = Setup {
        status: 1,
        protocol_major_version: 11,
        resource_id_base: 0x0020_0000,
        resource_id_mask: 0x001f_ffff,
        maximum_request_length: u16::MAX,
        min_keycode: 8,
        max_keycode: 255,
        roots: vec![screen],
        ..Setup::default()
    };
    let mut bytes = setup.serialize();
    let length = u16::try_from((bytes.len() - 8) / 4).expect("a small setup");
    bytes[6..8].copy_from_slice(&length.to_ne_bytes());
    stream.write_all(&bytes)
}

fn padded(length: usize) -> usize {
    length.div_ceil(4) * 4
}

/// One request: its major and minor opcode, and what follows its header. A
/// closed socket, a big request (length 0) or [`IDLE`] without one ends the fake.
fn read_request(stream: &mut UnixStream) -> Option<(u8, u8, Vec<u8>)> {
    let mut header = [0u8; 4];
    if let Err(error) = stream.read_exact(&mut header) {
        if error.kind() != std::io::ErrorKind::UnexpectedEof {
            eprintln!("fake X: hanging up: {error}");
        }
        return None;
    }
    let units = usize::from(u16::from_ne_bytes([header[2], header[3]]));
    let Some(length) = units.checked_mul(4).and_then(|bytes| bytes.checked_sub(4)) else {
        eprintln!("fake X: a big request, which the fake does not take");
        return None;
    };
    let mut body = vec![0u8; length];
    if let Err(error) = stream.read_exact(&mut body) {
        eprintln!("fake X: a request ended early: {error}");
        return None;
    }
    Some((header[0], header[1], body))
}

fn answer((major, minor): (u8, u8), body: &[u8], sequence: u16, error: u8) -> Vec<u8> {
    match major {
        QUERY_EXTENSION_REQUEST => {
            let length = usize::from(u16::from_ne_bytes([body[0], body[1]]));
            let present = body.get(4..4 + length) == Some(b"XTEST".as_slice());
            let reply = QueryExtensionReply {
                sequence,
                present,
                major_opcode: if present { XTEST_MAJOR } else { 0 },
                ..QueryExtensionReply::default()
            };
            packet(&reply.serialize())
        }
        GET_INPUT_FOCUS_REQUEST => {
            let reply = GetInputFocusReply {
                revert_to: InputFocus::NONE,
                sequence,
                length: 0,
                focus: ROOT,
            };
            packet(&reply.serialize())
        }
        XTEST_MAJOR if minor == FAKE_INPUT => refusal(error, sequence, major, minor),
        _ => refusal(BAD_IMPLEMENTATION, sequence, major, minor),
    }
}

/// A reply with no list: its fields, padded to the 32 bytes every reply has.
fn packet(fields: &[u8]) -> Vec<u8> {
    let mut packet = fields.to_vec();
    packet.resize(PACKET, 0);
    packet
}

/// An X error packet for the request `sequence` names.
fn refusal(code: u8, sequence: u16, major: u8, minor: u8) -> Vec<u8> {
    let mut packet = vec![0u8; PACKET];
    packet[1] = code;
    packet[2..4].copy_from_slice(&sequence.to_ne_bytes());
    packet[8..10].copy_from_slice(&u16::from(minor).to_ne_bytes());
    packet[10] = major;
    packet
}
