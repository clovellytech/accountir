//! `POST /adopt/events.ndjson` — a group takes over an existing set of books.
//!
//! This is the other half of a feature whose client side already shipped: the
//! desktop's "Share these books with the group" posts a whole local log here
//! ([`super::client::SyncClient::adopt_log`]), and until now nothing served the
//! route. A real group answered 404 mid-upload, which the proxy in front of it
//! turned into a 502, so the button failed with a status nobody could explain.
//!
//! ## Why adoption is a separate endpoint rather than a bulk submit
//!
//! Every other write path mints new events: the server assigns the id, the
//! timestamp and the hash. Adoption does the opposite — it keeps the ids, hashes
//! and times the books already have, because the point is that these *are* the same
//! books, and the desktop verifies the group's head hash against its own file before
//! binding itself as a replica. A path that re-stamped them would produce a ledger
//! that no longer matches the one on disk, and the binding check would (correctly)
//! refuse it.
//!
//! ## The one safety rule
//!
//! **The target ledger must be empty.** Two logs both start at sequence 1, so there
//! is no way to interleave them and no way to tell afterwards which event came from
//! where. [`super::replica::adopt_log`] enforces it under the write lock, and this
//! handler turns that refusal into a 409 the person can act on. An initial share is
//! the only moment when "these become the group's books" is unambiguous — after
//! that, joining a group means pulling its log, not pushing yours.
//!
//! ## Authorisation
//!
//! Any authenticated member, which is what the rest of this transport requires and
//! what the group boundary means here: membership grants full access to everything on
//! the instance (`MULTITENANT-SPEC §2a`), and the token proves *current* membership
//! because the instance re-checks its locally synced member set on every request.
//! Note what the empty-ledger rule already does for this: the window in which
//! adoption can do anything at all closes the moment a group has one event, so this
//! is not a lever for rewriting books that exist.

use super::{ApiError, AuthedUser, SyncState};
use crate::events::payload::hash_to_hex;
use crate::events::types::StoredEvent;
use crate::sync::replica::{self, ReplicaError};
use axum::body::Bytes;
use axum::extract::State;
use axum::Json;
use axum::{http::StatusCode, routing::post, Router};
use serde::Serialize;

/// The ndjson wire format this endpoint speaks, echoed so a client can tell an old
/// server from a new one by more than the absence of a 404.
pub const ADOPT_FORMAT_VERSION: u32 = 1;

/// Most a single adoption may carry: a whole ledger, once, at roughly 1 KB an event.
/// Sixty-odd thousand events of books is far past anything this product has seen, and
/// the cap is what stops one request from deciding how much memory the instance uses
/// — the body is parsed in full before anything is written, because a partially
/// readable log must not become a partially adopted one.
pub const ADOPT_MAX_BYTES: usize = 64 * 1024 * 1024;

/// What the group did, in the shape [`super::client::AdoptOutcome`] reads.
#[derive(Debug, Serialize)]
pub struct AdoptResponse {
    /// Which group adopted them, so the desktop can show the person what happened
    /// rather than echoing back the name it already sent.
    pub group_id: String,
    pub format_version: u32,
    pub adopted: usize,
    pub head_id: Option<i64>,
    /// Hex of the head hash afterwards. The desktop compares this against its own
    /// file before binding, so a mismatch stops a replica being bound to a log that
    /// is not the one it handed over.
    pub head_hash: Option<String>,
}

pub fn router() -> Router<SyncState> {
    Router::new().route("/adopt/events.ndjson", post(adopt))
}

/// One ndjson line per stored event, in order, exactly as [`replica::to_ndjson`]
/// writes them.
///
/// Refusals name the line, because the alternative — "could not read the log" about a
/// file of sixty thousand lines — is not something anyone can act on. Blank lines are
/// skipped so a trailing newline is not an error.
fn parse_ndjson(body: &str) -> Result<Vec<StoredEvent>, String> {
    let mut events = Vec::new();
    for (i, line) in body.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<StoredEvent>(line) {
            Ok(event) => events.push(event),
            Err(e) => return Err(format!("line {}: {e}", i + 1)),
        }
    }
    Ok(events)
}

fn bad_request(message: impl Into<String>) -> ApiError {
    ApiError {
        status: StatusCode::BAD_REQUEST,
        body: serde_json::json!({ "error": message.into() }),
    }
}

async fn adopt(
    _user: AuthedUser,
    State(st): State<SyncState>,
    body: Bytes,
) -> Result<Json<AdoptResponse>, ApiError> {
    if body.len() > ADOPT_MAX_BYTES {
        return Err(ApiError {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            body: serde_json::json!({
                "error": format!(
                    "that log is {} bytes; this endpoint takes at most {}",
                    body.len(),
                    ADOPT_MAX_BYTES
                )
            }),
        });
    }
    let text = std::str::from_utf8(&body).map_err(|_| bad_request("the log is not UTF-8"))?;
    let events = parse_ndjson(text).map_err(bad_request)?;
    // An empty upload would otherwise "succeed" at adopting nothing, and the desktop
    // would bind itself to a group that never took its books.
    if events.is_empty() {
        return Err(bad_request("that log has no events in it"));
    }

    let mut store = st.store.lock().map_err(|_| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        body: serde_json::json!({ "error": "the ledger lock is poisoned" }),
    })?;
    let applied = match replica::adopt_log(&mut store, &events) {
        Ok(applied) => applied,
        // The one refusal a person can do something about: the group already has
        // books. Worded for them, and carried through by the client as-is.
        Err(ReplicaError::NotEmpty { head }) => {
            return Err(ApiError {
                status: StatusCode::CONFLICT,
                body: serde_json::json!({
                    "error": format!(
                        "this group already has books ({head} events). Sharing an \
                         existing set of books is only possible into a group that has \
                         never been written to."
                    )
                }),
            })
        }
        // A gap or a hash mismatch means the log itself does not hold together —
        // nothing was written (the append is one transaction), so this is a 400 about
        // the upload rather than a 500 about the group.
        Err(e @ (ReplicaError::Gap { .. } | ReplicaError::HashMismatch { .. })) => {
            return Err(bad_request(format!("that log did not verify: {e}")))
        }
        Err(e) => {
            eprintln!("adopt: FAILED to adopt {} events: {e}", events.len());
            return Err(ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                body: serde_json::json!({ "error": "the group could not adopt that log" }),
            });
        }
    };

    // Read the head back rather than trusting what we just sent: this is the value
    // the desktop checks its own file against before binding, so it has to come from
    // the ledger as stored.
    let head_id = store.latest_id().ok().flatten();
    let head_hash = head_id
        .and_then(|id| store.get_hash(id).ok())
        .map(|h| hash_to_hex(&h));
    println!(
        "adopt: {} events adopted by {}, head {:?}",
        applied.applied,
        st.group_id(),
        head_id
    );
    Ok(Json(AdoptResponse {
        group_id: st.group_id().to_string(),
        format_version: ADOPT_FORMAT_VERSION,
        adopted: applied.applied,
        head_id,
        head_hash,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::types::{Event, EventAccountType, EventEnvelope};
    use crate::store::event_store::EventStore;
    use crate::store::migrations::SchemaStore;
    use crate::sync::{router, SyncState};
    use std::collections::HashMap;

    const TOKEN: &str = "tok-1";

    fn store() -> EventStore {
        let mut s = EventStore::in_memory().unwrap();
        s.init_schema().unwrap();
        s.run_migrations().unwrap();
        s
    }

    fn account_event(i: usize) -> EventEnvelope {
        EventEnvelope::new(
            Event::AccountCreated {
                account_id: format!("a{i}"),
                account_number: format!("1{i:03}"),
                name: format!("Account {i}"),
                account_type: EventAccountType::Asset,
                parent_id: None,
                currency: Some("USD".into()),
                description: None,
            },
            "the-desktop".into(),
        )
    }

    /// Books as they exist on someone's laptop, plus the ndjson they would hand over.
    fn books(n: usize) -> (EventStore, String) {
        let mut s = store();
        for i in 0..n {
            s.append(account_event(i)).unwrap();
        }
        let ndjson = crate::sync::replica::to_ndjson(&s).unwrap();
        (s, ndjson)
    }

    async fn serve(store: EventStore) -> String {
        let state = SyncState::new(store, HashMap::from([(TOKEN.to_string(), "u1".to_string())]))
            .with_group("acme");
        let app = router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    async fn post(base: &str, body: &str, token: Option<&str>) -> (u16, String) {
        let mut req = reqwest::Client::new()
            .post(format!("{base}/adopt/events.ndjson"))
            .header(reqwest::header::CONTENT_TYPE, "application/x-ndjson")
            .body(body.to_string());
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await.unwrap();
        (resp.status().as_u16(), resp.text().await.unwrap())
    }

    /// The whole point: the group ends up holding the same log, id for id and hash for
    /// hash, and says so in terms the desktop checks before it binds itself.
    #[tokio::test]
    async fn an_empty_group_adopts_a_log_verbatim() {
        let (mine, ndjson) = books(3);
        let base = serve(store()).await;

        let (status, body) = post(&base, &ndjson, Some(TOKEN)).await;
        assert_eq!(status, 200, "{body}");
        let out: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(out["adopted"], 3);
        assert_eq!(out["group_id"], "acme");
        assert_eq!(out["format_version"], ADOPT_FORMAT_VERSION);
        assert_eq!(out["head_id"].as_i64(), mine.latest_id().unwrap());
        assert_eq!(
            out["head_hash"].as_str().unwrap(),
            hash_to_hex(&mine.get_hash(mine.latest_id().unwrap().unwrap()).unwrap()),
            "the head hash is the one the desktop will compare against its own file"
        );

        // And the group's log really is the same log, not a re-stamped copy.
        let (head_status, head) = {
            let resp = reqwest::Client::new()
                .get(format!("{base}/sync/events?since=0"))
                .bearer_auth(TOKEN)
                .send()
                .await
                .unwrap();
            (resp.status().as_u16(), resp.text().await.unwrap())
        };
        assert_eq!(head_status, 200, "{head}");
        let theirs: serde_json::Value = serde_json::from_str(&head).unwrap();
        let events = theirs["events"].as_array().unwrap();
        assert_eq!(events.len(), 3);
        for (i, original) in mine.get_all().unwrap().iter().enumerate() {
            assert_eq!(events[i]["seq"], original.id, "id kept");
            assert_eq!(
                events[i]["hash"].as_str().unwrap(),
                hash_to_hex(&original.hash),
                "event {i}'s hash is carried over untouched"
            );
            // Compared as instants: the wire spells UTC "Z" and chrono's to_rfc3339
            // spells it "+00:00", which is the same moment written two ways.
            assert_eq!(
                chrono::DateTime::parse_from_rfc3339(events[i]["timestamp"].as_str().unwrap())
                    .unwrap(),
                original.timestamp,
                "and so is the time it was written on the laptop"
            );
        }
    }

    /// A group with books of its own refuses, and says why in a sentence the desktop
    /// shows the person as-is. Nothing about the group's ledger changes.
    #[tokio::test]
    async fn a_group_that_already_has_books_refuses_and_keeps_them() {
        let mut theirs = store();
        theirs.append(account_event(99)).unwrap();
        let existing_head = theirs.latest_id().unwrap();
        let existing_hash = theirs.get_hash(existing_head.unwrap()).unwrap();
        let base = serve(theirs).await;
        let (_, ndjson) = books(3);

        let (status, body) = post(&base, &ndjson, Some(TOKEN)).await;
        assert_eq!(status, 409, "{body}");
        assert!(body.contains("already has books"), "{body}");

        let resp = reqwest::Client::new()
            .get(format!("{base}/sync/head"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap();
        let head: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(
            head["head"].as_i64(),
            existing_head,
            "their books are untouched: {head}"
        );
        assert_eq!(existing_hash.len(), 32, "and still hashed as before");
    }

    /// A log that cannot be read is refused by the line, and nothing is written —
    /// which matters because a half-adopted ledger could never be adopted again.
    #[tokio::test]
    async fn an_unreadable_log_names_the_line_and_writes_nothing() {
        let (_, good) = books(2);
        let base = serve(store()).await;
        let mut lines: Vec<&str> = good.lines().collect();
        lines.insert(1, "{not json");
        let (status, body) = post(&base, &lines.join("\n"), Some(TOKEN)).await;
        assert_eq!(status, 400, "{body}");
        assert!(body.contains("line 2"), "{body}");

        let resp = reqwest::Client::new()
            .get(format!("{base}/sync/head"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap();
        let head: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(head["head"], 0, "nothing landed: {head}");
    }

    #[tokio::test]
    async fn an_empty_upload_is_refused_rather_than_adopting_nothing() {
        let base = serve(store()).await;
        for body in ["", "\n\n"] {
            let (status, text) = post(&base, body, Some(TOKEN)).await;
            assert_eq!(status, 400, "{text}");
            assert!(text.contains("no events"), "{text}");
        }
    }

    #[tokio::test]
    async fn adoption_needs_a_token_like_every_other_write() {
        let (_, ndjson) = books(1);
        let base = serve(store()).await;
        let (status, body) = post(&base, &ndjson, None).await;
        assert_eq!(status, 401, "{body}");
        let (status, body) = post(&base, &ndjson, Some("not-the-token")).await;
        assert_eq!(status, 401, "{body}");
    }

    /// Trailing newlines are how `to_ndjson` ends every file, so they must not read as
    /// a broken line.
    #[test]
    fn blank_lines_are_skipped_and_every_other_line_must_parse() {
        let (_, ndjson) = books(2);
        assert_eq!(parse_ndjson(&ndjson).unwrap().len(), 2);
        assert_eq!(parse_ndjson(&format!("{ndjson}\n\n")).unwrap().len(), 2);
        assert!(parse_ndjson("{}").is_err());
    }
}
