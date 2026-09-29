use crossterm::event::KeyCode;
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
    Frame,
};

use crate::events::types::StoredEvent;
use crate::tui::theme::Theme;
use crate::tui::widgets;

pub struct EventLogView {
    pub events: Vec<StoredEvent>,
    pub scroll_offset: usize,
    pub selected: Option<usize>,
}

impl EventLogView {
    pub fn new() -> Self {
        Self {
            events: Vec::new(),
            scroll_offset: 0,
            selected: Some(0),
        }
    }

    pub fn set_events(&mut self, events: Vec<StoredEvent>) {
        self.events = events;
        // Start at the most recent event (end of list)
        if !self.events.is_empty() {
            self.selected = Some(self.events.len() - 1);
        } else {
            self.selected = None;
        }
    }

    pub fn handle_key(&mut self, key: KeyCode) -> bool {
        match key {
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                true
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                true
            }
            KeyCode::PageUp => {
                self.move_selection(-10);
                true
            }
            KeyCode::PageDown => {
                self.move_selection(10);
                true
            }
            KeyCode::Home => {
                self.selected = if self.events.is_empty() {
                    None
                } else {
                    Some(0)
                };
                true
            }
            KeyCode::End => {
                self.selected = if self.events.is_empty() {
                    None
                } else {
                    Some(self.events.len() - 1)
                };
                true
            }
            _ => false,
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.events.is_empty() {
            return;
        }
        let current = self.selected.unwrap_or(0) as isize;
        let new_idx = (current + delta).clamp(0, self.events.len() as isize - 1) as usize;
        self.selected = Some(new_idx);
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent))
            .title(" Event Log ")
            .title_style(
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            );

        let inner = block.inner(area);
        frame.render_widget(block, area);

        if self.events.is_empty() {
            let empty =
                Paragraph::new("No events recorded yet").style(Style::default().fg(theme.fg_dim));
            frame.render_widget(empty, inner);
            return;
        }

        // Calculate visible area
        let visible_height = inner.height as usize;

        // Calculate scroll offset to keep selection visible
        let scroll_offset = if let Some(selected) = self.selected {
            if selected < self.scroll_offset {
                selected
            } else if selected >= self.scroll_offset + visible_height {
                selected.saturating_sub(visible_height - 1)
            } else {
                self.scroll_offset
            }
        } else {
            self.scroll_offset
        };

        // Build lines for visible events
        let lines: Vec<Line> = self
            .events
            .iter()
            .enumerate()
            .skip(scroll_offset)
            .take(visible_height)
            .map(|(idx, event)| {
                let is_selected = self.selected == Some(idx);
                let style = if is_selected {
                    theme.selected_style()
                } else {
                    Style::default()
                };

                // Format timestamp
                let timestamp = event.timestamp.format("%Y-%m-%d %H:%M:%S");

                // Format event type with color
                let event_type = event.event.event_type();
                let type_color = match event_type {
                    t if t.starts_with("journal") => theme.success,
                    t if t.starts_with("account") => theme.header,
                    t if t.starts_with("company") => Color::Magenta,
                    t if t.starts_with("user") => Color::Blue,
                    t if t.starts_with("bill") || t.starts_with("invoice") => Color::Cyan,
                    t if t.starts_with("reconciliation") || t.starts_with("transaction") => {
                        theme.accent
                    }
                    t if t.starts_with("fiscal")
                        || t.starts_with("period")
                        || t.starts_with("year") =>
                    {
                        theme.error
                    }
                    _ => theme.fg,
                };

                // Format entity ID if present
                let entity = event.event.entity_id().unwrap_or("-");
                let entity_display = if entity.len() > 12 {
                    format!("{}...", &entity[..12])
                } else {
                    entity.to_string()
                };

                // Format summary based on event type
                let summary = format_event_summary(&event.event);

                Line::from(vec![
                    Span::styled(format!("{:>5} ", event.id), style.fg(theme.fg_dim)),
                    Span::styled(format!("{} ", timestamp), style),
                    Span::styled(format!("{:<28} ", event_type), style.fg(type_color)),
                    Span::styled(format!("{:<15} ", entity_display), style.fg(theme.accent)),
                    Span::styled(summary, style),
                ])
            })
            .collect();

        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);

        // Render scrollbar if needed
        if self.events.len() > visible_height {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("↑"))
                .end_symbol(Some("↓"));
            let mut scrollbar_state =
                ScrollbarState::new(self.events.len()).position(scroll_offset);
            frame.render_stateful_widget(scrollbar, area, &mut scrollbar_state);
        }
    }

    pub fn title(&self) -> String {
        format!(" Event Log ({} events) ", self.events.len())
    }
}

impl Default for EventLogView {
    fn default() -> Self {
        Self::new()
    }
}

/// Format a human-readable summary of an event
/// What the form for a year prints beside a question, or the stable key when no
/// revision is carried for that year.
///
/// The number is not a property of the question — the audit-regime election is
/// 31 on the 2023 form and 33 on the later ones — so showing one without a year
/// is showing whichever year happened to be baked in.
fn number_for(tax_year: i32, key: &str) -> String {
    match crate::tax::schedule_b::table_for(tax_year) {
        Some(table) => {
            let n = crate::tax::schedule_b::printed_number(table, key);
            if n.is_empty() {
                key.to_string()
            } else {
                n.to_string()
            }
        }
        None => key.to_string(),
    }
}

/// Micro-shares as a human reads them: six places, trailing zeroes trimmed, so a
/// round holding prints as "10" and a reinvested fraction still prints in full.
fn shares(micro: i64) -> String {
    let mut s = format!("{:.6}", micro as f64 / 1_000_000.0);
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    s
}

fn format_event_summary(event: &crate::events::types::Event) -> String {
    use crate::events::types::Event;

    match event {
        Event::CompanyCreated {
            name,
            base_currency,
            ..
        } => {
            format!("Created company '{}' ({})", name, base_currency)
        }
        Event::CompanySettingsUpdated {
            field, new_value, ..
        } => {
            format!("Updated {} to '{}'", field, new_value)
        }
        Event::BusinessProfileSet(d) => {
            format!("Set partnership details for '{}' ({})", d.legal_name, d.ein)
        }
        Event::PartnerAdmitted(d) => {
            format!(
                "Admitted {} partner '{}' at {}% of profit",
                d.partner_type,
                d.name,
                crate::domain::format_ppm(d.shares.profit_ppm)
            )
        }
        Event::PartnerDetailsUpdated(d) => {
            format!(
                "Updated partner '{}' to {}% of profit",
                d.name,
                crate::domain::format_ppm(d.shares.profit_ppm)
            )
        }
        Event::PartnerWithdrawn {
            partner_id,
            end_date,
        } => {
            format!(
                "Partner {} left on {}",
                widgets::truncate(partner_id, 8),
                end_date
            )
        }
        Event::PartnerRelationshipSet {
            partner_id,
            related_partner_id,
            relationship,
        } => {
            let kind = crate::domain::RelationshipKind::parse(relationship)
                .map(|k| k.label())
                .unwrap_or(relationship.as_str());
            format!(
                "Partner {} is {} {}",
                widgets::truncate(partner_id, 8),
                kind,
                widgets::truncate(related_partner_id, 8)
            )
        }
        Event::PartnerRelationshipCleared {
            partner_id,
            related_partner_id,
        } => {
            format!(
                "Relationship between {} and {} removed",
                widgets::truncate(partner_id, 8),
                widgets::truncate(related_partner_id, 8)
            )
        }
        Event::Il1065SettingsSet(d) => {
            let apportion = if d.apportions_outside_illinois {
                "multi-state"
            } else {
                "Illinois-only"
            };
            let pte = if d.elects_pte_tax {
                ", PTE elected"
            } else {
                ""
            };
            format!("IL-1065 settings: {apportion}{pte}")
        }
        Event::DepreciableAssetAdded(d) | Event::DepreciableAssetUpdated(d) => {
            let verb = if matches!(event, Event::DepreciableAssetAdded(_)) {
                "Asset added"
            } else {
                "Asset updated"
            };
            let class = crate::domain::PropertyClass::parse(&d.property_class)
                .map(|c| c.label())
                .unwrap_or(&d.property_class);
            format!(
                "{verb}: {} — ${:.2}, in service {}, {class}",
                d.description,
                d.cost_cents as f64 / 100.0,
                d.placed_in_service
            )
        }
        Event::DepreciableAssetDisposed {
            asset_id,
            disposed_on,
        } => format!("Asset {asset_id} disposed of on {disposed_on}"),
        Event::DepreciableAssetRemoved { asset_id } => {
            format!("Asset {asset_id} removed from the register")
        }
        Event::DepreciationOverrideSet {
            asset_id,
            tax_year,
            amount_cents,
            note,
        } => format!(
            "Asset {asset_id}: {tax_year} depreciation forced to ${:.2} — {note}",
            *amount_cents as f64 / 100.0
        ),
        Event::DepreciationOverrideCleared { asset_id, tax_year } => {
            format!("Asset {asset_id}: {tax_year} depreciation back to the computed figure")
        }
        Event::DepreciationBasisAdjusted {
            asset_id,
            effective_year,
            amount_cents,
            note,
            ..
        } => format!(
            "Asset {asset_id}: basis {} by ${:.2} from {effective_year} — {note}",
            if *amount_cents < 0 { "reduced" } else { "increased" },
            amount_cents.abs() as f64 / 100.0
        ),
        Event::DepreciationBasisAdjustmentRemoved { asset_id, .. } => {
            format!("Asset {asset_id}: a basis adjustment removed")
        }
        // The brokerage register. Quantities are micro-shares in the log, so they
        // are divided back to shares for a human to read; six places trimmed of
        // trailing zeroes, because "10 shares" should not print as "10.000000".
        Event::SecurityDefined(d) => {
            format!("Security {} — {} ({})", d.ticker, d.name, d.kind)
        }
        Event::SecurityBought {
            security_id,
            quantity,
            total_cost_cents,
            trade_date,
            ..
        } => format!(
            "Bought {} of {} for ${:.2} on {trade_date}",
            shares(*quantity),
            widgets::truncate(security_id, 8),
            *total_cost_cents as f64 / 100.0
        ),
        Event::SecuritySold(d) => format!(
            "Sold {} of {} for ${:.2} on {} — {} of ${:.2} across {} lot(s)",
            shares(d.quantity),
            widgets::truncate(&d.security_id, 8),
            (d.proceeds_cents - d.fee_cents) as f64 / 100.0,
            d.trade_date,
            if d.realized_gain_cents < 0 {
                "loss"
            } else {
                "gain"
            },
            d.realized_gain_cents.abs() as f64 / 100.0,
            d.lots.len()
        ),
        Event::InvestmentIncomeReceived {
            kind,
            amount_cents,
            received_on,
            ..
        } => format!(
            "{} of ${:.2} received on {received_on}",
            match kind {
                crate::events::types::InvestmentIncomeKind::Dividend => "Dividend",
                crate::events::types::InvestmentIncomeKind::Interest => "Interest",
            },
            *amount_cents as f64 / 100.0
        ),
        Event::InvestmentFeeCharged {
            amount_cents,
            charged_on,
            ..
        } => format!(
            "Investment fee of ${:.2} charged on {charged_on}",
            *amount_cents as f64 / 100.0
        ),
        // The sheltered-account register. No quantities here, because nothing
        // inside a sheltered account is recorded — see `retirement_commands`.
        Event::RetirementAccountRegistered {
            account_id,
            institution,
            kind,
            ..
        } => format!(
            "Retirement account {} — {institution}, {}",
            widgets::truncate(account_id, 8),
            kind.label()
        ),
        Event::RetirementValueSet {
            account_id,
            as_of,
            value_cents,
        } => format!(
            "Retirement account {} worth ${:.2} as of {as_of}",
            widgets::truncate(account_id, 8),
            *value_cents as f64 / 100.0
        ),
        Event::RetirementContributionRecorded {
            account_id,
            amount_cents,
            on,
            ..
        } => format!(
            "Contributed ${:.2} to retirement account {} on {on}",
            *amount_cents as f64 / 100.0,
            widgets::truncate(account_id, 8)
        ),
        // The taxable amount is named because it is the one figure here that no
        // journal entry holds, and the one a 1099-R is filed on.
        Event::RetirementDistributionRecorded(d) => format!(
            "Distributed ${:.2} from retirement account {} on {} — ${:.2} taxable, ${:.2} withheld",
            d.gross_cents as f64 / 100.0,
            widgets::truncate(&d.account_id, 8),
            d.on,
            d.taxable_cents as f64 / 100.0,
            d.withheld_cents as f64 / 100.0
        ),
        // The investments importer. What each of these says is a *decision*, so
        // each names the thing decided and what was decided about it — a log entry
        // reading "imported" without saying what it became is one nobody can check.
        Event::InvestmentAccountConfigured(d) => format!(
            "Brokerage account {} imports as {}{}",
            widgets::truncate(&d.plaid_account_id, 8),
            d.accounts.treatment().label(),
            if d.subtype_recognised {
                String::new()
            } else {
                format!(
                    " — assumed, Plaid calls it {:?} and needs confirming",
                    d.plaid_subtype.as_deref().unwrap_or("nothing")
                )
            }
        ),
        Event::PlaidSecurityLinked(d) => format!(
            "Plaid security {} is security {}",
            widgets::truncate(&d.plaid_security_id, 10),
            widgets::truncate(&d.security_id, 8)
        ),
        Event::InvestmentActivityImported(d) => format!(
            "Imported {} from {} as a {}, entry {}",
            widgets::truncate(&d.provider_transaction_id, 12),
            widgets::truncate(&d.plaid_account_id, 8),
            d.outcome.as_str(),
            widgets::truncate(&d.entry_id, 8)
        ),
        Event::HoldingsSnapshotRecorded(d) => format!(
            "Holdings of {} as of {}: {} position(s){}",
            widgets::truncate(&d.plaid_account_id, 8),
            d.as_of,
            d.holdings.len(),
            match d.total_value_cents() {
                Some(total) => format!(", worth ${:.2}", total as f64 / 100.0),
                None => String::new(),
            }
        ),
        Event::BusinessTypeSet { business_type } => {
            let t = crate::domain::BusinessType::parse(business_type)
                .map(|t| t.label())
                .unwrap_or(business_type.as_str());
            format!("Business type set to {t}")
        }
        Event::SoleProprietorSet(d) => {
            let method = crate::domain::AccountingMethod::parse(&d.accounting_method)
                .map(|m| m.label())
                .unwrap_or(d.accounting_method.as_str());
            format!("Sole proprietor: {} ({method})", d.name)
        }
        Event::ScheduleCAnswerSet {
            tax_year,
            answer_key,
            value,
        } => format!("Schedule C {tax_year} {answer_key}: {value}"),
        Event::ScheduleCAnswerCleared {
            tax_year,
            answer_key,
        } => format!("Schedule C {tax_year} {answer_key} cleared"),
        Event::TaxLineMappingSet {
            account_id,
            line_key,
            ..
        } => {
            let line = crate::tax::any_line_def(line_key)
                .map(|d| d.number)
                .unwrap_or(line_key);
            format!(
                "Account {} reports on line {}",
                widgets::truncate(account_id, 8),
                line
            )
        }
        Event::TaxLineMappingCleared { account_id, .. } => {
            format!(
                "Account {} taken off the return",
                widgets::truncate(account_id, 8)
            )
        }
        Event::PartnerSharesChanged {
            partner_id,
            effective_from,
            profit_ppm,
            ..
        } => {
            format!(
                "Partner {} takes {}% of profits from {effective_from}",
                widgets::truncate(partner_id, 8),
                *profit_ppm as f64 / 10_000.0
            )
        }
        Event::PartnerEquityAccountLinked {
            partner_id,
            account_id,
            role,
        } => {
            format!(
                "Account {} is partner {}'s {role}",
                widgets::truncate(account_id, 8),
                widgets::truncate(partner_id, 8)
            )
        }
        Event::PartnerEquityAccountUnlinked { account_id, .. } => {
            format!(
                "Account {} is no longer a partner's capital",
                widgets::truncate(account_id, 8)
            )
        }
        Event::PartnerAllocationFixed {
            tax_year,
            partner_id,
            amount_cents,
            preferred,
            note,
        } => match amount_cents {
            Some(cents) if *preferred => format!(
                "Partner {} takes the first ${:.2} of {tax_year}, then a percentage share — {note}",
                widgets::truncate(partner_id, 8),
                *cents as f64 / 100.0
            ),
            Some(cents) => format!(
                "Partner {} takes ${:.2} of {tax_year} — {note}",
                widgets::truncate(partner_id, 8),
                *cents as f64 / 100.0
            ),
            None => format!(
                "Partner {} takes the rest of {tax_year} — {note}",
                widgets::truncate(partner_id, 8)
            ),
        },
        Event::PartnerAllocationCleared {
            tax_year,
            partner_id,
        } => format!(
            "Partner {}'s share of {tax_year} back on their percentages",
            widgets::truncate(partner_id, 8)
        ),
        Event::LiabilityClassified {
            account_id,
            kind,
            partner_id,
            guaranteed,
            note,
        } => format!(
            "Liability {} is {}{}{} — {note}",
            widgets::truncate(account_id, 8),
            kind.replace('_', " "),
            partner_id
                .as_deref()
                .map(|p| format!(" to partner {}", widgets::truncate(p, 8)))
                .unwrap_or_default(),
            if *guaranteed { ", guaranteed" } else { "" }
        ),
        Event::LiabilityClassificationCleared { account_id } => format!(
            "Liability {} back on the default classification",
            widgets::truncate(account_id, 8)
        ),
        Event::AccountDeleted { account_id } => {
            format!("Account {} deleted", widgets::truncate(account_id, 8))
        }
        Event::TaxDeductionLimitSet {
            account_id,
            deductible_pct,
            ..
        } => {
            format!(
                "Account {} is {}% deductible",
                widgets::truncate(account_id, 8),
                deductible_pct
            )
        }
        Event::TaxDeductionLimitCleared { account_id, .. } => {
            format!(
                "Account {} is fully deductible again",
                widgets::truncate(account_id, 8)
            )
        }
        Event::TaxStatementGroupingSet {
            account_id,
            grouped,
            ..
        } => {
            format!(
                "Account {} {} its children on attached statements",
                widgets::truncate(account_id, 8),
                if *grouped {
                    "groups"
                } else {
                    "no longer groups"
                }
            )
        }
        Event::IllinoisTaxAddbackSet {
            account_id,
            added_back,
            effective_from,
        } => format!(
            "Account {} {} Illinois tax added back on IL-1065 line 16 from {effective_from}",
            widgets::truncate(account_id, 8),
            if *added_back { "is" } else { "is no longer" }
        ),
        Event::DocumentAttached(d) => format!(
            "Document attached: {}{}",
            d.filename,
            d.tax_year.map(|y| format!(" ({y})")).unwrap_or_default()
        ),
        Event::DocumentRemoved { document_id } => {
            format!("Document {} removed", widgets::truncate(document_id, 8))
        }
        Event::TaxStatementRecorded(s) => {
            let form = crate::tax::information_returns::FormKind::parse(&s.form)
                .map(|f| f.label())
                .unwrap_or(s.form.as_str());
            format!("{} {form} from {} recorded", s.tax_year, s.issuer)
        }
        Event::TaxStatementRemoved { statement_id } => {
            format!("Tax statement {} removed", widgets::truncate(statement_id, 8))
        }
        Event::TaxStatementLinesRecorded(l) => match l.lines.len() {
            0 => format!(
                "Transaction detail cleared on statement {}",
                widgets::truncate(&l.statement_id, 8)
            ),
            1 => format!(
                "1 transaction recorded on statement {}",
                widgets::truncate(&l.statement_id, 8)
            ),
            n => format!(
                "{n} transactions recorded on statement {}",
                widgets::truncate(&l.statement_id, 8)
            ),
        },
        Event::K1SourceLinked {
            ledger_name,
            partner_name,
            ..
        } => format!("Receives {partner_name}'s K-1 from {ledger_name}"),
        Event::K1SourceUnlinked { link_id } => {
            format!("K-1 link {} removed", widgets::truncate(link_id, 8))
        }
        Event::ScheduleBAnswerSet {
            tax_year,
            answer_key,
            value,
        } => {
            let q = number_for(*tax_year, answer_key);
            format!("Schedule B {tax_year} question {q}: {value}")
        }
        Event::ScheduleBAnswerCleared {
            tax_year,
            answer_key,
        } => {
            let q = number_for(*tax_year, answer_key);
            format!("Schedule B {tax_year} question {q} back to unanswered")
        }
        Event::UserAdded { username, role, .. } => {
            format!("Added user '{}' as {:?}", username, role)
        }
        Event::UserModified { user_id, field, .. } => {
            format!(
                "Modified {} for user {}",
                field,
                widgets::truncate(user_id, 8)
            )
        }
        Event::UserRemoved { user_id } => {
            format!("Removed user {}", widgets::truncate(user_id, 8))
        }
        Event::AccountCreated {
            account_number,
            name,
            account_type,
            ..
        } => {
            format!(
                "{} {} - {} ({:?})",
                account_number,
                name,
                widgets::truncate(name, 20),
                account_type
            )
        }
        Event::AccountUpdated {
            account_id,
            field,
            new_value,
            ..
        } => {
            format!(
                "Updated {} to '{}' on {}",
                field,
                new_value,
                widgets::truncate(account_id, 8)
            )
        }
        Event::AccountDeactivated { account_id, .. } => {
            format!("Deactivated account {}", widgets::truncate(account_id, 8))
        }
        Event::AccountReactivated { account_id } => {
            format!("Reactivated account {}", widgets::truncate(account_id, 8))
        }
        Event::JournalEntryPosted { memo, lines, .. } => {
            let amount = lines
                .iter()
                .filter(|l| l.amount > 0)
                .map(|l| l.amount)
                .sum::<i64>();
            format!(
                "{} (${:.2})",
                widgets::truncate(memo, 40),
                amount as f64 / 100.0
            )
        }
        Event::JournalEntryVoided { entry_id, reason } => {
            format!(
                "Voided {} - {}",
                widgets::truncate(entry_id, 8),
                widgets::truncate(reason, 30)
            )
        }
        Event::JournalEntryUnvoided { entry_id, reason } => {
            format!(
                "Unvoided {} - {}",
                widgets::truncate(entry_id, 8),
                widgets::truncate(reason, 30)
            )
        }
        Event::JournalEntryAnnotated {
            entry_id,
            annotation,
        } => {
            format!(
                "Annotated {} - {}",
                widgets::truncate(entry_id, 8),
                widgets::truncate(annotation, 30)
            )
        }
        Event::JournalLineReassigned {
            line_id,
            new_account_id,
            ..
        } => {
            format!(
                "Reassigned line {} to account {}",
                widgets::truncate(line_id, 8),
                widgets::truncate(new_account_id, 8)
            )
        }
        Event::FiscalYearOpened { year, .. } => {
            format!("Opened fiscal year {}", year)
        }
        Event::YearEndClosed { year, .. } => {
            format!("Closed the books for {}", year)
        }
        Event::YearEndReopened { year, reason, .. } => {
            format!("Reopened {} - {}", year, widgets::truncate(reason, 20))
        }
        Event::CurrencyEnabled { code, name, .. } => {
            format!("Enabled currency {} ({})", code, name)
        }
        Event::ExchangeRateRecorded {
            from_currency,
            to_currency,
            rate,
            ..
        } => {
            format!("{}/{} = {}", from_currency, to_currency, rate)
        }
        Event::ReconciliationStarted {
            account_id,
            statement_date,
            ..
        } => {
            format!(
                "Started reconciliation for {} on {}",
                widgets::truncate(account_id, 8),
                statement_date
            )
        }
        Event::TransactionCleared { entry_id, .. } => {
            format!("Cleared transaction {}", widgets::truncate(entry_id, 8))
        }
        Event::TransactionUncleared { entry_id, .. } => {
            format!("Uncleared transaction {}", widgets::truncate(entry_id, 8))
        }
        Event::ReconciliationCompleted {
            reconciliation_id,
            difference,
        } => {
            format!(
                "Completed reconciliation {} (diff: {})",
                widgets::truncate(reconciliation_id, 8),
                difference
            )
        }
        Event::ReconciliationAbandoned { reconciliation_id } => {
            format!(
                "Abandoned reconciliation {}",
                widgets::truncate(reconciliation_id, 8)
            )
        }
        Event::PlaidItemConnected {
            institution_name,
            plaid_accounts,
            ..
        } => {
            format!(
                "Connected {} ({} accounts)",
                institution_name,
                plaid_accounts.len()
            )
        }
        Event::PlaidAccountsRefreshed { plaid_accounts, .. } => {
            format!("Refreshed connection accounts ({})", plaid_accounts.len())
        }
        Event::PlaidItemDisconnected { item_id, reason } => {
            format!(
                "Disconnected {} - {}",
                widgets::truncate(item_id, 8),
                widgets::truncate(reason, 30)
            )
        }
        Event::PlaidAccountMapped {
            plaid_account_id,
            local_account_id,
            ..
        } => {
            format!(
                "Mapped Plaid {} to {}",
                widgets::truncate(plaid_account_id, 8),
                widgets::truncate(local_account_id, 8)
            )
        }
        Event::PlaidAccountUnmapped {
            plaid_account_id,
            local_account_id,
            ..
        } => {
            format!(
                "Unmapped Plaid {} from {}",
                widgets::truncate(plaid_account_id, 8),
                widgets::truncate(local_account_id, 8)
            )
        }
        Event::PlaidTransactionsSynced {
            transactions_added,
            item_id,
            ..
        } => {
            format!(
                "Synced {} transactions for {}",
                transactions_added,
                widgets::truncate(item_id, 8)
            )
        }
        Event::EventServiceRegistered { name, root_url, .. } => {
            format!(
                "Registered service '{}' ({})",
                name,
                widgets::truncate(root_url, 30)
            )
        }
        Event::EventServiceReportingChanged {
            frequency,
            effective_from,
            ..
        } => format!("Sales reporting set to {frequency} from {effective_from}"),
        Event::EventServiceRemoved { service_id } => {
            format!("Removed service {}", widgets::truncate(service_id, 8))
        }
        Event::EventServiceSynced {
            service_id,
            events_processed,
            entries_created,
            errors,
        } => {
            format!(
                "Synced service {}: {} events, {} entries, {} errors",
                widgets::truncate(service_id, 8),
                events_processed,
                entries_created,
                errors
            )
        }
        Event::BillReceived {
            vendor,
            amount,
            due_date,
            ..
        } => {
            format!(
                "Bill from {} for ${:.2} due {}",
                vendor,
                *amount as f64 / 100.0,
                due_date
            )
        }
        Event::BillPaymentApplied {
            bill_id,
            amount_applied,
            ..
        } => {
            format!(
                "Payment ${:.2} applied to bill {}",
                *amount_applied as f64 / 100.0,
                widgets::truncate(bill_id, 8)
            )
        }
        Event::BillVoided { bill_id, reason } => {
            format!(
                "Voided bill {} - {}",
                widgets::truncate(bill_id, 8),
                widgets::truncate(reason, 30)
            )
        }
        Event::InvoiceIssued {
            customer,
            amount,
            due_date,
            ..
        } => {
            format!(
                "Invoice to {} for ${:.2} due {}",
                customer,
                *amount as f64 / 100.0,
                due_date
            )
        }
        Event::InvoicePaymentReceived {
            invoice_id,
            amount_applied,
            ..
        } => {
            format!(
                "Payment ${:.2} received on invoice {}",
                *amount_applied as f64 / 100.0,
                widgets::truncate(invoice_id, 8)
            )
        }
        Event::InvoiceVoided { invoice_id, reason } => {
            format!(
                "Voided invoice {} - {}",
                widgets::truncate(invoice_id, 8),
                widgets::truncate(reason, 30)
            )
        }
    }
}
