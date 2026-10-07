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

    #[tokio::test]
    async fn no_route_is_open() {
        let base = serve().await;
        for path in [
            "/sync/commands/set-personal-tax-profile",
            "/sync/commands/record-tax-statement",
            "/sync/commands/remove-tax-statement",
            "/sync/commands/record-tax-statement-lines",
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
