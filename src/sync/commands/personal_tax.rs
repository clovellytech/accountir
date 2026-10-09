//! Sync commands for a person's own return on group-hosted books: the year's
//! profile, and the statements the return is computed from.
//!
//! A replica may not append, so on hosted books — a person's books kept on their own
//! group server, which is how they reach every machine they use — each of these is
//! submitted here and comes back through the log. The checks a caller can get wrong
//! are made before the store is touched, so the answer is a 400 that names the field
//! rather than a 500; the ones that depend on what the books hold right now are made
//! inside the append, against the write-locked state.

use crate::commands::personal_tax_commands::{self, ProfileError};
use crate::events::types::{
    Event, PersonalTaxProfileData, TaxStatementData, TaxStatementLinesData,
};
use crate::store::event_store::Verdict;
use crate::sync::{outcome_to_response, project, stamp, ApiError, AuthedUser, SyncState};
use crate::tax::information_returns::FormKind;
use axum::{extract::State, routing::post, Json, Router};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

pub fn router() -> Router<SyncState> {
    Router::new()
        .route(
            "/sync/commands/set-personal-tax-profile",
            post(submit_set_profile),
        )
        .route("/sync/commands/record-tax-statement", post(submit_record))
        .route("/sync/commands/remove-tax-statement", post(submit_remove))
        .route(
            "/sync/commands/record-tax-statement-lines",
            post(submit_record_lines),
        )
        .route(
            "/sync/commands/record-state-tax-statement",
            post(submit_record_state),
        )
        .route(
            "/sync/commands/remove-state-tax-statement",
            post(submit_remove_state),
        )
        .route("/sync/commands/link-k1-source", post(submit_link_k1))
        .route("/sync/commands/unlink-k1-source", post(submit_unlink_k1))
        .route(
            "/sync/commands/link-schedule-c-source",
            post(submit_link_schedule_c),
        )
        .route(
            "/sync/commands/unlink-schedule-c-source",
            post(submit_unlink_schedule_c),
        )
}

/// A K-1 link, as the client resolved it against the partnership's books.
///
/// The instance cannot check it against those books: they are another group's,
/// or a file on the client's machine, and an instance never reads either
/// (MULTITENANT-SPEC §2a). The client that holds both made the checks in
/// [`crate::commands::tax_statement_commands::k1_link_for`]; this checks its shape.
#[derive(Serialize, Deserialize)]
pub struct LinkK1SourceRequest {
    pub expected_head_seq: i64,
    pub link_id: String,
    pub ledger_id: String,
    pub ledger_name: String,
    pub partner_id: String,
    pub partner_name: String,
}

/// A Schedule C link, resolved by the client the way a K-1 link is.
#[derive(Serialize, Deserialize)]
pub struct LinkScheduleCSourceRequest {
    pub expected_head_seq: i64,
    pub link_id: String,
    pub ledger_id: String,
    pub ledger_name: String,
    #[serde(default)]
    pub proprietor_name: String,
}

#[derive(Serialize, Deserialize)]
pub struct UnlinkSourceRequest {
    pub expected_head_seq: i64,
    pub link_id: String,
}

async fn submit_link_k1(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<LinkK1SourceRequest>,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    let event = Event::K1SourceLinked {
        link_id: req.link_id,
        ledger_id: req.ledger_id,
        ledger_name: req.ledger_name,
        partner_id: req.partner_id,
        partner_name: req.partner_name,
    };
    if let Err(e) = crate::events::validation::validate_event(&event) {
        return Err(ApiError::bad_request(&e.to_string()));
    }
    append_unchecked(st, req.expected_head_seq, actor, event)
}

async fn submit_link_schedule_c(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<LinkScheduleCSourceRequest>,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    let event = Event::ScheduleCSourceLinked {
        link_id: req.link_id,
        ledger_id: req.ledger_id,
        ledger_name: req.ledger_name,
        proprietor_name: req.proprietor_name,
    };
    if let Err(e) = crate::events::validation::validate_event(&event) {
        return Err(ApiError::bad_request(&e.to_string()));
    }
    append_unchecked(st, req.expected_head_seq, actor, event)
}

async fn submit_unlink_k1(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<UnlinkSourceRequest>,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    unlink(st, req, actor, "k1_links", |link_id| Event::K1SourceUnlinked { link_id })
}

async fn submit_unlink_schedule_c(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<UnlinkSourceRequest>,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    unlink(st, req, actor, "schedule_c_links", |link_id| {
        Event::ScheduleCSourceUnlinked { link_id }
    })
}

/// Unlink, refused inside the write when the link is not there — checked against
/// the books as they are under the lock, so two members unlinking at once is one
/// unlink and one clear refusal.
fn unlink(
    st: SyncState,
    req: UnlinkSourceRequest,
    actor: String,
    table: &'static str,
    event: fn(String) -> Event,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    if req.link_id.trim().is_empty() {
        return Err(ApiError::bad_request("link_id is required"));
    }
    let link_id = req.link_id;
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            req.expected_head_seq,
            move |tx| {
                let linked = tx
                    .query_row(
                        &format!("SELECT 1 FROM {table} WHERE link_id = ?1"),
                        [&link_id],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if !linked {
                    return Ok(Verdict::Reject(ProfileError::Invalid(format!(
                        "no link with id {link_id}"
                    ))));
                }
                Ok(Verdict::<_, ProfileError>::Append(stamp(
                    event(link_id.clone()),
                    &actor,
                )))
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response(outcome, ApiError::domain::<ProfileError>)
}

/// Append with nothing to check against the books' state: a link is
/// last-writer-wins — linking twice is one link.
fn append_unchecked(
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
                Ok(Verdict::<_, ProfileError>::Append(stamp(
                    event.clone(),
                    &actor,
                )))
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response(outcome, ApiError::domain::<ProfileError>)
}

#[derive(Serialize, Deserialize)]
pub struct SetPersonalTaxProfileRequest {
    pub expected_head_seq: i64,
    pub profile: PersonalTaxProfileData,
}

#[derive(Serialize, Deserialize)]
pub struct RecordTaxStatementRequest {
    pub expected_head_seq: i64,
    pub statement: TaxStatementData,
}

#[derive(Serialize, Deserialize)]
pub struct RecordStateTaxStatementRequest {
    pub expected_head_seq: i64,
    pub statement: crate::events::types::StateTaxStatementData,
}

#[derive(Serialize, Deserialize)]
pub struct RemoveTaxStatementRequest {
    pub expected_head_seq: i64,
    pub statement_id: String,
}

#[derive(Serialize, Deserialize)]
pub struct RecordTaxStatementLinesRequest {
    pub expected_head_seq: i64,
    pub lines: TaxStatementLinesData,
}

async fn submit_set_profile(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<SetPersonalTaxProfileRequest>,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    if !(1900..=2200).contains(&req.profile.tax_year) {
        return Err(ApiError::bad_request("tax_year is not a tax year"));
    }
    let profile = req.profile;
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            req.expected_head_seq,
            move |tx| {
                // Against the books as they are inside this write, so an account
                // deleted a moment ago is not named by a profile accepted after it.
                if let Err(e) = personal_tax_commands::check_accounts(tx, &profile) {
                    return Ok(Verdict::Reject(e));
                }
                Ok(Verdict::<_, ProfileError>::Append(stamp(
                    Event::PersonalTaxProfileSet(Box::new(profile.clone())),
                    &actor,
                )))
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response(outcome, ApiError::domain::<ProfileError>)
}

async fn submit_record(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<RecordTaxStatementRequest>,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    let statement = req.statement;
    if statement.statement_id.trim().is_empty() {
        return Err(ApiError::bad_request("statement_id is required"));
    }
    if statement.issuer.trim().is_empty() {
        return Err(ApiError::bad_request(
            "issuer is required — who sent the statement",
        ));
    }
    let Some(form) = FormKind::parse(&statement.form) else {
        return Err(ApiError::bad_request("form is not a statement this version knows"));
    };
    if let Some(code) = statement.amounts.keys().find(|c| !form.accepts_box(c)) {
        return Err(ApiError::bad_request(&format!(
            "{} has no box {code:?}",
            form.label()
        )));
    }
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            req.expected_head_seq,
            move |tx| {
                for id in &statement.document_ids {
                    let attached = tx
                        .query_row("SELECT 1 FROM documents WHERE document_id = ?1", [id], |_| {
                            Ok(())
                        })
                        .optional()?
                        .is_some();
                    if !attached {
                        return Ok(Verdict::Reject(ProfileError::Invalid(format!(
                            "the statement cites document {id}, which these books do not have"
                        ))));
                    }
                }
                Ok(Verdict::<_, ProfileError>::Append(stamp(
                    Event::TaxStatementRecorded(Box::new(statement.clone())),
                    &actor,
                )))
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response(outcome, ApiError::domain::<ProfileError>)
}

async fn submit_remove(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<RemoveTaxStatementRequest>,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    if req.statement_id.trim().is_empty() {
        return Err(ApiError::bad_request("statement_id is required"));
    }
    let statement_id = req.statement_id;
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            req.expected_head_seq,
            move |tx| {
                let recorded = tx
                    .query_row(
                        "SELECT 1 FROM tax_statements WHERE statement_id = ?1",
                        [&statement_id],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if !recorded {
                    return Ok(Verdict::Reject(ProfileError::Invalid(format!(
                        "no statement with id {statement_id}"
                    ))));
                }
                Ok(Verdict::<_, ProfileError>::Append(stamp(
                    Event::TaxStatementRemoved {
                        statement_id: statement_id.clone(),
                    },
                    &actor,
                )))
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response(outcome, ApiError::domain::<ProfileError>)
}

async fn submit_record_state(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<RecordStateTaxStatementRequest>,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    let statement = req.statement;
    let event = Event::StateTaxStatementRecorded(Box::new(statement.clone()));
    if let Err(e) = crate::events::validation::validate_event(&event) {
        return Err(ApiError::bad_request(&e.to_string()));
    }
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            req.expected_head_seq,
            move |tx| {
                for id in &statement.document_ids {
                    let attached = tx
                        .query_row("SELECT 1 FROM documents WHERE document_id = ?1", [id], |_| {
                            Ok(())
                        })
                        .optional()?
                        .is_some();
                    if !attached {
                        return Ok(Verdict::Reject(ProfileError::Invalid(format!(
                            "the state K-1 cites document {id}, which these books do not have"
                        ))));
                    }
                }
                Ok(Verdict::<_, ProfileError>::Append(stamp(event.clone(), &actor)))
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response(outcome, ApiError::domain::<ProfileError>)
}

async fn submit_remove_state(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<RemoveTaxStatementRequest>,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    if req.statement_id.trim().is_empty() {
        return Err(ApiError::bad_request("statement_id is required"));
    }
    let statement_id = req.statement_id;
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            req.expected_head_seq,
            move |tx| {
                let recorded = tx
                    .query_row(
                        "SELECT 1 FROM state_tax_statements WHERE statement_id = ?1",
                        [&statement_id],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if !recorded {
                    return Ok(Verdict::Reject(ProfileError::Invalid(format!(
                        "no state K-1 with id {statement_id}"
                    ))));
                }
                Ok(Verdict::<_, ProfileError>::Append(stamp(
                    Event::StateTaxStatementRemoved {
                        statement_id: statement_id.clone(),
                    },
                    &actor,
                )))
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response(outcome, ApiError::domain::<ProfileError>)
}

async fn submit_record_lines(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<RecordTaxStatementLinesRequest>,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    if req.lines.statement_id.trim().is_empty() {
        return Err(ApiError::bad_request("statement_id is required"));
    }
    let lines = req.lines;
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            req.expected_head_seq,
            move |tx| {
                // Lines belong to a 1099-B already on the books; without it they
                // would be Form 8949 rows with no statement behind them.
                let form: Option<String> = tx
                    .query_row(
                        "SELECT form FROM tax_statements WHERE statement_id = ?1",
                        [&lines.statement_id],
                        |r| r.get(0),
                    )
                    .optional()?;
                match form.as_deref().and_then(FormKind::parse) {
                    Some(FormKind::F1099B) => {}
                    Some(other) => {
                        return Ok(Verdict::Reject(ProfileError::Invalid(format!(
                            "only a 1099-B lists transactions, and this statement is a {}",
                            other.label()
                        ))))
                    }
                    None => {
                        return Ok(Verdict::Reject(ProfileError::Invalid(format!(
                            "no statement with id {}",
                            lines.statement_id
                        ))))
                    }
                }
                Ok(Verdict::<_, ProfileError>::Append(stamp(
                    Event::TaxStatementLinesRecorded(Box::new(lines.clone())),
                    &actor,
                )))
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response(outcome, ApiError::domain::<ProfileError>)
}

#[cfg(test)]
mod tests {
    use crate::store::event_store::EventStore;
    use crate::store::migrations::SchemaStore;
    use crate::sync::SyncState;
    use std::collections::HashMap;

    const TOKEN: &str = "t";

    async fn serve() -> String {
        let mut store = EventStore::in_memory().unwrap();
        store.init_schema().unwrap();
        let state = SyncState::new(
            store,
            HashMap::from([(TOKEN.to_string(), "u1".to_string())]),
        );
        let app = crate::sync::router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    async fn post(
        base: &str,
        path: &str,
        body: serde_json::Value,
    ) -> (reqwest::StatusCode, String) {
        let r = reqwest::Client::new()
            .post(format!("{base}{path}"))
            .bearer_auth(TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap();
        (r.status(), r.text().await.unwrap())
    }

    fn profile(head: i64, extra: serde_json::Value) -> serde_json::Value {
        let mut p = serde_json::json!({
            "tax_year": 2025,
            "filing_status": "married_filing_jointly",
            "state": "IL",
        });
        if let (Some(p), Some(extra)) = (p.as_object_mut(), extra.as_object()) {
            for (k, v) in extra {
                p.insert(k.clone(), v.clone());
            }
        }
        serde_json::json!({ "expected_head_seq": head, "profile": p })
    }

    /// Hosted books can record a profile; one naming an account the books do not
    /// have is refused as the caller's mistake, inside the write.
    #[tokio::test]
    async fn a_profile_lands_and_a_missing_account_is_refused() {
        let base = serve().await;
        let (ok, body) = post(
            &base,
            "/sync/commands/set-personal-tax-profile",
            profile(0, serde_json::json!({})),
        )
        .await;
        assert_eq!(ok, reqwest::StatusCode::OK, "{body}");

        let (refused, body) = post(
            &base,
            "/sync/commands/set-personal-tax-profile",
            profile(
                1,
                serde_json::json!({ "extra_interest_account_ids": ["nope"] }),
            ),
        )
        .await;
        assert_eq!(refused, reqwest::StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert!(body.contains("nope"), "{body}");
    }

    /// A statement on a form this version knows lands; a box the form does not have
    /// is a 400 that says so; and it can be taken off again.
    #[tokio::test]
    async fn a_statement_lands_is_checked_by_box_and_can_be_removed() {
        let base = serve().await;
        let statement = |head: i64, boxes: serde_json::Value| {
            serde_json::json!({
                "expected_head_seq": head,
                "statement": {
                    "statement_id": "s1",
                    "tax_year": 2025,
                    "form": "w2",
                    "issuer": "Acme",
                    "amounts": boxes,
                }
            })
        };
        let (bad, body) = post(
            &base,
            "/sync/commands/record-tax-statement",
            statement(0, serde_json::json!({ "99": 100 })),
        )
        .await;
        assert_eq!(bad, reqwest::StatusCode::BAD_REQUEST, "{body}");

        let (ok, body) = post(
            &base,
            "/sync/commands/record-tax-statement",
            statement(0, serde_json::json!({ "1": 5_000_000, "2": 600_000 })),
        )
        .await;
        assert_eq!(ok, reqwest::StatusCode::OK, "{body}");

        let (gone, body) = post(
            &base,
            "/sync/commands/remove-tax-statement",
            serde_json::json!({ "expected_head_seq": 1, "statement_id": "s1" }),
        )
        .await;
        assert_eq!(gone, reqwest::StatusCode::OK, "{body}");
    }

    /// Hosted personal books can link a partnership's K-1 and a business's
    /// Schedule C, resolved by the client; a link whose id is not the shape the
    /// log requires is the caller's mistake; and an unlink of nothing is refused
    /// inside the write.
    #[tokio::test]
    async fn links_land_are_checked_for_shape_and_unlinking_nothing_is_refused() {
        let base = serve().await;
        let (ok, body) = post(
            &base,
            "/sync/commands/link-k1-source",
            serde_json::json!({
                "expected_head_seq": 0, "link_id": "p-ledger:partner-1",
                "ledger_id": "p-ledger", "ledger_name": "Art House LLC",
                "partner_id": "partner-1", "partner_name": "Zak",
            }),
        )
        .await;
        assert_eq!(ok, reqwest::StatusCode::OK, "{body}");
        let (bad, body) = post(
            &base,
            "/sync/commands/link-k1-source",
            serde_json::json!({
                "expected_head_seq": 1, "link_id": "wrong",
                "ledger_id": "p-ledger", "ledger_name": "Art House LLC",
                "partner_id": "partner-1", "partner_name": "Zak",
            }),
        )
        .await;
        assert_eq!(bad, reqwest::StatusCode::BAD_REQUEST, "{body}");

        let (ok, body) = post(
            &base,
            "/sync/commands/link-schedule-c-source",
            serde_json::json!({
                "expected_head_seq": 1, "link_id": "b-ledger",
                "ledger_id": "b-ledger", "ledger_name": "Bugbear Bikes LLC",
                "proprietor_name": "Zak Patterson",
            }),
        )
        .await;
        assert_eq!(ok, reqwest::StatusCode::OK, "{body}");
        let (ok, body) = post(
            &base,
            "/sync/commands/unlink-schedule-c-source",
            serde_json::json!({ "expected_head_seq": 2, "link_id": "b-ledger" }),
        )
        .await;
        assert_eq!(ok, reqwest::StatusCode::OK, "{body}");
        let (refused, body) = post(
            &base,
            "/sync/commands/unlink-schedule-c-source",
            serde_json::json!({ "expected_head_seq": 3, "link_id": "b-ledger" }),
        )
        .await;
        assert_eq!(refused, reqwest::StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        let (ok, body) = post(
            &base,
            "/sync/commands/unlink-k1-source",
            serde_json::json!({ "expected_head_seq": 3, "link_id": "p-ledger:partner-1" }),
        )
        .await;
        assert_eq!(ok, reqwest::StatusCode::OK, "{body}");
    }

    #[tokio::test]
    async fn no_route_is_open() {
        let base = serve().await;
        for path in [
            "/sync/commands/set-personal-tax-profile",
            "/sync/commands/record-tax-statement",
            "/sync/commands/remove-tax-statement",
            "/sync/commands/record-tax-statement-lines",
            "/sync/commands/link-k1-source",
            "/sync/commands/unlink-k1-source",
            "/sync/commands/link-schedule-c-source",
            "/sync/commands/unlink-schedule-c-source",
        ] {
            let r = reqwest::Client::new()
                .post(format!("{base}{path}"))
                .json(&serde_json::json!({ "expected_head_seq": 0 }))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), reqwest::StatusCode::UNAUTHORIZED, "{path}");
        }
    }
}
