use crate::events::types::{Event, EventAccountType, StoredEvent};
use chrono::Datelike;
use rusqlite::{params, Connection, OptionalExtension};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ProjectionError {
    #[error("Database error: {0}")]
    DatabaseError(#[from] rusqlite::Error),
    #[error("Entity not found: {0}")]
    NotFound(String),
    #[error("Invalid state: {0}")]
    InvalidState(String),
}

/// Projects events into materialized tables
pub struct Projector<'a> {
    conn: &'a Connection,
}

impl<'a> Projector<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// Apply a single event to update projections
    pub fn apply(&self, stored_event: &StoredEvent) -> Result<(), ProjectionError> {
        match &stored_event.event {
            Event::CompanyCreated {
                company_id,
                name,
                base_currency,
                fiscal_year_start,
            } => {
                self.conn.execute(
                    "INSERT OR REPLACE INTO company (id, company_id, name, base_currency, fiscal_year_start_month, created_at_event)
                     VALUES ('default', ?1, ?2, ?3, ?4, ?5)",
                    params![company_id, name, base_currency, fiscal_year_start, stored_event.id],
                )?;
            }
            Event::CompanySettingsUpdated {
                field,
                old_value: _,
                new_value,
            } => {
                // Update the specific field
                let sql = format!("UPDATE company SET {} = ?1 WHERE id = 'default'", field);
                self.conn.execute(&sql, [new_value])?;
            }
            Event::TaxLineMappingSet {
                account_id,
                line_key,
            } => {
                self.conn.execute(
                    "INSERT INTO tax_line_mappings (account_id, line_key, updated_at, updated_at_event)
                     VALUES (?1, ?2, datetime('now'), ?3)
                     ON CONFLICT(account_id) DO UPDATE SET
                       line_key = ?2, updated_at = datetime('now'), updated_at_event = ?3",
                    params![account_id, line_key, stored_event.id],
                )?;
            }
            Event::TaxLineMappingCleared { account_id } => {
                self.conn.execute(
                    "DELETE FROM tax_line_mappings WHERE account_id = ?1",
                    params![account_id],
                )?;
            }
            Event::ScheduleBAnswerSet {
                tax_year,
                answer_key,
                value,
            } => {
                self.conn.execute(
                    "INSERT INTO schedule_b_answers
                        (tax_year, answer_key, value, updated_at, updated_at_event)
                     VALUES (?1, ?2, ?3, datetime('now'), ?4)
                     ON CONFLICT(tax_year, answer_key) DO UPDATE SET
                       value = ?3, updated_at = datetime('now'), updated_at_event = ?4",
                    params![tax_year, answer_key, value, stored_event.id],
                )?;
            }
            Event::ScheduleBAnswerCleared {
                tax_year,
                answer_key,
            } => {
                self.conn.execute(
                    "DELETE FROM schedule_b_answers WHERE tax_year = ?1 AND answer_key = ?2",
                    params![tax_year, answer_key],
                )?;
            }
            Event::BusinessProfileSet(d) => {
                let (legal_name, address, ein, naics_code, formation_date) = (
                    &d.legal_name,
                    &d.address,
                    &d.ein,
                    &d.naics_code,
                    &d.formation_date,
                );
                let (principal_activity, principal_product) =
                    (&d.principal_activity, &d.principal_product);
                self.conn.execute(
                    "INSERT OR REPLACE INTO business_profile
                        (id, legal_name, street, suite, city, state, postal_code, country,
                         ein, naics_code, formation_date, principal_activity, principal_product,
                         updated_at_event)
                     VALUES ('default', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                    params![
                        legal_name,
                        address.street,
                        address.suite,
                        address.city,
                        address.state,
                        address.postal_code,
                        address.country,
                        ein,
                        naics_code,
                        formation_date.to_string(),
                        principal_activity,
                        principal_product,
                        stored_event.id
                    ],
                )?;
            }
            Event::PartnerAdmitted(d) => {
                let (partner_id, name, partner_type, residency, entity_type) = (
                    &d.partner_id,
                    &d.name,
                    &d.partner_type,
                    &d.residency,
                    &d.entity_type,
                );
                let (address, start_date, shares) = (&d.address, &d.start_date, &d.shares);
                self.conn.execute(
                    "INSERT OR REPLACE INTO partners
                        (id, name, partner_type, residency, entity_type,
                         street, suite, city, state, postal_code, country,
                         start_date, end_date, profit_ppm, loss_ppm, capital_ppm,
                         admitted_at_event, updated_at_event)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, NULL,
                             ?13, ?14, ?15, ?16, ?16)",
                    params![
                        partner_id,
                        name,
                        partner_type,
                        residency,
                        entity_type,
                        address.street,
                        address.suite,
                        address.city,
                        address.state,
                        address.postal_code,
                        address.country,
                        start_date.to_string(),
                        shares.profit_ppm,
                        shares.loss_ppm,
                        shares.capital_ppm,
                        stored_event.id
                    ],
                )?;
            }
            Event::PartnerDetailsUpdated(d) => {
                let (partner_id, name, partner_type, residency, entity_type) = (
                    &d.partner_id,
                    &d.name,
                    &d.partner_type,
                    &d.residency,
                    &d.entity_type,
                );
                let (address, shares) = (&d.address, &d.shares);
                // Start and end dates are deliberately untouched: joining and
                // leaving are their own events, and letting an edit move them
                // would silently change which years a partner gets a K-1 for.
                self.conn.execute(
                    "UPDATE partners SET
                        name = ?2, partner_type = ?3, residency = ?4, entity_type = ?5,
                        street = ?6, suite = ?7, city = ?8, state = ?9, postal_code = ?10,
                        country = ?11, profit_ppm = ?12, loss_ppm = ?13, capital_ppm = ?14,
                        updated_at_event = ?15
                     WHERE id = ?1",
                    params![
                        partner_id,
                        name,
                        partner_type,
                        residency,
                        entity_type,
                        address.street,
                        address.suite,
                        address.city,
                        address.state,
                        address.postal_code,
                        address.country,
                        shares.profit_ppm,
                        shares.loss_ppm,
                        shares.capital_ppm,
                        stored_event.id
                    ],
                )?;
            }
            Event::PartnerWithdrawn {
                partner_id,
                end_date,
            } => {
                self.conn.execute(
                    "UPDATE partners SET end_date = ?2, updated_at_event = ?3 WHERE id = ?1",
                    params![partner_id, end_date.to_string(), stored_event.id],
                )?;
            }
            Event::UserAdded {
                user_id,
                username,
                role,
            } => {
                let role_str = match role {
                    crate::events::types::UserRole::Admin => "admin",
                    crate::events::types::UserRole::Accountant => "accountant",
                    crate::events::types::UserRole::Viewer => "viewer",
                };
                self.conn.execute(
                    "INSERT INTO users (id, username, role, is_active, created_at_event)
                     VALUES (?1, ?2, ?3, 1, ?4)",
                    params![user_id, username, role_str, stored_event.id],
                )?;
            }
            Event::UserModified {
                user_id,
                field,
                old_value: _,
                new_value,
            } => {
                let sql = format!("UPDATE users SET {} = ?1 WHERE id = ?2", field);
                self.conn.execute(&sql, params![new_value, user_id])?;
            }
            Event::UserRemoved { user_id } => {
                self.conn
                    .execute("UPDATE users SET is_active = 0 WHERE id = ?1", [user_id])?;
            }
            Event::AccountCreated {
                account_id,
                account_type,
                account_number,
                name,
                parent_id,
                currency,
                description,
            } => {
                let type_str = match account_type {
                    EventAccountType::Asset => "asset",
                    EventAccountType::Liability => "liability",
                    EventAccountType::Equity => "equity",
                    EventAccountType::Revenue => "revenue",
                    EventAccountType::Expense => "expense",
                };
                self.conn.execute(
                    "INSERT INTO accounts (id, account_type, account_number, name, parent_id, currency, description, is_active, created_at_event, updated_at_event)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8, ?8)",
                    params![
                        account_id,
                        type_str,
                        account_number,
                        name,
                        parent_id,
                        currency,
                        description,
                        stored_event.id,
                    ],
                )?;
            }
            Event::AccountUpdated {
                account_id,
                field,
                old_value: _,
                new_value,
            } => {
                let sql = format!(
                    "UPDATE accounts SET {} = ?1, updated_at_event = ?2 WHERE id = ?3",
                    field
                );
                self.conn
                    .execute(&sql, params![new_value, stored_event.id, account_id])?;
            }
            Event::AccountDeactivated {
                account_id,
                reason: _,
            } => {
                self.conn.execute(
                    "UPDATE accounts SET is_active = 0, updated_at_event = ?1 WHERE id = ?2",
                    params![stored_event.id, account_id],
                )?;
            }
            Event::AccountReactivated { account_id } => {
                self.conn.execute(
                    "UPDATE accounts SET is_active = 1, updated_at_event = ?1 WHERE id = ?2",
                    params![stored_event.id, account_id],
                )?;
            }
            Event::JournalEntryPosted {
                entry_id,
                date,
                memo,
                lines,
                reference,
                source,
            } => {
                let source_str = source.as_ref().map(|s| match s {
                    crate::events::types::JournalEntrySource::Manual => "manual",
                    crate::events::types::JournalEntrySource::Import => "import",
                    crate::events::types::JournalEntrySource::Recurring => "recurring",
                    crate::events::types::JournalEntrySource::System => "system",
                    crate::events::types::JournalEntrySource::Plaid => "plaid",
                    crate::events::types::JournalEntrySource::Pos => "pos",
                    crate::events::types::JournalEntrySource::PurchaseOrder => "purchase_order",
                    crate::events::types::JournalEntrySource::InventoryAdjustment => {
                        "inventory_adjustment"
                    }
                    crate::events::types::JournalEntrySource::EventService => "event_service",
                    crate::events::types::JournalEntrySource::BillPayable => "bill_payable",
                    crate::events::types::JournalEntrySource::InvoiceReceivable => {
                        "invoice_receivable"
                    }
                    crate::events::types::JournalEntrySource::BillPayment => "bill_payment",
                    crate::events::types::JournalEntrySource::InvoicePayment => "invoice_payment",
                });

                self.conn.execute(
                    "INSERT INTO journal_entries (id, date, memo, reference, source, is_void, posted_at_event)
                     VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)",
                    params![
                        entry_id,
                        date.to_string(),
                        memo,
                        reference,
                        source_str,
                        stored_event.id,
                    ],
                )?;

                for line in lines {
                    self.conn.execute(
                        "INSERT INTO journal_lines (id, entry_id, account_id, amount, currency, exchange_rate, memo, is_cleared)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)",
                        params![
                            line.line_id,
                            entry_id,
                            line.account_id,
                            line.amount,
                            line.currency,
                            line.exchange_rate.map(|r| r.to_string()),
                            line.memo,
                        ],
                    )?;
                }
            }
            Event::JournalEntryVoided {
                entry_id,
                reason: _,
            } => {
                self.conn.execute(
                    "UPDATE journal_entries SET is_void = 1 WHERE id = ?1",
                    params![entry_id],
                )?;
            }
            Event::JournalEntryUnvoided {
                entry_id,
                reason: _,
            } => {
                self.conn.execute(
                    "UPDATE journal_entries SET is_void = 0 WHERE id = ?1",
                    params![entry_id],
                )?;
            }
            Event::JournalEntryAnnotated {
                entry_id: _,
                annotation: _,
            } => {
                // Annotations could be stored in a separate table
                // For now, we'll skip this
            }
            Event::JournalLineReassigned {
                entry_id: _,
                line_id,
                old_account_id: _,
                new_account_id,
            } => {
                self.conn.execute(
                    "UPDATE journal_lines SET account_id = ?1 WHERE id = ?2",
                    params![new_account_id, line_id],
                )?;
            }
            Event::FiscalYearOpened {
                year,
                start_date,
                end_date,
            } => {
                self.conn.execute(
                    "INSERT INTO fiscal_years (year, start_date, end_date, is_closed)
                     VALUES (?1, ?2, ?3, 0)",
                    params![year, start_date.to_string(), end_date.to_string()],
                )?;

                // Create monthly periods
                let mut current = *start_date;
                let mut period = 1u8;
                while current <= *end_date && period <= 12 {
                    let period_end = {
                        let next_month = if current.month() == 12 {
                            chrono::NaiveDate::from_ymd_opt(current.year() + 1, 1, 1).unwrap()
                        } else {
                            chrono::NaiveDate::from_ymd_opt(current.year(), current.month() + 1, 1)
                                .unwrap()
                        };
                        next_month.pred_opt().unwrap().min(*end_date)
                    };

                    self.conn.execute(
                        "INSERT INTO fiscal_periods (year, period, start_date, end_date, status)
                         VALUES (?1, ?2, ?3, ?4, 'open')",
                        params![year, period, current.to_string(), period_end.to_string()],
                    )?;

                    current = period_end.succ_opt().unwrap_or(period_end);
                    period += 1;
                }
            }
            Event::PeriodClosed {
                year,
                period,
                closed_by_user_id,
            } => {
                self.conn.execute(
                    "UPDATE fiscal_periods SET status = 'closed', closed_by_user_id = ?1, closed_at = datetime('now')
                     WHERE year = ?2 AND period = ?3",
                    params![closed_by_user_id, year, period],
                )?;
            }
            Event::PeriodReopened {
                year,
                period,
                reason: _,
                reopened_by_user_id: _,
            } => {
                self.conn.execute(
                    "UPDATE fiscal_periods SET status = 'open', closed_by_user_id = NULL, closed_at = NULL
                     WHERE year = ?1 AND period = ?2",
                    params![year, period],
                )?;
            }
            Event::YearEndClosed {
                year,
                retained_earnings_entry_id,
            } => {
                self.conn.execute(
                    "UPDATE fiscal_years SET is_closed = 1, retained_earnings_entry_id = ?1 WHERE year = ?2",
                    params![retained_earnings_entry_id, year],
                )?;
            }
            Event::CurrencyEnabled {
                code,
                name,
                symbol,
                decimal_places,
            } => {
                self.conn.execute(
                    "INSERT OR REPLACE INTO currencies (code, name, symbol, decimal_places)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![code, name, symbol, decimal_places],
                )?;
            }
            Event::ExchangeRateRecorded {
                from_currency,
                to_currency,
                rate,
                effective_date,
            } => {
                self.conn.execute(
                    "INSERT INTO exchange_rates (from_currency, to_currency, rate, effective_date, recorded_at_event)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        from_currency,
                        to_currency,
                        rate.to_string().parse::<f64>().unwrap_or(0.0),
                        effective_date.to_string(),
                        stored_event.id,
                    ],
                )?;
            }
            Event::PlaidItemConnected {
                item_id,
                proxy_item_id,
                institution_name,
                plaid_accounts,
            } => {
                self.conn.execute(
                    "INSERT OR REPLACE INTO plaid_items (id, proxy_item_id, institution_name, status, connected_at_event)
                     VALUES (?1, ?2, ?3, 'active', ?4)",
                    params![item_id, proxy_item_id, institution_name, stored_event.id],
                )?;

                for acct in plaid_accounts {
                    self.conn.execute(
                        "INSERT OR REPLACE INTO plaid_local_accounts (item_id, plaid_account_id, name, account_type, mask)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![item_id, acct.plaid_account_id, acct.name, acct.account_type, acct.mask],
                    )?;
                }
            }
            Event::PlaidAccountsRefreshed {
                item_id,
                plaid_accounts,
            } => {
                // Upsert, and deliberately no delete of rows this list omits.
                // `local_account_id` lives on these rows: dropping one because the
                // bank stopped reporting a closed card would unmap transactions
                // already posted through it, and the mapping is not the bank's to
                // decide. `INSERT OR REPLACE` would also blank it, so the columns
                // the bank owns are updated by name and the mapping is left alone.
                //
                // Two passes, because the second depends on what the first
                // claimed. An account whose id the bank has changed must not be
                // matched against a row that some other incoming account already
                // is.
                let mut unmatched = Vec::new();
                for acct in plaid_accounts {
                    let updated = self.conn.execute(
                        "UPDATE plaid_local_accounts
                            SET name = ?3, account_type = ?4, mask = ?5,
                                persistent_account_id = COALESCE(?6, persistent_account_id)
                          WHERE item_id = ?1 AND plaid_account_id = ?2",
                        params![
                            item_id,
                            acct.plaid_account_id,
                            acct.name,
                            acct.account_type,
                            acct.mask,
                            acct.persistent_account_id
                        ],
                    )?;
                    if updated == 0 {
                        unmatched.push(acct);
                    }
                }

                let claimed: std::collections::HashSet<&str> = plaid_accounts
                    .iter()
                    .map(|a| a.plaid_account_id.as_str())
                    .collect();
                for acct in unmatched {
                    // The same account under a new id keeps its row — and with it
                    // the ledger account it is mapped to. Treating it as new
                    // instead is what showed one checking account twice, and it
                    // would also silently strand the mapping on the dead id: the
                    // connection looks healthy and imports nothing.
                    match same_account_under_a_new_id(self.conn, item_id, acct, &claimed)? {
                        Some(old_id) => {
                            self.conn.execute(
                                "UPDATE plaid_local_accounts
                                    SET plaid_account_id = ?3, name = ?4, account_type = ?5,
                                        mask = ?6, persistent_account_id = ?7
                                  WHERE item_id = ?1 AND plaid_account_id = ?2",
                                params![
                                    item_id,
                                    old_id,
                                    acct.plaid_account_id,
                                    acct.name,
                                    acct.account_type,
                                    acct.mask,
                                    acct.persistent_account_id
                                ],
                            )?;
                        }
                        None => {
                            self.conn.execute(
                                "INSERT INTO plaid_local_accounts (item_id, plaid_account_id, name, account_type, mask, persistent_account_id)
                                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                                params![
                                    item_id,
                                    acct.plaid_account_id,
                                    acct.name,
                                    acct.account_type,
                                    acct.mask,
                                    acct.persistent_account_id
                                ],
                            )?;
                        }
                    }
                }
            }
            Event::PlaidItemDisconnected { item_id, reason: _ } => {
                self.conn.execute(
                    "UPDATE plaid_items SET status = 'disconnected' WHERE id = ?1",
                    params![item_id],
                )?;
            }
            Event::PlaidAccountMapped {
                item_id,
                plaid_account_id,
                local_account_id,
            } => {
                self.conn.execute(
                    "UPDATE plaid_local_accounts SET local_account_id = ?1 WHERE item_id = ?2 AND plaid_account_id = ?3",
                    params![local_account_id, item_id, plaid_account_id],
                )?;
            }
            Event::PlaidAccountUnmapped {
                item_id,
                plaid_account_id,
                local_account_id: _,
            } => {
                self.conn.execute(
                    "UPDATE plaid_local_accounts SET local_account_id = NULL WHERE item_id = ?1 AND plaid_account_id = ?2",
                    params![item_id, plaid_account_id],
                )?;
            }
            Event::PlaidTransactionsSynced {
                item_id,
                transactions_added: _,
                transactions_modified: _,
                transactions_removed: _,
                sync_timestamp,
            } => {
                self.conn.execute(
                    "UPDATE plaid_items SET last_synced_at = ?1 WHERE id = ?2",
                    params![sync_timestamp, item_id],
                )?;
            }
            Event::EventServiceRegistered {
                service_id,
                name,
                root_url,
                api_key,
            } => {
                self.conn.execute(
                    "INSERT OR REPLACE INTO event_services (id, name, root_url, api_key, status, connected_at_event)
                     VALUES (?1, ?2, ?3, ?4, 'active', ?5)",
                    params![service_id, name, root_url, api_key, stored_event.id],
                )?;
            }
            Event::EventServiceReportingChanged {
                service_id,
                frequency,
                effective_from,
            } => {
                self.conn.execute(
                    "UPDATE event_services SET reporting_frequency = ?2, reporting_from = ?3
                      WHERE id = ?1",
                    params![service_id, frequency, effective_from.to_string()],
                )?;
            }
            Event::EventServiceRemoved { service_id } => {
                self.conn.execute(
                    "UPDATE event_services SET status = 'removed' WHERE id = ?1",
                    params![service_id],
                )?;
            }
            Event::EventServiceSynced {
                service_id,
                events_processed,
                entries_created,
                errors: _,
            } => {
                self.conn.execute(
                    "UPDATE event_services SET last_synced_at = datetime('now'),
                     events_processed = events_processed + ?1,
                     entries_created = entries_created + ?2
                     WHERE id = ?3",
                    params![events_processed, entries_created, service_id],
                )?;
            }
            Event::BillReceived {
                bill_id,
                vendor,
                amount,
                currency,
                due_date,
                terms,
                memo,
                entry_id,
            } => {
                self.conn.execute(
                    "INSERT INTO bills (id, vendor, amount, currency, amount_paid, status, due_date, terms, memo, entry_id, posted_at_event, updated_at_event)
                     VALUES (?1, ?2, ?3, ?4, 0, 'open', ?5, ?6, ?7, ?8, ?9, ?9)",
                    params![
                        bill_id,
                        vendor,
                        amount,
                        currency,
                        due_date.to_string(),
                        terms,
                        memo,
                        entry_id,
                        stored_event.id,
                    ],
                )?;
            }
            Event::BillPaymentApplied {
                bill_id,
                payment_entry_id,
                amount_applied,
            } => {
                self.conn.execute(
                    "INSERT INTO bill_payments (bill_id, payment_entry_id, amount_applied, applied_at_event)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![bill_id, payment_entry_id, amount_applied, stored_event.id],
                )?;
                self.conn.execute(
                    "UPDATE bills SET
                         amount_paid = amount_paid + ?1,
                         status = CASE
                             WHEN amount_paid + ?1 >= amount THEN 'paid'
                             WHEN amount_paid + ?1 > 0 THEN 'partial'
                             ELSE 'open'
                         END,
                         updated_at_event = ?2
                     WHERE id = ?3",
                    params![amount_applied, stored_event.id, bill_id],
                )?;
            }
            Event::BillVoided { bill_id, reason: _ } => {
                self.conn.execute(
                    "UPDATE bills SET status = 'void', updated_at_event = ?1 WHERE id = ?2",
                    params![stored_event.id, bill_id],
                )?;
            }
            Event::InvoiceIssued {
                invoice_id,
                customer,
                amount,
                currency,
                due_date,
                terms,
                memo,
                entry_id,
            } => {
                self.conn.execute(
                    "INSERT INTO invoices (id, customer, amount, currency, amount_paid, status, due_date, terms, memo, entry_id, posted_at_event, updated_at_event)
                     VALUES (?1, ?2, ?3, ?4, 0, 'open', ?5, ?6, ?7, ?8, ?9, ?9)",
                    params![
                        invoice_id,
                        customer,
                        amount,
                        currency,
                        due_date.to_string(),
                        terms,
                        memo,
                        entry_id,
                        stored_event.id,
                    ],
                )?;
            }
            Event::InvoicePaymentReceived {
                invoice_id,
                payment_entry_id,
                amount_applied,
            } => {
                self.conn.execute(
                    "INSERT INTO invoice_payments (invoice_id, payment_entry_id, amount_applied, applied_at_event)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![invoice_id, payment_entry_id, amount_applied, stored_event.id],
                )?;
                self.conn.execute(
                    "UPDATE invoices SET
                         amount_paid = amount_paid + ?1,
                         status = CASE
                             WHEN amount_paid + ?1 >= amount THEN 'paid'
                             WHEN amount_paid + ?1 > 0 THEN 'partial'
                             ELSE 'open'
                         END,
                         updated_at_event = ?2
                     WHERE id = ?3",
                    params![amount_applied, stored_event.id, invoice_id],
                )?;
            }
            Event::InvoiceVoided {
                invoice_id,
                reason: _,
            } => {
                self.conn.execute(
                    "UPDATE invoices SET status = 'void', updated_at_event = ?1 WHERE id = ?2",
                    params![stored_event.id, invoice_id],
                )?;
            }
            Event::ReconciliationStarted {
                reconciliation_id,
                account_id,
                statement_date,
                statement_ending_balance,
            } => {
                self.conn.execute(
                    "INSERT INTO reconciliations (id, account_id, statement_date, statement_ending_balance, status, started_at_event)
                     VALUES (?1, ?2, ?3, ?4, 'in_progress', ?5)",
                    params![
                        reconciliation_id,
                        account_id,
                        statement_date.to_string(),
                        statement_ending_balance,
                        stored_event.id,
                    ],
                )?;
            }
            Event::TransactionCleared {
                reconciliation_id,
                entry_id,
                line_id,
                cleared_amount,
            } => {
                self.conn.execute(
                    "INSERT INTO cleared_transactions (reconciliation_id, entry_id, line_id, cleared_amount, cleared_at_event)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        reconciliation_id,
                        entry_id,
                        line_id,
                        cleared_amount,
                        stored_event.id,
                    ],
                )?;
                self.conn.execute(
                    "UPDATE journal_lines SET is_cleared = 1, cleared_at_event = ?1 WHERE id = ?2",
                    params![stored_event.id, line_id],
                )?;
            }
            Event::TransactionUncleared {
                reconciliation_id,
                entry_id,
                line_id,
            } => {
                self.conn.execute(
                    "DELETE FROM cleared_transactions WHERE reconciliation_id = ?1 AND entry_id = ?2 AND line_id = ?3",
                    params![reconciliation_id, entry_id, line_id],
                )?;
                self.conn.execute(
                    "UPDATE journal_lines SET is_cleared = 0, cleared_at_event = NULL WHERE id = ?1",
                    [line_id],
                )?;
            }
            Event::ReconciliationCompleted {
                reconciliation_id,
                difference: _,
            } => {
                self.conn.execute(
                    "UPDATE reconciliations SET status = 'completed', completed_at_event = ?1 WHERE id = ?2",
                    params![stored_event.id, reconciliation_id],
                )?;
            }
            Event::ReconciliationAbandoned { reconciliation_id } => {
                self.conn.execute(
                    "UPDATE reconciliations SET status = 'abandoned' WHERE id = ?1",
                    [reconciliation_id],
                )?;
                // Remove cleared transactions for this reconciliation
                self.conn.execute(
                    "DELETE FROM cleared_transactions WHERE reconciliation_id = ?1",
                    [reconciliation_id],
                )?;
            }
        }

        Ok(())
    }

    /// Replay all events to rebuild projections
    pub fn rebuild(&self, events: &[StoredEvent]) -> Result<(), ProjectionError> {
        // Clear all projections
        self.conn.execute_batch(
            "DELETE FROM bill_payments;
             DELETE FROM invoice_payments;
             DELETE FROM bills;
             DELETE FROM invoices;
             DELETE FROM event_services;
             DELETE FROM plaid_imported_transactions;
             DELETE FROM plaid_local_accounts;
             DELETE FROM plaid_items;
             DELETE FROM cleared_transactions;
             DELETE FROM reconciliations;
             DELETE FROM exchange_rates;
             DELETE FROM currencies;
             DELETE FROM fiscal_periods;
             DELETE FROM fiscal_years;
             DELETE FROM journal_lines;
             DELETE FROM journal_entries;
             DELETE FROM accounts;
             DELETE FROM users;
             DELETE FROM company;
             -- Projections like the rest. Their absence here made a rebuild a
             -- merge for these two tables: a partner deleted from the log, or a
             -- header replaced, would survive a replay that should have derived
             -- the state from the log alone. `partner_tins` is deliberately NOT
             -- cleared — it is local configuration, not derived from anything,
             -- and is exactly what a replay must leave alone.
             DELETE FROM partners;
             DELETE FROM business_profile;
             -- Fed by events since migration 027. Truncated like any other
             -- projection: a row no event justifies is a mapping one machine has
             -- and its colleagues do not, which is the divergence that made these
             -- events necessary in the first place.
             DELETE FROM tax_line_mappings;
             DELETE FROM schedule_b_answers;",
        )?;

        // Replay all events
        for event in events {
            self.apply(event)?;
        }

        Ok(())
    }
}

/// Backend-level projection application (SPEC §6.1 storage abstraction).
///
/// The event store folds events into its materialized projection tables
/// (`accounts`, `journal_entries`, …). This trait is the backend-agnostic
/// surface for that fold: callers say "apply this event to the projections"
/// without reaching for a raw `rusqlite::Connection`, so the same command and
/// import code runs against either the local SQLite store or the Postgres
/// group-server backend (which will implement this trait with Postgres SQL).
///
/// The SQLite implementation below just drives the existing [`Projector`]. The
/// methods take `&mut self` because a Postgres client requires a mutable
/// The row this account already has under an id the bank has since changed.
///
/// Plaid mints account ids per Item, so re-linking a bank returns the same real
/// accounts wearing new ids. Recognising that is what keeps one bank account as
/// one row — and, more importantly, keeps the ledger account it is mapped to
/// attached to it. Treating a re-link as a set of new accounts leaves the mapping
/// on ids the bank will never mention again: the connection looks healthy and
/// imports nothing.
///
/// Two ways to recognise it, in order of how much they are worth trusting:
///
/// 1. `persistent_account_id` — Plaid's own answer to this, stable across Items.
///    Present for the institutions that support it, absent for the rest and for
///    every row written before we stored it.
/// 2. Failing that, the account's mask, name and type together — **and only when
///    exactly one stored row matches.** Two cards ending 3082 held by the same
///    person is a shape a bank can produce, and merging them would put one
///    account's transactions into another's ledger account. Silently. So an
///    ambiguous match is not a match: the account is added as new, which is
///    visible and correctable, rather than merged, which is neither.
///
/// Rows whose id the incoming list already names are excluded — they are spoken
/// for, and matching against them would move an account onto a row that belongs
/// to a different one.
fn same_account_under_a_new_id(
    conn: &Connection,
    item_id: &str,
    acct: &crate::events::types::PlaidAccountInfo,
    claimed: &std::collections::HashSet<&str>,
) -> Result<Option<String>, ProjectionError> {
    let mut candidates: Vec<String> = Vec::new();

    if let Some(persistent) = acct.persistent_account_id.as_deref() {
        let mut stmt = conn.prepare(
            "SELECT plaid_account_id FROM plaid_local_accounts
              WHERE item_id = ?1 AND persistent_account_id = ?2",
        )?;
        let rows = stmt.query_map(params![item_id, persistent], |r| r.get::<_, String>(0))?;
        for row in rows {
            let id = row?;
            if !claimed.contains(id.as_str()) {
                candidates.push(id);
            }
        }
        // A persistent id that matched nothing does not fall through to the
        // weaker rule. The bank told us which account this is; if we hold no row
        // for it, it is new.
        if !candidates.is_empty() {
            return Ok(candidates.pop().filter(|_| candidates.is_empty()));
        }
        if stored_any_persistent_id(conn, item_id)? {
            return Ok(None);
        }
    }

    let mut stmt = conn.prepare(
        "SELECT plaid_account_id FROM plaid_local_accounts
          WHERE item_id = ?1 AND name = ?2 AND account_type = ?3
            AND mask IS ?4 AND persistent_account_id IS NULL",
    )?;
    let rows = stmt.query_map(
        params![item_id, acct.name, acct.account_type, acct.mask],
        |r| r.get::<_, String>(0),
    )?;
    for row in rows {
        let id = row?;
        if !claimed.contains(id.as_str()) {
            candidates.push(id);
        }
    }
    if candidates.len() == 1 {
        return Ok(candidates.pop());
    }
    Ok(None)
}

/// Whether this connection has any account carrying a persistent id.
///
/// The guard on falling back to mask-and-name matching. Where the institution
/// supplies persistent ids, a persistent id that matched nothing means the
/// account really is new — and guessing by name after that could merge it into an
/// unrelated row. Where it supplies none, every row is NULL and the weaker rule
/// is all there is.
fn stored_any_persistent_id(conn: &Connection, item_id: &str) -> Result<bool, ProjectionError> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM plaid_local_accounts
              WHERE item_id = ?1 AND persistent_account_id IS NOT NULL LIMIT 1",
            params![item_id],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false))
}

/// borrow to execute; SQLite is happy either way.
pub trait ProjectionStore {
    /// Apply a single event to the projection tables.
    fn apply_projection(&mut self, stored_event: &StoredEvent) -> Result<(), ProjectionError>;

    /// Clear and replay `events` to rebuild every projection from scratch.
    fn rebuild_projections(&mut self, events: &[StoredEvent]) -> Result<(), ProjectionError>;
}

impl ProjectionStore for crate::store::event_store::EventStore {
    fn apply_projection(&mut self, stored_event: &StoredEvent) -> Result<(), ProjectionError> {
        Projector::new(self.connection()).apply(stored_event)
    }

    fn rebuild_projections(&mut self, events: &[StoredEvent]) -> Result<(), ProjectionError> {
        Projector::new(self.connection()).rebuild(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::types::{EventEnvelope, JournalLineData};
    use crate::store::event_store::EventStore;
    use crate::store::migrations::SchemaStore;
    use chrono::NaiveDate;

    fn setup() -> EventStore {
        let mut store = EventStore::in_memory().unwrap();
        store.init_schema().unwrap();
        store
    }

    fn append_and_project(store: &mut EventStore, event: Event, user_id: &str) -> StoredEvent {
        let stored = store
            .append(EventEnvelope::new(event, user_id.to_string()))
            .unwrap();
        store.apply_projection(&stored).unwrap();
        stored
    }

    #[test]
    fn test_project_account_created() {
        let mut store = setup();

        let event = Event::AccountCreated {
            account_id: "acc-001".to_string(),
            account_type: EventAccountType::Asset,
            account_number: "1000".to_string(),
            name: "Cash".to_string(),
            parent_id: None,
            currency: Some("USD".to_string()),
            description: Some("Main cash account".to_string()),
        };

        append_and_project(&mut store, event, "user-001");

        // Verify projection
        let (name, is_active): (String, i32) = store
            .connection()
            .query_row(
                "SELECT name, is_active FROM accounts WHERE id = ?1",
                ["acc-001"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        assert_eq!(name, "Cash");
        assert_eq!(is_active, 1);
    }

    #[test]
    fn test_project_journal_entry() {
        let mut store = setup();

        // First create accounts
        let acc1 = Event::AccountCreated {
            account_id: "expense".to_string(),
            account_type: EventAccountType::Expense,
            account_number: "5000".to_string(),
            name: "Supplies".to_string(),
            parent_id: None,
            currency: None,
            description: None,
        };
        let acc2 = Event::AccountCreated {
            account_id: "cash".to_string(),
            account_type: EventAccountType::Asset,
            account_number: "1000".to_string(),
            name: "Cash".to_string(),
            parent_id: None,
            currency: None,
            description: None,
        };

        append_and_project(&mut store, acc1, "user-001");
        append_and_project(&mut store, acc2, "user-001");

        // Now create journal entry
        let entry = Event::JournalEntryPosted {
            entry_id: "entry-001".to_string(),
            date: NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
            memo: "Bought supplies".to_string(),
            lines: vec![
                JournalLineData {
                    line_id: "line-001".to_string(),
                    account_id: "expense".to_string(),
                    amount: 10000,
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
                JournalLineData {
                    line_id: "line-002".to_string(),
                    account_id: "cash".to_string(),
                    amount: -10000,
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
            ],
            reference: Some("CHK-001".to_string()),
            source: None,
        };

        append_and_project(&mut store, entry, "user-001");

        // Verify entry
        let memo: String = store
            .connection()
            .query_row(
                "SELECT memo FROM journal_entries WHERE id = ?1",
                ["entry-001"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(memo, "Bought supplies");

        // Verify lines
        let line_count: i32 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM journal_lines WHERE entry_id = ?1",
                ["entry-001"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(line_count, 2);

        // Verify balance
        let sum: i64 = store
            .connection()
            .query_row(
                "SELECT SUM(amount) FROM journal_lines WHERE entry_id = ?1",
                ["entry-001"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(sum, 0); // Balanced
    }

    #[test]
    fn test_project_void_entry() {
        let mut store = setup();

        // Create accounts and entry
        let acc1 = Event::AccountCreated {
            account_id: "expense".to_string(),
            account_type: EventAccountType::Expense,
            account_number: "5000".to_string(),
            name: "Supplies".to_string(),
            parent_id: None,
            currency: None,
            description: None,
        };
        let acc2 = Event::AccountCreated {
            account_id: "cash".to_string(),
            account_type: EventAccountType::Asset,
            account_number: "1000".to_string(),
            name: "Cash".to_string(),
            parent_id: None,
            currency: None,
            description: None,
        };

        append_and_project(&mut store, acc1, "user-001");
        append_and_project(&mut store, acc2, "user-001");

        let entry = Event::JournalEntryPosted {
            entry_id: "entry-001".to_string(),
            date: NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
            memo: "Original entry".to_string(),
            lines: vec![
                JournalLineData {
                    line_id: "line-001".to_string(),
                    account_id: "expense".to_string(),
                    amount: 10000,
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
                JournalLineData {
                    line_id: "line-002".to_string(),
                    account_id: "cash".to_string(),
                    amount: -10000,
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
            ],
            reference: None,
            source: None,
        };

        append_and_project(&mut store, entry, "user-001");

        // Void the entry
        let void_event = Event::JournalEntryVoided {
            entry_id: "entry-001".to_string(),
            reason: "Error".to_string(),
        };

        append_and_project(&mut store, void_event, "user-001");

        // Verify void status
        let is_void: i32 = store
            .connection()
            .query_row(
                "SELECT is_void FROM journal_entries WHERE id = ?1",
                ["entry-001"],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(is_void, 1);
    }

    #[test]
    fn test_rebuild_projections() {
        let mut store = setup();

        // Create some events
        let events_data = vec![
            Event::AccountCreated {
                account_id: "acc-001".to_string(),
                account_type: EventAccountType::Asset,
                account_number: "1000".to_string(),
                name: "Cash".to_string(),
                parent_id: None,
                currency: None,
                description: None,
            },
            Event::AccountCreated {
                account_id: "acc-002".to_string(),
                account_type: EventAccountType::Expense,
                account_number: "5000".to_string(),
                name: "Supplies".to_string(),
                parent_id: None,
                currency: None,
                description: None,
            },
        ];

        let mut stored_events = Vec::new();
        for event in events_data {
            let stored = append_and_project(&mut store, event, "user-001");
            stored_events.push(stored);
        }

        // Verify initial state
        let count: i32 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM accounts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);

        // Rebuild from scratch
        store.rebuild_projections(&stored_events).unwrap();

        // Verify same state after rebuild
        let count_after: i32 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM accounts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count_after, 2);
    }
}

#[cfg(test)]
mod rebuild_survives_local_config {
    use super::*;
    use crate::events::types::{EventAccountType, EventEnvelope};
    use crate::store::event_store::EventStore;
    use crate::store::migrations::SchemaStore;

    fn store() -> EventStore {
        let mut s = EventStore::in_memory().unwrap();
        s.init_schema().unwrap();
        s
    }

    /// A rebuild must survive an ordinary act of configuration.
    ///
    /// `rebuild` truncates every projection to replay the log from nothing, and
    /// the store runs with `PRAGMA foreign_keys = ON`. When `tax_line_mappings`
    /// and `partner_tins` still carried `REFERENCES`, a single mapped account or
    /// one partner's TIN made `DELETE FROM accounts` fail outright — so mapping
    /// an account to a Form 1065 line disabled the ledger's own recovery path.
    /// Migration 025 removed those keys; this is the test that says why they
    /// must not come back.
    ///
    /// Since migration 027 the mappings are a projection, so the rebuild is
    /// *expected* to clear them — what must not happen is the rebuild being
    /// blocked. The TIN stays local and still survives.
    #[test]
    fn a_rebuild_still_runs_with_tax_mappings_and_a_tin_on_file() {
        let mut s = store();

        let account = Event::AccountCreated {
            account_id: "rent".into(),
            account_type: EventAccountType::Expense,
            account_number: "6100".into(),
            name: "Rent".into(),
            parent_id: None,
            currency: Some("USD".into()),
            description: None,
        };
        let stored = s.append(EventEnvelope::new(account, "u".into())).unwrap();
        s.apply_projection(&stored).unwrap();

        // The two rows that used to make recovery impossible.
        s.connection()
            .execute(
                "INSERT INTO tax_line_mappings (account_id, line_key) VALUES ('rent', 'l13')",
                [],
            )
            .unwrap();
        s.connection()
            .execute(
                "INSERT INTO partner_tins (partner_id, tin) VALUES ('p1', '123-45-6789')",
                [],
            )
            .unwrap();

        let events = s.get_all().unwrap();
        Projector::new(s.connection())
            .rebuild(&events)
            .expect("a rebuild must not be blocked by local configuration");

        // The projection came back from the log.
        let accounts: i64 = s
            .connection()
            .query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(accounts, 1, "the replay did not restore the account");

        // The TIN, which is still local config and derived from nothing, is
        // untouched. That is the point of keeping it out of the log: a secret
        // that lives on one machine survives that machine replaying its log.
        let tins: i64 = s
            .connection()
            .query_row("SELECT COUNT(*) FROM partner_tins", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tins, 1, "a rebuild discarded the partner TINs");

        // The mapping written by hand is *gone*, and that is the intended
        // behaviour since migration 027. It is a projection now: a row no event
        // justifies is a mapping this machine has and its colleagues do not, and
        // a rebuild is exactly where that divergence should be resolved in
        // favour of the log. `adopt_pending` is what rescues rows that predate
        // the change; anything written directly afterwards is a bug in the
        // caller.
        let mappings: i64 = s
            .connection()
            .query_row("SELECT COUNT(*) FROM tax_line_mappings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            mappings, 0,
            "a hand-written mapping must not survive a rebuild — it is a projection now"
        );
    }

    /// The other half of the same change: a mapping that *is* in the log comes
    /// back from it, which is what makes a second machine show the same return.
    #[test]
    fn a_mapping_in_the_log_is_restored_by_a_rebuild() {
        let mut s = store();

        let account = Event::AccountCreated {
            account_id: "rent".into(),
            account_type: EventAccountType::Expense,
            account_number: "6100".into(),
            name: "Rent".into(),
            parent_id: None,
            currency: Some("USD".into()),
            description: None,
        };
        let stored = s.append(EventEnvelope::new(account, "u".into())).unwrap();
        s.apply_projection(&stored).unwrap();

        crate::commands::tax_setup_commands::set_account_line(&mut s, "u", "rent", "l13").unwrap();
        crate::commands::tax_setup_commands::set_schedule_b_answer(&mut s, "u", 2025, "b5", "no")
            .unwrap();

        let events = s.get_all().unwrap();
        Projector::new(s.connection()).rebuild(&events).unwrap();

        assert_eq!(
            crate::tax::lines::load_mapping(s.connection())
                .get("rent")
                .map(String::as_str),
            Some("l13"),
            "the log did not restore the mapping"
        );
        assert_eq!(
            crate::tax::schedule_b::load(s.connection(), 2025).get("b5"),
            Some("no"),
            "the log did not restore the Schedule B answer"
        );
    }
}
