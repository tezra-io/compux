//! Owning the X `CLIPBOARD` for `paste`, as decisions: what a `SelectionRequest`
//! is answered, how a text too large for one property goes out in INCR chunks,
//! and how the person's own text is read back in them. Pure; `clipboard` applies
//! it to the connection.
//!
//! Only `TARGETS` and `UTF8_STRING` are answered; any other target is refused
//! (property `None`), which every toolkit takes as "ask for something else".

use std::collections::VecDeque;
use std::sync::Arc;

/// The largest text compux owns or reads back. A larger text read from another
/// application is not kept, and is not put back.
pub const MAX_TEXT_BYTES: usize = 4 << 20;

/// The most INCR transfers in flight at once; the oldest goes when another starts.
pub const MAX_TRANSFERS: usize = 8;

/// The atoms a request is answered in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Atoms {
    pub targets: u32,
    pub utf8_string: u32,
    pub incr: u32,
    pub atom: u32,
}

/// What one request is answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Property `None`: not a target compux has, or nothing is owned.
    Refuse,
    /// The targets, as a list of atoms.
    Targets(Vec<u32>),
    /// The whole text in one property.
    Text(Arc<[u8]>),
    /// Too large for one property: an `INCR` property with the total, then chunks.
    Incr(Arc<[u8]>),
}

/// The answer to a request for `target` while owning `text`; `chunk` is the most
/// bytes one property may carry.
pub fn answer(target: u32, text: Option<&Arc<[u8]>>, atoms: &Atoms, chunk: usize) -> Answer {
    let Some(text) = text else {
        return Answer::Refuse;
    };
    if target == atoms.targets {
        return Answer::Targets(vec![atoms.targets, atoms.utf8_string]);
    }
    if target != atoms.utf8_string {
        return Answer::Refuse;
    }
    if text.len() <= chunk {
        Answer::Text(Arc::clone(text))
    } else {
        Answer::Incr(Arc::clone(text))
    }
}

/// One INCR transfer: the text, and how much of it has gone out.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Transfer {
    requestor: u32,
    property: u32,
    data: Arc<[u8]>,
    sent: usize,
}

/// The INCR transfers in flight, oldest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Transfers {
    list: VecDeque<Transfer>,
}

impl Transfers {
    /// Begin a transfer, after the `INCR` property is written. Starting one past
    /// [`MAX_TRANSFERS`] drops the oldest, which has stalled.
    pub fn start(&mut self, requestor: u32, property: u32, data: Arc<[u8]>) {
        self.list
            .retain(|t| !(t.requestor == requestor && t.property == property));
        if self.list.len() >= MAX_TRANSFERS {
            self.list.pop_front();
        }
        self.list.push_back(Transfer {
            requestor,
            property,
            data,
            sent: 0,
        });
    }

    /// The requestor deleted the property: the next chunk to write, or `None`
    /// where no transfer is waiting on it. The empty chunk ends the transfer.
    pub fn deleted(&mut self, requestor: u32, property: u32, chunk: usize) -> Option<Vec<u8>> {
        let at = self
            .list
            .iter()
            .position(|t| t.requestor == requestor && t.property == property)?;
        let transfer = &mut self.list[at];
        let end = (transfer.sent + chunk.max(1)).min(transfer.data.len());
        let next = transfer.data[transfer.sent..end].to_vec();
        transfer.sent = end;
        if next.is_empty() {
            self.list.remove(at);
        }
        Some(next)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.list.len()
    }
}

/// What the first read of another application's answer holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Converted {
    /// The whole text.
    Text(Vec<u8>),
    /// An `INCR` property: the text follows in chunks.
    Incremental,
    /// More than one read takes, so more than compux keeps.
    TooLarge,
}

/// The answer as one read of it found it. `bytes_after` is what the read left
/// behind: a read is as long as the largest text compux keeps, so anything left
/// is too large.
pub fn converted(type_: u32, value: Vec<u8>, bytes_after: u32, atoms: &Atoms) -> Converted {
    if bytes_after != 0 {
        return Converted::TooLarge;
    }
    if type_ == atoms.incr {
        return Converted::Incremental;
    }
    Converted::Text(value)
}

/// Reading another application's text that came in INCR chunks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Receiving {
    data: Vec<u8>,
}

/// After one chunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Received {
    More,
    Done(Vec<u8>),
    TooLarge,
}

impl Receiving {
    /// One chunk; the empty one ends the text. `bytes_after` is what the read of
    /// it left behind.
    pub fn chunk(&mut self, bytes: &[u8], bytes_after: u32) -> Received {
        if bytes_after != 0 {
            self.data.clear();
            return Received::TooLarge;
        }
        if bytes.is_empty() {
            return Received::Done(std::mem::take(&mut self.data));
        }
        if self.data.len() + bytes.len() > MAX_TEXT_BYTES {
            self.data.clear();
            return Received::TooLarge;
        }
        self.data.extend_from_slice(bytes);
        Received::More
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ATOMS: Atoms = Atoms {
        targets: 10,
        utf8_string: 11,
        incr: 12,
        atom: 4,
    };

    fn text(s: &str) -> Arc<[u8]> {
        Arc::from(s.as_bytes())
    }

    #[test]
    fn targets_name_what_is_offered_and_utf8_string_carries_the_text() {
        let owned = text("hello");
        assert_eq!(
            answer(ATOMS.targets, Some(&owned), &ATOMS, 100),
            Answer::Targets(vec![10, 11])
        );
        assert_eq!(
            answer(ATOMS.utf8_string, Some(&owned), &ATOMS, 100),
            Answer::Text(owned)
        );
    }

    #[test]
    fn any_other_target_or_nothing_owned_is_refused() {
        let owned = text("hello");
        assert_eq!(answer(31, Some(&owned), &ATOMS, 100), Answer::Refuse);
        assert_eq!(answer(ATOMS.utf8_string, None, &ATOMS, 100), Answer::Refuse);
        assert_eq!(answer(ATOMS.targets, None, &ATOMS, 100), Answer::Refuse);
    }

    #[test]
    fn a_text_larger_than_one_property_goes_out_incrementally() {
        let owned = text("abcdefg");
        assert_eq!(
            answer(ATOMS.utf8_string, Some(&owned), &ATOMS, 3),
            Answer::Incr(Arc::clone(&owned))
        );

        let mut transfers = Transfers::default();
        transfers.start(77, 5, owned);
        assert_eq!(transfers.deleted(77, 5, 3), Some(b"abc".to_vec()));
        assert_eq!(transfers.deleted(77, 6, 3), None, "another property");
        assert_eq!(transfers.deleted(77, 5, 3), Some(b"def".to_vec()));
        assert_eq!(transfers.deleted(77, 5, 3), Some(b"g".to_vec()));
        assert_eq!(transfers.deleted(77, 5, 3), Some(Vec::new()), "the end");
        assert_eq!(transfers.deleted(77, 5, 3), None, "done and gone");
        assert_eq!(transfers.len(), 0);
    }

    #[test]
    fn transfers_in_flight_are_bounded_and_the_oldest_goes_first() {
        let mut transfers = Transfers::default();
        for requestor in 0..(MAX_TRANSFERS as u32 + 2) {
            transfers.start(requestor, 5, text("abcdef"));
        }
        assert_eq!(transfers.len(), MAX_TRANSFERS);
        assert_eq!(transfers.deleted(0, 5, 3), None, "the oldest was dropped");
        assert_eq!(transfers.deleted(1, 5, 3), None);
        assert_eq!(transfers.deleted(2, 5, 3), Some(b"abc".to_vec()));
    }

    #[test]
    fn a_text_read_in_chunks_ends_at_the_empty_one_and_a_huge_one_is_dropped() {
        let mut receiving = Receiving::default();
        assert_eq!(receiving.chunk(b"ab", 0), Received::More);
        assert_eq!(receiving.chunk(b"cd", 0), Received::More);
        assert_eq!(receiving.chunk(b"", 0), Received::Done(b"abcd".to_vec()));

        let mut huge = Receiving::default();
        let big = vec![b'x'; MAX_TEXT_BYTES];
        assert_eq!(huge.chunk(&big, 0), Received::More);
        assert_eq!(huge.chunk(b"y", 0), Received::TooLarge);
    }

    // A read takes at most what compux keeps; the server says what it left
    // behind. Anything left is a text too large to keep, never a cut one.
    #[test]
    fn an_answer_one_read_does_not_reach_the_end_of_is_too_large_not_cut() {
        let whole = converted(ATOMS.utf8_string, b"abc".to_vec(), 0, &ATOMS);
        let cut = converted(ATOMS.utf8_string, b"abc".to_vec(), 4, &ATOMS);
        let incr = converted(ATOMS.incr, vec![0, 0, 0, 9], 0, &ATOMS);

        assert_eq!(whole, Converted::Text(b"abc".to_vec()));
        assert_eq!(cut, Converted::TooLarge);
        assert_eq!(incr, Converted::Incremental);
    }

    #[test]
    fn a_chunk_one_read_does_not_reach_the_end_of_ends_the_text_as_too_large() {
        let mut receiving = Receiving::default();
        assert_eq!(receiving.chunk(b"ab", 0), Received::More);

        assert_eq!(receiving.chunk(b"cd", 1), Received::TooLarge);
        assert_eq!(
            receiving.chunk(b"", 0),
            Received::Done(Vec::new()),
            "nothing of the text is kept"
        );
    }
}
