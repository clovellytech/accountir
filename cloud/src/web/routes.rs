use askama::Template;
use axum::{
    extract::{Form, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Json, Router,
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use chrono::{NaiveDate, Utc};
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::password::{hash_password, verify_password};
use crate::auth::session::{create_session, delete_session, lookup_session, SESSION_COOKIE_NAME};
use crate::auth::AuthenticatedUser;
use crate::commands::{
    account::{create_account, CreateAccountInput},
    entry::{post_entry, EntryLineInput, PostEntryInput},
    mutations::{reassign_line, unvoid_entry, void_entry},
};
use crate::error::AppError;
use crate::http::AppState;
use crate::queries;
use accountir_core::events::types::EventAccountType;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(root))
        .route("/login", get(login_form).post(login_submit))
        .route("/signup", get(signup_form).post(signup_submit))
        .route("/logout", post(logout_submit))
        .route("/app/accounts", get(accounts_list).post(account_create))
        .route("/app/accounts/new", get(account_new))
        .route("/app/entries", get(entries_list).post(entry_create))
        .route("/app/entries/new", get(entry_new))
        .route("/app/entries/lines/new", get(entry_line_fragment))
        .route("/app/banks", get(banks_list))
        .route("/app/banks/link", get(banks_link))
        .route("/app/banks/{id}/sync", post(banks_sync))
        .route("/app/banks/{id}/unlink", post(banks_unlink))
        .route("/app/banks/{id}/provision", post(banks_provision))
        .route("/app/banks/{id}/historical", get(banks_historical))
        .route("/app/banks/{id}/statements-check", get(banks_statements_check))
        .route("/app/admin/companies", get(admin_companies).post(admin_company_create))
        .route("/app/admin/companies/{id}/switch", post(admin_company_switch))
        .route("/app/admin/members", get(admin_members).post(admin_member_add))
        .route("/app/admin/members/{user_id}/remove", post(admin_member_remove))
        .route("/app/admin/members/{user_id}/role", post(admin_member_role))
        .route("/app/admin/settings", get(admin_settings_view).post(admin_settings_save))
        .route("/app/admin/invitations", post(admin_invitation_create))
        .route("/accept-invite/{token}", get(accept_invite_view).post(accept_invite_submit))
        .route("/app/chat", get(chat_view))
        .route("/app/chat/messages", post(chat_send))
        .route("/app/chat/clear", post(chat_clear_route))
        .route("/app/dashboard", get(dashboard))
        .route("/app/transactions", get(transactions_list))
        .route("/app/transactions/bulk-reclassify", post(transactions_bulk_reclassify))
        .route("/app/transactions/{line_id}/reclassify", post(transaction_reclassify))
        .route("/app/entries/{entry_id}/void", post(entry_void))
        .route("/app/entries/{entry_id}/unvoid", post(entry_unvoid))
        .route("/app/reports", get(reports_index))
        .route("/app/reports/trial-balance", get(trial_balance))
        .route("/app/reports/income-statement", get(report_income))
        .route("/app/reports/balance-sheet", get(report_balance_sheet))
        .route("/app/reports/cash-flow", get(report_cash_flow))
        .route("/app/customers", get(customers_list).post(customer_create))
        .route("/app/customers/new", get(customer_new_view))
        .route("/app/invoices", get(invoices_list).post(invoice_create))
        .route("/app/invoices/new", get(invoice_new_view))
        .route("/app/invoices/{id}", get(invoice_detail_view))
        .route("/app/invoices/{id}/issue", post(invoice_issue))
        .route("/app/invoices/{id}/payment", post(invoice_payment))
        .route("/app/invoices/{id}/void", post(invoice_void))
        .route("/app/invoices/{id}/send", post(invoice_send))
        .route("/invoice/{token}", get(invoice_public_view))
}

// ---------------------------------------------------------------------------
// Templates
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTpl {
    user_email: Option<String>,
    flash: Option<String>,
    flash_kind: Option<String>,
    nav: NavCtx,
}

#[derive(Template)]
#[template(path = "signup.html")]
struct SignupTpl {
    user_email: Option<String>,
    flash: Option<String>,
    flash_kind: Option<String>,
    nav: NavCtx,
}

#[derive(Template)]
#[template(path = "accounts_list.html")]
struct AccountsListTpl {
    user_email: Option<String>,
    flash: Option<String>,
    flash_kind: Option<String>,
    nav: NavCtx,
    accounts: Vec<queries::AccountRow>,
}

#[derive(Template)]
#[template(path = "accounts_new.html")]
struct AccountsNewTpl {
    user_email: Option<String>,
    flash: Option<String>,
    flash_kind: Option<String>,
    nav: NavCtx,
}

#[derive(Template)]
#[template(path = "entries_list.html")]
struct EntriesListTpl {
    user_email: Option<String>,
    flash: Option<String>,
    flash_kind: Option<String>,
    nav: NavCtx,
    entries: Vec<queries::EntryRow>,
}

#[derive(Template)]
#[template(path = "entries_new.html")]
struct EntriesNewTpl {
    user_email: Option<String>,
    flash: Option<String>,
    flash_kind: Option<String>,
    nav: NavCtx,
    accounts: Vec<queries::AccountRow>,
    today: String,
}

#[derive(Template)]
#[template(path = "entry_line_fragment.html")]
struct EntryLineFragmentTpl {
    accounts: Vec<queries::AccountRow>,
}

#[derive(Template)]
#[template(path = "banks_list.html")]
struct BanksListTpl {
    user_email: Option<String>,
    flash: Option<String>,
    flash_kind: Option<String>,
    nav: NavCtx,
    items: Vec<queries::PlaidItemRow>,
}

#[derive(Template)]
#[template(path = "banks_link.html")]
struct BanksLinkTpl {
    user_email: Option<String>,
    flash: Option<String>,
    flash_kind: Option<String>,
    nav: NavCtx,
}

#[derive(Template)]
#[template(path = "chat.html")]
struct ChatTpl {
    user_email: Option<String>,
    flash: Option<String>,
    flash_kind: Option<String>,
    nav: NavCtx,
    messages: Vec<ChatGroup>,
    api_key_set: bool,
}

pub struct ChatGroup {
    pub lines: Vec<ChatLine>,
}
pub struct ChatLine {
    pub role: String,
    pub body: String,
}

#[derive(Template)]
#[template(path = "admin_settings.html")]
struct AdminSettingsTpl {
    user_email: Option<String>,
    flash: Option<String>,
    flash_kind: Option<String>,
    nav: NavCtx,
    active_company_name: String,
    company_name: String,
    base_currency: String,
    fiscal_year_start_month: i16,
    can_edit: bool,
    all_companies: Vec<queries::CompanyRow>,
    active_company_id: String,
}

#[derive(Template)]
#[template(path = "accept_invite.html")]
struct AcceptInviteTpl {
    user_email: Option<String>,
    flash: Option<String>,
    flash_kind: Option<String>,
    nav: NavCtx,
    company_name: String,
    role: String,
    token: String,
    logged_in: bool,
    all_companies: Vec<queries::CompanyRow>,
    active_company_id: String,
    active_company_name: String,
}

#[derive(Template)]
#[template(path = "admin_companies.html")]
struct AdminCompaniesTpl {
    user_email: Option<String>,
    flash: Option<String>,
    flash_kind: Option<String>,
    nav: NavCtx,
    companies: Vec<queries::CompanyRow>,
    active_company_id: Uuid,
    active_company_name: String,
}

#[derive(Template)]
#[template(path = "admin_members.html")]
struct AdminMembersTpl {
    user_email: Option<String>,
    flash: Option<String>,
    flash_kind: Option<String>,
    nav: NavCtx,
    members: Vec<queries::MemberRow>,
    active_company_name: String,
    can_admin: bool,
    invitations: Vec<queries::InvitationRow>,
    invite_url_base: String,
}

#[derive(Template)]
#[template(path = "transactions.html")]
struct TransactionsTpl {
    user_email: Option<String>,
    flash: Option<String>,
    flash_kind: Option<String>,
    nav: NavCtx,
    rows: Vec<queries::TransactionLine>,
    accounts: Vec<queries::AccountRow>,
    sources: Vec<&'static str>,
    selected_account_id: Option<String>,
    selected_source: Option<String>,
    search: Option<String>,
    start_str: String,
    end_str: String,
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardTpl {
    user_email: Option<String>,
    flash: Option<String>,
    flash_kind: Option<String>,
    nav: NavCtx,
    company_name: String,
    kpis: queries::DashboardKpis,
}

#[derive(Template)]
#[template(path = "reports_index.html")]
struct ReportsIndexTpl {
    user_email: Option<String>,
    flash: Option<String>,
    flash_kind: Option<String>,
    nav: NavCtx,
}

#[derive(Template)]
#[template(path = "report_income.html")]
struct ReportIncomeTpl {
    user_email: Option<String>,
    flash: Option<String>,
    flash_kind: Option<String>,
    nav: NavCtx,
    report: queries::IncomeStatement,
    company_name: String,
    generated_on: String,
}

#[derive(Template)]
#[template(path = "report_balance.html")]
struct ReportBalanceTpl {
    user_email: Option<String>,
    flash: Option<String>,
    flash_kind: Option<String>,
    nav: NavCtx,
    report: queries::BalanceSheet,
    company_name: String,
    generated_on: String,
}

#[derive(Template)]
#[template(path = "report_cashflow.html")]
struct ReportCashFlowTpl {
    user_email: Option<String>,
    flash: Option<String>,
    flash_kind: Option<String>,
    nav: NavCtx,
    report: queries::CashFlow,
    company_name: String,
    generated_on: String,
}

#[derive(Template)]
#[template(path = "trial_balance.html")]
struct TrialBalanceTpl {
    user_email: Option<String>,
    flash: Option<String>,
    flash_kind: Option<String>,
    nav: NavCtx,
    rows: Vec<queries::TrialBalanceRow>,
    total_debit_display: String,
    total_credit_display: String,
    balance_msg: String,
    balance_class: String,
    company_name: String,
    generated_on: String,
}

fn render<T: Template>(t: T) -> Response {
    match t.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "template render failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "template error").into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Auth helpers
// ---------------------------------------------------------------------------

/// Nav context — needed by base.html for the company switcher. Public so templates can reference it.
#[derive(Default, Clone)]
pub struct NavCtx {
    pub all_companies: Vec<queries::CompanyRow>,
    pub active_company_id: String,
    pub active_company_name: String,
}

async fn build_nav(state: &AppState, jar: &CookieJar, user_id: Uuid) -> NavCtx {
    let companies = queries::list_companies_for_user(&state.pool, user_id).await.unwrap_or_default();
    let active = active_company(state, jar, user_id).await.unwrap_or_else(Uuid::nil);
    let name = companies.iter().find(|c| c.id == active).map(|c| c.name.clone()).unwrap_or_default();
    NavCtx {
        all_companies: companies,
        active_company_id: active.to_string(),
        active_company_name: name,
    }
}

async fn current_user(state: &AppState, jar: &CookieJar) -> Option<AuthenticatedUser> {
    let token = jar.get(SESSION_COOKIE_NAME)?.value().to_string();
    let session = lookup_session(&state.pool, &token).await.ok().flatten()?;
    Some(session.into())
}

async fn require_user(
    state: &AppState,
    jar: &CookieJar,
) -> Result<AuthenticatedUser, Response> {
    match current_user(state, jar).await {
        Some(u) => Ok(u),
        None => Err(Redirect::to("/login").into_response()),
    }
}

const ACTIVE_COMPANY_COOKIE: &str = "accountir_active_company";

/// Active company = cookie value if user has membership in it, else first membership.
async fn active_company(
    state: &AppState,
    jar: &CookieJar,
    user_id: Uuid,
) -> Option<Uuid> {
    if let Some(c) = jar.get(ACTIVE_COMPANY_COOKIE) {
        if let Ok(uuid) = Uuid::parse_str(c.value()) {
            if queries::user_has_membership(&state.pool, user_id, uuid)
                .await
                .unwrap_or(false)
            {
                return Some(uuid);
            }
        }
    }
    queries::resolve_company_id(&state.pool, user_id).await.ok().flatten()
}

fn build_active_company_cookie(state: &AppState, company_id: Uuid) -> Cookie<'static> {
    Cookie::build((ACTIVE_COMPANY_COOKIE, company_id.to_string()))
        .http_only(true)
        .secure(state.config.cookie_secure)
        .same_site(SameSite::Lax)
        .path("/")
        .max_age(time::Duration::days(state.config.session_ttl_days))
        .build()
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn root(State(state): State<AppState>, jar: CookieJar) -> Response {
    if current_user(&state, &jar).await.is_some() {
        Redirect::to("/app/dashboard").into_response()
    } else {
        Redirect::to("/login").into_response()
    }
}

async fn login_form(State(state): State<AppState>, jar: CookieJar) -> Response {
    let user = current_user(&state, &jar).await;
    render(LoginTpl {
        user_email: user.map(|u| u.email),
        flash: None,
        flash_kind: None,
        nav: NavCtx::default(),
    })
}

#[derive(Deserialize)]
struct LoginForm {
    email: String,
    password: String,
}

async fn login_submit(
    State(state): State<AppState>,
    jar: CookieJar,
    Form(req): Form<LoginForm>,
) -> Response {
    let normalized = req.email.trim().to_lowercase();

    let row: Result<Option<(Uuid, String)>, sqlx::Error> = sqlx::query_as(
        "SELECT id, password_hash FROM auth_users WHERE email_normalized = $1 AND is_active = true",
    )
    .bind(&normalized)
    .fetch_optional(&state.pool)
    .await;

    let (id, hash) = match row {
        Ok(Some(v)) => v,
        Ok(None) => return render_login_err("invalid email or password"),
        Err(e) => {
            tracing::error!(error = %e, "login query failed");
            return render_login_err("internal error");
        }
    };

    let ok = verify_password(&req.password, &hash).unwrap_or(false);
    if !ok {
        return render_login_err("invalid email or password");
    }

    match create_session(&state.pool, id, state.config.session_ttl_days, None).await {
        Ok((_, token)) => {
            let cookie = build_session_cookie(&state, token);
            (jar.add(cookie), Redirect::to("/app/dashboard")).into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "session create failed");
            render_login_err("internal error")
        }
    }
}

fn render_login_err(msg: &str) -> Response {
    render(LoginTpl {
        user_email: None,
        flash: Some(msg.into()),
        flash_kind: Some("err".into()),
        nav: NavCtx::default(),
    })
}

async fn signup_form(State(state): State<AppState>, jar: CookieJar) -> Response {
    let user = current_user(&state, &jar).await;
    render(SignupTpl {
        user_email: user.map(|u| u.email),
        flash: None,
        flash_kind: None,
        nav: NavCtx::default(),
    })
}

#[derive(Deserialize)]
struct SignupForm {
    email: String,
    password: String,
    name: Option<String>,
}

async fn signup_submit(
    State(state): State<AppState>,
    jar: CookieJar,
    Form(req): Form<SignupForm>,
) -> Response {
    let email = req.email.trim().to_string();
    let normalized = email.to_lowercase();
    if !email.contains('@') {
        return render_signup_err("valid email required");
    }
    if req.password.len() < 8 {
        return render_signup_err("password must be at least 8 characters");
    }

    let pw_hash = match hash_password(&req.password) {
        Ok(h) => h,
        Err(_) => return render_signup_err("internal error"),
    };

    let mut tx = match state.pool.begin().await {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, "tx begin failed");
            return render_signup_err("internal error");
        }
    };

    let user_row: Result<(Uuid,), sqlx::Error> = sqlx::query_as(
        "INSERT INTO auth_users (email, email_normalized, password_hash, name) VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(&email)
    .bind(&normalized)
    .bind(&pw_hash)
    .bind(req.name.as_deref())
    .fetch_one(&mut *tx)
    .await;

    let user_id = match user_row {
        Ok((id,)) => id,
        Err(sqlx::Error::Database(db_err)) if db_err.code().as_deref() == Some("23505") => {
            return render_signup_err("email already registered — try logging in");
        }
        Err(e) => {
            tracing::error!(error = %e, "signup insert failed");
            return render_signup_err("internal error");
        }
    };

    // Auto-create personal company + owner membership.
    let display = req
        .name
        .as_deref()
        .map(str::to_string)
        .unwrap_or_else(|| email.split('@').next().unwrap_or("Personal").to_string());
    let slug_base = slugify(&display);
    let slug = format!("{}-{}", slug_base, &Uuid::new_v4().simple().to_string()[..8]);

    let company_row: Result<(Uuid,), sqlx::Error> = sqlx::query_as(
        "INSERT INTO companies (slug, name, owner_user_id) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(&slug)
    .bind(&display)
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await;

    let company_id = match company_row {
        Ok((id,)) => id,
        Err(e) => {
            tracing::error!(error = %e, "company create failed");
            return render_signup_err("internal error");
        }
    };

    let _ = sqlx::query(
        "INSERT INTO memberships (user_id, company_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(user_id)
    .bind(company_id)
    .execute(&mut *tx)
    .await;

    if let Err(e) = tx.commit().await {
        tracing::error!(error = %e, "signup commit failed");
        return render_signup_err("internal error");
    }

    match create_session(&state.pool, user_id, state.config.session_ttl_days, None).await {
        Ok((_, token)) => {
            let cookie = build_session_cookie(&state, token);
            (jar.add(cookie), Redirect::to("/app/dashboard")).into_response()
        }
        Err(_) => Redirect::to("/login").into_response(),
    }
}

fn render_signup_err(msg: &str) -> Response {
    render(SignupTpl {
        user_email: None,
        flash: Some(msg.into()),
        flash_kind: Some("err".into()),
        nav: NavCtx::default(),
    })
}

async fn logout_submit(State(state): State<AppState>, jar: CookieJar) -> Response {
    if let Some(c) = jar.get(SESSION_COOKIE_NAME) {
        let _ = delete_session(&state.pool, c.value()).await;
    }
    let jar = jar.remove(Cookie::from(SESSION_COOKIE_NAME));
    (jar, Redirect::to("/login")).into_response()
}

fn build_session_cookie(state: &AppState, token: String) -> Cookie<'static> {
    Cookie::build((SESSION_COOKIE_NAME, token))
        .http_only(true)
        .secure(state.config.cookie_secure)
        .same_site(SameSite::Lax)
        .path("/")
        .max_age(time::Duration::days(state.config.session_ttl_days))
        .build()
}

fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = true;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    let slugged = if trimmed.is_empty() { "user" } else { trimmed };
    slugged.chars().take(31).collect()
}

// ---------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------

async fn accounts_list(State(state): State<AppState>, jar: CookieJar) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c,
        None => return forbidden(),
    };
    let accounts = queries::list_accounts(&state.pool, company_id)
        .await
        .unwrap_or_default();
    render(AccountsListTpl {
        user_email: Some(user.email),
        flash: None,
        flash_kind: None,
        nav: build_nav(&state, &jar, user.id).await,
        accounts,
    })
}

async fn account_new(State(state): State<AppState>, jar: CookieJar) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    render(AccountsNewTpl {
        user_email: Some(user.email),
        flash: None,
        flash_kind: None,
        nav: build_nav(&state, &jar, user.id).await,
    })
}

#[derive(Deserialize)]
struct AccountForm {
    account_number: String,
    name: String,
    account_type: String,
    currency: Option<String>,
    description: Option<String>,
}

async fn account_create(
    State(state): State<AppState>,
    jar: CookieJar,
    Form(req): Form<AccountForm>,
) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c,
        None => return forbidden(),
    };

    let acct_type = match req.account_type.as_str() {
        "asset" => EventAccountType::Asset,
        "liability" => EventAccountType::Liability,
        "equity" => EventAccountType::Equity,
        "revenue" => EventAccountType::Revenue,
        "expense" => EventAccountType::Expense,
        _ => return account_form_err(&user.email, "invalid account type"),
    };

    let input = CreateAccountInput {
        account_type: acct_type,
        account_number: req.account_number.trim().to_string(),
        name: req.name.trim().to_string(),
        currency: req
            .currency
            .map(|c| c.trim().to_uppercase())
            .filter(|c| !c.is_empty()),
        description: req.description.filter(|d| !d.is_empty()),
    };

    match create_account(&state.pool, company_id, user.id, input).await {
        Ok(_) => Redirect::to("/app/accounts").into_response(),
        Err(AppError::Conflict(msg)) | Err(AppError::BadRequest(msg)) => {
            account_form_err(&user.email, &msg)
        }
        Err(e) => {
            tracing::error!(error = ?e, "account create failed");
            account_form_err(&user.email, "internal error")
        }
    }
}

fn account_form_err(email: &str, msg: &str) -> Response {
    render(AccountsNewTpl {
        user_email: Some(email.to_string()),
        flash: Some(msg.into()),
        flash_kind: Some("err".into()),
        nav: NavCtx::default(),
    })
}

// ---------------------------------------------------------------------------
// Entries
// ---------------------------------------------------------------------------

async fn entries_list(State(state): State<AppState>, jar: CookieJar) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c,
        None => return forbidden(),
    };
    let entries = queries::list_entries(&state.pool, company_id)
        .await
        .unwrap_or_default();
    render(EntriesListTpl {
        user_email: Some(user.email),
        flash: None,
        flash_kind: None,
        nav: build_nav(&state, &jar, user.id).await,
        entries,
    })
}

async fn entry_new(State(state): State<AppState>, jar: CookieJar) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c,
        None => return forbidden(),
    };
    let accounts = queries::list_accounts(&state.pool, company_id)
        .await
        .unwrap_or_default();
    render(EntriesNewTpl {
        user_email: Some(user.email),
        flash: None,
        flash_kind: None,
        nav: build_nav(&state, &jar, user.id).await,
        accounts,
        today: Utc::now().date_naive().to_string(),
    })
}

async fn entry_line_fragment(State(state): State<AppState>, jar: CookieJar) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c,
        None => return forbidden(),
    };
    let accounts = queries::list_accounts(&state.pool, company_id)
        .await
        .unwrap_or_default();
    render(EntryLineFragmentTpl { accounts })
}

async fn entry_create(
    State(state): State<AppState>,
    jar: CookieJar,
    body: String,
) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c,
        None => return forbidden(),
    };

    // Parse repeating fields manually — serde_urlencoded doesn't collect repeats into Vec.
    let parsed: Vec<(String, String)> = url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    let mut date = String::new();
    let mut memo = String::new();
    let mut reference: Option<String> = None;
    let mut account_ids: Vec<String> = Vec::new();
    let mut amounts: Vec<String> = Vec::new();
    let mut currencies: Vec<String> = Vec::new();
    for (k, v) in parsed {
        match k.as_str() {
            "date" => date = v,
            "memo" => memo = v,
            "reference" => {
                if !v.is_empty() {
                    reference = Some(v);
                }
            }
            "account_id" => account_ids.push(v),
            "amount" => amounts.push(v),
            "currency" => currencies.push(v),
            _ => {}
        }
    }
    let date_parsed = match NaiveDate::parse_from_str(&date, "%Y-%m-%d") {
        Ok(d) => d,
        Err(_) => return entry_form_err(&state, &user, "invalid date").await,
    };

    if account_ids.is_empty()
        || account_ids.len() != amounts.len()
        || account_ids.len() != currencies.len()
    {
        return entry_form_err(&state, &user, "incomplete lines").await;
    }

    let mut lines: Vec<EntryLineInput> = Vec::with_capacity(account_ids.len());
    for ((acct, amt), curr) in account_ids
        .iter()
        .zip(amounts.iter())
        .zip(currencies.iter())
    {
        let acct_uuid = match Uuid::parse_str(acct) {
            Ok(u) => u,
            Err(_) => return entry_form_err(&state, &user, "invalid account").await,
        };
        let amount_cents = match parse_amount_cents(amt) {
            Some(c) => c,
            None => return entry_form_err(&state, &user, "invalid amount").await,
        };
        lines.push(EntryLineInput {
            account_id: acct_uuid,
            amount: amount_cents,
            currency: curr.trim().to_uppercase(),
            memo: None,
        });
    }

    let input = PostEntryInput {
        date: date_parsed,
        memo: memo.trim().to_string(),
        reference,
        lines,
    };

    match post_entry(&state.pool, company_id, user.id, input).await {
        Ok(_) => Redirect::to("/app/entries").into_response(),
        Err(AppError::BadRequest(msg)) | Err(AppError::Conflict(msg)) => {
            entry_form_err(&state, &user, &msg).await
        }
        Err(e) => {
            tracing::error!(error = ?e, "post_entry failed");
            entry_form_err(&state, &user, "internal error").await
        }
    }
}

fn parse_amount_cents(s: &str) -> Option<i64> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    let dec: rust_decimal::Decimal = trimmed.parse().ok()?;
    let scaled = dec * rust_decimal::Decimal::from(100i64);
    use rust_decimal::prelude::ToPrimitive;
    scaled.round().to_i64()
}

async fn entry_form_err(state: &AppState, user: &AuthenticatedUser, msg: &str) -> Response {
    let company_id = match queries::resolve_company_id(&state.pool, user.id).await {
        Ok(Some(c)) => c,
        _ => return forbidden(),
    };
    let accounts = queries::list_accounts(&state.pool, company_id)
        .await
        .unwrap_or_default();
    render(EntriesNewTpl {
        user_email: Some(user.email.clone()),
        flash: Some(msg.into()),
        flash_kind: Some("err".into()),
        nav: NavCtx::default(),
        accounts,
        today: Utc::now().date_naive().to_string(),
    })
}

// ---------------------------------------------------------------------------
// AI chat
// ---------------------------------------------------------------------------

fn render_chat_message(m: &crate::ai::chat::StoredMessage) -> ChatGroup {
    let mut lines: Vec<ChatLine> = Vec::new();
    let role = m.role.clone();
    match &m.content {
        serde_json::Value::String(s) => {
            lines.push(ChatLine { role: role.clone(), body: s.clone() });
        }
        serde_json::Value::Array(blocks) => {
            for b in blocks {
                let typ = b.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match typ {
                    "text" => {
                        let txt = b.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        if !txt.is_empty() {
                            lines.push(ChatLine { role: role.clone(), body: txt });
                        }
                    }
                    "tool_use" => {
                        let name = b.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                        let input = b.get("input").map(|v| v.to_string()).unwrap_or_default();
                        lines.push(ChatLine {
                            role: format!("{} · tool_use ({})", role, name),
                            body: input,
                        });
                    }
                    "tool_result" => {
                        let content = b.get("content").and_then(|v| v.as_str()).unwrap_or("");
                        lines.push(ChatLine {
                            role: format!("{} · tool_result", role),
                            body: content.to_string(),
                        });
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    ChatGroup { lines }
}

async fn chat_view(State(state): State<AppState>, jar: CookieJar) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c,
        None => return forbidden(),
    };
    let history = crate::ai::chat::load_history(&state.pool, user.id, company_id)
        .await
        .unwrap_or_default();
    let messages: Vec<ChatGroup> = history.iter().map(render_chat_message).collect();
    render(ChatTpl {
        user_email: Some(user.email),
        flash: None,
        flash_kind: None,
        nav: build_nav(&state, &jar, user.id).await,
        messages,
        api_key_set: state.config.anthropic_api_key.is_some(),
    })
}

#[derive(Deserialize)]
struct ChatForm {
    message: String,
}

async fn chat_send(
    State(state): State<AppState>,
    jar: CookieJar,
    Form(req): Form<ChatForm>,
) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c,
        None => return forbidden(),
    };
    let api_key = match state.config.anthropic_api_key.as_deref() {
        Some(k) => k,
        None => return Redirect::to("/app/chat").into_response(),
    };
    let text = req.message.trim().to_string();
    if text.is_empty() {
        return Redirect::to("/app/chat").into_response();
    }
    if let Err(e) = crate::ai::chat::send_message(&state.pool, api_key, user.id, company_id, text).await {
        tracing::error!(error = ?e, "chat send failed");
    }
    Redirect::to("/app/chat").into_response()
}

async fn chat_clear_route(State(state): State<AppState>, jar: CookieJar) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c,
        None => return forbidden(),
    };
    let _ = crate::ai::chat::clear_history(&state.pool, user.id, company_id).await;
    Redirect::to("/app/chat").into_response()
}

// ---------------------------------------------------------------------------
// Admin (companies + members)
// ---------------------------------------------------------------------------

async fn admin_companies(State(state): State<AppState>, jar: CookieJar) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let companies = queries::list_companies_for_user(&state.pool, user.id)
        .await
        .unwrap_or_default();
    let active_id = active_company(&state, &jar, user.id).await.unwrap_or_else(Uuid::nil);
    let active_company_name = companies
        .iter()
        .find(|c| c.id == active_id)
        .map(|c| c.name.clone())
        .unwrap_or_else(|| "—".to_string());
    render(AdminCompaniesTpl {
        user_email: Some(user.email),
        flash: None,
        flash_kind: None,
        nav: build_nav(&state, &jar, user.id).await,
        companies,
        active_company_id: active_id,
        active_company_name,
    })
}

#[derive(Deserialize)]
struct CompanyForm {
    name: String,
}

async fn admin_company_create(
    State(state): State<AppState>,
    jar: CookieJar,
    Form(req): Form<CompanyForm>,
) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let name = req.name.trim();
    if name.is_empty() {
        return Redirect::to("/app/admin/companies").into_response();
    }
    let new_id = match queries::create_company(&state.pool, user.id, name).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = ?e, "create company failed");
            return Redirect::to("/app/admin/companies").into_response();
        }
    };
    // Switch to the new company immediately.
    let cookie = build_active_company_cookie(&state, new_id);
    (jar.add(cookie), Redirect::to("/app/dashboard")).into_response()
}

async fn admin_company_switch(
    State(state): State<AppState>,
    jar: CookieJar,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let target = match Uuid::parse_str(&id) {
        Ok(u) => u,
        Err(_) => return Redirect::to("/app/admin/companies").into_response(),
    };
    if !queries::user_has_membership(&state.pool, user.id, target)
        .await
        .unwrap_or(false)
    {
        return forbidden();
    }
    let cookie = build_active_company_cookie(&state, target);
    (jar.add(cookie), Redirect::to("/app/dashboard")).into_response()
}

async fn admin_members(State(state): State<AppState>, jar: CookieJar) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c,
        None => return forbidden(),
    };
    let members = queries::list_members(&state.pool, company_id).await.unwrap_or_default();
    let active_company_name = lookup_company_name(&state, company_id).await;
    let role = queries::user_role_in(&state.pool, user.id, company_id).await.ok().flatten().unwrap_or_default();
    let can_admin = queries::role_can_admin(&role);
    let invitations = queries::list_invitations(&state.pool, company_id).await.unwrap_or_default();
    let nav = build_nav(&state, &jar, user.id).await;
    render(AdminMembersTpl {
        user_email: Some(user.email),
        flash: None,
        flash_kind: None,
        nav,
        members,
        active_company_name,
        can_admin,
        invitations,
        invite_url_base: "http://127.0.0.1:9877".to_string(),
    })
}

async fn admin_member_remove(
    State(state): State<AppState>,
    jar: CookieJar,
    axum::extract::Path(target_user_id): axum::extract::Path<String>,
) -> Response {
    let user = match require_user(&state, &jar).await { Ok(u) => u, Err(r) => return r };
    let company_id = match active_company(&state, &jar, user.id).await { Some(c) => c, None => return forbidden() };
    let role = queries::user_role_in(&state.pool, user.id, company_id).await.ok().flatten().unwrap_or_default();
    if !queries::role_can_admin(&role) { return forbidden(); }
    let target = match Uuid::parse_str(&target_user_id) { Ok(u) => u, Err(_) => return (StatusCode::BAD_REQUEST, "bad user_id").into_response() };
    let _ = queries::remove_member(&state.pool, company_id, target).await;
    Redirect::to("/app/admin/members").into_response()
}

#[derive(Deserialize)]
struct ChangeRoleForm { role: String }

async fn admin_member_role(
    State(state): State<AppState>,
    jar: CookieJar,
    axum::extract::Path(target_user_id): axum::extract::Path<String>,
    Form(req): Form<ChangeRoleForm>,
) -> Response {
    let user = match require_user(&state, &jar).await { Ok(u) => u, Err(r) => return r };
    let company_id = match active_company(&state, &jar, user.id).await { Some(c) => c, None => return forbidden() };
    let role = queries::user_role_in(&state.pool, user.id, company_id).await.ok().flatten().unwrap_or_default();
    if !queries::role_can_admin(&role) { return forbidden(); }
    let target = match Uuid::parse_str(&target_user_id) { Ok(u) => u, Err(_) => return (StatusCode::BAD_REQUEST, "bad user_id").into_response() };
    let _ = queries::change_member_role(&state.pool, company_id, target, &req.role).await;
    Redirect::to("/app/admin/members").into_response()
}

async fn admin_settings_view(State(state): State<AppState>, jar: CookieJar) -> Response {
    let user = match require_user(&state, &jar).await { Ok(u) => u, Err(r) => return r };
    let company_id = match active_company(&state, &jar, user.id).await { Some(c) => c, None => return forbidden() };
    let role = queries::user_role_in(&state.pool, user.id, company_id).await.ok().flatten().unwrap_or_default();
    let can_edit = queries::role_can_admin(&role);
    let (name, base_currency, fysm) = queries::get_company(&state.pool, company_id).await
        .ok().flatten().unwrap_or_else(|| ("".into(), "USD".into(), 1));
    let nav = build_nav(&state, &jar, user.id).await;
    let active_id = nav.active_company_id.clone();
    let active_name = nav.active_company_name.clone();
    let all = nav.all_companies.clone();
    render(AdminSettingsTpl {
        user_email: Some(user.email),
        flash: None,
        flash_kind: None,
        nav,
        active_company_name: active_name,
        company_name: name,
        base_currency,
        fiscal_year_start_month: fysm,
        can_edit,
        all_companies: all,
        active_company_id: active_id,
    })
}

#[derive(Deserialize)]
struct SettingsForm {
    name: String,
    base_currency: String,
    fiscal_year_start_month: i16,
}

async fn admin_settings_save(
    State(state): State<AppState>,
    jar: CookieJar,
    Form(req): Form<SettingsForm>,
) -> Response {
    let user = match require_user(&state, &jar).await { Ok(u) => u, Err(r) => return r };
    let company_id = match active_company(&state, &jar, user.id).await { Some(c) => c, None => return forbidden() };
    let role = queries::user_role_in(&state.pool, user.id, company_id).await.ok().flatten().unwrap_or_default();
    if !queries::role_can_admin(&role) { return forbidden(); }
    let _ = queries::update_company_settings(&state.pool, company_id, &req.name, &req.base_currency, req.fiscal_year_start_month).await;
    Redirect::to("/app/admin/settings").into_response()
}

#[derive(Deserialize)]
struct InvitationForm { role: String }

async fn admin_invitation_create(
    State(state): State<AppState>,
    jar: CookieJar,
    Form(req): Form<InvitationForm>,
) -> Response {
    let user = match require_user(&state, &jar).await { Ok(u) => u, Err(r) => return r };
    let company_id = match active_company(&state, &jar, user.id).await { Some(c) => c, None => return forbidden() };
    let role = queries::user_role_in(&state.pool, user.id, company_id).await.ok().flatten().unwrap_or_default();
    if !queries::role_can_admin(&role) { return forbidden(); }
    let _ = queries::create_invitation(&state.pool, company_id, user.id, &req.role, 14).await;
    Redirect::to("/app/admin/members").into_response()
}

async fn accept_invite_view(
    State(state): State<AppState>,
    jar: CookieJar,
    axum::extract::Path(token): axum::extract::Path<String>,
) -> Response {
    let row: Option<(Uuid, String, chrono::DateTime<chrono::Utc>, Option<chrono::DateTime<chrono::Utc>>)> = sqlx::query_as(
        "SELECT company_id, role::text, expires_at, accepted_at FROM company_invitations WHERE token = $1",
    )
    .bind(&token)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten();
    let (company_id, role, expires_at, accepted_at) = match row {
        Some(r) => r,
        None => return (StatusCode::NOT_FOUND, "invitation not found").into_response(),
    };
    if accepted_at.is_some() {
        return (StatusCode::OK, Html("<p>This invitation has already been used.</p>".to_string())).into_response();
    }
    if chrono::Utc::now() > expires_at {
        return (StatusCode::OK, Html("<p>This invitation has expired.</p>".to_string())).into_response();
    }
    let company_name = lookup_company_name(&state, company_id).await;
    let user = current_user(&state, &jar).await;
    let logged_in = user.is_some();
    let user_email = user.as_ref().map(|u| u.email.clone());
    let nav = if let Some(u) = &user { build_nav(&state, &jar, u.id).await } else { NavCtx::default() };
    render(AcceptInviteTpl {
        user_email,
        flash: None,
        flash_kind: None,
        nav: nav.clone(),
        company_name,
        role,
        token,
        logged_in,
        all_companies: nav.all_companies.clone(),
        active_company_id: nav.active_company_id.clone(),
        active_company_name: nav.active_company_name.clone(),
    })
}

async fn accept_invite_submit(
    State(state): State<AppState>,
    jar: CookieJar,
    axum::extract::Path(token): axum::extract::Path<String>,
) -> Response {
    let user = match require_user(&state, &jar).await { Ok(u) => u, Err(r) => return r };
    match queries::accept_invitation(&state.pool, &token, user.id).await {
        Ok(company_id) => {
            let cookie = build_active_company_cookie(&state, company_id);
            (jar.add(cookie), Redirect::to("/app/dashboard")).into_response()
        }
        Err(e) => {
            tracing::warn!(error = ?e, "accept invitation failed");
            Redirect::to("/login").into_response()
        }
    }
}

#[derive(Deserialize)]
struct AddMemberForm {
    email: String,
    role: String,
}

async fn admin_member_add(
    State(state): State<AppState>,
    jar: CookieJar,
    Form(req): Form<AddMemberForm>,
) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c,
        None => return forbidden(),
    };
    let _ = queries::add_member_by_email(&state.pool, company_id, &req.email, &req.role).await;
    Redirect::to("/app/admin/members").into_response()
}

// ---------------------------------------------------------------------------
// Banks (Plaid)
// ---------------------------------------------------------------------------

async fn banks_list(State(state): State<AppState>, jar: CookieJar) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c,
        None => return forbidden(),
    };
    let items = queries::list_plaid_items(&state.pool, company_id)
        .await
        .unwrap_or_default();
    render(BanksListTpl {
        user_email: Some(user.email),
        flash: None,
        flash_kind: None,
        nav: build_nav(&state, &jar, user.id).await,
        items,
    })
}

async fn banks_link(State(state): State<AppState>, jar: CookieJar) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    render(BanksLinkTpl {
        user_email: Some(user.email),
        flash: None,
        flash_kind: None,
        nav: build_nav(&state, &jar, user.id).await,
    })
}

#[derive(Deserialize)]
struct HistoricalQuery {
    start: Option<String>,
    end: Option<String>,
    min_dollars: Option<f64>,
    max_dollars: Option<f64>,
}

async fn banks_historical(
    State(state): State<AppState>,
    jar: CookieJar,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<HistoricalQuery>,
) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c,
        None => return forbidden(),
    };
    let item_uuid = match Uuid::parse_str(&id) {
        Ok(u) => u,
        Err(_) => return (StatusCode::BAD_REQUEST, "bad item id").into_response(),
    };

    use sqlx::Acquire;
    let mut conn = match state.pool.acquire().await { Ok(c) => c, Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "db").into_response() };
    let mut tx = match conn.begin().await { Ok(t) => t, Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "db").into_response() };
    if crate::store::event_store::set_tenant(&mut tx, company_id).await.is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR, "tenant").into_response();
    }
    let row: Option<(Vec<u8>, Vec<u8>)> = sqlx::query_as(
        "SELECT access_token_ciphertext, access_token_nonce FROM plaid_items WHERE id = $1",
    )
    .bind(item_uuid)
    .fetch_optional(&mut *tx)
    .await
    .ok()
    .flatten();
    let (ct, nonce) = match row { Some(r) => r, None => return (StatusCode::NOT_FOUND, "item").into_response() };
    let _ = tx.commit().await;

    let cipher = crate::plaid::crypto::TokenCipher::new(&state.config.plaid.token_enc_key);
    let access_token = match cipher.decrypt(&ct, &nonce) {
        Ok(t) => t,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "decrypt").into_response(),
    };

    let today = Utc::now().date_naive();
    let start = q.start.as_deref().and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
        .unwrap_or_else(|| today - chrono::Duration::days(365 * 2));
    let end = q.end.as_deref().and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
        .unwrap_or(today);

    let plaid = crate::plaid::client::PlaidClient::new(state.config.plaid.clone());
    let txns = match plaid.transactions_get(&access_token, start, end).await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "transactions_get failed");
            return (StatusCode::BAD_GATEWAY, format!("plaid: {e}")).into_response();
        }
    };

    let total = txns.len();
    let oldest = txns.iter().filter_map(|t| t.get("date").and_then(|v| v.as_str())).min().unwrap_or("").to_string();
    let newest = txns.iter().filter_map(|t| t.get("date").and_then(|v| v.as_str())).max().unwrap_or("").to_string();

    let matches: Vec<serde_json::Value> = if q.min_dollars.is_some() || q.max_dollars.is_some() {
        let min_c = q.min_dollars.map(|d| (d * 100.0).round() as i64);
        let max_c = q.max_dollars.map(|d| (d * 100.0).round() as i64);
        txns.iter()
            .filter(|t| {
                let amt = t.get("amount").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let cents = (amt * 100.0).round() as i64;
                let abs = cents.abs();
                min_c.map(|m| abs >= m).unwrap_or(true) && max_c.map(|m| abs <= m).unwrap_or(true)
            })
            .cloned()
            .collect()
    } else {
        Vec::new()
    };

    let body = serde_json::json!({
        "total_transactions": total,
        "oldest_date": oldest,
        "newest_date": newest,
        "matches": matches,
    });
    Json(body).into_response()
}

async fn banks_statements_check(
    State(state): State<AppState>,
    jar: CookieJar,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c,
        None => return forbidden(),
    };
    let item_uuid = match Uuid::parse_str(&id) {
        Ok(u) => u,
        Err(_) => return (StatusCode::BAD_REQUEST, "bad item id").into_response(),
    };

    use sqlx::Acquire;
    let mut conn = match state.pool.acquire().await { Ok(c) => c, Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "db").into_response() };
    let mut tx = match conn.begin().await { Ok(t) => t, Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "db").into_response() };
    if crate::store::event_store::set_tenant(&mut tx, company_id).await.is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR, "tenant").into_response();
    }
    let row: Option<(Vec<u8>, Vec<u8>)> = sqlx::query_as(
        "SELECT access_token_ciphertext, access_token_nonce FROM plaid_items WHERE id = $1",
    )
    .bind(item_uuid)
    .fetch_optional(&mut *tx).await.ok().flatten();
    let (ct, nonce) = match row { Some(r) => r, None => return (StatusCode::NOT_FOUND, "item").into_response() };
    let _ = tx.commit().await;

    let cipher = crate::plaid::crypto::TokenCipher::new(&state.config.plaid.token_enc_key);
    let access_token = match cipher.decrypt(&ct, &nonce) {
        Ok(t) => t,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "decrypt").into_response(),
    };

    let plaid = crate::plaid::client::PlaidClient::new(state.config.plaid.clone());
    match plaid.statements_list(&access_token).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::OK, Json(serde_json::json!({
            "statements_enabled": false,
            "error": format!("{e}"),
        }))).into_response(),
    }
}

async fn banks_unlink(
    State(state): State<AppState>,
    jar: CookieJar,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c,
        None => return forbidden(),
    };
    let item_uuid = match Uuid::parse_str(&id) {
        Ok(u) => u,
        Err(_) => return (StatusCode::BAD_REQUEST, "bad item id").into_response(),
    };
    // Delete CASCADEs plaid_local_accounts + plaid_imported_transactions + plaid_staged.
    // We don't try to call /item/remove on Plaid here — best-effort local cleanup.
    let _ = sqlx::query("DELETE FROM plaid_items WHERE id = $1 AND company_id = $2")
        .bind(item_uuid)
        .bind(company_id)
        .execute(&state.pool)
        .await;
    Redirect::to("/app/banks").into_response()
}

async fn banks_provision(
    State(state): State<AppState>,
    jar: CookieJar,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c,
        None => return forbidden(),
    };
    let item_uuid = match Uuid::parse_str(&id) {
        Ok(u) => u,
        Err(_) => return (StatusCode::BAD_REQUEST, "bad item id").into_response(),
    };
    match crate::plaid::sync::provision_existing_item(&state, company_id, user.id, item_uuid).await {
        Ok(n) => {
            tracing::info!(item_id = %item_uuid, provisioned = n, "provision ok");
            Redirect::to("/app/banks").into_response()
        }
        Err(e) => {
            tracing::error!(error = ?e, "provision failed");
            Redirect::to("/app/banks").into_response()
        }
    }
}

async fn banks_sync(
    State(state): State<AppState>,
    jar: CookieJar,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c,
        None => return forbidden(),
    };
    let item_uuid = match Uuid::parse_str(&id) {
        Ok(u) => u,
        Err(_) => return (StatusCode::BAD_REQUEST, "bad item id").into_response(),
    };
    match crate::plaid::sync::run_sync_for_item(&state, company_id, user.id, item_uuid).await {
        Ok((imported, skipped)) => {
            tracing::info!(item_id = %item_uuid, imported, skipped, "sync ok");
            Redirect::to("/app/banks").into_response()
        }
        Err(e) => {
            tracing::error!(error = ?e, "sync failed");
            Redirect::to("/app/banks").into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Transactions
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TxFilterQuery {
    start: Option<String>,
    end: Option<String>,
    account_id: Option<String>,
    source: Option<String>,
    search: Option<String>,
}

async fn transactions_list(
    State(state): State<AppState>,
    jar: CookieJar,
    axum::extract::Query(q): axum::extract::Query<TxFilterQuery>,
) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c,
        None => return forbidden(),
    };
    let start = q.start.as_deref().and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());
    let end = q.end.as_deref().and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());
    let account_id = q.account_id.as_deref().and_then(|s| {
        if s.is_empty() { None } else { Uuid::parse_str(s).ok() }
    });
    let source = q.source.as_deref().filter(|s| !s.is_empty()).map(str::to_string);
    let search = q.search.as_deref().filter(|s| !s.is_empty()).map(str::to_string);
    let filter = queries::TransactionFilter {
        start, end, account_id,
        source: source.clone(),
        search: search.clone(),
        include_void: false,
    };
    let rows = queries::list_transactions(&state.pool, company_id, &filter)
        .await
        .unwrap_or_default();
    let accounts = queries::list_accounts(&state.pool, company_id).await.unwrap_or_default();
    render(TransactionsTpl {
        user_email: Some(user.email),
        flash: None,
        flash_kind: None,
        nav: build_nav(&state, &jar, user.id).await,
        rows,
        accounts,
        sources: vec!["manual", "import", "recurring", "system", "plaid"],
        selected_account_id: account_id.map(|u| u.to_string()),
        selected_source: source,
        search,
        start_str: start.map(|d| d.to_string()).unwrap_or_default(),
        end_str: end.map(|d| d.to_string()).unwrap_or_default(),
    })
}

#[derive(Deserialize)]
struct ReclassifyForm {
    new_account_id: String,
}

async fn transaction_reclassify(
    State(state): State<AppState>,
    jar: CookieJar,
    axum::extract::Path(line_id): axum::extract::Path<String>,
    Form(req): Form<ReclassifyForm>,
) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c,
        None => return forbidden(),
    };
    let line_uuid = match Uuid::parse_str(&line_id) {
        Ok(u) => u,
        Err(_) => return (StatusCode::BAD_REQUEST, "bad line_id").into_response(),
    };
    let new_acct = match Uuid::parse_str(&req.new_account_id) {
        Ok(u) => u,
        Err(_) => return (StatusCode::BAD_REQUEST, "bad new_account_id").into_response(),
    };
    if let Err(e) = reassign_line(&state.pool, company_id, user.id, line_uuid, new_acct).await {
        tracing::error!(error = ?e, "reassign failed");
    }
    Redirect::to("/app/transactions").into_response()
}

async fn transactions_bulk_reclassify(
    State(state): State<AppState>,
    jar: CookieJar,
    body: String,
) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c,
        None => return forbidden(),
    };
    let parsed: Vec<(String, String)> = url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    let mut line_ids = Vec::new();
    let mut new_account_id: Option<String> = None;
    for (k, v) in parsed {
        match k.as_str() {
            "line_ids" => line_ids.push(v),
            "new_account_id" => new_account_id = Some(v),
            _ => {}
        }
    }
    let new_acct = match new_account_id.and_then(|s| Uuid::parse_str(&s).ok()) {
        Some(u) => u,
        None => return (StatusCode::BAD_REQUEST, "missing new_account_id").into_response(),
    };
    for line in line_ids {
        if let Ok(line_uuid) = Uuid::parse_str(&line) {
            let _ = reassign_line(&state.pool, company_id, user.id, line_uuid, new_acct).await;
        }
    }
    Redirect::to("/app/transactions").into_response()
}

async fn entry_void(
    State(state): State<AppState>,
    jar: CookieJar,
    axum::extract::Path(entry_id): axum::extract::Path<String>,
) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c,
        None => return forbidden(),
    };
    let entry_uuid = match Uuid::parse_str(&entry_id) {
        Ok(u) => u,
        Err(_) => return (StatusCode::BAD_REQUEST, "bad entry_id").into_response(),
    };
    let _ = void_entry(&state.pool, company_id, user.id, entry_uuid, "voided via UI".into()).await;
    Redirect::to("/app/transactions").into_response()
}

async fn entry_unvoid(
    State(state): State<AppState>,
    jar: CookieJar,
    axum::extract::Path(entry_id): axum::extract::Path<String>,
) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c,
        None => return forbidden(),
    };
    let entry_uuid = match Uuid::parse_str(&entry_id) {
        Ok(u) => u,
        Err(_) => return (StatusCode::BAD_REQUEST, "bad entry_id").into_response(),
    };
    let _ = unvoid_entry(&state.pool, company_id, user.id, entry_uuid, "unvoided via UI".into()).await;
    Redirect::to("/app/transactions").into_response()
}

// ---------------------------------------------------------------------------
// Dashboard
// ---------------------------------------------------------------------------

async fn lookup_company_name(state: &AppState, company_id: Uuid) -> String {
    sqlx::query_scalar::<_, String>("SELECT name FROM companies WHERE id = $1")
        .bind(company_id)
        .fetch_one(&state.pool)
        .await
        .unwrap_or_else(|_| "your company".to_string())
}

fn generated_on_str() -> String {
    Utc::now().format("%Y-%m-%d %H:%M UTC").to_string()
}

async fn dashboard(State(state): State<AppState>, jar: CookieJar) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c,
        None => return forbidden(),
    };
    let company_name = lookup_company_name(&state, company_id).await;
    let kpis = match queries::dashboard_kpis(&state.pool, company_id).await {
        Ok(k) => k,
        Err(e) => {
            tracing::error!(error = ?e, "dashboard kpis failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
        }
    };
    render(DashboardTpl {
        user_email: Some(user.email),
        flash: None,
        flash_kind: None,
        nav: build_nav(&state, &jar, user.id).await,
        company_name,
        kpis,
    })
}

// ---------------------------------------------------------------------------
// Reports
// ---------------------------------------------------------------------------

async fn reports_index(State(state): State<AppState>, jar: CookieJar) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    render(ReportsIndexTpl {
        user_email: Some(user.email),
        flash: None,
        flash_kind: None,
        nav: build_nav(&state, &jar, user.id).await,
    })
}

#[derive(Deserialize)]
struct DateRangeQuery {
    start: Option<String>,
    end: Option<String>,
}

fn default_year_range() -> (NaiveDate, NaiveDate) {
    let today = Utc::now().date_naive();
    let year = today.format("%Y").to_string().parse::<i32>().unwrap_or(2026);
    let start = NaiveDate::from_ymd_opt(year, 1, 1).unwrap();
    (start, today)
}

async fn report_income(
    State(state): State<AppState>,
    jar: CookieJar,
    axum::extract::Query(q): axum::extract::Query<DateRangeQuery>,
) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c,
        None => return forbidden(),
    };
    let (default_start, default_end) = default_year_range();
    let start = q
        .start
        .as_deref()
        .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
        .unwrap_or(default_start);
    let end = q
        .end
        .as_deref()
        .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
        .unwrap_or(default_end);
    let report = queries::income_statement(&state.pool, company_id, start, end)
        .await
        .unwrap_or_else(|_| queries::IncomeStatement {
            start,
            end,
            revenues: vec![],
            expenses: vec![],
            total_revenue_cents: 0,
            total_expense_cents: 0,
        });
    let company_name = lookup_company_name(&state, company_id).await;
    render(ReportIncomeTpl {
        user_email: Some(user.email),
        flash: None,
        flash_kind: None,
        nav: build_nav(&state, &jar, user.id).await,
        report,
        company_name,
        generated_on: generated_on_str(),
    })
}

#[derive(Deserialize)]
struct AsOfQuery {
    as_of: Option<String>,
}

async fn report_balance_sheet(
    State(state): State<AppState>,
    jar: CookieJar,
    axum::extract::Query(q): axum::extract::Query<AsOfQuery>,
) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c,
        None => return forbidden(),
    };
    let as_of = q
        .as_of
        .as_deref()
        .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
        .unwrap_or_else(|| Utc::now().date_naive());
    let report = queries::balance_sheet(&state.pool, company_id, as_of)
        .await
        .unwrap_or_else(|_| queries::BalanceSheet {
            as_of,
            assets: vec![],
            liabilities: vec![],
            equity: vec![],
            net_income_cents: 0,
            total_assets_cents: 0,
            total_liab_cents: 0,
            total_equity_cents: 0,
        });
    let company_name = lookup_company_name(&state, company_id).await;
    render(ReportBalanceTpl {
        user_email: Some(user.email),
        flash: None,
        flash_kind: None,
        nav: build_nav(&state, &jar, user.id).await,
        report,
        company_name,
        generated_on: generated_on_str(),
    })
}

async fn report_cash_flow(
    State(state): State<AppState>,
    jar: CookieJar,
    axum::extract::Query(q): axum::extract::Query<DateRangeQuery>,
) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c,
        None => return forbidden(),
    };
    let (default_start, default_end) = default_year_range();
    let start = q
        .start
        .as_deref()
        .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
        .unwrap_or(default_start);
    let end = q
        .end
        .as_deref()
        .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
        .unwrap_or(default_end);
    let report = queries::cash_flow(&state.pool, company_id, start, end)
        .await
        .unwrap_or_else(|_| queries::CashFlow {
            start,
            end,
            opening_cash_cents: 0,
            closing_cash_cents: 0,
            change_cents: 0,
            by_other_account: vec![],
        });
    let company_name = lookup_company_name(&state, company_id).await;
    render(ReportCashFlowTpl {
        user_email: Some(user.email),
        flash: None,
        flash_kind: None,
        nav: build_nav(&state, &jar, user.id).await,
        report,
        company_name,
        generated_on: generated_on_str(),
    })
}

// ---------------------------------------------------------------------------
// Trial balance
// ---------------------------------------------------------------------------

async fn trial_balance(State(state): State<AppState>, jar: CookieJar) -> Response {
    let user = match require_user(&state, &jar).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c,
        None => return forbidden(),
    };
    let (rows, total_debit, total_credit) = queries::trial_balance(&state.pool, company_id)
        .await
        .unwrap_or_else(|_| (vec![], 0, 0));
    let (balance_msg, balance_class) = if total_debit == total_credit {
        ("✓ Balanced".to_string(), "ok".to_string())
    } else {
        (
            format!("⚠ Off by {} cents", (total_debit - total_credit).abs()),
            "err".to_string(),
        )
    };
    let company_name = lookup_company_name(&state, company_id).await;
    render(TrialBalanceTpl {
        user_email: Some(user.email),
        flash: None,
        flash_kind: None,
        nav: build_nav(&state, &jar, user.id).await,
        rows,
        total_debit_display: queries::format_cents(total_debit),
        total_credit_display: queries::format_cents(total_credit),
        balance_msg,
        balance_class,
        company_name,
        generated_on: generated_on_str(),
    })
}

fn forbidden() -> Response {
    (StatusCode::FORBIDDEN, "no company membership for this user").into_response()
}

// ===========================================================================
// Invoicing
// ===========================================================================

#[derive(Template)]
#[template(path = "customers_list.html")]
struct CustomersListTpl {
    user_email: Option<String>,
    flash: Option<String>,
    flash_kind: Option<String>,
    nav: NavCtx,
    customers: Vec<queries::CustomerRow>,
}

#[derive(Template)]
#[template(path = "customer_new.html")]
struct CustomerNewTpl {
    user_email: Option<String>,
    flash: Option<String>,
    flash_kind: Option<String>,
    nav: NavCtx,
}

#[derive(Template)]
#[template(path = "invoices_list.html")]
struct InvoicesListTpl {
    user_email: Option<String>,
    flash: Option<String>,
    flash_kind: Option<String>,
    nav: NavCtx,
    invoices: Vec<queries::InvoiceRow>,
    filter: Option<String>,
    summary_subtitle: String,
    outstanding_count: usize,
    outstanding_display: String,
    overdue_count: usize,
    overdue_display: String,
    paid_30d_display: String,
    draft_count: usize,
}

#[derive(Template)]
#[template(path = "invoice_new.html")]
struct InvoiceNewTpl {
    user_email: Option<String>,
    flash: Option<String>,
    flash_kind: Option<String>,
    nav: NavCtx,
    customers: Vec<queries::CustomerRow>,
    revenue_accounts: Vec<queries::DepositAccountOption>,
    today_iso: String,
    preselect_customer: Option<String>,
}

#[derive(Template)]
#[template(path = "invoice_detail.html")]
struct InvoiceDetailTpl {
    user_email: Option<String>,
    flash: Option<String>,
    flash_kind: Option<String>,
    nav: NavCtx,
    invoice: queries::InvoiceDetail,
    deposit_accounts: Vec<queries::DepositAccountOption>,
    public_url: String,
    today_iso: String,
    balance_dollars: String,
}

#[derive(Template)]
#[template(path = "invoice_public.html")]
struct InvoicePublicTpl {
    invoice: queries::InvoiceDetail,
    company_name: String,
    company_subtitle: String,
}

async fn customers_list(State(state): State<AppState>, jar: CookieJar) -> Response {
    let user = match require_user(&state, &jar).await { Ok(u) => u, Err(r) => return r };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c, None => return forbidden(),
    };
    let customers = queries::list_customers(&state.pool, company_id).await.unwrap_or_default();
    render(CustomersListTpl {
        user_email: Some(user.email),
        flash: None, flash_kind: None,
        nav: build_nav(&state, &jar, user.id).await,
        customers,
    })
}

async fn customer_new_view(State(state): State<AppState>, jar: CookieJar) -> Response {
    let user = match require_user(&state, &jar).await { Ok(u) => u, Err(r) => return r };
    if active_company(&state, &jar, user.id).await.is_none() { return forbidden(); }
    render(CustomerNewTpl {
        user_email: Some(user.email),
        flash: None, flash_kind: None,
        nav: build_nav(&state, &jar, user.id).await,
    })
}

#[derive(Deserialize)]
struct CustomerForm {
    name: String,
    email: Option<String>,
    phone: Option<String>,
    address_line1: Option<String>,
    address_line2: Option<String>,
    city: Option<String>,
    state: Option<String>,
    postal_code: Option<String>,
    default_terms: Option<String>,
    notes: Option<String>,
}

async fn customer_create(
    State(state): State<AppState>,
    jar: CookieJar,
    Form(req): Form<CustomerForm>,
) -> Response {
    let user = match require_user(&state, &jar).await { Ok(u) => u, Err(r) => return r };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c, None => return forbidden(),
    };
    let clean = |s: Option<String>| s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, "name is required").into_response();
    }
    let state_field = clean(req.state).map(|s| s.to_uppercase());
    let terms = req.default_terms.unwrap_or_else(|| "net_30".into());
    let input = queries::CreateCustomerInput {
        name,
        email: clean(req.email),
        phone: clean(req.phone),
        address_line1: clean(req.address_line1),
        address_line2: clean(req.address_line2),
        city: clean(req.city),
        state: state_field,
        postal_code: clean(req.postal_code),
        country: "US".into(),
        default_terms: terms,
        notes: clean(req.notes),
    };
    match queries::create_customer(&state.pool, company_id, input).await {
        Ok(_) => Redirect::to("/app/customers").into_response(),
        Err(e) => {
            tracing::error!(error = ?e, "create customer failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "failed").into_response()
        }
    }
}

#[derive(Deserialize)]
struct InvoiceListQuery {
    status: Option<String>,
}

async fn invoices_list(
    State(state): State<AppState>,
    jar: CookieJar,
    axum::extract::Query(q): axum::extract::Query<InvoiceListQuery>,
) -> Response {
    let user = match require_user(&state, &jar).await { Ok(u) => u, Err(r) => return r };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c, None => return forbidden(),
    };
    let filter = q.status.clone().filter(|s| !s.is_empty());
    let invoices = queries::list_invoices(&state.pool, company_id, filter.as_deref())
        .await
        .unwrap_or_default();

    let all = queries::list_invoices(&state.pool, company_id, None).await.unwrap_or_default();
    let today = chrono::Local::now().date_naive();
    let mut outstanding_count = 0usize;
    let mut outstanding_cents: i64 = 0;
    let mut overdue_count = 0usize;
    let mut overdue_cents: i64 = 0;
    let mut paid_30d: i64 = 0;
    let mut draft_count = 0usize;
    for i in &all {
        match i.status.as_str() {
            "sent" | "partial" => {
                outstanding_count += 1;
                outstanding_cents += i.balance_cents();
                if i.due_date < today {
                    overdue_count += 1;
                    overdue_cents += i.balance_cents();
                }
            }
            "paid" => {
                if i.issue_date >= today - chrono::Duration::days(30) {
                    paid_30d += i.total_cents;
                }
            }
            "draft" => { draft_count += 1; }
            _ => {}
        }
    }

    render(InvoicesListTpl {
        user_email: Some(user.email),
        flash: None, flash_kind: None,
        nav: build_nav(&state, &jar, user.id).await,
        invoices,
        filter,
        summary_subtitle: format!("{} total", all.len()),
        outstanding_count,
        outstanding_display: queries::format_cents(outstanding_cents),
        overdue_count,
        overdue_display: queries::format_cents(overdue_cents),
        paid_30d_display: queries::format_cents(paid_30d),
        draft_count,
    })
}

#[derive(Deserialize)]
struct InvoiceNewQuery {
    customer_id: Option<String>,
}

async fn invoice_new_view(
    State(state): State<AppState>,
    jar: CookieJar,
    axum::extract::Query(q): axum::extract::Query<InvoiceNewQuery>,
) -> Response {
    let user = match require_user(&state, &jar).await { Ok(u) => u, Err(r) => return r };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c, None => return forbidden(),
    };
    let customers = queries::list_customers(&state.pool, company_id).await.unwrap_or_default();
    let revenue_accounts = queries::list_revenue_accounts(&state.pool, company_id).await.unwrap_or_default();
    render(InvoiceNewTpl {
        user_email: Some(user.email),
        flash: None, flash_kind: None,
        nav: build_nav(&state, &jar, user.id).await,
        customers,
        revenue_accounts,
        today_iso: chrono::Local::now().date_naive().format("%Y-%m-%d").to_string(),
        preselect_customer: q.customer_id,
    })
}

async fn invoice_create(
    State(state): State<AppState>,
    jar: CookieJar,
    body: String,
) -> Response {
    let user = match require_user(&state, &jar).await { Ok(u) => u, Err(r) => return r };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c, None => return forbidden(),
    };

    let parsed: Vec<(String, String)> =
        url::form_urlencoded::parse(body.as_bytes()).into_owned().collect();
    let mut customer_id: Option<Uuid> = None;
    let mut issue_date_str = String::new();
    let mut terms = String::new();
    let mut memo: Option<String> = None;
    let mut customer_notes: Option<String> = None;
    let mut desc: Vec<String> = Vec::new();
    let mut qty: Vec<String> = Vec::new();
    let mut unit_dollars: Vec<String> = Vec::new();
    let mut tax_pct: Vec<String> = Vec::new();
    let mut rev_acct: Vec<String> = Vec::new();
    for (k, v) in parsed {
        match k.as_str() {
            "customer_id" => customer_id = Uuid::parse_str(&v).ok(),
            "issue_date" => issue_date_str = v,
            "terms" => terms = v,
            "memo" if !v.is_empty() => memo = Some(v),
            "customer_notes" if !v.is_empty() => customer_notes = Some(v),
            "desc" | "desc[]" => desc.push(v),
            "qty" | "qty[]" => qty.push(v),
            "unit_dollars" | "unit_dollars[]" => unit_dollars.push(v),
            "tax_pct" | "tax_pct[]" => tax_pct.push(v),
            "revenue_account_id" | "revenue_account_id[]" => rev_acct.push(v),
            _ => {}
        }
    }
    let Some(customer_id) = customer_id else {
        return (StatusCode::BAD_REQUEST, "missing customer").into_response();
    };
    let issue_date = match NaiveDate::parse_from_str(&issue_date_str, "%Y-%m-%d") {
        Ok(d) => d,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid date").into_response(),
    };
    if terms.is_empty() { terms = "net_30".into(); }
    let due_date = crate::commands::invoice::default_due_date(issue_date, &terms);

    let n = desc.len();
    if n == 0 || qty.len() != n || unit_dollars.len() != n
        || tax_pct.len() != n || rev_acct.len() != n {
        return (StatusCode::BAD_REQUEST, "line arrays mismatched").into_response();
    }
    let mut lines = Vec::with_capacity(n);
    for i in 0..n {
        let d = desc[i].trim();
        if d.is_empty() { continue; }
        let q: f64 = qty[i].parse().unwrap_or(0.0);
        let u: f64 = unit_dollars[i].parse().unwrap_or(0.0);
        let t: f64 = tax_pct[i].parse().unwrap_or(0.0);
        let r = match Uuid::parse_str(&rev_acct[i]) {
            Ok(r) => r,
            Err(_) => return (StatusCode::BAD_REQUEST, "invalid revenue account").into_response(),
        };
        lines.push(crate::commands::invoice::DraftLine {
            description: d.to_string(),
            quantity: q,
            unit_price_cents: (u * 100.0).round() as i64,
            tax_rate_pct: t,
            revenue_account_id: r,
        });
    }
    if lines.is_empty() {
        return (StatusCode::BAD_REQUEST, "need at least one non-empty line").into_response();
    }

    let input = crate::commands::invoice::DraftInvoiceInput {
        customer_id,
        issue_date,
        due_date,
        terms,
        memo,
        customer_notes,
        lines,
    };
    match crate::commands::invoice::create_draft(&state.pool, company_id, input).await {
        Ok(id) => Redirect::to(&format!("/app/invoices/{}", id)).into_response(),
        Err(e) => {
            tracing::error!(error = ?e, "create invoice failed");
            (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response()
        }
    }
}

async fn invoice_detail_view(
    State(state): State<AppState>,
    jar: CookieJar,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Response {
    let user = match require_user(&state, &jar).await { Ok(u) => u, Err(r) => return r };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c, None => return forbidden(),
    };
    let invoice = match queries::get_invoice(&state.pool, company_id, id).await {
        Ok(Some(i)) => i,
        Ok(None) => return (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => {
            tracing::error!(error = ?e, "get invoice failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "internal").into_response();
        }
    };
    let deposit_accounts = queries::list_deposit_accounts(&state.pool, company_id).await.unwrap_or_default();
    let public_url = format!("{}/invoice/{}", state.config.public_base_url, invoice.public_token);
    let balance_dollars = format!("{:.2}", invoice.balance_cents() as f64 / 100.0);
    render(InvoiceDetailTpl {
        user_email: Some(user.email),
        flash: None, flash_kind: None,
        nav: build_nav(&state, &jar, user.id).await,
        invoice,
        deposit_accounts,
        public_url,
        today_iso: chrono::Local::now().date_naive().format("%Y-%m-%d").to_string(),
        balance_dollars,
    })
}

async fn invoice_issue(
    State(state): State<AppState>,
    jar: CookieJar,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Response {
    let user = match require_user(&state, &jar).await { Ok(u) => u, Err(r) => return r };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c, None => return forbidden(),
    };
    match crate::commands::invoice::issue_invoice(&state.pool, company_id, user.id, id).await {
        Ok(_) => Redirect::to(&format!("/app/invoices/{}", id)).into_response(),
        Err(e) => {
            tracing::error!(error = ?e, "issue invoice failed");
            (StatusCode::CONFLICT, format!("{e}")).into_response()
        }
    }
}

#[derive(Deserialize)]
struct InvoicePaymentForm {
    payment_date: String,
    amount_dollars: String,
    method: String,
    deposit_account_id: Uuid,
    reference: Option<String>,
}

async fn invoice_payment(
    State(state): State<AppState>,
    jar: CookieJar,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
    Form(req): Form<InvoicePaymentForm>,
) -> Response {
    let user = match require_user(&state, &jar).await { Ok(u) => u, Err(r) => return r };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c, None => return forbidden(),
    };
    let date = match NaiveDate::parse_from_str(&req.payment_date, "%Y-%m-%d") {
        Ok(d) => d,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid date").into_response(),
    };
    let dollars: f64 = req.amount_dollars.parse().unwrap_or(0.0);
    let cents = (dollars * 100.0).round() as i64;
    let input = crate::commands::invoice::PaymentInput {
        payment_date: date,
        amount_cents: cents,
        method: req.method,
        deposit_account_id: req.deposit_account_id,
        reference: req.reference.filter(|s| !s.is_empty()),
        memo: None,
    };
    match crate::commands::invoice::record_payment(&state.pool, company_id, user.id, id, input).await {
        Ok(_) => Redirect::to(&format!("/app/invoices/{}", id)).into_response(),
        Err(e) => {
            tracing::error!(error = ?e, "payment failed");
            (StatusCode::CONFLICT, format!("{e}")).into_response()
        }
    }
}

#[derive(Deserialize)]
struct VoidForm {
    reason: String,
}

async fn invoice_void(
    State(state): State<AppState>,
    jar: CookieJar,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
    Form(req): Form<VoidForm>,
) -> Response {
    let user = match require_user(&state, &jar).await { Ok(u) => u, Err(r) => return r };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c, None => return forbidden(),
    };
    match crate::commands::invoice::void_invoice(&state.pool, company_id, user.id, id, req.reason).await {
        Ok(_) => Redirect::to(&format!("/app/invoices/{}", id)).into_response(),
        Err(e) => (StatusCode::CONFLICT, format!("{e}")).into_response(),
    }
}

async fn invoice_send(
    State(state): State<AppState>,
    jar: CookieJar,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Response {
    let user = match require_user(&state, &jar).await { Ok(u) => u, Err(r) => return r };
    let company_id = match active_company(&state, &jar, user.id).await {
        Some(c) => c, None => return forbidden(),
    };
    let invoice = match queries::get_invoice(&state.pool, company_id, id).await {
        Ok(Some(i)) => i,
        _ => return (StatusCode::NOT_FOUND, "not found").into_response(),
    };
    let email = match &invoice.customer.email {
        Some(e) if !e.is_empty() => e.clone(),
        _ => return (StatusCode::BAD_REQUEST, "customer has no email").into_response(),
    };
    if !state.email.is_configured() {
        return (StatusCode::SERVICE_UNAVAILABLE,
            "email not configured. Set RESEND_API_KEY env var. Public link is in the invoice detail page.")
            .into_response();
    }
    let company_name = queries::get_company(&state.pool, company_id).await
        .ok().flatten().map(|c| c.0).unwrap_or_else(|| "Maven".to_string());
    let public_url = format!("{}/invoice/{}", state.config.public_base_url, invoice.public_token);
    let html = format!(
        r#"<div style="font-family: -apple-system, sans-serif; max-width: 560px; margin: 0 auto; padding: 24px; color: #0f0f10;">
            <p>Hi {customer},</p>
            <p>Your invoice <strong>{number}</strong> from <strong>{company}</strong> is ready: <strong>{total}</strong> due by <strong>{due}</strong>.</p>
            <p><a href="{url}" style="display: inline-block; padding: 10px 18px; background: #0f0f10; color: #fff; text-decoration: none; border-radius: 4px;">View & pay invoice</a></p>
            <p style="font-size: 12px; color: #5b5b62; margin-top: 24px;">Or copy this link: {url}</p>
        </div>"#,
        customer = invoice.customer.name,
        number = invoice.invoice_number,
        company = company_name,
        total = invoice.total_display(),
        due = invoice.due_us(),
        url = public_url,
    );
    match state.email.send(
        &email,
        &format!("Invoice {} from {}", invoice.invoice_number, company_name),
        &html,
        None,
    ).await {
        Ok(_) => {
            let _ = crate::commands::invoice::mark_sent(&state.pool, company_id, id, &email).await;
            Redirect::to(&format!("/app/invoices/{}", id)).into_response()
        }
        Err(e) => {
            tracing::error!(error = ?e, "send email failed");
            (StatusCode::BAD_GATEWAY, format!("email send failed: {e}")).into_response()
        }
    }
}

async fn invoice_public_view(
    State(state): State<AppState>,
    axum::extract::Path(token): axum::extract::Path<String>,
) -> Response {
    let lookup = match queries::get_invoice_by_token(&state.pool, &token).await {
        Ok(Some(v)) => v,
        Ok(None) => return (StatusCode::NOT_FOUND, "invoice not found").into_response(),
        Err(e) => {
            tracing::error!(error = ?e, "invoice token lookup failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "internal").into_response();
        }
    };
    let (invoice_id, _company_id, company_name) = lookup;
    let invoice = match queries::get_invoice_public(&state.pool, &token, invoice_id).await {
        Ok(Some(i)) => i,
        _ => return (StatusCode::NOT_FOUND, "not found").into_response(),
    };
    render(InvoicePublicTpl { invoice, company_name, company_subtitle: String::new() })
}
