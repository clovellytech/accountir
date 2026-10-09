use crate::events::types::{
    Event, ImportedActivityKind, InvestmentPostingAccounts, JournalLineData,
};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ValidationError {
    #[error("Journal entry is not balanced: sum is {0}, expected 0")]
    JournalEntryNotBalanced(i64),
    #[error("Journal entry must have at least two lines")]
    InsufficientLines,
    #[error("Empty field: {0}")]
    EmptyField(String),
    #[error("Invalid value: {0}")]
    InvalidValue(String),
    #[error("Duplicate ID: {0}")]
    DuplicateId(String),
    #[error("Invalid currency code: {0}")]
    InvalidCurrencyCode(String),
    #[error("Invalid account number: {0}")]
    InvalidAccountNumber(String),
    #[error("Invalid fiscal year start month: {0}")]
    InvalidFiscalYearStart(u32),
}

/// A tax year a return could plausibly be filed for.
///
/// Bounded rather than accepted as any `i32` because the year is half the key
/// of every Schedule B answer: a typo puts an answer in a year nobody will ever
/// open again, where it is neither visible nor obviously missing.
fn validate_tax_year(year: i32) -> Result<(), ValidationError> {
    if (1900..=2200).contains(&year) {
        Ok(())
    } else {
        Err(ValidationError::InvalidValue(format!(
            "{year} is not a tax year"
        )))
    }
}

/// A statement form code this version knows.
///
/// Refused rather than stored when it is not: a statement under a code nothing
/// recognises is a set of figures no return will ever pick up.
fn validate_form(code: &str) -> Result<crate::tax::information_returns::FormKind, ValidationError> {
    crate::tax::information_returns::FormKind::parse(code).ok_or_else(|| {
        ValidationError::InvalidValue(format!(
            "form: {code:?} is not a statement this version knows"
        ))
    })
}

/// Validate an event before storing
pub fn validate_event(event: &Event) -> Result<(), ValidationError> {
    match event {
        Event::CompanyCreated {
            company_id,
            name,
            base_currency,
            fiscal_year_start,
        } => {
            validate_non_empty(company_id, "company_id")?;
            validate_non_empty(name, "company name")?;
            validate_currency_code(base_currency)?;
            validate_fiscal_year_start(*fiscal_year_start)?;
        }
        Event::CompanySettingsUpdated {
            field,
            old_value: _,
            new_value,
        } => {
            validate_non_empty(field, "field")?;
            validate_non_empty(new_value, "new_value")?;
        }
        Event::BusinessProfileSet(d) => {
            let (legal_name, address, ein, naics_code) =
                (&d.legal_name, &d.address, &d.ein, &d.naics_code);
            validate_non_empty(legal_name, "legal_name")?;
            // Checked for shape, and only when there is one to check. A sole
            // proprietorship may have no EIN, because many never need to apply
            // for one — an EIN it *does* have is reported on Schedule C line D.
            // This event does not carry the business type, so it cannot know
            // which case it is in. Whether a *particular return* can be filed
            // without one is a question for whoever is filing it: the command
            // layer refuses it for a partnership, and `form1065` warns.
            //
            // Shape when present is still worth catching here: a mistyped EIN is
            // a rejected return weeks later.
            if !ein.is_empty() && !crate::domain::is_valid_ein(ein) {
                return Err(ValidationError::InvalidValue(format!(
                    "EIN {ein:?} is not NN-NNNNNNN"
                )));
            }
            if !crate::domain::is_valid_naics(naics_code) {
                return Err(ValidationError::InvalidValue(format!(
                    "NAICS code {naics_code:?} is not six digits"
                )));
            }
            validate_address(address)?;
        }
        Event::PartnerAdmitted(d) => {
            let (partner_id, name, partner_type, residency, entity_type, address, shares) = (
                &d.partner_id,
                &d.name,
                &d.partner_type,
                &d.residency,
                &d.entity_type,
                &d.address,
                &d.shares,
            );
            validate_non_empty(partner_id, "partner_id")?;
            validate_non_empty(name, "partner name")?;
            validate_non_empty(entity_type, "entity_type")?;
            validate_partner_type(partner_type)?;
            validate_residency(residency)?;
            validate_shares(shares)?;
            validate_address(address)?;
        }
        Event::PartnerDetailsUpdated(d) => {
            let (partner_id, name, partner_type, residency, entity_type, address, shares) = (
                &d.partner_id,
                &d.name,
                &d.partner_type,
                &d.residency,
                &d.entity_type,
                &d.address,
                &d.shares,
            );
            validate_non_empty(partner_id, "partner_id")?;
            validate_non_empty(name, "partner name")?;
            validate_non_empty(entity_type, "entity_type")?;
            validate_partner_type(partner_type)?;
            validate_residency(residency)?;
            validate_shares(shares)?;
            validate_address(address)?;
        }
        Event::PartnerWithdrawn {
            partner_id,
            end_date: _,
        } => {
            validate_non_empty(partner_id, "partner_id")?;
        }
        Event::PartnerRelationshipSet {
            partner_id,
            related_partner_id,
            relationship,
        } => {
            validate_non_empty(partner_id, "partner_id")?;
            validate_non_empty(related_partner_id, "related_partner_id")?;
            // A partner cannot be their own relative: a self-edge would attribute
            // their own share to themselves twice if the family walk ever failed
            // to exclude them, and it means nothing on the form either way.
            if partner_id == related_partner_id {
                return Err(ValidationError::InvalidValue(
                    "a partner cannot be related to themselves".to_string(),
                ));
            }
            // Checked against the known kinds, not merely for emptiness: an
            // unrecognised kind is a tie the attribution walk would silently
            // ignore, so a partner who should be on Schedule B-1 quietly is not.
            if crate::domain::RelationshipKind::parse(relationship).is_none() {
                return Err(ValidationError::InvalidValue(format!(
                    "{relationship:?} is not a relationship kind (spouse, sibling, parent_of)"
                )));
            }
        }
        Event::PartnerRelationshipCleared {
            partner_id,
            related_partner_id,
        } => {
            validate_non_empty(partner_id, "partner_id")?;
            validate_non_empty(related_partner_id, "related_partner_id")?;
        }
        // Two booleans; every combination is a state the form can express, so
        // there is nothing to refuse.
        Event::Il1065SettingsSet(_) => {}
        // The asset register. Everything state-dependent — that the accounts
        // exist, that the asset does — is checked under the write lock by
        // `depreciation_commands`; what is left here is what can be judged from
        // the event alone, and it is checked because these strings replay onto
        // every member's machine.
        Event::DepreciableAssetAdded(d) | Event::DepreciableAssetUpdated(d) => {
            validate_non_empty(&d.asset_id, "asset_id")?;
            validate_non_empty(&d.description, "description")?;
            validate_non_empty(&d.asset_account_id, "asset_account_id")?;
            validate_non_empty(&d.expense_account_id, "expense_account_id")?;
            validate_non_empty(&d.accumulated_account_id, "accumulated_account_id")?;
            // Checked against the catalogue rather than for emptiness, the same
            // rule `TaxLineMappingSet` follows below: a class nothing recognises
            // is an asset that silently depreciates at nothing, and the register
            // would look complete while the deduction quietly went missing.
            if crate::domain::PropertyClass::parse(&d.property_class).is_none() {
                return Err(ValidationError::InvalidValue(format!(
                    "{} is not a MACRS property class",
                    d.property_class
                )));
            }
            if crate::domain::System::parse(&d.system).is_none() {
                return Err(ValidationError::InvalidValue(format!(
                    "{} is not a depreciation system",
                    d.system
                )));
            }
            if crate::domain::BonusElection::parse(&d.bonus).is_none() {
                return Err(ValidationError::InvalidValue(format!(
                    "{} is not a bonus depreciation election",
                    d.bonus
                )));
            }
            if d.cost_cents <= 0 {
                return Err(ValidationError::InvalidValue(
                    "an asset costs more than nothing".to_string(),
                ));
            }
            if d.section_179_cents < 0 {
                return Err(ValidationError::InvalidValue(
                    "a §179 election cannot be negative".to_string(),
                ));
            }
            // An election with nowhere to post it is an election that would
            // silently reach page 1 line 16a through the ordinary depreciation
            // account — the one place §179 must never appear, since the partner
            // applies their own limits to it on Schedule K line 12. Refused here
            // rather than at posting time so the register never holds the
            // broken state at all.
            if d.section_179_cents > 0 {
                match &d.section_179_account_id {
                    None => {
                        return Err(ValidationError::InvalidValue(format!(
                            "{}: a §179 election needs its own expense account — it is \
                             separately stated on Schedule K line 12, not deducted on page 1 \
                             line 16a with ordinary depreciation",
                            d.description
                        )));
                    }
                    Some(a) if a == &d.expense_account_id => {
                        return Err(ValidationError::InvalidValue(format!(
                            "{}: the §179 account and the depreciation expense account are the \
                             same. They reach different lines of the return — Schedule K line \
                             12 and page 1 line 16a — so they cannot be one account",
                            d.description
                        )));
                    }
                    Some(a) => validate_non_empty(a, "section_179_account_id")?,
                }
            }
            // Placed in service before it was acquired is not a late fit-out,
            // it is a typo — and it would silently reach for the wrong bonus
            // rate, since the two dates answer different questions.
            if d.placed_in_service < d.acquired_on {
                return Err(ValidationError::InvalidValue(format!(
                    "{} was placed in service on {} but acquired on {}, after",
                    d.description, d.placed_in_service, d.acquired_on
                )));
            }
        }
        Event::DepreciableAssetDisposed { asset_id, .. } => {
            validate_non_empty(asset_id, "asset_id")?;
        }
        Event::DepreciableAssetRemoved { asset_id } => {
            validate_non_empty(asset_id, "asset_id")?;
        }
        Event::DepreciationOverrideSet {
            asset_id,
            tax_year,
            amount_cents,
            note,
        } => {
            validate_non_empty(asset_id, "asset_id")?;
            // The reason is the point: an override nobody can explain is a
            // figure on a return nobody can defend.
            validate_non_empty(note, "note")?;
            if *amount_cents < 0 {
                return Err(ValidationError::InvalidValue(format!(
                    "amount_cents: {amount_cents} — a year's depreciation cannot be negative"
                )));
            }
            if !(1900..=2200).contains(tax_year) {
                return Err(ValidationError::InvalidValue(format!(
                    "tax_year: {tax_year} is not a tax year"
                )));
            }
        }
        Event::DepreciationOverrideCleared { asset_id, .. } => {
            validate_non_empty(asset_id, "asset_id")?;
        }
        Event::DepreciationBasisAdjusted {
            adjustment_id,
            asset_id,
            effective_year,
            amount_cents,
            note,
        } => {
            validate_non_empty(adjustment_id, "adjustment_id")?;
            validate_non_empty(asset_id, "asset_id")?;
            // What moved the basis is the point: it is what the Form 4562
            // statement prints beside the adjusted figure.
            validate_non_empty(note, "note")?;
            if *amount_cents == 0 {
                return Err(ValidationError::InvalidValue(
                    "amount_cents: an adjustment of nothing changes no basis".to_string(),
                ));
            }
            if !(1900..=2200).contains(effective_year) {
                return Err(ValidationError::InvalidValue(format!(
                    "effective_year: {effective_year} is not a tax year"
                )));
            }
        }
        Event::DepreciationBasisAdjustmentRemoved { adjustment_id, .. } => {
            validate_non_empty(adjustment_id, "adjustment_id")?;
        }
        // --- the taxable-brokerage register (migration 047) ---
        //
        // Shape only, here as everywhere in this function: whether a sale can
        // consume a lot is a question about ledger state, and is answered under
        // the write lock in `investment_commands`. What this catches is an event
        // that is nonsense on its own terms — a holding of no shares, a cost of
        // nothing — and would sit in the log forever if it landed.
        Event::SecurityDefined(d) => {
            validate_non_empty(&d.security_id, "security_id")?;
            validate_non_empty(&d.ticker, "ticker")?;
            validate_non_empty(&d.name, "name")?;
            validate_non_empty(&d.kind, "kind")?;
            validate_currency_code(&d.currency)?;
        }
        Event::SecurityBought {
            lot_id,
            security_id,
            securities_account_id,
            cash_account_id,
            quantity,
            total_cost_cents,
            trade_date: _,
        } => {
            validate_non_empty(lot_id, "lot_id")?;
            validate_non_empty(security_id, "security_id")?;
            validate_non_empty(securities_account_id, "securities_account_id")?;
            validate_non_empty(cash_account_id, "cash_account_id")?;
            if *quantity <= 0 {
                return Err(ValidationError::InvalidValue(
                    "a purchase of no shares is not a purchase".to_string(),
                ));
            }
            if *total_cost_cents <= 0 {
                return Err(ValidationError::InvalidValue(
                    "a lot costs something; a free lot comes from a corporate action, which is \
                     out of scope"
                        .to_string(),
                ));
            }
        }
        Event::SecuritySold(d) => {
            validate_non_empty(&d.sale_id, "sale_id")?;
            validate_non_empty(&d.security_id, "security_id")?;
            validate_non_empty(&d.securities_account_id, "securities_account_id")?;
            validate_non_empty(&d.cash_account_id, "cash_account_id")?;
            if d.quantity <= 0 {
                return Err(ValidationError::InvalidValue(
                    "a sale of no shares is not a sale".to_string(),
                ));
            }
            if d.proceeds_cents < 0 || d.fee_cents < 0 {
                return Err(ValidationError::InvalidValue(
                    "proceeds and fees are amounts, not directions".to_string(),
                ));
            }
            if d.lots.is_empty() {
                return Err(ValidationError::InvalidValue(
                    "a sale with no lots behind it has no basis, and a gain computed against no \
                     basis is the whole proceeds"
                        .to_string(),
                ));
            }
            // The event has to add up against itself, because it is what a filed
            // gain will be checked against years from now and nothing else will
            // be left to check it with.
            let mut seen = std::collections::HashSet::new();
            let mut quantity = 0i64;
            let mut basis = 0i64;
            for lot in &d.lots {
                validate_non_empty(&lot.lot_id, "lot_id")?;
                if !seen.insert(&lot.lot_id) {
                    return Err(ValidationError::DuplicateId(lot.lot_id.clone()));
                }
                if lot.quantity <= 0 {
                    return Err(ValidationError::InvalidValue(format!(
                        "lot {} contributes no shares to the sale",
                        lot.lot_id
                    )));
                }
                if lot.basis_cents < 0 {
                    return Err(ValidationError::InvalidValue(format!(
                        "lot {} contributes a negative basis",
                        lot.lot_id
                    )));
                }
                quantity += lot.quantity;
                basis += lot.basis_cents;
            }
            if quantity != d.quantity {
                return Err(ValidationError::InvalidValue(format!(
                    "the lots account for {quantity} micro-shares and the sale is of {}",
                    d.quantity
                )));
            }
            if d.realized_gain_cents != d.proceeds_cents - d.fee_cents - basis {
                return Err(ValidationError::InvalidValue(format!(
                    "a realized gain of {} does not follow from proceeds {} less fees {} less \
                     basis {basis}",
                    d.realized_gain_cents, d.proceeds_cents, d.fee_cents
                )));
            }
        }
        Event::InvestmentIncomeReceived {
            kind: _,
            security_id: _,
            cash_account_id,
            amount_cents,
            received_on: _,
        } => {
            validate_non_empty(cash_account_id, "cash_account_id")?;
            if *amount_cents <= 0 {
                return Err(ValidationError::InvalidValue(
                    "income of nothing is not income".to_string(),
                ));
            }
        }
        Event::InvestmentFeeCharged {
            cash_account_id,
            expense_account_id,
            amount_cents,
            charged_on: _,
            security_id: _,
        } => {
            validate_non_empty(cash_account_id, "cash_account_id")?;
            validate_non_empty(expense_account_id, "expense_account_id")?;
            if *amount_cents <= 0 {
                return Err(ValidationError::InvalidValue(
                    "a fee of nothing is not a fee".to_string(),
                ));
            }
        }
        // --- the sheltered-account register (migration 048) ---
        //
        // Shape only, as everywhere in this function. Whether the value series
        // runs forwards and whether the account is on the register are questions
        // about ledger state, answered under the write lock in
        // `retirement_commands`.
        Event::RetirementAccountRegistered {
            account_id,
            institution,
            kind: _,
            value_change_account_id,
        } => {
            validate_non_empty(account_id, "account_id")?;
            validate_non_empty(institution, "institution")?;
            validate_non_empty(value_change_account_id, "value_change_account_id")?;
            // The retirement account and the value-change account being the same
            // account would make every value update a posting to itself: an entry
            // of a debit and an equal credit to one account, which balances, posts
            // nothing, and leaves the balance sheet silently short of the whole
            // account's growth.
            if account_id == value_change_account_id {
                return Err(ValidationError::InvalidValue(
                    "the retirement account and its value-change account cannot be the same \
                     account: every value update would post to itself and change nothing"
                        .to_string(),
                ));
            }
        }
        Event::RetirementValueSet {
            account_id,
            as_of: _,
            value_cents,
        } => {
            validate_non_empty(account_id, "account_id")?;
            // Zero is allowed — an account really can be emptied — but negative is
            // not: nothing is worth less than nothing, and a negative value here
            // posts a loss that never happened.
            if *value_cents < 0 {
                return Err(ValidationError::InvalidValue(format!(
                    "a retirement account cannot be worth {value_cents} cents"
                )));
            }
        }
        Event::RetirementContributionRecorded {
            account_id,
            funding_account_id,
            amount_cents,
            on: _,
        } => {
            validate_non_empty(account_id, "account_id")?;
            validate_non_empty(funding_account_id, "funding_account_id")?;
            if *amount_cents <= 0 {
                return Err(ValidationError::InvalidValue(
                    "a contribution of nothing is not a contribution; money coming back out is a \
                     distribution"
                        .to_string(),
                ));
            }
            if account_id == funding_account_id {
                return Err(ValidationError::InvalidValue(
                    "a contribution from an account to itself moves no money".to_string(),
                ));
            }
        }
        Event::RetirementDistributionRecorded(d) => {
            validate_non_empty(&d.account_id, "account_id")?;
            validate_non_empty(&d.receiving_account_id, "receiving_account_id")?;
            validate_non_empty(&d.withheld_account_id, "withheld_account_id")?;
            if d.gross_cents <= 0 {
                return Err(ValidationError::InvalidValue(
                    "a distribution of nothing is not a distribution".to_string(),
                ));
            }
            if d.withheld_cents < 0 {
                return Err(ValidationError::InvalidValue(
                    "withholding is an amount, not a direction".to_string(),
                ));
            }
            // Withholding comes out of the distribution, so it cannot exceed it.
            // Equal is legitimate — a distribution taken entirely to cover tax,
            // which happens with a Roth conversion — and leaves the receiving
            // account with a zero line, which is why the entry is built from
            // signed lines rather than from a net that might be zero.
            if d.withheld_cents > d.gross_cents {
                return Err(ValidationError::InvalidValue(format!(
                    "{} cents withheld out of a distribution of {} cents",
                    d.withheld_cents, d.gross_cents
                )));
            }
            // Box 2a is a part of box 1. More than the gross would be income the
            // owner never received; less is ordinary (a Roth, or after-tax basis).
            if d.taxable_cents < 0 || d.taxable_cents > d.gross_cents {
                return Err(ValidationError::InvalidValue(format!(
                    "a taxable amount of {} cents does not fit inside a distribution of {} cents",
                    d.taxable_cents, d.gross_cents
                )));
            }
        }
        // --- the investments importer (migration 050) ---
        //
        // Shape only, as everywhere in this function. Whether the dividend account
        // is really an income account, and whether the retirement account is on the
        // register, are questions about ledger state and are answered under the
        // write lock in `investment_import`.
        Event::InvestmentAccountConfigured(d) => {
            validate_non_empty(&d.item_id, "item_id")?;
            validate_non_empty(&d.plaid_account_id, "plaid_account_id")?;
            match &d.accounts {
                InvestmentPostingAccounts::Taxable(a) => {
                    validate_non_empty(&a.stocks_account_id, "securities_account_id")?;
                    validate_non_empty(&a.cash_account_id, "cash_account_id")?;
                    validate_non_empty(
                        &a.dividend_income_account_id,
                        "dividend_income_account_id",
                    )?;
                    validate_non_empty(
                        &a.interest_income_account_id,
                        "interest_income_account_id",
                    )?;
                    validate_non_empty(&a.realized_gain_account_id, "realized_gain_account_id")?;
                    validate_non_empty(&a.fee_expense_account_id, "fee_expense_account_id")?;
                    // Every optional slot, by the name it is configured under. An
                    // empty string in one of them is not "not configured": it would
                    // reach a posting as an account id nothing matches, and the
                    // entry would be refused somewhere far from the mistake.
                    for (value, field) in [
                        (&a.mutual_funds_account_id, "mutual_funds_account_id"),
                        (
                            &a.other_securities_account_id,
                            "other_securities_account_id",
                        ),
                        (
                            &a.tax_exempt_interest_account_id,
                            "tax_exempt_interest_account_id",
                        ),
                        (
                            &a.capital_gain_distribution_account_id,
                            "capital_gain_distribution_account_id",
                        ),
                        (
                            &a.transfer_clearing_account_id,
                            "transfer_clearing_account_id",
                        ),
                    ] {
                        if let Some(id) = value {
                            validate_non_empty(id, field)?;
                        }
                    }
                    // Securities at cost and the sweep cash being one account would
                    // make every purchase an entry to itself: a debit and an equal
                    // credit to one account, which balances, posts nothing, and
                    // leaves the balance sheet silently missing the whole holding.
                    // The same mistake phase 2 refuses for a retirement account and
                    // its value-change account.
                    //
                    // Checked for all three securities slots, not just the stocks
                    // one: the mistake is as available on a slot added later, and
                    // the consequence is identical.
                    for group in crate::events::types::SecurityKindGroup::ALL {
                        if a.securities_account_of(group) == a.cash_account_id {
                            return Err(ValidationError::InvalidValue(format!(
                                "the {} securities account and the cash account cannot be the \
                                 same account: every purchase would post to itself and change \
                                 nothing",
                                group.label().to_lowercase()
                            )));
                        }
                    }
                }
                InvestmentPostingAccounts::Sheltered {
                    retirement_account_id,
                } => {
                    validate_non_empty(retirement_account_id, "retirement_account_id")?;
                }
            }
        }
        Event::PlaidSecurityLinked(d) => {
            validate_non_empty(&d.plaid_security_id, "plaid_security_id")?;
            validate_non_empty(&d.security_id, "security_id")?;
        }
        Event::InvestmentActivityImported(d) => {
            validate_non_empty(&d.provider_transaction_id, "provider_transaction_id")?;
            validate_non_empty(&d.item_id, "item_id")?;
            validate_non_empty(&d.plaid_account_id, "plaid_account_id")?;
            // Every outcome recorded here posted an entry. The things that post
            // nothing — a sheltered account's trades, anything held for review —
            // are deliberately absent from this register, so an import record with
            // no entry would mean the two had fallen out of step.
            validate_non_empty(&d.entry_id, "entry_id")?;
            match d.outcome {
                ImportedActivityKind::Buy if d.lot_id.is_none() => {
                    return Err(ValidationError::InvalidValue(
                        "an imported purchase without a lot id: the lot is where its basis \
                         lives, and a purchase that created none relieved nothing when sold"
                            .to_string(),
                    ))
                }
                ImportedActivityKind::Sell if d.sale_id.is_none() => {
                    return Err(ValidationError::InvalidValue(
                        "an imported sale without a sale id: the sale is what carries the lots \
                         it consumed, which is what a Form 8949 row is built from"
                            .to_string(),
                    ))
                }
                _ => {}
            }
        }
        Event::DocumentAttached(d) => {
            validate_non_empty(&d.document_id, "document_id")?;
            // The digest is the key the bytes are fetched by and checked against, so one
            // in any other shape names a file nobody can ever find.
            if !crate::documents::is_sha256_hex(&d.sha256) {
                return Err(ValidationError::InvalidValue(format!(
                    "sha256: {:?} is not a lowercase hex SHA-256 digest",
                    d.sha256
                )));
            }
            if d.size_bytes == 0 || d.size_bytes > crate::documents::MAX_DOCUMENT_BYTES {
                return Err(ValidationError::InvalidValue(format!(
                    "size_bytes: {} is not between 1 and {}",
                    d.size_bytes,
                    crate::documents::MAX_DOCUMENT_BYTES
                )));
            }
            validate_non_empty(&d.media_type, "media_type")?;
            validate_non_empty(&d.filename, "filename")?;
            // A name, not a path. The stored file is named by its digest, so nothing
            // here steers where anything is written — but a name with a separator in it
            // reads as a path everywhere it is shown, and that is its own problem.
            if d.filename.contains(['/', '\\']) {
                return Err(ValidationError::InvalidValue(
                    "filename: a document's name, not a path".to_string(),
                ));
            }
            if let Some(year) = d.tax_year {
                if !(1900..=2200).contains(&year) {
                    return Err(ValidationError::InvalidValue(format!(
                        "{year} is not a tax year"
                    )));
                }
            }
            // Free text, kept short: the personal-tax work holds this to a form that
            // version knows, and until it lands a code nothing recognises is a label
            // rather than a figure anything computes from.
            if let Some(form) = &d.form {
                validate_non_empty(form, "form")?;
                if form.chars().count() > 32 {
                    return Err(ValidationError::InvalidValue(
                        "form: a form's code, not a description".to_string(),
                    ));
                }
            }
            if let Some(subject) = &d.subject {
                let (kind, id) = subject.as_columns();
                validate_non_empty(id, kind)?;
            }
            validate_document_kind(d.kind.as_deref(), &d.parts)?;
        }
        Event::DocumentRemoved { document_id } => {
            validate_non_empty(document_id, "document_id")?;
        }
        Event::DocumentClassified {
            document_id,
            kind,
            parts,
        } => {
            validate_non_empty(document_id, "document_id")?;
            validate_document_kind(kind.as_deref(), parts)?;
        }
        Event::StateTaxStatementRecorded(d) => {
            validate_non_empty(&d.statement_id, "statement_id")?;
            validate_non_empty(&d.issuer, "issuer")?;
            validate_non_empty(&d.form, "form")?;
            validate_tax_year(d.tax_year)?;
            if !is_state_code(&d.state) {
                return Err(ValidationError::InvalidValue(format!(
                    "state: {:?} is not a two-letter postal code",
                    d.state
                )));
            }
            // Codes from the shared vocabulary only: a code nothing reads is a figure
            // that silently reaches no return.
            if let Some(code) = d
                .amounts
                .keys()
                .find(|c| !crate::tax::k1_extract::state_codes::ALL.contains(&c.as_str()))
            {
                return Err(ValidationError::InvalidValue(format!(
                    "a state K-1 has no figure {code:?}"
                )));
            }
            if let Some(ppm) = d.apportionment_ppm {
                if !(0..=1_000_000).contains(&ppm) {
                    return Err(ValidationError::InvalidValue(format!(
                        "apportionment_ppm: {ppm} is not between 0 and 1,000,000"
                    )));
                }
            }
        }
        Event::StateTaxStatementRemoved { statement_id } => {
            validate_non_empty(statement_id, "statement_id")?;
        }
        Event::InvestmentImportsForgotten(d) => {
            validate_non_empty(&d.item_id, "item_id")?;
            validate_non_empty(&d.plaid_account_id, "plaid_account_id")?;
            // A reason, because this is the one operation that takes imported
            // activity out of the books wholesale, and the log is the only place
            // anybody will ever read why.
            validate_non_empty(&d.reason, "reason")?;
            // Forgetting nothing is not a thing that happened. An event that names
            // no transaction lifts no fence, and in the log it reads as if an import
            // had been undone.
            if d.provider_transaction_ids.is_empty() {
                return Err(ValidationError::InvalidValue(
                    "forgetting no transactions at all: the event names what its fence is \
                     lifted for, and an empty list lifts nothing"
                        .to_string(),
                ));
            }
            let mut seen = std::collections::HashSet::new();
            for id in &d.provider_transaction_ids {
                validate_non_empty(id, "provider_transaction_id")?;
                if !seen.insert(id.as_str()) {
                    return Err(ValidationError::DuplicateId(id.clone()));
                }
            }
        }
        Event::HoldingsSnapshotRecorded(d) => {
            validate_non_empty(&d.snapshot_id, "snapshot_id")?;
            validate_non_empty(&d.item_id, "item_id")?;
            validate_non_empty(&d.plaid_account_id, "plaid_account_id")?;
            let mut seen = std::collections::HashSet::new();
            for holding in &d.holdings {
                validate_non_empty(&holding.plaid_security_id, "plaid_security_id")?;
                // One security twice in one snapshot would double a position in the
                // reconciliation, and the reconciliation is the safeguard the whole
                // phase rests on (spec §7).
                if !seen.insert(holding.plaid_security_id.as_str()) {
                    return Err(ValidationError::DuplicateId(
                        holding.plaid_security_id.clone(),
                    ));
                }
                // A negative quantity is a short position, which is out of scope for
                // v1 (spec §10), and reads as a negative holding everywhere it is
                // summed. Zero is ordinary: brokers report closed positions.
                if holding.quantity < 0 {
                    return Err(ValidationError::InvalidValue(format!(
                        "a holding of {} micro-shares: a short position is out of scope",
                        holding.quantity
                    )));
                }
            }
        }
        // --- sole proprietorships (migration 031) ---
        Event::BusinessTypeSet { business_type } => {
            // Checked against the catalogue rather than for emptiness: an
            // unrecognised type reads back as the default, so the books would
            // quietly go on filing Form 1065 while the screen said otherwise.
            if crate::domain::BusinessType::parse(business_type).is_none() {
                return Err(ValidationError::InvalidValue(format!(
                    "{business_type} is not a business type"
                )));
            }
        }
        Event::SoleProprietorSet(d) => {
            validate_non_empty(&d.name, "the proprietor's name")?;
            if crate::domain::AccountingMethod::parse(&d.accounting_method).is_none() {
                return Err(ValidationError::InvalidValue(format!(
                    "{} is not an accounting method",
                    d.accounting_method
                )));
            }
            // Line F(3) makes you name the method. A form ticking "Other" with
            // nothing beside it is incomplete on its face.
            if d.accounting_method == crate::domain::AccountingMethod::Other.as_str()
                && d.accounting_method_other
                    .as_deref()
                    .is_none_or(|o| o.trim().is_empty())
            {
                return Err(ValidationError::InvalidValue(
                    "Schedule C line F(3) asks which other accounting method — name it".to_string(),
                ));
            }
        }
        Event::ScheduleCAnswerSet {
            tax_year,
            answer_key,
            value,
        } => {
            validate_tax_year(*tax_year)?;
            validate_non_empty(answer_key, "answer_key")?;
            validate_non_empty(value, "value")?;
        }
        Event::ScheduleCAnswerCleared {
            tax_year,
            answer_key,
        } => {
            validate_tax_year(*tax_year)?;
            validate_non_empty(answer_key, "answer_key")?;
        }
        Event::ScheduleCInputsSet(i) => {
            validate_tax_year(i.tax_year)?;
            // Line 30 and a carryover are amounts claimed, never negative; other
            // business income may be a loss.
            for (v, field) in [
                (i.home_office_cents, "home_office_cents"),
                (i.section_179_carryover_cents, "section_179_carryover_cents"),
            ] {
                if v.is_some_and(|c| c < 0) {
                    return Err(ValidationError::InvalidValue(format!(
                        "{field}: cannot be negative"
                    )));
                }
            }
        }
        Event::ScheduleCSourceLinked {
            link_id,
            ledger_id,
            ledger_name,
            ..
        } => {
            validate_non_empty(ledger_id, "ledger_id")?;
            validate_non_empty(ledger_name, "ledger_name")?;
            if link_id != ledger_id {
                return Err(ValidationError::InvalidValue(
                    "link_id: must be the business's ledger id, so linking twice is one link"
                        .to_string(),
                ));
            }
        }
        Event::ScheduleCSourceUnlinked { link_id } => {
            validate_non_empty(link_id, "link_id")?;
        }
        Event::TaxLineMappingSet {
            account_id,
            line_key,
            form,
            ..
        } => {
            validate_non_empty(account_id, "account_id")?;
            validate_mapping_form(form.as_deref(), Some(line_key))?;
            // Checked against the catalogue, not merely for emptiness: a key
            // nothing recognises is an account whose balance silently reaches no
            // line on the return, which is the failure `tax::lines` exists to
            // prevent. Refusing it here keeps it out of the log, where it would
            // replay onto every member's machine.
            // Both catalogues, because one mapping table serves both returns
            // — see `tax::any_line_def`.
            //
            // `OFF_RETURN` is the exception, and it belongs here rather than in
            // either catalogue: it is not a line, it is the statement that this
            // account is deliberately on none. Every reader already honours it
            // (`sum_by_line`, `schedule_l`, `il1065`) and until now nothing could
            // write it, so "deliberately off the return" was a state the code
            // could read and no command could reach. Phase 2 of
            // INVESTMENTS-SPEC.md is the first writer: registering a sheltered
            // account puts its value-change account off the return explicitly,
            // which is louder than an absence and — unlike an absence — draws no
            // "this account has a balance and no line" warning every year.
            if line_key != crate::tax::lines::OFF_RETURN
                && crate::tax::any_line_def(line_key).is_none()
            {
                return Err(ValidationError::InvalidValue(format!(
                    "no Form 1065 or Schedule C line has key {line_key:?}"
                )));
            }
        }
        Event::TaxLineMappingCleared {
            account_id, form, ..
        } => {
            validate_non_empty(account_id, "account_id")?;
            validate_mapping_form(form.as_deref(), None)?;
        }
        Event::TaxDeductionLimitSet {
            account_id,
            deductible_pct,
            ..
        } => {
            validate_non_empty(account_id, "account_id")?;
            // A percentage outside 0-100 is not a limit, it is arithmetic that
            // would invent or destroy a deduction. Refused at the door rather
            // than clamped, because a clamp hides the mistake that made it.
            if *deductible_pct > 100 {
                return Err(ValidationError::InvalidValue(format!(
                    "deductible_pct: {deductible_pct} is not a percentage between 0 and 100"
                )));
            }
        }
        Event::TaxDeductionLimitCleared { account_id, .. } => {
            validate_non_empty(account_id, "account_id")?;
        }
        Event::TaxStatementGroupingSet { account_id, .. } => {
            validate_non_empty(account_id, "account_id")?;
        }
        Event::IllinoisTaxAddbackSet { account_id, .. } => {
            validate_non_empty(account_id, "account_id")?;
        }
        Event::TaxStatementRecorded(s) => {
            validate_non_empty(&s.statement_id, "statement_id")?;
            validate_tax_year(s.tax_year)?;
            let form = validate_form(&s.form)?;
            validate_non_empty(&s.issuer, "issuer")?;
            for code in s.amounts.keys() {
                if !form.accepts_box(code) {
                    return Err(ValidationError::InvalidValue(format!(
                        "{} has no box {code:?}",
                        form.label()
                    )));
                }
            }
            for id in &s.document_ids {
                validate_non_empty(id, "document_ids")?;
            }
            if let crate::events::types::StatementSourceData::Ledger {
                ledger_id,
                partner_id,
                through_event,
                event_hash,
                ..
            } = &s.source
            {
                validate_non_empty(ledger_id, "source.ledger_id")?;
                validate_non_empty(partner_id, "source.partner_id")?;
                // Provenance that pins nothing cannot later say whether the
                // source books have moved on.
                if *through_event < 1 {
                    return Err(ValidationError::InvalidValue(format!(
                        "source.through_event: {through_event} is not an event"
                    )));
                }
                let hex = !event_hash.is_empty()
                    && event_hash.len() % 2 == 0
                    && event_hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
                if !hex {
                    return Err(ValidationError::InvalidValue(
                        "source.event_hash: not a lowercase hex digest".to_string(),
                    ));
                }
            }
        }
        Event::TaxStatementRemoved { statement_id } => {
            validate_non_empty(statement_id, "statement_id")?;
        }
        Event::PersonalTaxProfileSet(p) => {
            validate_tax_year(p.tax_year)?;
            for (field, cents) in [
                (
                    "federal_estimated_payments_cents",
                    p.federal_estimated_payments_cents,
                ),
                (
                    "state_estimated_payments_cents",
                    p.state_estimated_payments_cents,
                ),
                (
                    "short_term_loss_carryover_cents",
                    p.short_term_loss_carryover_cents,
                ),
                (
                    "long_term_loss_carryover_cents",
                    p.long_term_loss_carryover_cents,
                ),
            ] {
                if cents < 0 {
                    return Err(ValidationError::InvalidValue(format!(
                        "{field}: cannot be negative"
                    )));
                }
            }
            // A household, not a headcount typo: past this, the number is a mistake.
            if p.qualifying_children > 20 || p.other_dependents > 20 {
                return Err(ValidationError::InvalidValue(
                    "dependents: more than twenty is not a household this checks".to_string(),
                ));
            }
            if let Some(state) = &p.state {
                if state.len() != 2 || !state.chars().all(|c| c.is_ascii_uppercase()) {
                    return Err(ValidationError::InvalidValue(format!(
                        "state: {state:?} is not a two-letter code like IL"
                    )));
                }
            }
            // Age and sight are the spouse's only on a joint return; anywhere else
            // they would quietly raise the standard deduction for someone not on it.
            if !p.filing_status.has_spouse() && (p.spouse_65_or_older || p.spouse_blind) {
                return Err(ValidationError::InvalidValue(format!(
                    "a spouse's age or blindness counts only on a joint return, not {}",
                    p.filing_status.label()
                )));
            }
            for property in &p.rental_properties {
                validate_non_empty(&property.name, "rental_properties[].name")?;
                for id in property
                    .income_account_ids
                    .iter()
                    .chain(&property.expense_account_ids)
                {
                    validate_non_empty(id, "rental_properties[] account id")?;
                }
            }
            for id in p
                .extra_interest_account_ids
                .iter()
                .chain(&p.extra_dividend_account_ids)
            {
                validate_non_empty(id, "extra income account id")?;
            }
        }
        Event::TaxStatementLinesRecorded(l) => {
            validate_non_empty(&l.statement_id, "statement_id")?;
            let mut seen = std::collections::BTreeSet::new();
            for line in &l.lines {
                validate_non_empty(&line.line_id, "lines[].line_id")?;
                if !seen.insert(&line.line_id) {
                    return Err(ValidationError::InvalidValue(format!(
                        "lines[].line_id: {:?} twice — one row cannot be two rows",
                        line.line_id
                    )));
                }
                if crate::tax::schedule_d::Category::parse(&line.category).is_none() {
                    return Err(ValidationError::InvalidValue(format!(
                        "lines[].category: {:?} is not a Form 8949 category; they are a, b, c, \
                         d, e and f",
                        line.category
                    )));
                }
                validate_non_empty(&line.description, "lines[].description")?;
                if line.description.chars().count() > 200 {
                    return Err(ValidationError::InvalidValue(
                        "lines[].description: longer than 200 characters".to_string(),
                    ));
                }
                // Column (b) is one answer, and a row with neither is a row Form
                // 8949 cannot print: the date acquired decides the holding period,
                // and "VARIOUS" or "INHERITED" is what stands in its place when
                // there is no single date.
                match (&line.acquired_on, &line.acquired_label) {
                    (Some(_), None) => {}
                    (None, Some(label)) => {
                        validate_non_empty(label, "lines[].acquired_label")?;
                        if !label.bytes().all(|b| b.is_ascii_uppercase()) {
                            return Err(ValidationError::InvalidValue(format!(
                                "lines[].acquired_label: {label:?} — Form 8949 column (b) takes \
                                 a date or one of its own words, in capitals"
                            )));
                        }
                    }
                    (Some(_), Some(_)) => {
                        return Err(ValidationError::InvalidValue(
                            "lines[]: acquired on a date and acquired \"various\" are two \
                             answers to column (b)"
                                .to_string(),
                        ))
                    }
                    (None, None) => {
                        return Err(ValidationError::InvalidValue(
                            "lines[]: column (b) needs the date acquired, or the word that \
                             stands in its place"
                                .to_string(),
                        ))
                    }
                }
                if let Some(acquired) = line.acquired_on {
                    if acquired > line.sold_on {
                        return Err(ValidationError::InvalidValue(format!(
                            "lines[]: acquired {acquired} and sold {} — a holding period cannot \
                             run backwards",
                            line.sold_on
                        )));
                    }
                }
                if let Some(code) = &line.adjustment_code {
                    let shape = !code.is_empty()
                        && code.len() <= 4
                        && code.bytes().all(|b| b.is_ascii_uppercase());
                    if !shape {
                        return Err(ValidationError::InvalidValue(format!(
                            "lines[].adjustment_code: {code:?} — column (f) takes up to four of \
                             the form's own capital letters"
                        )));
                    }
                }
                // The reverse is allowed: code M with no amount is a real row.
                if line.adjustment_cents != 0 && line.adjustment_code.is_none() {
                    return Err(ValidationError::InvalidValue(
                        "lines[]: an amount in column (g) needs the letter in column (f) that \
                         says what the adjustment is"
                            .to_string(),
                    ));
                }
            }
        }
        Event::K1SourceLinked {
            link_id,
            ledger_id,
            ledger_name,
            partner_id,
            partner_name,
        } => {
            validate_non_empty(ledger_id, "ledger_id")?;
            validate_non_empty(ledger_name, "ledger_name")?;
            validate_non_empty(partner_id, "partner_id")?;
            validate_non_empty(partner_name, "partner_name")?;
            if *link_id != crate::domain::documents::K1Link::id_for(ledger_id, partner_id) {
                return Err(ValidationError::InvalidValue(
                    "link_id: must be <ledger_id>:<partner_id>, so linking twice is one link"
                        .to_string(),
                ));
            }
        }
        Event::K1SourceUnlinked { link_id } => {
            validate_non_empty(link_id, "link_id")?;
        }
        Event::AccountDeleted { account_id } => {
            validate_non_empty(account_id, "account_id")?;
        }
        Event::PartnerSharesChanged {
            partner_id,
            profit_ppm,
            loss_ppm,
            capital_ppm,
            ..
        } => {
            validate_non_empty(partner_id, "partner_id")?;
            // A percentage outside 0-100% is not a share of anything. Refused at
            // the door: a negative or >100% figure would foot on the form while
            // meaning nothing, which is the shape of error this file exists for.
            for (what, ppm) in [
                ("profit_ppm", profit_ppm),
                ("loss_ppm", loss_ppm),
                ("capital_ppm", capital_ppm),
            ] {
                if !(0..=1_000_000).contains(ppm) {
                    return Err(ValidationError::InvalidValue(format!(
                        "{what}: {ppm} is not a percentage between 0 and 100"
                    )));
                }
            }
        }
        Event::PartnerEquityAccountLinked {
            partner_id,
            account_id,
            role,
        } => {
            validate_non_empty(partner_id, "partner_id")?;
            validate_non_empty(account_id, "account_id")?;
            // Item L wants contributions and withdrawals on their own lines, so
            // an account has to say which it is. An unknown role would land in
            // neither and go missing from the capital account silently.
            if !matches!(role.as_str(), "contribution" | "draw") {
                return Err(ValidationError::InvalidValue(format!(
                    "role: {role:?} is neither \"contribution\" nor \"draw\""
                )));
            }
        }
        Event::PartnerEquityAccountUnlinked {
            partner_id,
            account_id,
        } => {
            validate_non_empty(partner_id, "partner_id")?;
            validate_non_empty(account_id, "account_id")?;
        }
        Event::PartnerAllocationFixed {
            tax_year,
            partner_id,
            note,
            ..
        } => {
            validate_non_empty(partner_id, "partner_id")?;
            // A split that departs from the percentages on file has to say why.
            validate_non_empty(note, "note")?;
            if !(1900..=2200).contains(tax_year) {
                return Err(ValidationError::InvalidValue(format!(
                    "tax_year: {tax_year} is not a tax year"
                )));
            }
        }
        Event::PartnerAllocationCleared { partner_id, .. } => {
            validate_non_empty(partner_id, "partner_id")?;
        }
        Event::LiabilityClassified {
            account_id,
            kind,
            partner_id,
            guaranteed,
            note,
        } => {
            validate_non_empty(account_id, "account_id")?;
            // A classification is a statement about loan documents.
            validate_non_empty(note, "note")?;
            match crate::domain::LiabilityKind::parse(kind) {
                None => {
                    return Err(ValidationError::InvalidValue(format!(
                        "kind: {kind:?} is not nonrecourse, qualified_nonrecourse or recourse"
                    )))
                }
                Some(k) if partner_id.is_some() && k != crate::domain::LiabilityKind::Recourse => {
                    return Err(ValidationError::InvalidValue(
                        "partner_id: only a recourse liability belongs to one partner".to_string(),
                    ))
                }
                _ => {}
            }
            if *guaranteed && partner_id.is_none() {
                return Err(ValidationError::InvalidValue(
                    "guaranteed: a guarantee needs the partner who gave it".to_string(),
                ));
            }
        }
        Event::LiabilityClassificationCleared { account_id } => {
            validate_non_empty(account_id, "account_id")?;
        }
        Event::ScheduleBAnswerSet {
            tax_year,
            answer_key,
            value,
        } => {
            validate_tax_year(*tax_year)?;
            validate_non_empty(value, "value")?;
            if !crate::tax::schedule_b::known_key(answer_key) {
                return Err(ValidationError::InvalidValue(format!(
                    "Schedule B has no answer keyed {answer_key:?}"
                )));
            }
        }
        Event::ScheduleBAnswerCleared {
            tax_year,
            answer_key,
        } => {
            validate_tax_year(*tax_year)?;
            if !crate::tax::schedule_b::known_key(answer_key) {
                return Err(ValidationError::InvalidValue(format!(
                    "Schedule B has no answer keyed {answer_key:?}"
                )));
            }
        }
        Event::UserAdded {
            user_id,
            username,
            role: _,
        } => {
            validate_non_empty(user_id, "user_id")?;
            validate_non_empty(username, "username")?;
        }
        Event::UserModified {
            user_id,
            field,
            old_value: _,
            new_value,
        } => {
            validate_non_empty(user_id, "user_id")?;
            validate_non_empty(field, "field")?;
            validate_non_empty(new_value, "new_value")?;
        }
        Event::UserRemoved { user_id } => {
            validate_non_empty(user_id, "user_id")?;
        }
        Event::AccountCreated {
            account_id,
            account_type: _,
            account_number,
            name,
            parent_id: _,
            currency,
            description: _,
        } => {
            validate_non_empty(account_id, "account_id")?;
            validate_account_number(account_number)?;
            validate_non_empty(name, "name")?;
            if let Some(curr) = currency {
                validate_currency_code(curr)?;
            }
        }
        Event::AccountUpdated {
            account_id,
            field,
            old_value: _,
            new_value,
        } => {
            validate_non_empty(account_id, "account_id")?;
            validate_non_empty(field, "field")?;
            validate_non_empty(new_value, "new_value")?;
        }
        Event::AccountDeactivated {
            account_id,
            reason: _,
        } => {
            validate_non_empty(account_id, "account_id")?;
        }
        Event::AccountReactivated { account_id } => {
            validate_non_empty(account_id, "account_id")?;
        }
        Event::JournalEntryPosted {
            entry_id,
            date: _,
            memo,
            lines,
            reference: _,
            source: _,
        } => {
            validate_non_empty(entry_id, "entry_id")?;
            validate_non_empty(memo, "memo")?;
            validate_journal_lines(lines)?;
        }
        Event::JournalEntryVoided { entry_id, reason } => {
            validate_non_empty(entry_id, "entry_id")?;
            validate_non_empty(reason, "reason")?;
        }
        Event::JournalEntryUnvoided { entry_id, reason } => {
            validate_non_empty(entry_id, "entry_id")?;
            validate_non_empty(reason, "reason")?;
        }
        Event::JournalEntryAnnotated {
            entry_id,
            annotation,
        } => {
            validate_non_empty(entry_id, "entry_id")?;
            validate_non_empty(annotation, "annotation")?;
        }
        Event::JournalLineReassigned {
            entry_id,
            line_id,
            old_account_id,
            new_account_id,
        } => {
            validate_non_empty(entry_id, "entry_id")?;
            validate_non_empty(line_id, "line_id")?;
            validate_non_empty(old_account_id, "old_account_id")?;
            validate_non_empty(new_account_id, "new_account_id")?;
        }
        Event::FiscalYearOpened {
            year: _,
            start_date: _,
            end_date: _,
        } => {
            // Dates are validated by chrono
        }
        Event::YearEndClosed {
            year: _,
            retained_earnings_entry_id,
        } => {
            validate_non_empty(retained_earnings_entry_id, "retained_earnings_entry_id")?;
        }
        Event::YearEndReopened {
            year: _,
            reason,
            reopened_by_user_id,
        } => {
            validate_non_empty(reason, "reason")?;
            validate_non_empty(reopened_by_user_id, "reopened_by_user_id")?;
        }
        Event::CurrencyEnabled {
            code,
            name,
            symbol: _,
            decimal_places: _,
        } => {
            validate_currency_code(code)?;
            validate_non_empty(name, "name")?;
        }
        Event::ExchangeRateRecorded {
            from_currency,
            to_currency,
            rate,
            effective_date: _,
        } => {
            validate_currency_code(from_currency)?;
            validate_currency_code(to_currency)?;
            if *rate <= rust_decimal::Decimal::ZERO {
                return Err(ValidationError::InvalidValue(
                    "Exchange rate must be positive".to_string(),
                ));
            }
        }
        Event::ReconciliationStarted {
            reconciliation_id,
            account_id,
            statement_date: _,
            statement_ending_balance: _,
        } => {
            validate_non_empty(reconciliation_id, "reconciliation_id")?;
            validate_non_empty(account_id, "account_id")?;
        }
        Event::TransactionCleared {
            reconciliation_id,
            entry_id,
            line_id,
            cleared_amount: _,
        } => {
            validate_non_empty(reconciliation_id, "reconciliation_id")?;
            validate_non_empty(entry_id, "entry_id")?;
            validate_non_empty(line_id, "line_id")?;
        }
        Event::TransactionUncleared {
            reconciliation_id,
            entry_id,
            line_id,
        } => {
            validate_non_empty(reconciliation_id, "reconciliation_id")?;
            validate_non_empty(entry_id, "entry_id")?;
            validate_non_empty(line_id, "line_id")?;
        }
        Event::ReconciliationCompleted {
            reconciliation_id,
            difference: _,
        } => {
            validate_non_empty(reconciliation_id, "reconciliation_id")?;
        }
        Event::ReconciliationAbandoned { reconciliation_id } => {
            validate_non_empty(reconciliation_id, "reconciliation_id")?;
        }
        Event::PlaidItemConnected {
            item_id,
            proxy_item_id,
            institution_name,
            plaid_accounts: _,
        } => {
            validate_non_empty(item_id, "item_id")?;
            // Present-but-empty is still a bug; absent is legitimate on hosted
            // books. See `Event::PlaidItemConnected::proxy_item_id`.
            if let Some(p) = proxy_item_id {
                validate_non_empty(p, "proxy_item_id")?;
            }
            validate_non_empty(institution_name, "institution_name")?;
        }
        Event::PlaidAccountsRefreshed {
            item_id,
            plaid_accounts,
        } => {
            validate_non_empty(item_id, "item_id")?;
            // A refresh that found nothing is a no-op somebody has to reason
            // about later; the caller knows it found nothing and should not
            // append. An empty list here is a bug in that caller, not a state.
            if plaid_accounts.is_empty() {
                return Err(ValidationError::EmptyField("plaid_accounts".to_string()));
            }
            for acct in plaid_accounts {
                validate_non_empty(&acct.plaid_account_id, "plaid_account_id")?;
            }
        }
        Event::PlaidItemDisconnected { item_id, reason } => {
            validate_non_empty(item_id, "item_id")?;
            validate_non_empty(reason, "reason")?;
        }
        Event::PlaidAccountMapped {
            item_id,
            plaid_account_id,
            local_account_id,
        } => {
            validate_non_empty(item_id, "item_id")?;
            validate_non_empty(plaid_account_id, "plaid_account_id")?;
            validate_non_empty(local_account_id, "local_account_id")?;
        }
        Event::PlaidAccountUnmapped {
            item_id,
            plaid_account_id,
            local_account_id,
        } => {
            validate_non_empty(item_id, "item_id")?;
            validate_non_empty(plaid_account_id, "plaid_account_id")?;
            validate_non_empty(local_account_id, "local_account_id")?;
        }
        Event::PlaidTransactionsSynced {
            item_id,
            sync_timestamp,
            ..
        } => {
            validate_non_empty(item_id, "item_id")?;
            validate_non_empty(sync_timestamp, "sync_timestamp")?;
        }
        Event::EventServiceRegistered {
            service_id,
            name,
            root_url,
            api_key,
        } => {
            validate_non_empty(service_id, "service_id")?;
            validate_non_empty(name, "name")?;
            validate_non_empty(root_url, "root_url")?;
            // Absent is legitimate — a service registered on hosted books keeps
            // its key on the instance. Present-but-blank is not: that is a caller
            // who meant to send one and sent nothing.
            if let Some(api_key) = api_key {
                validate_non_empty(api_key, "api_key")?;
            }
        }
        Event::EventServiceReportingChanged {
            service_id,
            frequency,
            ..
        } => {
            validate_non_empty(service_id, "service_id")?;
            // Parsed rather than accepted: an unrecognised frequency would leave
            // the projection holding a value nothing knows how to aggregate on,
            // and the failure would surface as sales quietly not posting.
            if crate::domain::ReportingFrequency::parse(frequency).is_none() {
                return Err(ValidationError::InvalidValue(format!(
                    "reporting frequency {frequency:?}"
                )));
            }
        }
        Event::EventServiceRemoved { service_id } => {
            validate_non_empty(service_id, "service_id")?;
        }
        Event::EventServiceSynced { service_id, .. } => {
            validate_non_empty(service_id, "service_id")?;
        }
        Event::BillReceived {
            bill_id,
            vendor,
            amount,
            currency,
            entry_id,
            ..
        } => {
            validate_non_empty(bill_id, "bill_id")?;
            validate_non_empty(vendor, "vendor")?;
            validate_non_empty(entry_id, "entry_id")?;
            validate_currency_code(currency)?;
            if *amount <= 0 {
                return Err(ValidationError::InvalidValue(
                    "Bill amount must be positive".to_string(),
                ));
            }
        }
        Event::BillPaymentApplied {
            bill_id,
            payment_entry_id,
            amount_applied,
        } => {
            validate_non_empty(bill_id, "bill_id")?;
            validate_non_empty(payment_entry_id, "payment_entry_id")?;
            if *amount_applied <= 0 {
                return Err(ValidationError::InvalidValue(
                    "Applied amount must be positive".to_string(),
                ));
            }
        }
        Event::BillVoided { bill_id, reason } => {
            validate_non_empty(bill_id, "bill_id")?;
            validate_non_empty(reason, "reason")?;
        }
        Event::InvoiceIssued {
            invoice_id,
            customer,
            amount,
            currency,
            entry_id,
            ..
        } => {
            validate_non_empty(invoice_id, "invoice_id")?;
            validate_non_empty(customer, "customer")?;
            validate_non_empty(entry_id, "entry_id")?;
            validate_currency_code(currency)?;
            if *amount <= 0 {
                return Err(ValidationError::InvalidValue(
                    "Invoice amount must be positive".to_string(),
                ));
            }
        }
        Event::InvoicePaymentReceived {
            invoice_id,
            payment_entry_id,
            amount_applied,
        } => {
            validate_non_empty(invoice_id, "invoice_id")?;
            validate_non_empty(payment_entry_id, "payment_entry_id")?;
            if *amount_applied <= 0 {
                return Err(ValidationError::InvalidValue(
                    "Applied amount must be positive".to_string(),
                ));
            }
        }
        Event::InvoiceVoided { invoice_id, reason } => {
            validate_non_empty(invoice_id, "invoice_id")?;
            validate_non_empty(reason, "reason")?;
        }
    }
    Ok(())
}

/// A document kind, if any, is a kind with a name; its parts are postal codes,
/// the only parts any kind has today.
fn validate_document_kind(kind: Option<&str>, parts: &[String]) -> Result<(), ValidationError> {
    if let Some(k) = kind {
        validate_non_empty(k, "kind")?;
    }
    if let Some(p) = parts.iter().find(|p| !is_state_code(p)) {
        return Err(ValidationError::InvalidValue(format!(
            "parts: {p:?} is not a two-letter postal code"
        )));
    }
    if kind.is_none() && !parts.is_empty() {
        return Err(ValidationError::InvalidValue(
            "parts without a kind: parts are what a kind of document carries".to_string(),
        ));
    }
    Ok(())
}

fn is_state_code(s: &str) -> bool {
    s.len() == 2 && s.bytes().all(|b| b.is_ascii_uppercase())
}

fn validate_non_empty(value: &str, field_name: &str) -> Result<(), ValidationError> {
    if value.trim().is_empty() {
        Err(ValidationError::EmptyField(field_name.to_string()))
    } else {
        Ok(())
    }
}

/// A mapping event's `form`, when it names one, has to be a return this
/// version knows — and a line key has to be a line of that return.
///
/// The second half is what keeps the two sets of assignments apart: a
/// Schedule C key filed under Form 1065 would sit in the 1065 set as a key no
/// 1065 line recognises, and its account would quietly reach no line.
/// [`crate::tax::lines::OFF_RETURN`] belongs to no catalogue and may be either.
fn validate_mapping_form(
    form: Option<&str>,
    line_key: Option<&str>,
) -> Result<(), ValidationError> {
    let Some(form) = form else { return Ok(()) };
    let Some(parsed) = crate::tax::ReturnForm::parse(form) else {
        return Err(ValidationError::InvalidValue(format!(
            "no return is called {form:?}"
        )));
    };
    if let Some(key) = line_key {
        if let Some(of_key) = crate::tax::ReturnForm::of_key(key) {
            if of_key != parsed {
                return Err(ValidationError::InvalidValue(format!(
                    "{key:?} is a {} line, not a {} one",
                    of_key.label(),
                    parsed.label()
                )));
            }
        }
    }
    Ok(())
}

fn validate_currency_code(code: &str) -> Result<(), ValidationError> {
    // ISO 4217 currency codes are 3 uppercase letters
    if code.len() != 3 || !code.chars().all(|c| c.is_ascii_uppercase()) {
        Err(ValidationError::InvalidCurrencyCode(code.to_string()))
    } else {
        Ok(())
    }
}

fn validate_account_number(number: &str) -> Result<(), ValidationError> {
    if number.trim().is_empty() {
        return Err(ValidationError::InvalidAccountNumber(
            "Account number cannot be empty".to_string(),
        ));
    }
    // Account numbers should be alphanumeric (allow dashes and dots for sub-accounts)
    if !number
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '.')
    {
        return Err(ValidationError::InvalidAccountNumber(format!(
            "Invalid characters in account number: {}",
            number
        )));
    }
    Ok(())
}

fn validate_fiscal_year_start(month: u32) -> Result<(), ValidationError> {
    if !(1..=12).contains(&month) {
        Err(ValidationError::InvalidFiscalYearStart(month))
    } else {
        Ok(())
    }
}

fn validate_journal_lines(lines: &[JournalLineData]) -> Result<(), ValidationError> {
    if lines.len() < 2 {
        return Err(ValidationError::InsufficientLines);
    }

    // Check that the entry is balanced
    let sum: i64 = lines.iter().map(|l| l.amount).sum();
    if sum != 0 {
        return Err(ValidationError::JournalEntryNotBalanced(sum));
    }

    // Validate each line
    for line in lines {
        validate_non_empty(&line.line_id, "line_id")?;
        validate_non_empty(&line.account_id, "account_id")?;
        validate_currency_code(&line.currency)?;
    }

    // Check for duplicate line IDs
    let mut seen_ids = std::collections::HashSet::new();
    for line in lines {
        if !seen_ids.insert(&line.line_id) {
            return Err(ValidationError::DuplicateId(line.line_id.clone()));
        }
    }

    Ok(())
}

/// A partner is general or limited; the K-1 offers no third box.
fn validate_partner_type(s: &str) -> Result<(), ValidationError> {
    crate::domain::PartnerType::parse(s)
        .map(|_| ())
        .ok_or_else(|| ValidationError::InvalidValue(format!("partner type {s:?}")))
}

fn validate_residency(s: &str) -> Result<(), ValidationError> {
    crate::domain::Residency::parse(s)
        .map(|_| ())
        .ok_or_else(|| ValidationError::InvalidValue(format!("residency {s:?}")))
}

/// A share is between nothing and the whole.
///
/// Whether a partnership's shares *sum* to the whole is not checked here: a
/// partnership passes through states where they do not — the moment after the
/// first partner is admitted, most obviously — and refusing those would make the
/// books unbuildable. The sum is checked when a return is generated, which is
/// the point at which it has to be true.
fn validate_shares(s: &crate::events::types::ShareData) -> Result<(), ValidationError> {
    // The bounds live on `Shares` and are not restated here: this is the check
    // guarding the log, and it is the one that must not drift from the domain's.
    let shares = crate::domain::Shares {
        profit_ppm: s.profit_ppm,
        loss_ppm: s.loss_ppm,
        capital_ppm: s.capital_ppm,
    };
    match shares.out_of_range() {
        Some((name, ppm)) => Err(ValidationError::InvalidValue(format!(
            "{name} share {ppm} ppm is outside 0..=100%"
        ))),
        None => Ok(()),
    }
}

fn validate_address(a: &crate::events::types::AddressData) -> Result<(), ValidationError> {
    validate_non_empty(&a.street, "street")?;
    validate_non_empty(&a.city, "city")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::types::{EventAccountType, JournalEntrySource};
    use chrono::NaiveDate;

    #[test]
    fn test_validate_company_created() {
        let event = Event::CompanyCreated {
            company_id: "test-id".to_string(),
            name: "Test Company".to_string(),
            base_currency: "USD".to_string(),
            fiscal_year_start: 1,
        };
        assert!(validate_event(&event).is_ok());

        let invalid = Event::CompanyCreated {
            company_id: "test-id".to_string(),
            name: "".to_string(),
            base_currency: "USD".to_string(),
            fiscal_year_start: 1,
        };
        assert!(validate_event(&invalid).is_err());

        let invalid_currency = Event::CompanyCreated {
            company_id: "test-id".to_string(),
            name: "Test".to_string(),
            base_currency: "usd".to_string(), // lowercase
            fiscal_year_start: 1,
        };
        assert!(validate_event(&invalid_currency).is_err());

        let invalid_month = Event::CompanyCreated {
            company_id: "test-id".to_string(),
            name: "Test".to_string(),
            base_currency: "USD".to_string(),
            fiscal_year_start: 13, // invalid
        };
        assert!(validate_event(&invalid_month).is_err());
    }

    #[test]
    fn test_validate_journal_entry() {
        let valid = Event::JournalEntryPosted {
            entry_id: "entry-001".to_string(),
            date: NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
            memo: "Test entry".to_string(),
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
            source: Some(JournalEntrySource::Manual),
        };
        assert!(validate_event(&valid).is_ok());

        // Unbalanced entry
        let unbalanced = Event::JournalEntryPosted {
            entry_id: "entry-001".to_string(),
            date: NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
            memo: "Test entry".to_string(),
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
                    amount: -5000, // Not balanced!
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
            ],
            reference: None,
            source: None,
        };
        assert!(matches!(
            validate_event(&unbalanced),
            Err(ValidationError::JournalEntryNotBalanced(_))
        ));

        // Single line entry
        let single_line = Event::JournalEntryPosted {
            entry_id: "entry-001".to_string(),
            date: NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
            memo: "Test entry".to_string(),
            lines: vec![JournalLineData {
                line_id: "line-001".to_string(),
                account_id: "expense".to_string(),
                amount: 0,
                currency: "USD".to_string(),
                exchange_rate: None,
                memo: None,
            }],
            reference: None,
            source: None,
        };
        assert!(matches!(
            validate_event(&single_line),
            Err(ValidationError::InsufficientLines)
        ));

        // Duplicate line IDs
        let duplicate = Event::JournalEntryPosted {
            entry_id: "entry-001".to_string(),
            date: NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
            memo: "Test entry".to_string(),
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
                    line_id: "line-001".to_string(), // Duplicate!
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
        assert!(matches!(
            validate_event(&duplicate),
            Err(ValidationError::DuplicateId(_))
        ));
    }

    #[test]
    fn test_validate_account_created() {
        let valid = Event::AccountCreated {
            account_id: "acc-001".to_string(),
            account_type: EventAccountType::Asset,
            account_number: "1000".to_string(),
            name: "Cash".to_string(),
            parent_id: None,
            currency: Some("USD".to_string()),
            description: None,
        };
        assert!(validate_event(&valid).is_ok());

        let invalid_number = Event::AccountCreated {
            account_id: "acc-001".to_string(),
            account_type: EventAccountType::Asset,
            account_number: "".to_string(),
            name: "Cash".to_string(),
            parent_id: None,
            currency: None,
            description: None,
        };
        assert!(validate_event(&invalid_number).is_err());
    }

    const DIGEST: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn attached(sha: &str, size: u64, filename: &str) -> Event {
        Event::DocumentAttached(Box::new(crate::events::types::DocumentAttachedData {
            document_id: "d1".to_string(),
            sha256: sha.to_string(),
            size_bytes: size,
            media_type: "application/pdf".to_string(),
            filename: filename.to_string(),
            title: None,
            tax_year: None,
            form: None,
            subject: None,
            kind: None,
            parts: Vec::new(),
        }))
    }

    /// The digest is the key the bytes are fetched by and checked against, so the shape
    /// is not cosmetic: a document whose digest is anything else names a file nobody can
    /// ever find, and the event is the thing that is replicated for ever.
    ///
    /// Checked here rather than only at the route, because validation is what runs
    /// whichever way an event arrives — a local command, a sync submit, a replay.
    #[test]
    fn a_document_whose_digest_is_not_a_digest_is_refused() {
        assert!(validate_event(&attached(DIGEST, 10, "s.pdf")).is_ok());
        assert!(validate_event(&attached("nope", 10, "s.pdf")).is_err());
        assert!(
            validate_event(&attached(&DIGEST.to_uppercase(), 10, "s.pdf")).is_err(),
            "two spellings of one digest would be two keys for one file"
        );
        assert!(
            validate_event(&attached(&DIGEST[..63], 10, "s.pdf")).is_err(),
            "a digest of the wrong length"
        );
    }

    /// The size bounds the blob store enforces, enforced on the event too: a record of a
    /// file that could not have been stored is a document nobody can open.
    #[test]
    fn a_document_of_an_impossible_size_is_refused() {
        assert!(validate_event(&attached(DIGEST, 0, "s.pdf")).is_err());
        assert!(validate_event(&attached(
            DIGEST,
            crate::documents::MAX_DOCUMENT_BYTES + 1,
            "s.pdf"
        ))
        .is_err());
        assert!(validate_event(&attached(
            DIGEST,
            crate::documents::MAX_DOCUMENT_BYTES,
            "s.pdf"
        ))
        .is_ok());
    }

    /// A name, not a path. The stored file is named by its digest so nothing is steered
    /// by this, but a name with a separator reads as a path everywhere it is shown.
    #[test]
    fn a_documents_filename_is_a_name_and_not_a_path() {
        assert!(validate_event(&attached(DIGEST, 10, "")).is_err());
        assert!(validate_event(&attached(DIGEST, 10, "/etc/passwd")).is_err());
        assert!(validate_event(&attached(DIGEST, 10, "..\\win.ini")).is_err());
    }
}
