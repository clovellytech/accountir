//! Recording the partnership and its partners on group-hosted books.
//!
//! A Form 1065 is a return *about* a partnership: who it legally is, and who its
//! partners were during the year. On standalone books a member records that
//! locally. On group-hosted books they could not, because a replica may not
//! append — its event ids are the group server's to mint. These endpoints are the
//! route that was missing.
//!
//! # What does and does not cross this boundary
//!
//! **Not a taxpayer identification number.** No request or response here has a
//! field for one, and no partnership event carries one. This log is replicated in
//! full to every member's laptop and into every backup they take, so a partner's
//! SSN written here is that SSN on every other partner's machine, permanently, in
//! an append-only file that cannot be redacted. It is the argument that keeps
//! event-service API keys out of the log, applied to something considerably worse
//! to leak.
//!
//! A TIN is needed only on the machine where a return is actually prepared, so it
//! stays there, in `partner_tins` (migration 023) — an ordinary local table that
//! is not a projection of anything. The consequence is deliberate: on hosted books
//! the partner *records* sync and the numbers do not, so whoever prepares the
//! return types the TINs on their own machine, and everybody else sees partners
//! with no numbers against them.
//!
//! What the group's books do carry is everything the partnership needs to agree
//! on — who the partners are, when they joined and left, and what share of profit,
//! loss and capital each holds. Those are not secrets; they are the things a K-1
//! allocates by, and every member is entitled to see them.
//!
//! # Why the server mints the partner id
//!
//! The projector writes `INSERT OR REPLACE INTO partners (id, …)`, so a
//! client-supplied id is a way to overwrite another member's partner record with a
//! perfectly valid-looking event. That is not a defacement anybody would notice: it
//! rewrites the victim's shares, and the first sign of it is a K-1 allocating them
//! the wrong income. The id is minted here and returned, exactly as
//! `register-event-service` and `connect-plaid-item` mint theirs.
//!
//! Belt and braces: [`build_admit_partner_in_txn`] *also* refuses an id that is
//! already taken, so the clobber is impossible under the write lock whoever minted
//! the id and however this endpoint is later changed.
//!
//! # Where the rules live
//!
//! Not here. Every state-dependent check is a `build_*_in_txn` function in
//! [`crate::commands::partnership_commands`], called by both this module and the
//! local command path, so a rule enforced on standalone books is the same rule
//! enforced on the group's server. These handlers are transport: parse, run the
//! shared predicate under the client's `expected_head_seq`, map the outcome.

use crate::commands::partnership_commands::{
    AdmitPartner, PartnerStep, PartnershipError, UpdatePartner, build_admit_partner_in_txn,
    build_set_profile_event, build_update_partner_in_txn, build_withdraw_partner_in_txn,
    check_admit_partner_pure, check_set_profile_pure, check_update_partner_pure,
};
use crate::domain::{Address, BusinessProfile, PartnerType, Residency, Shares};
use crate::store::event_store::{CheckedOutcome, Verdict};
use crate::sync::{ApiError, AuthedUser, SyncState, outcome_to_response, project, stamp};
use axum::{Json, Router, extract::State, routing::post};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

pub fn router() -> Router<SyncState> {
    Router::new()
        .route(
            "/sync/commands/set-business-profile",
            post(submit_set_profile),
        )
        .route("/sync/commands/admit-partner", post(submit_admit_partner))
        .route("/sync/commands/update-partner", post(submit_update_partner))
        .route(
            "/sync/commands/withdraw-partner",
            post(submit_withdraw_partner),
        )
}

/// Parse the two K-1 checkbox fields, or say which word was not understood.
///
/// A `400` rather than a `422`: "limitd" is a malformed request, not a command
/// the books refused, and telling a client its partner was rejected on a domain
/// rule would send them looking at the ledger for a typo.
fn parse_partner_type(s: &str) -> Result<PartnerType, ApiError> {
    PartnerType::parse(s)
        .ok_or_else(|| ApiError::bad_request("partner_type must be general or limited"))
}

fn parse_residency(s: &str) -> Result<Residency, ApiError> {
    Residency::parse(s).ok_or_else(|| ApiError::bad_request("residency must be domestic or foreign"))
}

// ---------------------------------------------------------------------------
// The partnership header
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
pub struct SetBusinessProfileRequest {
    pub expected_head_seq: i64,
    /// The name on the SS-4, which is what the IRS matches the EIN against.
    pub legal_name: String,
    pub address: Address,
    /// `NN-NNNNNNN`.
    pub ein: String,
    /// Six digits — Form 1065 box C.
    pub naics_code: String,
    /// Form 1065 box E, "Date business started".
    pub formation_date: NaiveDate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_activity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_product: Option<String>,
}

/// Record the group's partnership details, replacing whatever was there.
///
/// Last-writer-wins, and that is the right answer rather than a shortcut: the
/// row is keyed `'default'` by a CHECK constraint so there is exactly one
/// header, and the header is filed as a unit — an EIN that belongs to a
/// different legal name is a rejected return, so a half-updated header is not a
/// state worth being able to reach. Two members setting it at once is two people
/// disagreeing about a fact, which no locking discipline can resolve.
async fn submit_set_profile(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<SetBusinessProfileRequest>,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    let profile = BusinessProfile {
        legal_name: req.legal_name,
        address: req.address,
        ein: req.ein,
        naics_code: req.naics_code,
        formation_date: req.formation_date,
        principal_activity: req.principal_activity,
        principal_product: req.principal_product,
    };
    // Pure validation up front: `validate_event` inside the append would catch a
    // malformed EIN too, but as a store error — a 500 for what is squarely the
    // caller's mistake.
    check_set_profile_pure(&profile).map_err(ApiError::domain)?;

    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            req.expected_head_seq,
            move |_tx| Ok(Verdict::<_, PartnershipError>::Append(stamp(
                build_set_profile_event(&profile),
                &actor,
            ))),
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response(outcome, ApiError::domain::<PartnershipError>)
}

// ---------------------------------------------------------------------------
// Partners
// ---------------------------------------------------------------------------

/// Note the absence of a TIN field, and see the module docs for why it is not an
/// oversight.
#[derive(Serialize, Deserialize)]
pub struct AdmitPartnerRequest {
    pub expected_head_seq: i64,
    pub name: String,
    /// "general" or "limited" — K-1 item G.
    pub partner_type: String,
    /// "domestic" or "foreign" — K-1 item H1.
    pub residency: String,
    /// K-1 item I1, free text because the form's own answer is free text.
    pub entity_type: String,
    pub address: Address,
    /// `None` means "since the business started", resolved against the group's
    /// header **inside** the append transaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_date: Option<NaiveDate>,
    pub shares: Shares,
}

#[derive(Serialize, Deserialize)]
pub struct AdmitPartnerResponse {
    pub head: i64,
    /// Minted server-side. The caller needs it to file the partner's TIN locally,
    /// and cannot choose it — see the module docs.
    pub partner_id: String,
}

async fn submit_admit_partner(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<AdmitPartnerRequest>,
) -> Result<Json<AdmitPartnerResponse>, ApiError> {
    let cmd = AdmitPartner {
        name: req.name,
        partner_type: parse_partner_type(&req.partner_type)?,
        residency: parse_residency(&req.residency)?,
        entity_type: req.entity_type,
        address: req.address,
        start_date: req.start_date,
        shares: req.shares,
        // Never from the wire. The local command struct carries one because on
        // standalone books the same call records it; here there is nothing to
        // record and no field to record it from.
        tin: None,
    };
    check_admit_partner_pure(&cmd).map_err(ApiError::domain)?;

    let partner_id = uuid::Uuid::new_v4().to_string();
    let minted = partner_id.clone();

    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            req.expected_head_seq,
            move |tx| match build_admit_partner_in_txn(tx, &minted, &cmd)? {
                PartnerStep::Append(event) => Ok(Verdict::Append(stamp(event, &actor))),
                PartnerStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            project,
        )
        .map_err(ApiError::store)?;

    match outcome {
        CheckedOutcome::Appended(stored) => Ok(Json(AdmitPartnerResponse {
            head: stored.id,
            partner_id,
        })),
        CheckedOutcome::HeadMismatch { actual, .. } => Err(ApiError::conflict(actual)),
        CheckedOutcome::Rejected(e) => Err(ApiError::domain(e)),
    }
}

#[derive(Serialize, Deserialize)]
pub struct UpdatePartnerRequest {
    pub expected_head_seq: i64,
    pub partner_id: String,
    pub name: String,
    pub partner_type: String,
    pub residency: String,
    pub entity_type: String,
    pub address: Address,
    pub shares: Shares,
}

/// Change a partner's details or shares.
///
/// Their start and end dates are deliberately not fields here — joining and
/// leaving are their own events, and letting an edit move them would quietly
/// change which tax years that partner gets a K-1 for.
async fn submit_update_partner(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<UpdatePartnerRequest>,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    let cmd = UpdatePartner {
        partner_id: req.partner_id,
        name: req.name,
        partner_type: parse_partner_type(&req.partner_type)?,
        residency: parse_residency(&req.residency)?,
        entity_type: req.entity_type,
        address: req.address,
        shares: req.shares,
    };
    check_update_partner_pure(&cmd).map_err(ApiError::domain)?;

    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            req.expected_head_seq,
            move |tx| match build_update_partner_in_txn(tx, &cmd)? {
                PartnerStep::Append(event) => Ok(Verdict::Append(stamp(event, &actor))),
                PartnerStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response(outcome, ApiError::domain::<PartnershipError>)
}

#[derive(Serialize, Deserialize)]
pub struct WithdrawPartnerRequest {
    pub expected_head_seq: i64,
    pub partner_id: String,
    /// The day they left. Their K-1 for the year containing it is their final one.
    pub end_date: NaiveDate,
}

/// Record that a partner has left.
///
/// Refused if they already have. Two members recording the same departure would
/// otherwise both read "still in", both append, and the second end date would
/// silently move which year that partner's *final* K-1 falls in — which is not a
/// difference anybody notices until the K-1 is wrong.
async fn submit_withdraw_partner(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<WithdrawPartnerRequest>,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            req.expected_head_seq,
            move |tx| match build_withdraw_partner_in_txn(tx, &req.partner_id, req.end_date)? {
                PartnerStep::Append(event) => Ok(Verdict::Append(stamp(event, &actor))),
                PartnerStep::Reject(e) => Ok(Verdict::Reject(e)),
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
    use crate::store::migrations::init_schema;
    use crate::sync::router;
    use std::collections::HashMap;

    const TOKEN: &str = "tok-1";

    async fn serve() -> String {
        let store = {
            let s = EventStore::in_memory().unwrap();
            init_schema(s.connection()).unwrap();
            crate::store::migrations::run_migrations(s.connection()).unwrap();
            s
        };
        let state = SyncState::new(
            store,
            HashMap::from([(TOKEN.to_string(), "alice@example.com".to_string())]),
        );
        let app = router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
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

    fn address() -> serde_json::Value {
        serde_json::json!({
            "street": "1 Example Street",
            "city": "Cape Town",
            "state": "WC",
            "postal_code": "8001",
        })
    }

    fn shares(pct: f64) -> serde_json::Value {
        let ppm = (pct * 10_000.0).round() as i64;
        serde_json::json!({ "profit_ppm": ppm, "loss_ppm": ppm, "capital_ppm": ppm })
    }

    async fn set_profile(base: &str) -> reqwest::Response {
        post(
            base,
            "set-business-profile",
            serde_json::json!({
                "expected_head_seq": head_of(base).await,
                "legal_name": "Clovelly Technology Partners LLC",
                "address": address(),
                "ein": "88-1234567",
                "naics_code": "541511",
                "formation_date": "2021-07-01",
            }),
        )
        .await
    }

    async fn admit(base: &str, name: &str, pct: f64) -> serde_json::Value {
        post(
            base,
            "admit-partner",
            serde_json::json!({
                "expected_head_seq": head_of(base).await,
                "name": name,
                "partner_type": "general",
                "residency": "domestic",
                "entity_type": "Individual",
                "address": address(),
                "shares": shares(pct),
            }),
        )
        .await
        .json()
        .await
        .unwrap()
    }

    /// The thing that was blocked: a member on hosted books records the
    /// partnership and its partners, and gets back ids they can file TINs against
    /// locally.
    #[tokio::test]
    async fn a_partnership_and_its_partners_are_recorded_on_hosted_books() {
        let base = serve().await;
        assert_eq!(set_profile(&base).await.status(), reqwest::StatusCode::OK);

        let alice = admit(&base, "Alice Example", 50.0).await;
        assert!(
            uuid::Uuid::parse_str(alice["partner_id"].as_str().unwrap()).is_ok(),
            "the server must mint a real id: {alice}"
        );
        let bob = admit(&base, "Bob Example", 50.0).await;
        assert_ne!(alice["partner_id"], bob["partner_id"]);
    }

    /// The whole reason a TIN is not a field here. Every member replicates this
    /// log, so a number in it is a number on every member's laptop — and the
    /// endpoint must not even accept one, or a client could smuggle it in.
    #[tokio::test]
    async fn no_taxpayer_identification_number_can_reach_the_shared_log() {
        let base = serve().await;
        set_profile(&base).await;

        let r = post(
            &base,
            "admit-partner",
            serde_json::json!({
                "expected_head_seq": head_of(&base).await,
                "name": "Alice Example",
                "partner_type": "general",
                "residency": "domestic",
                "entity_type": "Individual",
                "address": address(),
                "shares": shares(50.0),
                "tin": "123-45-6789",
            }),
        )
        .await;
        assert_eq!(r.status(), reqwest::StatusCode::OK);

        let events: serde_json::Value = reqwest::Client::new()
            .get(format!("{base}/sync/events?since=0&limit=50"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let dump = events.to_string();
        assert!(
            !dump.contains("123-45-6789"),
            "a client-supplied SSN reached the shared log: {dump}"
        );
        assert!(
            !dump.contains("tin"),
            "the field must be absent entirely, not null: {dump}"
        );
    }

    /// A client-chosen id would overwrite someone else's partner — the projector
    /// does INSERT OR REPLACE on `partners(id)`, so the victim's shares, dates and
    /// name would all be replaced, and the first sign of it is a K-1 allocating
    /// them the wrong income.
    #[tokio::test]
    async fn a_client_cannot_choose_an_id_and_clobber_another_partner() {
        let base = serve().await;
        set_profile(&base).await;

        let victim = admit(&base, "Alice Example", 60.0).await;
        let victim_id = victim["partner_id"].as_str().unwrap().to_string();

        // An attacker naming the victim's id, with shares of their choosing.
        let attack: serde_json::Value = post(
            &base,
            "admit-partner",
            serde_json::json!({
                "expected_head_seq": head_of(&base).await,
                "partner_id": victim_id,
                "name": "Mallory",
                "partner_type": "limited",
                "residency": "foreign",
                "entity_type": "Corporation",
                "address": address(),
                "shares": shares(99.0),
            }),
        )
        .await
        .json()
        .await
        .unwrap();

        assert_ne!(
            attack["partner_id"], victim_id,
            "the server honoured a client-supplied partner id"
        );

        // And the victim's record is untouched.
        let partners: serde_json::Value = reqwest::Client::new()
            .get(format!("{base}/sync/events?since=0&limit=50"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let dump = partners.to_string();
        assert!(
            dump.contains("Alice Example"),
            "the victim was overwritten: {dump}"
        );
    }

    /// A second departure would silently move which year the partner's final K-1
    /// falls in.
    #[tokio::test]
    async fn a_partner_can_only_be_recorded_as_leaving_once() {
        let base = serve().await;
        set_profile(&base).await;
        let alice = admit(&base, "Alice Example", 50.0).await;
        let id = alice["partner_id"].as_str().unwrap().to_string();

        let first = post(
            &base,
            "withdraw-partner",
            serde_json::json!({
                "expected_head_seq": head_of(&base).await,
                "partner_id": id,
                "end_date": "2025-06-30",
            }),
        )
        .await;
        assert_eq!(first.status(), reqwest::StatusCode::OK);

        let second = post(
            &base,
            "withdraw-partner",
            serde_json::json!({
                "expected_head_seq": head_of(&base).await,
                "partner_id": id,
                "end_date": "2025-09-01",
            }),
        )
        .await;
        assert_eq!(
            second.status(),
            reqwest::StatusCode::UNPROCESSABLE_ENTITY,
            "a second end date was accepted"
        );
    }

    /// An id nobody has must be a refusal, not a silent success. The projector's
    /// `UPDATE … WHERE id = ?` matches no rows, so without the in-txn check the
    /// append succeeds and changes nothing whatsoever.
    #[tokio::test]
    async fn editing_a_partner_who_does_not_exist_is_refused_rather_than_ignored() {
        let base = serve().await;
        set_profile(&base).await;

        let r = post(
            &base,
            "update-partner",
            serde_json::json!({
                "expected_head_seq": head_of(&base).await,
                "partner_id": "nobody",
                "name": "Ghost",
                "partner_type": "general",
                "residency": "domestic",
                "entity_type": "Individual",
                "address": address(),
                "shares": shares(50.0),
            }),
        )
        .await;
        assert_eq!(r.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(head_of(&base).await, 1, "an event was appended for a no-op");
    }

    /// Optimistic concurrency: a client whose view of the log is stale is told to
    /// refetch rather than allowed to append against state it has not seen.
    #[tokio::test]
    async fn a_stale_head_is_a_conflict_not_an_append() {
        let base = serve().await;
        set_profile(&base).await;

        let r = post(
            &base,
            "admit-partner",
            serde_json::json!({
                "expected_head_seq": 0, // the profile already moved it to 1
                "name": "Alice Example",
                "partner_type": "general",
                "residency": "domestic",
                "entity_type": "Individual",
                "address": address(),
                "shares": shares(50.0),
            }),
        )
        .await;
        assert_eq!(r.status(), reqwest::StatusCode::CONFLICT);
        let v: serde_json::Value = r.json().await.unwrap();
        assert_eq!(v["current_head"], 1, "the client is told where to resume");
    }

    /// A partner admitted with no start date means "since the business started",
    /// which is a read of the group's header — so the header has to exist.
    #[tokio::test]
    async fn admitting_a_partner_before_the_header_exists_is_refused() {
        let base = serve().await;
        let r = post(
            &base,
            "admit-partner",
            serde_json::json!({
                "expected_head_seq": 0,
                "name": "Alice Example",
                "partner_type": "general",
                "residency": "domestic",
                "entity_type": "Individual",
                "address": address(),
                "shares": shares(50.0),
            }),
        )
        .await;
        assert_eq!(r.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    }

    /// A malformed EIN is the caller's mistake, and must read as one. Left to
    /// `validate_event` inside the append it would surface as a store error and a
    /// 500, sending somebody to look at the server's logs for their own typo.
    #[tokio::test]
    async fn a_malformed_identifier_is_the_callers_mistake_not_a_server_error() {
        let base = serve().await;
        for (field, bad) in [("ein", "881234567"), ("naics_code", "54151")] {
            let mut body = serde_json::json!({
                "expected_head_seq": 0,
                "legal_name": "Example LLC",
                "address": address(),
                "ein": "88-1234567",
                "naics_code": "541511",
                "formation_date": "2021-07-01",
            });
            body[field] = serde_json::json!(bad);
            let r = post(&base, "set-business-profile", body).await;
            assert_eq!(
                r.status(),
                reqwest::StatusCode::UNPROCESSABLE_ENTITY,
                "{field}={bad} did not read as a caller error"
            );
        }
    }

    /// A word the form does not offer is a malformed request, not a command the
    /// books refused — telling a client its partner was rejected on a domain rule
    /// would send them looking at the ledger for a typo.
    #[tokio::test]
    async fn an_unknown_partner_type_or_residency_is_a_bad_request() {
        let base = serve().await;
        set_profile(&base).await;

        for (field, bad) in [("partner_type", "limitd"), ("residency", "martian")] {
            let mut body = serde_json::json!({
                "expected_head_seq": head_of(&base).await,
                "name": "Alice Example",
                "partner_type": "general",
                "residency": "domestic",
                "entity_type": "Individual",
                "address": address(),
                "shares": shares(50.0),
            });
            body[field] = serde_json::json!(bad);
            let r = post(&base, "admit-partner", body).await;
            assert_eq!(
                r.status(),
                reqwest::StatusCode::BAD_REQUEST,
                "{field}={bad} was not reported as a malformed request"
            );
        }
    }

    /// Every endpoint is behind the same bearer auth as the rest of the transport.
    #[tokio::test]
    async fn the_partnership_endpoints_refuse_an_unauthenticated_caller() {
        let base = serve().await;
        for path in [
            "set-business-profile",
            "admit-partner",
            "update-partner",
            "withdraw-partner",
        ] {
            let r = reqwest::Client::new()
                .post(format!("{base}/sync/commands/{path}"))
                .json(&serde_json::json!({ "expected_head_seq": 0 }))
                .send()
                .await
                .unwrap();
            assert_eq!(
                r.status(),
                reqwest::StatusCode::UNAUTHORIZED,
                "{path} served an unauthenticated caller"
            );
        }
    }

    /// The client half of the seam, end to end: the desktop's route onto hosted
    /// books. Exercises the retry loop's head bookkeeping — each call must leave
    /// the client's cached head where the next one can use it, or the second write
    /// 409s forever.
    #[tokio::test]
    async fn the_sync_client_records_a_partnership_and_gets_the_minted_ids_back() {
        use crate::domain::{Address, BusinessProfile, PartnerType, Residency, Shares};
        use crate::sync::SyncClient;

        let base = serve().await;
        let mut client = SyncClient::with_head(&base, TOKEN, 0);

        let addr = Address {
            street: "1 Example Street".into(),
            suite: None,
            city: "Cape Town".into(),
            state: "WC".into(),
            postal_code: "8001".into(),
            country: None,
        };
        let profile = BusinessProfile {
            legal_name: "Clovelly Technology Partners LLC".into(),
            address: addr.clone(),
            ein: "88-1234567".into(),
            naics_code: "541511".into(),
            formation_date: NaiveDate::from_ymd_opt(2021, 7, 1).unwrap(),
            principal_activity: Some("Software".into()),
            principal_product: None,
        };
        client.set_business_profile(&profile).await.unwrap();

        let alice = client
            .admit_partner(
                "Alice Example",
                PartnerType::General,
                Residency::Domestic,
                "Individual",
                &addr,
                None,
                Shares::from_percents(50.0, 50.0, 50.0),
            )
            .await
            .unwrap();
        let bob = client
            .admit_partner(
                "Bob Example",
                PartnerType::Limited,
                Residency::Foreign,
                "Corporation",
                &addr,
                None,
                Shares::from_percents(50.0, 50.0, 50.0),
            )
            .await
            .unwrap();

        assert_ne!(alice.partner_id, bob.partner_id);
        assert!(uuid::Uuid::parse_str(&alice.partner_id).is_ok());
        assert!(bob.head > alice.head, "the head must advance with each write");

        // Editing and withdrawing go through the same cached head.
        client
            .update_partner(&UpdatePartner {
                partner_id: bob.partner_id.clone(),
                name: "Bob Renamed".into(),
                partner_type: PartnerType::Limited,
                residency: Residency::Foreign,
                entity_type: "Corporation".into(),
                address: addr.clone(),
                shares: Shares::from_percents(40.0, 40.0, 40.0),
            })
            .await
            .unwrap();
        let head = client
            .withdraw_partner(&bob.partner_id, NaiveDate::from_ymd_opt(2025, 6, 30).unwrap())
            .await
            .unwrap();
        assert_eq!(head, head_of(&base).await);

        // And the domain refusal reaches the caller as one.
        let err = client
            .withdraw_partner(&bob.partner_id, NaiveDate::from_ymd_opt(2025, 9, 1).unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(err, crate::sync::SyncClientError::Rejected(ref m) if m.contains("already left")),
            "got {err:?}"
        );
    }

    /// A share over the whole would allocate more income than exists.
    #[tokio::test]
    async fn a_share_outside_nothing_to_everything_is_refused() {
        let base = serve().await;
        set_profile(&base).await;
        let r = post(
            &base,
            "admit-partner",
            serde_json::json!({
                "expected_head_seq": head_of(&base).await,
                "name": "Greedy",
                "partner_type": "general",
                "residency": "domestic",
                "entity_type": "Individual",
                "address": address(),
                "shares": shares(101.0),
            }),
        )
        .await;
        assert_eq!(r.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    }
}
