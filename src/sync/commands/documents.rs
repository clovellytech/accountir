//! Sync commands for files attached to group-hosted books.
//!
//! # The bytes do not come here
//!
//! Attaching is two writes to two places: the bytes to a blob store, the
//! `DocumentAttached` event to the log. On hosted books only the second can be done
//! locally — a replica may not append — so the event is submitted and the bytes stay on
//! the machine that attached them, under the ledger's own id.
//!
//! That is not a gap to be closed later by accident: it is the property
//! [`crate::documents`] was built for. A replica can hold the log without the files and
//! know exactly which it is missing, because every document names the SHA-256 of its
//! bytes. So a colleague sees the attachment, sees what it is, and is told the file is
//! not on their machine — rather than seeing nothing at all.
//!
//! When a group server grows its own blob store, the digest already in the log is what
//! makes bytes fetched from it checkable rather than merely trusted.
//!
//! # What is checked here
//!
//! A caller's mistakes, named before the store sees them: a digest in the wrong shape,
//! a size outside the limit, a filename that is a path. The same checks live in
//! [`crate::events::validation`] and run whichever route an event arrives by; these
//! exist so the answer is a 400 that says which field, rather than a 500.

use crate::commands::document_commands::DocumentError;
use crate::events::types::{DocumentAttachedData, DocumentSubjectData, Event};
use crate::store::event_store::Verdict;
use crate::sync::{outcome_to_response, project, stamp, ApiError, AuthedUser, SyncState};
use axum::{extract::State, routing::post, Json, Router};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

pub fn router() -> Router<SyncState> {
    Router::new()
        .route("/sync/commands/attach-document", post(submit_attach))
        .route("/sync/commands/remove-document", post(submit_remove))
        .route("/sync/commands/classify-document", post(submit_classify))
}

/// Record that a file has been attached. The bytes are not in this request — see the
/// module docs.
#[derive(Serialize, Deserialize)]
pub struct AttachDocumentRequest {
    pub expected_head_seq: i64,
    /// Minted by the client, so a retry after a lost answer records one document rather
    /// than two.
    pub document_id: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub media_type: String,
    pub filename: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tax_year: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub form: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<DocumentSubjectData>,
    /// What the client recognised the file as. The instance never sees the bytes,
    /// so it records what it is told; the shape is checked like any event's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<String>,
}

/// What a client recognised an attached document as. See
/// [`crate::documents::classify`].
#[derive(Serialize, Deserialize)]
pub struct ClassifyDocumentRequest {
    pub expected_head_seq: i64,
    pub document_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct RemoveDocumentRequest {
    pub expected_head_seq: i64,
    pub document_id: String,
}

async fn submit_attach(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<AttachDocumentRequest>,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    if req.document_id.trim().is_empty() {
        return Err(ApiError::bad_request("document_id is required"));
    }
    if !crate::documents::is_sha256_hex(&req.sha256) {
        return Err(ApiError::bad_request(
            "sha256 is not a lowercase hex SHA-256 digest",
        ));
    }
    if crate::documents::check_size(req.size_bytes).is_err() {
        return Err(ApiError::bad_request(
            "size_bytes is not a size a document can have",
        ));
    }
    if req.filename.trim().is_empty() || req.filename.contains(['/', '\\']) {
        return Err(ApiError::bad_request("filename is a name, not a path"));
    }
    append(
        st,
        req.expected_head_seq,
        actor,
        Event::DocumentAttached(Box::new(DocumentAttachedData {
            document_id: req.document_id,
            sha256: req.sha256,
            size_bytes: req.size_bytes,
            media_type: req.media_type,
            filename: req.filename,
            title: req.title,
            tax_year: req.tax_year,
            form: req.form,
            subject: req.subject,
            kind: req.kind,
            parts: req.parts,
        })),
    )
}

async fn submit_classify(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<ClassifyDocumentRequest>,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    if req.document_id.trim().is_empty() {
        return Err(ApiError::bad_request("document_id is required"));
    }
    let event = Event::DocumentClassified {
        document_id: req.document_id.clone(),
        kind: req.kind,
        parts: req.parts,
    };
    // Shape first, so a malformed kind is the caller's 400 rather than the store's 500.
    if let Err(e) = crate::events::validation::validate_event(&event) {
        return Err(ApiError::bad_request(&e.to_string()));
    }
    let document_id = req.document_id;
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            req.expected_head_seq,
            move |tx| {
                let attached = tx
                    .query_row(
                        "SELECT 1 FROM documents WHERE document_id = ?1",
                        [&document_id],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if !attached {
                    return Ok(Verdict::Reject(DocumentError::NoSuchDocument(
                        document_id.clone(),
                    )));
                }
                Ok(Verdict::<_, DocumentError>::Append(stamp(event.clone(), &actor)))
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response(outcome, ApiError::domain::<DocumentError>)
}

async fn submit_remove(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<RemoveDocumentRequest>,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    if req.document_id.trim().is_empty() {
        return Err(ApiError::bad_request("document_id is required"));
    }
    append(
        st,
        req.expected_head_seq,
        actor,
        Event::DocumentRemoved {
            document_id: req.document_id,
        },
    )
}

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
                Ok(Verdict::<_, DocumentError>::Append(stamp(
                    event.clone(),
                    &actor,
                )))
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response(outcome, ApiError::domain::<DocumentError>)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::event_store::EventStore;
    use crate::store::migrations::SchemaStore;
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

    fn body(sha: &str, size: u64, filename: &str) -> serde_json::Value {
        serde_json::json!({
            "expected_head_seq": 0,
            "document_id": "d1",
            "sha256": sha,
            "size_bytes": size,
            "media_type": "application/pdf",
            "filename": filename,
        })
    }

    async fn post(base: &str, path: &str, body: serde_json::Value) -> reqwest::StatusCode {
        reqwest::Client::new()
            .post(format!("{base}{path}"))
            .bearer_auth(TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status()
    }

    const DIGEST: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    /// A caller's mistake is a 400 that names the field, not a 500 from the store. Each
    /// of these is also refused by `validate_event` whichever route it arrives by; these
    /// exist so the answer is readable.
    #[tokio::test]
    async fn a_malformed_attachment_is_refused_by_field() {
        let base = serve().await;
        assert_eq!(
            post(
                &base,
                "/sync/commands/attach-document",
                body("nope", 10, "s.pdf")
            )
            .await,
            reqwest::StatusCode::BAD_REQUEST,
            "a digest in the wrong shape names a file nobody can find"
        );
        assert_eq!(
            post(
                &base,
                "/sync/commands/attach-document",
                body(DIGEST, 0, "s.pdf")
            )
            .await,
            reqwest::StatusCode::BAD_REQUEST,
            "an empty file"
        );
        assert_eq!(
            post(
                &base,
                "/sync/commands/attach-document",
                body(DIGEST, 10, "/etc/passwd")
            )
            .await,
            reqwest::StatusCode::BAD_REQUEST,
            "a filename is a name, not a path"
        );
    }

    /// And a well-formed one lands, with the subject it was given.
    #[tokio::test]
    async fn an_attachment_lands_and_can_be_taken_off_again() {
        let base = serve().await;
        let mut with_subject = body(DIGEST, 10, "statement.pdf");
        with_subject["subject"] = serde_json::json!({
            "kind": "reconciliation",
            "reconciliation_id": "r1"
        });
        assert_eq!(
            post(&base, "/sync/commands/attach-document", with_subject).await,
            reqwest::StatusCode::OK
        );
        assert_eq!(
            post(
                &base,
                "/sync/commands/remove-document",
                serde_json::json!({ "expected_head_seq": 1, "document_id": "d1" })
            )
            .await,
            reqwest::StatusCode::OK
        );
    }

    /// Both routes are behind the same authentication as every other write.
    #[tokio::test]
    async fn neither_route_is_open() {
        let base = serve().await;
        for path in [
            "/sync/commands/attach-document",
            "/sync/commands/remove-document",
        ] {
            let status = reqwest::Client::new()
                .post(format!("{base}{path}"))
                .json(&serde_json::json!({ "expected_head_seq": 0 }))
                .send()
                .await
                .unwrap()
                .status();
            assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED, "{path}");
        }
    }
}
