//! Vendor payable rules and recurring transfer rules on group-hosted books.
//!
//! Until these routes the rules lived on each member's machine, so the group's
//! books carried the bills and transfers the rules produced but not the rules
//! themselves. The checks are the local commands' — see
//! [`crate::commands::shared_rules`] — so these handlers are transport only.

use crate::commands::recurring_transfers::AmountMode;
use crate::commands::shared_rules::{
    check_create_recurring_rule, check_delete_recurring_rule, check_delete_vendor_rule,
    check_set_recurring_rule_active, check_set_vendor_rule, Checked, NewRecurringRule, RuleError,
};
use crate::store::event_store::Verdict;
use crate::sync::{outcome_to_response, project, stamp, ApiError, AuthedUser, SyncState};
use axum::{extract::State, routing::post, Json, Router};
use serde::{Deserialize, Serialize};

pub fn router() -> Router<SyncState> {
    Router::new()
        .route("/sync/commands/set-vendor-rule", post(submit_set_vendor_rule))
        .route(
            "/sync/commands/delete-vendor-rule",
            post(submit_delete_vendor_rule),
        )
        .route(
            "/sync/commands/create-recurring-transfer-rule",
            post(submit_create_recurring_rule),
        )
        .route(
            "/sync/commands/set-recurring-transfer-rule-active",
            post(submit_set_recurring_rule_active),
        )
        .route(
            "/sync/commands/delete-recurring-transfer-rule",
            post(submit_delete_recurring_rule),
        )
}

#[derive(Serialize, Deserialize)]
pub struct SetVendorRuleRequest {
    pub expected_head_seq: i64,
    /// Used only when no rule names this pattern yet, so publishing a rule made
    /// before rules were events keeps its id. Minted when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    pub pattern: String,
    pub account_id: String,
}

#[derive(Serialize, Deserialize)]
pub struct RuleIdRequest {
    pub expected_head_seq: i64,
    pub rule_id: String,
}

#[derive(Serialize, Deserialize)]
pub struct CreateRecurringRuleRequest {
    pub expected_head_seq: i64,
    /// Present when publishing an existing rule: its posted transfers carry the
    /// id in their references. Refused if taken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    pub source_account_id: String,
    pub dest_account_id: String,
    pub day_of_month: u32,
    /// `"full_balance"` or `"fixed"`.
    pub amount_mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_amount_cents: Option<i64>,
    #[serde(default)]
    pub memo: String,
    pub start_month: String,
}

#[derive(Serialize, Deserialize)]
pub struct SetRecurringRuleActiveRequest {
    pub expected_head_seq: i64,
    pub rule_id: String,
    pub active: bool,
}

async fn submit_set_vendor_rule(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<SetVendorRuleRequest>,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    append(st, req.expected_head_seq, actor, move |tx| {
        check_set_vendor_rule(tx, req.rule_id.as_deref(), &req.pattern, &req.account_id)
    })
}

async fn submit_delete_vendor_rule(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<RuleIdRequest>,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    append(st, req.expected_head_seq, actor, move |tx| {
        check_delete_vendor_rule(tx, &req.rule_id)
    })
}

async fn submit_create_recurring_rule(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<CreateRecurringRuleRequest>,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    let amount_mode = match req.amount_mode.as_str() {
        "full_balance" => AmountMode::FullBalance,
        "fixed" => AmountMode::Fixed,
        _ => {
            return Err(ApiError::bad_request(
                "amount_mode must be full_balance or fixed",
            ))
        }
    };
    let rule = NewRecurringRule {
        rule_id: req.rule_id,
        source_account_id: req.source_account_id,
        dest_account_id: req.dest_account_id,
        day_of_month: req.day_of_month,
        amount_mode,
        fixed_amount_cents: req.fixed_amount_cents,
        memo: req.memo,
        start_month: req.start_month,
    };
    append(st, req.expected_head_seq, actor, move |tx| {
        check_create_recurring_rule(tx, &rule)
    })
}

async fn submit_set_recurring_rule_active(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<SetRecurringRuleActiveRequest>,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    append(st, req.expected_head_seq, actor, move |tx| {
        check_set_recurring_rule_active(tx, &req.rule_id, req.active)
    })
}

async fn submit_delete_recurring_rule(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<RuleIdRequest>,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    append(st, req.expected_head_seq, actor, move |tx| {
        check_delete_recurring_rule(tx, &req.rule_id)
    })
}

fn append(
    st: SyncState,
    expected_head_seq: i64,
    actor: String,
    check: impl FnOnce(&rusqlite::Transaction<'_>) -> Checked,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            expected_head_seq,
            move |tx| {
                Ok(match check(tx)? {
                    Ok(event) => Verdict::Append(stamp(event, &actor)),
                    Err(e) => Verdict::Reject(e),
                })
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response(outcome, ApiError::domain::<RuleError>)
}

#[cfg(test)]
mod tests {
    use crate::domain::AccountType;
    use crate::store::event_store::EventStore;
    use crate::store::migrations::{init_schema, run_migrations};
    use crate::sync::{router, SyncClient, SyncState};
    use std::collections::HashMap;

    const TOKEN: &str = "tok-1";

    async fn serve() -> String {
        let store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        run_migrations(store.connection()).unwrap();
        let state = SyncState::new(
            store,
            HashMap::from([(TOKEN.to_string(), "alice@example.com".to_string())]),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
        format!("http://{addr}")
    }

    /// Both kinds of rule set through the group land in its log, where every
    /// member's next sync picks them up.
    #[tokio::test]
    async fn rules_set_through_the_group_land_in_the_log() {
        let base = serve().await;
        let mut client = SyncClient::new(base.clone(), TOKEN);
        let qbp = client
            .ensure_account_path("AP - QBP", AccountType::Liability)
            .await
            .unwrap()
            .account_id;
        let card = client
            .ensure_account_path("Zak card", AccountType::Liability)
            .await
            .unwrap()
            .account_id;

        client
            .set_vendor_rule(None, "Quality Bicycle", &qbp)
            .await
            .unwrap();
        client
            .create_recurring_transfer_rule(
                Some("rule-1".to_string()),
                &card,
                &qbp,
                22,
                "full_balance",
                None,
                "",
                "2026-01",
            )
            .await
            .unwrap();
        client
            .set_recurring_transfer_rule_active("rule-1", false)
            .await
            .unwrap();

        let dump = reqwest::Client::new()
            .get(format!("{base}/sync/events?since=0&limit=20"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        for kind in [
            "vendor_rule_set",
            "recurring_transfer_rule_created",
            "recurring_transfer_rule_active_set",
        ] {
            assert!(dump.contains(kind), "{kind} missing: {dump}");
        }

        // The same id again is refused, not overwritten.
        let again = client
            .create_recurring_transfer_rule(
                Some("rule-1".to_string()),
                &card,
                &qbp,
                1,
                "full_balance",
                None,
                "",
                "2026-01",
            )
            .await;
        assert!(again.is_err());
        client.delete_recurring_transfer_rule("rule-1").await.unwrap();
    }
}
