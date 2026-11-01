//! The X `CLIPBOARD`, owned in-process for `paste` for as long as the session
//! lives.
//!
//! An X selection is not stored anywhere: its owner answers every application
//! that asks, for as long as it owns it. So compux keeps a window and a thread
//! that answers `SelectionRequest`s (`TARGETS` and `UTF8_STRING`, INCR for a large
//! text, per `selection`) from the moment of the first `paste` until the helper
//! exits, when it gives the selection up. The person's own text, read before a
//! paste and put back after it, is then compux's to serve as well.
//!
//! The thread and the worker share one connection, which x11rb allows: the thread
//! blocks in `wait_for_event`, the worker makes requests. The worker reads another
//! application's text by asking for it and waiting, bounded, for the thread to
//! hand it over.

use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConnectionExt as _, CreateWindowAux,
    EventMask, PropMode, Property, SelectionNotifyEvent, SelectionRequestEvent, WindowClass,
    SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, CURRENT_TIME, NONE};

use super::selection::{
    self, Answer, Atoms, Converted, Received, Receiving, Transfers, MAX_TEXT_BYTES,
};
use crate::linux::display::Env;

/// The most bytes one property carries; a larger text goes out in INCR chunks.
const CHUNK_BYTES_CAP: usize = 256 << 10;

/// How long a read of another application's text may take.
const READ_TIMEOUT: Duration = Duration::from_millis(500);

/// The selection owner: its window, its thread, and what it serves.
pub struct Clipboard {
    connection: Arc<RustConnection>,
    window: u32,
    names: Names,
    shared: Arc<Mutex<Shared>>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Clone, Copy)]
struct Names {
    clipboard: u32,
    atoms: Atoms,
    /// The property on compux's window another application's text arrives in.
    landing: u32,
    /// The message that tells the thread to stop.
    stop: u32,
    chunk: usize,
}

#[derive(Default)]
struct Shared {
    owned: Option<Arc<[u8]>>,
    transfers: Transfers,
    reading: Option<Reading>,
}

/// A read of another application's text in progress.
struct Reading {
    reply: mpsc::Sender<Option<Vec<u8>>>,
    incremental: Option<Receiving>,
}

impl Clipboard {
    /// A window of compux's own and the thread that answers for it.
    pub fn start(env: &Env) -> Result<Clipboard, String> {
        let (connection, screen) =
            super::connect(env).map_err(|why| format!("clipboard: {why}"))?;
        let root = connection
            .setup()
            .roots
            .get(screen)
            .map(|screen| screen.root)
            .ok_or("clipboard: the X display has no such screen")?;
        let names = names(&connection)?;
        let window = connection
            .generate_id()
            .map_err(|e| format!("clipboard: {e}"))?;
        connection
            .create_window(
                COPY_DEPTH_FROM_PARENT,
                window,
                root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                0,
                &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )
            .map_err(|e| format!("clipboard: {e}"))?;
        connection.flush().map_err(|e| format!("clipboard: {e}"))?;
        let connection = Arc::new(connection);
        let shared = Arc::new(Mutex::new(Shared::default()));
        let thread = {
            let (connection, shared) = (Arc::clone(&connection), Arc::clone(&shared));
            std::thread::Builder::new()
                .name("compux-clipboard".to_string())
                .spawn(move || serve(&connection, window, names, &shared))
                .map_err(|e| format!("clipboard: {e}"))?
        };
        Ok(Clipboard {
            connection,
            window,
            names,
            shared,
            thread: Some(thread),
        })
    }

    /// Own the clipboard with `text`, and prove it by asking who owns it.
    pub fn own(&mut self, text: &str) -> Result<(), String> {
        if text.len() > MAX_TEXT_BYTES {
            return Err(format!("clipboard set: over {MAX_TEXT_BYTES} bytes"));
        }
        lock(&self.shared).owned = Some(Arc::from(text.as_bytes()));
        let owner = self
            .connection
            .set_selection_owner(self.window, self.names.clipboard, CURRENT_TIME)
            .and_then(|_| self.connection.get_selection_owner(self.names.clipboard))
            .map_err(|e| format!("clipboard set: {e}"))?
            .reply()
            .map_err(|e| format!("clipboard set: {e}"))?
            .owner;
        if owner != self.window {
            return Err("clipboard set: another application kept the clipboard".to_string());
        }
        Ok(())
    }

    /// The clipboard's text now: compux's own where it owns it, else asked of
    /// its owner. `None` is "nothing text-shaped to save": no owner, a refusal, a
    /// text too large, or an owner that did not answer in time.
    pub fn text(&mut self) -> Result<Option<String>, String> {
        let owner = self
            .connection
            .get_selection_owner(self.names.clipboard)
            .map_err(|e| format!("clipboard: {e}"))?
            .reply()
            .map_err(|e| format!("clipboard: {e}"))?
            .owner;
        if owner == NONE {
            return Ok(None);
        }
        if owner == self.window {
            let owned = lock(&self.shared).owned.clone();
            return Ok(owned.and_then(|bytes| String::from_utf8(bytes.to_vec()).ok()));
        }
        let (reply, answer) = mpsc::channel();
        lock(&self.shared).reading = Some(Reading {
            reply,
            incremental: None,
        });
        self.connection
            .convert_selection(
                self.window,
                self.names.clipboard,
                self.names.atoms.utf8_string,
                self.names.landing,
                CURRENT_TIME,
            )
            .map_err(|e| format!("clipboard: {e}"))?;
        self.connection
            .flush()
            .map_err(|e| format!("clipboard: {e}"))?;
        let read = answer.recv_timeout(READ_TIMEOUT).ok().flatten();
        lock(&self.shared).reading = None;
        if read.is_none() {
            eprintln!("compux: the clipboard's text could not be read, so it is not put back");
        }
        Ok(read.and_then(|bytes| String::from_utf8(bytes).ok()))
    }
}

impl Drop for Clipboard {
    /// Give the selection up, and stop the thread.
    fn drop(&mut self) {
        let stop = ClientMessageEvent::new(32, self.window, self.names.stop, [0u32; 5]);
        let sent = self
            .connection
            .send_event(false, self.window, EventMask::NO_EVENT, stop)
            .and_then(|_| self.connection.flush());
        if let Err(error) = sent {
            eprintln!("compux: the clipboard owner could not be told to stop: {error}");
            return;
        }
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                eprintln!("compux: the clipboard owner's thread panicked");
            }
        }
    }
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn names(connection: &RustConnection) -> Result<Names, String> {
    let atom = |name: &str| -> Result<u32, String> {
        connection
            .intern_atom(false, name.as_bytes())
            .map_err(|e| format!("clipboard: {e}"))?
            .reply()
            .map(|reply| reply.atom)
            .map_err(|e| format!("clipboard: {e}"))
    };
    let largest = connection.maximum_request_bytes().saturating_sub(64);
    Ok(Names {
        clipboard: atom("CLIPBOARD")?,
        atoms: Atoms {
            targets: atom("TARGETS")?,
            utf8_string: atom("UTF8_STRING")?,
            incr: atom("INCR")?,
            atom: AtomEnum::ATOM.into(),
        },
        landing: atom("COMPUX_CLIPBOARD")?,
        stop: atom("COMPUX_CLIPBOARD_STOP")?,
        chunk: largest.clamp(1, CHUNK_BYTES_CAP),
    })
}

/// The thread: answer until told to stop or the display goes, then let go.
fn serve(connection: &RustConnection, window: u32, names: Names, shared: &Mutex<Shared>) {
    loop {
        let event = match connection.wait_for_event() {
            Ok(event) => event,
            Err(error) => {
                eprintln!("compux: the clipboard owner lost the X display: {error}");
                return;
            }
        };
        let handled = match event {
            Event::ClientMessage(message) if message.type_ == names.stop => break,
            Event::SelectionRequest(request) => answer_request(connection, names, shared, &request),
            Event::SelectionClear(clear) if clear.selection == names.clipboard => {
                lock(shared).owned = None;
                Ok(())
            }
            Event::PropertyNotify(notify) if notify.state == Property::DELETE => {
                next_chunk(connection, names, shared, notify.window, notify.atom)
            }
            Event::PropertyNotify(notify)
                if notify.window == window && notify.atom == names.landing =>
            {
                landed_chunk(connection, window, names, shared)
            }
            Event::SelectionNotify(notify) if notify.requestor == window => {
                converted(connection, window, names, shared, notify.property)
            }
            _ => Ok(()),
        };
        if let Err(error) = handled {
            eprintln!("compux: the clipboard owner: {error}");
        }
    }
    let released = connection
        .get_selection_owner(names.clipboard)
        .map_err(|e| e.to_string())
        .and_then(|cookie| cookie.reply().map_err(|e| e.to_string()))
        .and_then(|reply| {
            if reply.owner == window {
                connection
                    .set_selection_owner(NONE, names.clipboard, CURRENT_TIME)
                    .map_err(|e| e.to_string())?;
            }
            connection
                .destroy_window(window)
                .map_err(|e| e.to_string())?;
            connection.flush().map_err(|e| e.to_string())
        });
    if let Err(error) = released {
        eprintln!("compux: the clipboard was not given up: {error}");
    }
}

/// Answer one request, then tell the requestor where the answer is.
fn answer_request(
    connection: &RustConnection,
    names: Names,
    shared: &Mutex<Shared>,
    request: &SelectionRequestEvent,
) -> Result<(), String> {
    // An old client names no property; ICCCM says the target serves as one.
    let property = if request.property == NONE {
        request.target
    } else {
        request.property
    };
    let owned = lock(shared).owned.clone();
    let owned = owned.filter(|_| request.selection == names.clipboard);
    let answer = selection::answer(request.target, owned.as_ref(), &names.atoms, names.chunk);
    let place = (request.requestor, property);
    let answered = write_answer(connection, names.atoms, shared, place, answer)?;
    let notify = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: request.time,
        requestor: request.requestor,
        selection: request.selection,
        target: request.target,
        property: answered,
    };
    connection
        .send_event(false, request.requestor, EventMask::NO_EVENT, notify)
        .map_err(|e| e.to_string())?;
    connection.flush().map_err(|e| e.to_string())
}

/// Write an answer into the requestor's property: the property it is in, or
/// `NONE` for a refusal.
fn write_answer(
    connection: &RustConnection,
    atoms: Atoms,
    shared: &Mutex<Shared>,
    (requestor, property): (u32, u32),
    answer: Answer,
) -> Result<u32, String> {
    match answer {
        Answer::Refuse => return Ok(NONE),
        Answer::Targets(targets) => connection
            .change_property32(PropMode::REPLACE, requestor, property, atoms.atom, &targets)
            .map(drop)
            .map_err(|e| e.to_string())?,
        Answer::Text(text) => connection
            .change_property8(
                PropMode::REPLACE,
                requestor,
                property,
                atoms.utf8_string,
                &text,
            )
            .map(drop)
            .map_err(|e| e.to_string())?,
        Answer::Incr(text) => start_incr(connection, atoms, shared, (requestor, property), text)?,
    }
    Ok(property)
}

/// Announce an INCR transfer of `text`, and watch the requestor's property for
/// each chunk it takes.
fn start_incr(
    connection: &RustConnection,
    atoms: Atoms,
    shared: &Mutex<Shared>,
    (requestor, property): (u32, u32),
    text: Arc<[u8]>,
) -> Result<(), String> {
    let total = u32::try_from(text.len()).unwrap_or(u32::MAX);
    connection
        .change_window_attributes(
            requestor,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .map_err(|e| e.to_string())?;
    connection
        .change_property32(PropMode::REPLACE, requestor, property, atoms.incr, &[total])
        .map_err(|e| e.to_string())?;
    lock(shared).transfers.start(requestor, property, text);
    Ok(())
}

/// A requestor took the last chunk of an INCR transfer: write the next one.
fn next_chunk(
    connection: &RustConnection,
    names: Names,
    shared: &Mutex<Shared>,
    window: u32,
    property: u32,
) -> Result<(), String> {
    let next = lock(shared)
        .transfers
        .deleted(window, property, names.chunk);
    let Some(chunk) = next else {
        return Ok(());
    };
    connection
        .change_property8(
            PropMode::REPLACE,
            window,
            property,
            names.atoms.utf8_string,
            &chunk,
        )
        .map_err(|e| e.to_string())?;
    connection.flush().map_err(|e| e.to_string())
}

/// The owner answered compux's request for its text.
fn converted(
    connection: &RustConnection,
    window: u32,
    names: Names,
    shared: &Mutex<Shared>,
    property: u32,
) -> Result<(), String> {
    if property == NONE {
        return finish_read(shared, None);
    }
    let landed = take_landed(connection, window, names)?;
    let read = selection::converted(landed.type_, landed.value, landed.bytes_after, &names.atoms);
    match read {
        Converted::Text(text) => finish_read(shared, Some(text)),
        Converted::TooLarge => finish_read(shared, None),
        Converted::Incremental => {
            // Deleting the INCR property (done by the read) starts the chunks.
            if let Some(reading) = lock(shared).reading.as_mut() {
                reading.incremental = Some(Receiving::default());
            }
            Ok(())
        }
    }
}

/// One chunk of an INCR read arrived.
fn landed_chunk(
    connection: &RustConnection,
    window: u32,
    names: Names,
    shared: &Mutex<Shared>,
) -> Result<(), String> {
    let incremental = lock(shared)
        .reading
        .as_ref()
        .is_some_and(|reading| reading.incremental.is_some());
    if !incremental {
        return Ok(());
    }
    let landed = take_landed(connection, window, names)?;
    let step = lock(shared)
        .reading
        .as_mut()
        .and_then(|reading| reading.incremental.as_mut())
        .map(|receiving| receiving.chunk(&landed.value, landed.bytes_after));
    match step {
        Some(Received::Done(text)) => finish_read(shared, Some(text)),
        Some(Received::TooLarge) => finish_read(shared, None),
        Some(Received::More) | None => Ok(()),
    }
}

/// The landing property as one read found it.
struct Landed {
    type_: u32,
    value: Vec<u8>,
    /// What the read left behind; anything is more than compux keeps.
    bytes_after: u32,
}

/// Read and delete the landing property. The read is as long as the largest
/// text compux keeps; the server deletes only a property read to its end, so one
/// with more left is deleted here, and none of a text too large stays on
/// compux's window. (An INCR owner may then write one more chunk, which no read
/// takes; the next read replaces it.)
fn take_landed(connection: &RustConnection, window: u32, names: Names) -> Result<Landed, String> {
    let longest = u32::try_from(MAX_TEXT_BYTES / 4).unwrap_or(u32::MAX);
    let reply = connection
        .get_property(true, window, names.landing, AtomEnum::ANY, 0, longest)
        .map_err(|e| e.to_string())?
        .reply()
        .map_err(|e| e.to_string())?;
    if reply.bytes_after != 0 {
        connection
            .delete_property(window, names.landing)
            .map_err(|e| e.to_string())?;
    }
    Ok(Landed {
        type_: reply.type_,
        value: reply.value,
        bytes_after: reply.bytes_after,
    })
}

fn finish_read(shared: &Mutex<Shared>, text: Option<Vec<u8>>) -> Result<(), String> {
    if let Some(reading) = lock(shared).reading.take() {
        // The worker may have stopped waiting: then the text arrived too late.
        if reading.reply.send(text).is_err() {
            eprintln!("compux: the clipboard's text arrived after its reader gave up");
        }
    }
    Ok(())
}
