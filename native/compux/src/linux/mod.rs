//! Linux: the accessibility reader behind `ax::Ax` (M86 L1), and what it needs from
//! the session.
//!
//! - `session`: which process a reference belongs to, from `/proc`;
//! - `display`: which display server this session runs, decided by its sockets,
//!   and the display a helper started before login takes from the user manager;
//! - `bus`: where the accessibility bus is, and a connection to it that answers;
//! - `probe`: what `probe` reports of the bus, its packages and the toolkits;
//! - `vocabulary`: AT-SPI's role and state numbers as words;
//! - `routes`: which verbs a node has a route for, from one read of it;
//! - `atspi`: the reader itself.
//!
//! Compiled only on Linux.

mod acting;
pub mod atspi;
pub mod bus;
pub mod display;
mod identity;
pub mod probe;
pub mod routes;
pub mod session;
pub mod vocabulary;

#[cfg(test)]
pub mod fake;
