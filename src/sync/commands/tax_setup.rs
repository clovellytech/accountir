//! Sync commands for the Form 1065 setup.
//!
//! A member on group-hosted books cannot append to the log directly — the ledger
//! is a replica and the instance owns the writes. So every change to the mapping
//! or to Schedule B has to arrive here, be checked, and be appended by the
//! instance, exactly as the partnership commands are.
//!
//! Without these routes the setup would be event-sourced and still unusable on
//! hosted books: the desktop would build an event it had no way to submit.
//!
//! The domain checks live in [`crate::events::validation`] rather than here, so
//! a line key nobody recognises is refused identically whether it came from a
//! local command or over the wire.

use crate::commands::partnership_commands::PartnershipError;
use crate::events::types::Event;
use crate::store::event_store::Verdict;
use crate::sync::{outcome_to_response, project, stamp, ApiError, AuthedUser, SyncState};
use axum::{extract::State, routing::post, Json, Router};
use serde::{Deserialize, Serialize};

pub fn router() -> Router<SyncState> {
    Router::new()
        .route(
            "/sync/commands/set-tax-line-mapping",
            post(submit_set_mapping),
        )
        .route(
            "/sync/commands/clear-tax-line-mapping",
            post(submit_clear_mapping),
        )
        .route(
            "/sync/commands/set-schedule-b-answer",
            post(submit_set_answer),
        )
}

#[derive(Serialize, Deserialize)]
pub struct SetTaxLineMappingRequest {
    pub expected_head_seq: i64,
    pub account_id: String,
    /// A key from `tax::lines::MAPPABLE_LINES` — `l21`, `k13a`, `sl1`. The key
    /// and not the printed number, because the IRS renumbers between revisions.
    pub line_key: String,
}

#[derive(Serialize, Deserialize)]
pub struct ClearTaxLineMappingRequest {
    pub expected_head_seq: i64,
    pub account_id: String,
}

#[derive(Serialize, Deserialize)]
pub struct SetScheduleBAnswerRequest {
    pub expected_head_seq: i64,
    pub tax_year: i32,
    pub answer_key: String,
    /// Empty clears the answer back to unanswered, which is a different state
    /// from "No" and gets its own event.
    pub value: String,
}

async fn submit_set_mapping(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<SetTaxLineMappingRequest>,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    // Checked before the append rather than inside it: `validate_event` would
    // catch this too, but as a store error — a 500 for what is squarely the
    // caller's mistake. Same reasoning as `set-business-profile`.
    if crate::tax::lines::line_def(&req.line_key).is_none() {
        return Err(ApiError::bad_request(
            "line_key is not a Form 1065 line this version knows",
        ));
    }
    if req.account_id.trim().is_empty() {
        return Err(ApiError::bad_request("account_id is required"));
    }
    append(
        st,
        req.expected_head_seq,
        actor,
        Event::TaxLineMappingSet {
            account_id: req.account_id,
            line_key: req.line_key,
        },
    )
}

async fn submit_clear_mapping(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<ClearTaxLineMappingRequest>,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    if req.account_id.trim().is_empty() {
        return Err(ApiError::bad_request("account_id is required"));
    }
    append(
        st,
        req.expected_head_seq,
        actor,
        Event::TaxLineMappingCleared {
            account_id: req.account_id,
        },
    )
}

async fn submit_set_answer(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<SetScheduleBAnswerRequest>,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    if !crate::tax::schedule_b::known_key(&req.answer_key) {
        return Err(ApiError::bad_request(
            "answer_key is not a Schedule B question this version knows",
        ));
    }
    if !(1900..=2200).contains(&req.tax_year) {
        return Err(ApiError::bad_request("tax_year is not a tax year"));
    }
    let value = req.value.trim();
    let event = if value.is_empty() {
        Event::ScheduleBAnswerCleared {
            tax_year: req.tax_year,
            answer_key: req.answer_key,
        }
    } else {
        Event::ScheduleBAnswerSet {
            tax_year: req.tax_year,
            answer_key: req.answer_key,
            value: value.to_string(),
        }
    };
    append(st, req.expected_head_seq, actor, event)
}

/// The shared tail of all three: stamp the actor on, append under the client's
/// head, project.
///
/// No state-dependent check in the transaction, deliberately. Both of these are
/// last-writer-wins by nature — pointing an account at a line it already reports
/// on, or answering a question twice, is idempotent — so there is no invariant
/// that two concurrent writers could break, only an ordering the log records.
fn append(
    st: SyncState,
    expected_head_seq: i64,
    actor: String,
    event: Event,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            expected_head_seq,
            move |_tx| {
                Ok(Verdict::<_, PartnershipError>::Append(stamp(
                    event.clone(),
                    &actor,
                )))
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response(outcome, ApiError::domain::<PartnershipError>)
}
