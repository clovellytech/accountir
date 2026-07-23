//! Server-side command endpoints for the sync transport. Each command family
//! lives in its own submodule and exposes a `router() -> Router<SyncState>`;
//! this module merges them. Every command endpoint follows the same contract as
//! `post-entry` (see `sync/mod.rs`): bearer-authenticated, runs the command's
//! real domain invariants *inside* the `append_checked` transaction under the
//! client's `expected_head_seq`, and returns 200 + new head / 409 stale head /
//! 422 domain rejection. Add a new command by filling in a submodule's `router()`.

use crate::sync::SyncState;
use axum::Router;

pub mod account;
pub mod bill;
pub mod bill_ops;
pub mod entry_ops;
pub mod reconciliation;

pub fn router() -> Router<SyncState> {
    Router::new()
        .merge(account::router())
        .merge(bill::router())
        .merge(bill_ops::router())
        .merge(entry_ops::router())
        .merge(reconciliation::router())
}
