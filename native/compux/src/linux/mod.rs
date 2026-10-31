//! Linux: the accessibility reader behind `ax::Ax` (M86 L1), and what it needs from
//! the session.
//!
//! - `session`: which process a reference belongs to, from `/proc`;
//! - `bus`: where the accessibility bus is, and a connection to it that answers;
//! - `vocabulary`: AT-SPI's role and state numbers as words;
//! - `atspi`: the reader itself.
//!
//! Compiled only on Linux. Nothing here acts on a control yet: the reader reads.

pub mod atspi;
pub mod bus;
pub mod session;
pub mod vocabulary;

#[cfg(test)]
pub mod fake;
