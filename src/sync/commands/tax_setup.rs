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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::event_store::EventStore;
    use crate::store::migrations::{init_schema, run_migrations};
    use crate::sync::router;
    use std::collections::HashMap;

    const TOKEN: &str = "tok-1";

    async fn serve() -> (String, std::sync::Arc<std::sync::Mutex<EventStore>>) {
        let store = {
            let s = EventStore::in_memory().unwrap();
            init_schema(s.connection()).unwrap();
            run_migrations(s.connection()).unwrap();
            s
        };
        let state = SyncState::new(
            store,
            HashMap::from([(TOKEN.to_string(), "alice@example.com".to_string())]),
        );
        let handle = state.store.clone();
        let app = router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), handle)
    }

    async fn head_of(base: &str) -> i64 {
        reqwest::Client::new()
            .get(format!("{base}/sync/head"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap()["head"]
            .as_i64()
            .unwrap()
    }

    async fn post(base: &str, path: &str, body: serde_json::Value) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("{base}/sync/commands/{path}"))
            .bearer_auth(TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    /// The round trip a member on group-hosted books actually makes. Without
    /// these routes working, the mapping editor is dead on a replica — which is
    /// exactly how it shipped before this test existed.
    #[tokio::test]
    async fn a_replica_can_map_an_account_to_a_schedule_l_line() {
        let (base, store) = serve().await;
        let head = head_of(&base).await;

        let r = post(
            &base,
            "set-tax-line-mapping",
            serde_json::json!({
                "expected_head_seq": head,
                "account_id": "checking",
                "line_key": "sl1",
            }),
        )
        .await;
        assert_eq!(r.status(), 200, "{:?}", r.text().await);

        let guard = store.lock().unwrap();
        assert_eq!(
            crate::tax::lines::load_mapping(guard.connection())
                .get("checking")
                .map(String::as_str),
            Some("sl1"),
            "the instance did not project the mapping"
        );
    }

    #[tokio::test]
    async fn a_replica_can_take_an_account_off_the_return() {
        let (base, store) = serve().await;
        let head = head_of(&base).await;
        post(
            &base,
            "set-tax-line-mapping",
            serde_json::json!({"expected_head_seq": head, "account_id": "checking", "line_key": "sl1"}),
        )
        .await;
        let head = head_of(&base).await;
        let r = post(
            &base,
            "clear-tax-line-mapping",
            serde_json::json!({"expected_head_seq": head, "account_id": "checking"}),
        )
        .await;
        assert_eq!(r.status(), 200);

        let guard = store.lock().unwrap();
        assert!(crate::tax::lines::load_mapping(guard.connection()).is_empty());
    }

    #[tokio::test]
    async fn a_replica_can_answer_and_unanswer_schedule_b() {
        let (base, store) = serve().await;
        let head = head_of(&base).await;
        let r = post(
            &base,
            "set-schedule-b-answer",
            serde_json::json!({"expected_head_seq": head, "tax_year": 2025, "answer_key": "b5", "value": "no"}),
        )
        .await;
        assert_eq!(r.status(), 200, "{:?}", r.text().await);
        {
            let guard = store.lock().unwrap();
            assert_eq!(
                crate::tax::schedule_b::load(guard.connection(), 2025).get("b5"),
                Some("no")
            );
        }

        // Empty clears, and unanswered is not the same as No.
        let head = head_of(&base).await;
        let r = post(
            &base,
            "set-schedule-b-answer",
            serde_json::json!({"expected_head_seq": head, "tax_year": 2025, "answer_key": "b5", "value": ""}),
        )
        .await;
        assert_eq!(r.status(), 200);
        let guard = store.lock().unwrap();
        assert_eq!(
            crate::tax::schedule_b::load(guard.connection(), 2025).get("b5"),
            None
        );
    }

    /// A key the catalogue does not have is the caller's mistake, not the books
    /// refusing — 400, so a client does not go looking at the ledger for a typo.
    #[tokio::test]
    async fn a_line_key_the_catalogue_does_not_have_is_a_bad_request() {
        let (base, _) = serve().await;
        let head = head_of(&base).await;
        let r = post(
            &base,
            "set-tax-line-mapping",
            serde_json::json!({"expected_head_seq": head, "account_id": "x", "line_key": "l99"}),
        )
        .await;
        assert_eq!(r.status(), 400);
    }

    #[tokio::test]
    async fn a_schedule_b_key_the_catalogue_does_not_have_is_a_bad_request() {
        let (base, _) = serve().await;
        let head = head_of(&base).await;
        let r = post(
            &base,
            "set-schedule-b-answer",
            serde_json::json!({"expected_head_seq": head, "tax_year": 2025, "answer_key": "b99", "value": "yes"}),
        )
        .await;
        assert_eq!(r.status(), 400);
    }

    /// A stale head is a 409, which is what makes the client's blind retry safe.
    #[tokio::test]
    async fn a_stale_head_is_refused_rather_than_applied() {
        let (base, _) = serve().await;
        let head = head_of(&base).await;
        post(
            &base,
            "set-tax-line-mapping",
            serde_json::json!({"expected_head_seq": head, "account_id": "a", "line_key": "sl1"}),
        )
        .await;
        let r = post(
            &base,
            "set-tax-line-mapping",
            serde_json::json!({"expected_head_seq": head, "account_id": "b", "line_key": "sl3"}),
        )
        .await;
        assert_eq!(r.status(), 409);
    }
}
