pub mod event_store;
pub mod projections;

pub use event_store::{append_event, StoreError};
