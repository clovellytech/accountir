pub mod commands;
pub mod config;
pub mod gnucash;
pub mod queries;
pub mod server;
pub mod store;
pub mod tui;

// Domain + event types live in the accountir-core crate. Re-export them so
// existing `crate::domain::...` and `crate::events::...` paths keep working.
pub use accountir_core::{domain, events};

pub use domain::*;
pub use events::*;
pub use store::*;
