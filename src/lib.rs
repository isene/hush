//! The parts of hush that both ends share: the shape of a packet, and
//! the relay that passes packets between two people who cannot reach
//! each other directly.
//!
//! The relay needs none of the sound handling, so it is a binary of its
//! own and builds on a server with nothing installed but Rust.

pub mod net;
