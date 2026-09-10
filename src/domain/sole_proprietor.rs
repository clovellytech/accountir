//! Which return a set of books files, and who files it when there is only one
//! owner.
//!
//! # Why the business type is a setting rather than an inference
//!
//! A ledger with one owner and a ledger with four look identical from the
//! accounts. What differs is the return: a partnership files Form 1065 with a
//! Schedule K-1 per partner, and a sole proprietorship files Schedule C attached
//! to the owner's own Form 1040. The two forms want the same money on completely
//! different lines — a partnership's ordinary income is split across partners and
//! separately stated items, where a sole proprietor's is one net figure that
//! lands on their 1040 — so nothing in the books can pick between them. Somebody
//! has to say.
//!
//! Guessing from the number of partners would be worse than asking. A sole
//! proprietor who has entered no partners looks exactly like a partnership whose
//! partners have not been entered yet, and the return that comes out either way
//! looks finished.
//!
//! # Why the proprietor is not a partner
//!
//! It is tempting to model a sole proprietor as a partnership with one partner,
//! and it is wrong in a way that reaches the paper. A partner is an owner *of an
//! entity that files its own return*; a sole proprietor is an individual whose
//! business has no return of its own at all. There is no Schedule K-1, no capital
//! account on a Schedule L, and the identifying number on the form is the
//! owner's **SSN** rather than the business's EIN — the EIN is optional and
//! frequently absent. Modelling them as a partner would produce a K-1 for
//! somebody who must never receive one.
//!
//! # The SSN is not in the event log
//!
//! `sole_proprietor` carries the owner's name and nothing that identifies them to
//! a tax authority. The SSN goes to `sole_proprietor_tin`, a plain local table,
//! and never to an event — the same rule `partner_tins` follows in migration 023,
//! for the same reason: the log is replicated in full to every member's machine
//! and is append-only, so a number written there is on every other machine
//! permanently, with no way to take it back.
//!
//! For a sole proprietorship the argument is if anything stronger. A partnership
//! TIN is often an EIN; a sole proprietor's identifying number on Schedule C is
//! their own social security number, and the whole return is about one person.

use serde::{Deserialize, Serialize};

/// Which return these books file.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum BusinessType {
    /// Form 1065, with a Schedule K-1 for each partner.
    #[default]
    Partnership,
    /// Schedule C, attached to the owner's Form 1040.
    SoleProprietorship,
}

impl BusinessType {
    pub const ALL: [BusinessType; 2] =
        [BusinessType::Partnership, BusinessType::SoleProprietorship];

    /// The stable string the event log stores.
    pub fn as_str(self) -> &'static str {
        match self {
            BusinessType::Partnership => "partnership",
            BusinessType::SoleProprietorship => "sole_proprietorship",
        }
    }

    pub fn parse(s: &str) -> Option<BusinessType> {
        BusinessType::ALL.into_iter().find(|t| t.as_str() == s)
    }

    pub fn label(self) -> &'static str {
        match self {
            BusinessType::Partnership => "Partnership",
            BusinessType::SoleProprietorship => "Sole proprietorship",
        }
    }

    /// The return these books produce, for a screen that has to say so.
    pub fn form_name(self) -> &'static str {
        match self {
            BusinessType::Partnership => "Form 1065",
            BusinessType::SoleProprietorship => "Schedule C (Form 1040)",
        }
    }

    pub fn is_sole_proprietorship(self) -> bool {
        self == BusinessType::SoleProprietorship
    }
}

impl std::fmt::Display for BusinessType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Schedule C line F — the method the books are kept on.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum AccountingMethod {
    #[default]
    Cash,
    Accrual,
    /// Anything else, which the form makes you name.
    Other,
}

impl AccountingMethod {
    pub const ALL: [AccountingMethod; 3] = [
        AccountingMethod::Cash,
        AccountingMethod::Accrual,
        AccountingMethod::Other,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            AccountingMethod::Cash => "cash",
            AccountingMethod::Accrual => "accrual",
            AccountingMethod::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Option<AccountingMethod> {
        AccountingMethod::ALL.into_iter().find(|m| m.as_str() == s)
    }

    pub fn label(self) -> &'static str {
        match self {
            AccountingMethod::Cash => "Cash",
            AccountingMethod::Accrual => "Accrual",
            AccountingMethod::Other => "Other (specify)",
        }
    }
}

impl std::fmt::Display for AccountingMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// The individual who owns the business, as Schedule C's header asks for them.
///
/// Deliberately small. Everything the form wants about the *business* — its name,
/// address, EIN and business code — is already in
/// [`crate::domain::BusinessProfile`] and is read from there, because a sole
/// proprietorship's books describe one business exactly as a partnership's do. All
/// that is missing is the person, and that is this.
///
/// No identifying number here: see the module docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SoleProprietor {
    /// As it appears on the owner's Form 1040 — the two have to match, because
    /// Schedule C is attached to that return and the IRS pairs them by name and
    /// SSN.
    pub name: String,
    /// The method the books are kept on. Schedule C line F.
    pub accounting_method: AccountingMethod,
    /// What line F(3) makes you write when the method is neither cash nor
    /// accrual. Ignored otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accounting_method_other: Option<String>,
}

impl SoleProprietor {
    /// What line F(3) should print, if anything.
    ///
    /// Empty unless the method is `Other`, so switching back to cash cannot leave
    /// a stale description printed beside a ticked "Cash" box.
    pub fn method_description(&self) -> &str {
        match self.accounting_method {
            AccountingMethod::Other => self.accounting_method_other.as_deref().unwrap_or(""),
            _ => "",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_business_type_round_trips_through_its_stored_string() {
        for t in BusinessType::ALL {
            assert_eq!(BusinessType::parse(t.as_str()), Some(t), "{t:?}");
        }
        assert_eq!(BusinessType::parse("sole trader"), None);
    }

    #[test]
    fn every_accounting_method_round_trips_through_its_stored_string() {
        for m in AccountingMethod::ALL {
            assert_eq!(AccountingMethod::parse(m.as_str()), Some(m), "{m:?}");
        }
    }

    /// A book that has never been told what it is files the return the rest of
    /// this program was written for.
    #[test]
    fn the_default_is_a_partnership() {
        assert_eq!(BusinessType::default(), BusinessType::Partnership);
        assert_eq!(BusinessType::default().form_name(), "Form 1065");
    }

    /// Line F(3) is printed only when the box beside it is ticked, so switching
    /// back to cash cannot leave a description stranded on the form.
    #[test]
    fn the_method_description_is_only_printed_for_other() {
        let mut p = SoleProprietor {
            name: "Jinny Choi".into(),
            accounting_method: AccountingMethod::Other,
            accounting_method_other: Some("Hybrid".into()),
        };
        assert_eq!(p.method_description(), "Hybrid");

        p.accounting_method = AccountingMethod::Cash;
        assert_eq!(p.method_description(), "", "the text is stale now");
    }
}
