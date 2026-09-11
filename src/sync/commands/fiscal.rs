//! Sync commands for closing and reopening a fiscal year.
//!
//! A member on group-hosted books cannot append to their own log — the ledger is
//! a replica and the instance owns the writes. Without these routes the Year end
//! page would compute a perfectly good preview and then refuse to act on it, with
//! a message about the feature not being available on server-hosted books: true,
//! and no use to somebody trying to close a year.
//!
//! # Why the whole close is one command
//!
//! Closing appends between three and five events — the fiscal year, if it was
//! never opened; the closing entry; the lock; and the Schedule L assignment for
//! the account the result landed in. They have to arrive together: the closing
//! entry is dated inside the year the lock fences, so a log carrying one without
//! the other is a year that either cannot be closed or cannot be corrected.
//!
//! So this is not "post an entry, then close a year" over two round trips. The
//! desktop holds a single pending write and nothing spans several of them, and a
//! member who closed their laptop between the two would leave the group's books
//! half-closed. [`build_close_books_in_txn`] produces the whole batch inside one
//! append transaction, and it is the same function the local command uses — the
//! invariants cannot drift between the two doors.
//!
//! # Why retrying these is safe
//!
//! `SyncClient::submit_retrying` resends a command against the head the server
//! reported, which is sound only for a command whose payload does not depend on
//! a projection that may have moved. Both of these qualify: the request carries a
//! year, an account id the user picked, and a flag. **Every figure — which
//! accounts are swept, what they hold, whether the books balance, whether an
//! earlier year is still open — is derived on the server inside the append
//! transaction**, against state nobody else can be writing to. A request that
//! carried the amounts would be exactly the sort that must not be retried.
//!
//! # What is deliberately not here
//!
//! A bare `close-year` that raises the fence without sweeping anything. The lock
//! and the closing entry are one operation; a route that could produce a locked
//! year with nothing to show what it earned would only ever be used by mistake.

use crate::commands::closing_commands::{
    build_close_books_in_txn, build_reopen_books_in_txn, CloseBooksCommand, ClosingError,
    ClosingTarget,
};
use crate::store::event_store::Verdict;
use crate::sync::{
    outcome_to_response_many, project, stamp, ApiError, AuthedUser, SubmitResponse, SyncState,
};
use axum::{extract::State, routing::post, Json, Router};
use serde::{Deserialize, Serialize};

pub fn router() -> Router<SyncState> {
    Router::new()
        .route("/sync/commands/close-books", post(submit_close_books))
        .route("/sync/commands/reopen-year", post(submit_reopen_year))
}

#[derive(Serialize, Deserialize)]
pub struct CloseBooksRequest {
    pub expected_head_seq: i64,
    pub year: i32,
    /// The equity account the year's result lands in — an id rather than a path,
    /// because resolving `Equity:Years:2023` may create accounts and creating
    /// them is its own command with its own answer. The server checks it exists
    /// and is equity, under the write lock.
    ///
    /// **`None` means allocate to partner capital instead**: one line per
    /// partner, into their own capital account, split on the percentages in
    /// force across the year. Absent rather than a separate flag so the two are
    /// mutually exclusive by construction — there is no request that names an
    /// account *and* asks for the split.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equity_account_id: Option<String>,
    #[serde(default)]
    pub include_draws: bool,
}

#[derive(Serialize, Deserialize)]
pub struct ReopenYearRequest {
    pub expected_head_seq: i64,
    pub year: i32,
    /// Why. Required, and it goes in the group's event log: reopening a filed
    /// year is legitimate and is also what a mistake looks like, and on shared
    /// books the person who finds it later is not the person who did it.
    pub reason: String,
}

/// A tax year somebody could plausibly be closing. Bounds rather than a real
/// range: this is the envelope check, not a domain rule — a year with no
/// activity is refused by `NothingToClose` inside the transaction.
fn plausible_year(year: i32) -> bool {
    (1900..=2200).contains(&year)
}

async fn submit_close_books(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<CloseBooksRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    if !plausible_year(req.year) {
        return Err(ApiError::bad_request("year is not a tax year"));
    }
    let target = match req.equity_account_id {
        None => ClosingTarget::PartnerCapital,
        Some(id) if id.trim().is_empty() => {
            return Err(ApiError::bad_request(
                "equity_account_id is empty; omit it entirely to allocate to partner capital",
            ))
        }
        Some(id) => ClosingTarget::Account(id),
    };

    let cmd = CloseBooksCommand {
        year: req.year,
        target,
        include_draws: req.include_draws,
    };
    let expected = req.expected_head_seq;

    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked_many(
            expected,
            move |tx| {
                Ok(match build_close_books_in_txn(tx, &cmd)? {
                    Verdict::Append(events) => {
                        Verdict::Append(events.into_iter().map(|e| stamp(e, &actor)).collect())
                    }
                    Verdict::Reject(e) => Verdict::Reject(e),
                })
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response_many(outcome, expected, ApiError::domain::<ClosingError>)
}

async fn submit_reopen_year(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<ReopenYearRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    if !plausible_year(req.year) {
        return Err(ApiError::bad_request("year is not a tax year"));
    }
    let reason = req.reason.trim().to_string();
    if reason.is_empty() {
        return Err(ApiError::bad_request(
            "reason is required — reopening a closed year goes in the event log",
        ));
    }
    let expected = req.expected_head_seq;
    let year = req.year;

    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked_many(
            expected,
            move |tx| {
                Ok(
                    match build_reopen_books_in_txn(tx, year, &reason, &actor)? {
                        Verdict::Append(events) => {
                            Verdict::Append(events.into_iter().map(|e| stamp(e, &actor)).collect())
                        }
                        Verdict::Reject(e) => Verdict::Reject(e),
                    },
                )
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response_many(outcome, expected, ApiError::domain::<ClosingError>)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::account_commands::{AccountCommands, CreateAccountCommand};
    use crate::commands::entry_commands::{EntryCommands, EntryLine, PostEntryCommand};
    use crate::domain::AccountType;
    use crate::events::types::JournalEntrySource;
    use crate::store::event_store::EventStore;
    use crate::store::migrations::{init_schema, run_migrations};
    use crate::sync::router;
    use chrono::NaiveDate;
    use std::collections::HashMap;

    const TOKEN: &str = "tok-1";

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    /// A group's books with 2023 trading: 5,000 of sales against 3,000 of rent.
    /// Returns the base URL, the store handle, and the equity account's id.
    async fn serve_with_a_year_to_close(
    ) -> (String, std::sync::Arc<std::sync::Mutex<EventStore>>, String) {
        let mut store = {
            let s = EventStore::in_memory().unwrap();
            init_schema(s.connection()).unwrap();
            run_migrations(s.connection()).unwrap();
            s
        };
        store
            .connection()
            .execute(
                "INSERT INTO company (id, company_id, name, base_currency, fiscal_year_start_month)
                 VALUES ('c', 'c', 'Co', 'USD', 1)",
                [],
            )
            .unwrap();

        for (ty, number, name) in [
            (AccountType::Asset, "1000", "Cash"),
            (AccountType::Revenue, "4000", "Sales"),
            (AccountType::Expense, "6100", "Rent"),
            (AccountType::Equity, "3023", "2023"),
        ] {
            AccountCommands::new(&mut store, "seed".to_string())
                .create_account(CreateAccountCommand {
                    account_type: ty,
                    account_number: number.to_string(),
                    name: name.to_string(),
                    parent_id: None,
                    currency: Some("USD".to_string()),
                    description: None,
                })
                .unwrap();
        }
        let id = |store: &EventStore, n: &str| -> String {
            store
                .connection()
                .query_row(
                    "SELECT id FROM accounts WHERE account_number = ?1",
                    [n],
                    |r| r.get(0),
                )
                .unwrap()
        };
        let (cash, sales, rent, equity) = (
            id(&store, "1000"),
            id(&store, "4000"),
            id(&store, "6100"),
            id(&store, "3023"),
        );

        for (date, debit, credit, cents) in [
            (day(2023, 3, 1), &cash, &sales, 500_000i64),
            (day(2023, 9, 1), &rent, &cash, 300_000),
        ] {
            EntryCommands::new(&mut store, "seed".to_string())
                .post_entry(PostEntryCommand {
                    date,
                    memo: "t".to_string(),
                    lines: vec![
                        EntryLine::debit(debit, cents, "USD"),
                        EntryLine::credit(credit, cents, "USD"),
                    ],
                    reference: None,
                    source: Some(JournalEntrySource::Manual),
                })
                .unwrap();
        }

        let state = SyncState::new(
            store,
            HashMap::from([(TOKEN.to_string(), "alice@example.com".to_string())]),
        );
        let handle = state.store.clone();
        let app = router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), handle, equity)
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

    fn count(store: &std::sync::Arc<std::sync::Mutex<EventStore>>, sql: &str) -> i64 {
        store
            .lock()
            .unwrap()
            .connection()
            .query_row(sql, [], |r| r.get(0))
            .unwrap()
    }

    /// The round trip the module exists for: a member on hosted books closes a
    /// year, and the instance's own projection shows it closed and swept.
    #[tokio::test]
    async fn a_year_closes_on_the_instance() {
        let (base, store, equity) = serve_with_a_year_to_close().await;
        let head = head_of(&base).await;

        let r = post_json(
            &base,
            "/sync/commands/close-books",
            &CloseBooksRequest {
                expected_head_seq: head,
                year: 2023,
                equity_account_id: Some(equity.clone()),
                include_draws: false,
            },
        )
        .await;
        assert_eq!(r.status(), reqwest::StatusCode::OK);
        let new_head = r.json::<serde_json::Value>().await.unwrap()["head"]
            .as_i64()
            .unwrap();
        assert!(new_head > head);

        assert_eq!(
            count(
                &store,
                "SELECT is_closed FROM fiscal_years WHERE year = 2023"
            ),
            1
        );
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM journal_entries WHERE source = 'closing' AND is_void = 0"
            ),
            1
        );
        // Revenue and expense are flat at year end.
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM (
                   SELECT a.id, SUM(jl.amount) bal FROM accounts a
                     JOIN journal_lines jl ON jl.account_id = a.id
                     JOIN journal_entries je ON jl.entry_id = je.id
                    WHERE je.is_void = 0 AND je.date <= '2023-12-31'
                      AND a.account_type IN ('revenue','expense')
                    GROUP BY a.id HAVING bal != 0)"
            ),
            0
        );
        // The result is in the year account.
        assert_eq!(
            count(
                &store,
                &format!(
                    "SELECT COALESCE(SUM(jl.amount), 0) FROM journal_lines jl
                       JOIN journal_entries je ON jl.entry_id = je.id
                      WHERE je.is_void = 0 AND jl.account_id = '{equity}'"
                )
            ),
            -200_000
        );
    }

    /// The whole close is one append, so the year that had never been opened is
    /// opened in the same batch — the client sees one head move, not four.
    #[tokio::test]
    async fn the_whole_close_lands_as_one_batch() {
        let (base, store, equity) = serve_with_a_year_to_close().await;
        let head = head_of(&base).await;

        post_json(
            &base,
            "/sync/commands/close-books",
            &CloseBooksRequest {
                expected_head_seq: head,
                year: 2023,
                equity_account_id: Some(equity),
                include_draws: false,
            },
        )
        .await;

        let types: Vec<String> = {
            let guard = store.lock().unwrap();
            let conn = guard.connection();
            let mut stmt = conn
                .prepare("SELECT event_type FROM events WHERE id > ?1 ORDER BY id")
                .unwrap();
            let rows = stmt.query_map([head], |r| r.get::<_, String>(0)).unwrap();
            rows.filter_map(|r| r.ok()).collect()
        };
        assert_eq!(
            types,
            vec![
                "fiscal_year_opened",
                "journal_entry_posted",
                "year_end_closed",
                "tax_line_mapping_set",
            ],
        );
    }

    /// Every event carries the authenticated member, not the desktop's idea of
    /// who it is — on shared books "who closed 2023" has to be answerable.
    #[tokio::test]
    async fn the_actor_on_the_batch_is_the_authenticated_member() {
        let (base, store, equity) = serve_with_a_year_to_close().await;
        let head = head_of(&base).await;
        post_json(
            &base,
            "/sync/commands/close-books",
            &CloseBooksRequest {
                expected_head_seq: head,
                year: 2023,
                equity_account_id: Some(equity),
                include_draws: false,
            },
        )
        .await;

        let actors: Vec<String> = {
            let guard = store.lock().unwrap();
            let conn = guard.connection();
            let mut stmt = conn
                .prepare("SELECT DISTINCT actor_id FROM events WHERE id > ?1")
                .unwrap();
            let rows = stmt.query_map([head], |r| r.get::<_, String>(0)).unwrap();
            rows.filter_map(|r| r.ok()).collect()
        };
        assert_eq!(actors, vec!["alice@example.com".to_string()]);
    }

    /// Two members closing the same year: the second gets a terminal 422 naming
    /// the entry that already did it, not a second closing entry.
    #[tokio::test]
    async fn closing_a_year_twice_is_a_domain_rejection() {
        let (base, store, equity) = serve_with_a_year_to_close().await;
        let head = head_of(&base).await;

        let first = post_json(
            &base,
            "/sync/commands/close-books",
            &CloseBooksRequest {
                expected_head_seq: head,
                year: 2023,
                equity_account_id: Some(equity.clone()),
                include_draws: false,
            },
        )
        .await;
        assert_eq!(first.status(), reqwest::StatusCode::OK);
        let head = head_of(&base).await;

        let second = post_json(
            &base,
            "/sync/commands/close-books",
            &CloseBooksRequest {
                expected_head_seq: head,
                year: 2023,
                equity_account_id: Some(equity),
                include_draws: false,
            },
        )
        .await;
        assert_eq!(second.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        let body = second.json::<serde_json::Value>().await.unwrap();
        assert!(
            body["error"].as_str().unwrap().contains("already closed"),
            "{body}"
        );
        assert_eq!(
            head_of(&base).await,
            head,
            "a rejection must not move the log"
        );
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM journal_entries WHERE source = 'closing' AND is_void = 0"
            ),
            1
        );
    }

    /// A stale head is a 409 carrying the server's, which is what lets the
    /// client's retry rebuild the request rather than resend a known-stale one.
    #[tokio::test]
    async fn a_stale_head_is_a_conflict() {
        let (base, _store, equity) = serve_with_a_year_to_close().await;

        let r = post_json(
            &base,
            "/sync/commands/close-books",
            &CloseBooksRequest {
                expected_head_seq: 0,
                year: 2023,
                equity_account_id: Some(equity),
                include_draws: false,
            },
        )
        .await;
        assert_eq!(r.status(), reqwest::StatusCode::CONFLICT);
    }

    /// The server derives the figures, so a request naming an account that is not
    /// equity is refused there rather than trusted.
    #[tokio::test]
    async fn the_target_has_to_be_an_equity_account() {
        let (base, store, _equity) = serve_with_a_year_to_close().await;
        let cash: String = store
            .lock()
            .unwrap()
            .connection()
            .query_row(
                "SELECT id FROM accounts WHERE account_number = '1000'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let head = head_of(&base).await;

        let r = post_json(
            &base,
            "/sync/commands/close-books",
            &CloseBooksRequest {
                expected_head_seq: head,
                year: 2023,
                equity_account_id: Some(cash),
                include_draws: false,
            },
        )
        .await;
        assert_eq!(r.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(head_of(&base).await, head);
    }

    #[tokio::test]
    async fn a_malformed_request_is_a_bad_request_not_a_rejection() {
        let (base, _store, equity) = serve_with_a_year_to_close().await;
        let head = head_of(&base).await;

        for (year, account) in [(12, equity.as_str()), (2023, "  ")] {
            let r = post_json(
                &base,
                "/sync/commands/close-books",
                &CloseBooksRequest {
                    expected_head_seq: head,
                    year,
                    equity_account_id: Some(account.to_string()),
                    include_draws: false,
                },
            )
            .await;
            assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);
        }

        let r = post_json(
            &base,
            "/sync/commands/reopen-year",
            &ReopenYearRequest {
                expected_head_seq: head,
                year: 2023,
                reason: "   ".to_string(),
            },
        )
        .await;
        assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);
    }

    /// Reopening voids the closing entry and lifts the fence together, and the
    /// year can then be closed again.
    #[tokio::test]
    async fn a_year_reopens_and_can_be_closed_again() {
        let (base, store, equity) = serve_with_a_year_to_close().await;

        let head = head_of(&base).await;
        post_json(
            &base,
            "/sync/commands/close-books",
            &CloseBooksRequest {
                expected_head_seq: head,
                year: 2023,
                equity_account_id: Some(equity.clone()),
                include_draws: false,
            },
        )
        .await;

        let head = head_of(&base).await;
        let r = post_json(
            &base,
            "/sync/commands/reopen-year",
            &ReopenYearRequest {
                expected_head_seq: head,
                year: 2023,
                reason: "a missing invoice turned up".to_string(),
            },
        )
        .await;
        assert_eq!(r.status(), reqwest::StatusCode::OK);

        assert_eq!(
            count(
                &store,
                "SELECT is_closed FROM fiscal_years WHERE year = 2023"
            ),
            0
        );
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM journal_entries WHERE source = 'closing' AND is_void = 0"
            ),
            0
        );

        let head = head_of(&base).await;
        let again = post_json(
            &base,
            "/sync/commands/close-books",
            &CloseBooksRequest {
                expected_head_seq: head,
                year: 2023,
                equity_account_id: Some(equity),
                include_draws: false,
            },
        )
        .await;
        assert_eq!(again.status(), reqwest::StatusCode::OK);
    }

    #[tokio::test]
    async fn reopening_a_year_that_is_not_closed_is_rejected() {
        let (base, _store, _equity) = serve_with_a_year_to_close().await;
        let head = head_of(&base).await;

        let r = post_json(
            &base,
            "/sync/commands/reopen-year",
            &ReopenYearRequest {
                expected_head_seq: head,
                year: 2023,
                reason: "no reason".to_string(),
            },
        )
        .await;
        assert_eq!(r.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn both_routes_need_a_token() {
        let (base, _store, equity) = serve_with_a_year_to_close().await;
        let http = reqwest::Client::new();

        for (path, body) in [
            (
                "/sync/commands/close-books",
                serde_json::json!({ "expected_head_seq": 0, "year": 2023,
                                    "equity_account_id": equity }),
            ),
            (
                "/sync/commands/reopen-year",
                serde_json::json!({ "expected_head_seq": 0, "year": 2023, "reason": "x" }),
            ),
        ] {
            let r = http
                .post(format!("{base}{path}"))
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), reqwest::StatusCode::UNAUTHORIZED, "{path}");
        }
    }
}
