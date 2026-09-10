//! Sync commands for the Schedule C setup.
//!
//! A member on group-hosted books cannot append to their own log — the ledger is
//! a replica and the instance owns the writes. Without these routes the Schedule
//! C setup would be event-sourced and still unusable on hosted books: the desktop
//! would build an event it had no way to submit, and the write gate would refuse
//! it with a message about the feature not being available on server-hosted
//! books, which would be true and unhelpful.
//!
//! Three commands, and the business type is the one that has to exist for the
//! other two to be reachable at all — a member who cannot say the books are a
//! sole proprietorship never gets shown the proprietor card.
//!
//! # What is deliberately not here
//!
//! The social security number. It has no event and no route, because it has no
//! business leaving the machine it was typed on: the log is replicated in full
//! and cannot be redacted afterwards, so an SSN submitted here would be on every
//! member's laptop permanently. `sole_proprietor_tin` stays local, and the
//! consequence — that a colleague's Schedule C comes out with the number box
//! blank — is the intended one.
//!
//! The domain checks live in [`crate::events::validation`] rather than here, so a
//! malformed record is refused identically whether it came from a local command
//! or over the wire. What this module checks is only what deserves a 400 rather
//! than a 500: a caller's mistake, named before the store ever sees it.

use crate::commands::partnership_commands::PartnershipError;
use crate::events::types::{Event, SoleProprietorData};
use crate::store::event_store::Verdict;
use crate::sync::{outcome_to_response, project, stamp, ApiError, AuthedUser, SyncState};
use axum::{extract::State, routing::post, Json, Router};
use serde::{Deserialize, Serialize};

pub fn router() -> Router<SyncState> {
    Router::new()
        .route("/sync/commands/set-business-type", post(submit_set_type))
        .route(
            "/sync/commands/set-sole-proprietor",
            post(submit_set_proprietor),
        )
        .route(
            "/sync/commands/set-schedule-c-answer",
            post(submit_set_answer),
        )
}

#[derive(Serialize, Deserialize)]
pub struct SetBusinessTypeRequest {
    pub expected_head_seq: i64,
    /// "partnership" or "sole_proprietorship" — see [`crate::domain::BusinessType`].
    pub business_type: String,
}

#[derive(Serialize, Deserialize)]
pub struct SetSoleProprietorRequest {
    pub expected_head_seq: i64,
    pub name: String,
    /// "cash", "accrual" or "other" — Schedule C line F.
    pub accounting_method: String,
    /// What line F(3) prints when the method is "other".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accounting_method_other: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct SetScheduleCAnswerRequest {
    pub expected_head_seq: i64,
    pub tax_year: i32,
    pub answer_key: String,
    /// Empty clears the answer back to unanswered, which is a different state
    /// from "No" and gets its own event.
    pub value: String,
}

async fn submit_set_type(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<SetBusinessTypeRequest>,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    // Checked before the append rather than inside it: `validate_event` would
    // catch this too, but as a store error — a 500 for what is squarely the
    // caller's mistake. The same reasoning `set-tax-line-mapping` gives.
    if crate::domain::BusinessType::parse(&req.business_type).is_none() {
        return Err(ApiError::bad_request(
            "business_type is not one this version knows",
        ));
    }
    append(
        st,
        req.expected_head_seq,
        actor,
        Event::BusinessTypeSet {
            business_type: req.business_type,
        },
    )
}

async fn submit_set_proprietor(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<SetSoleProprietorRequest>,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    if req.name.trim().is_empty() {
        return Err(ApiError::bad_request("name is required"));
    }
    let Some(method) = crate::domain::AccountingMethod::parse(&req.accounting_method) else {
        return Err(ApiError::bad_request(
            "accounting_method is not one this version knows",
        ));
    };
    // Line F(3) makes you name the method. Refused here rather than left to
    // validation so the caller gets a 400 that says which field is missing.
    let other = req
        .accounting_method_other
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if method == crate::domain::AccountingMethod::Other && other.is_none() {
        return Err(ApiError::bad_request(
            "accounting_method_other is required when the method is \"other\" — Schedule C line \
             F(3) asks which",
        ));
    }

    append(
        st,
        req.expected_head_seq,
        actor,
        Event::SoleProprietorSet(Box::new(SoleProprietorData {
            name: req.name.trim().to_string(),
            accounting_method: req.accounting_method,
            accounting_method_other: other,
        })),
    )
}

async fn submit_set_answer(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<SetScheduleCAnswerRequest>,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    if crate::tax::schedule_c::question(&req.answer_key).is_none() {
        return Err(ApiError::bad_request(
            "answer_key is not a Schedule C question this version knows",
        ));
    }
    if !(1900..=2200).contains(&req.tax_year) {
        return Err(ApiError::bad_request("tax_year is not a tax year"));
    }
    let value = req.value.trim();
    let event = if value.is_empty() {
        Event::ScheduleCAnswerCleared {
            tax_year: req.tax_year,
            answer_key: req.answer_key,
        }
    } else {
        Event::ScheduleCAnswerSet {
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
/// No state-dependent check in the transaction, deliberately, for the reason
/// `tax_setup` gives about its own three: every one of these is
/// last-writer-wins by nature. Saying the books are a sole proprietorship twice,
/// or recording the same proprietor twice, is idempotent — there is no invariant
/// two concurrent writers could break, only an ordering the log records.
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

    async fn post_json<T: Serialize>(base: &str, path: &str, body: &T) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("{base}{path}"))
            .bearer_auth(TOKEN)
            .json(body)
            .send()
            .await
            .unwrap()
    }

    /// The round trip the whole module exists for: a member on hosted books says
    /// what the books are, and the instance's projection agrees.
    #[tokio::test]
    async fn the_business_type_reaches_the_instance() {
        let (base, store) = serve().await;
        let head = head_of(&base).await;

        let r = post_json(
            &base,
            "/sync/commands/set-business-type",
            &SetBusinessTypeRequest {
                expected_head_seq: head,
                business_type: "sole_proprietorship".into(),
            },
        )
        .await;
        assert!(r.status().is_success(), "{:?}", r.status());

        let s = store.lock().unwrap();
        assert_eq!(
            crate::commands::sole_proprietor_commands::business_type(s.connection()),
            crate::domain::BusinessType::SoleProprietorship
        );
    }

    #[tokio::test]
    async fn the_proprietor_reaches_the_instance() {
        let (base, store) = serve().await;
        let head = head_of(&base).await;

        let r = post_json(
            &base,
            "/sync/commands/set-sole-proprietor",
            &SetSoleProprietorRequest {
                expected_head_seq: head,
                name: "Jinny Choi".into(),
                accounting_method: "cash".into(),
                accounting_method_other: None,
            },
        )
        .await;
        assert!(r.status().is_success(), "{:?}", r.status());

        let s = store.lock().unwrap();
        let p = crate::commands::sole_proprietor_commands::get_proprietor(s.connection())
            .expect("recorded");
        assert_eq!(p.name, "Jinny Choi");
        assert_eq!(p.accounting_method, crate::domain::AccountingMethod::Cash);
    }

    /// The number has no route, so it cannot be submitted even by a caller
    /// determined to — and nothing that *is* submitted carries it.
    #[tokio::test]
    async fn no_social_security_number_ever_reaches_the_log() {
        let (base, store) = serve().await;
        let head = head_of(&base).await;

        post_json(
            &base,
            "/sync/commands/set-sole-proprietor",
            &SetSoleProprietorRequest {
                expected_head_seq: head,
                name: "Jinny Choi".into(),
                accounting_method: "cash".into(),
                accounting_method_other: None,
            },
        )
        .await;

        let s = store.lock().unwrap();
        let digits: i64 = s
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE payload GLOB '*[0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9][0-9][0-9]*'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(digits, 0, "something shaped like an SSN is in the log");
    }

    #[tokio::test]
    async fn a_schedule_c_answer_reaches_the_instance_and_can_be_cleared() {
        let (base, store) = serve().await;

        let head = head_of(&base).await;
        let r = post_json(
            &base,
            "/sync/commands/set-schedule-c-answer",
            &SetScheduleCAnswerRequest {
                expected_head_seq: head,
                tax_year: 2025,
                answer_key: "g".into(),
                value: "yes".into(),
            },
        )
        .await;
        assert!(r.status().is_success());
        {
            let s = store.lock().unwrap();
            assert_eq!(
                crate::tax::schedule_c::load(s.connection(), 2025).get("g"),
                Some("yes")
            );
        }

        // Empty clears rather than storing one: unanswered is not "No".
        let head = head_of(&base).await;
        let r = post_json(
            &base,
            "/sync/commands/set-schedule-c-answer",
            &SetScheduleCAnswerRequest {
                expected_head_seq: head,
                tax_year: 2025,
                answer_key: "g".into(),
                value: "".into(),
            },
        )
        .await;
        assert!(r.status().is_success());
        let s = store.lock().unwrap();
        assert_eq!(
            crate::tax::schedule_c::load(s.connection(), 2025).get("g"),
            None
        );
    }

    /// A caller's mistake gets a 400 that names the field, not a 500 from deep
    /// inside the store.
    #[tokio::test]
    async fn a_callers_mistake_is_a_bad_request_rather_than_a_store_error() {
        let (base, _) = serve().await;

        for (path, body) in [
            (
                "/sync/commands/set-business-type",
                serde_json::json!({"expected_head_seq": head_of(&base).await, "business_type": "sole trader"}),
            ),
            (
                "/sync/commands/set-sole-proprietor",
                serde_json::json!({"expected_head_seq": head_of(&base).await, "name": "  ", "accounting_method": "cash"}),
            ),
            (
                "/sync/commands/set-sole-proprietor",
                serde_json::json!({"expected_head_seq": head_of(&base).await, "name": "Jinny", "accounting_method": "other"}),
            ),
            (
                "/sync/commands/set-schedule-c-answer",
                serde_json::json!({"expected_head_seq": head_of(&base).await, "tax_year": 2025, "answer_key": "nope", "value": "yes"}),
            ),
            (
                "/sync/commands/set-schedule-c-answer",
                serde_json::json!({"expected_head_seq": head_of(&base).await, "tax_year": 3000, "answer_key": "g", "value": "yes"}),
            ),
        ] {
            let r = post_json(&base, path, &body).await;
            assert_eq!(
                r.status(),
                reqwest::StatusCode::BAD_REQUEST,
                "{path} with {body} should be a 400"
            );
        }
    }
}
