pub mod event_log;
pub mod event_store;
pub mod merkle;
pub mod migrations;
pub mod projections;

// Phase-0 spike (SPEC §7): the `expected_head_seq` compare-and-append
// handshake under concurrent writers. Tests only — no product code path.
#[cfg(test)]
mod spike_compare_and_append;

pub use event_log::*;
pub use event_store::*;
pub use merkle::*;
pub use migrations::*;
pub use projections::*;
