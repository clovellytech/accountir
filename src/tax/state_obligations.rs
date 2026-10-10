//! Which states a year's K-1s may need a return for, in one list.
//!
//! A partnership with property in many states sends a state K-1 for each. Most
//! show a small loss or nothing; a few show income, and some show tax the
//! partnership already paid or withheld for the partner. This puts each state
//! next to what the books can say about it: for Maryland and Virginia, whose
//! returns are prepared here, the computed result; for the rest, what the K-1
//! shows and what that usually means. It is a starting point for the decision,
//! not the decision — each state's own threshold rules have the last word.

use rusqlite::Connection;

use crate::commands::k1_import_commands;
use crate::tax::form1040::Form1040;
use crate::tax::k1_extract::state_codes as sc;
use crate::tax::{md505, va763};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Tax is due: a return must be filed.
    FileTaxDue,
    /// Nothing is due but money is to be claimed back.
    FileForRefund,
    /// A return is required with nothing to pay or claim.
    File,
    /// Income is positive and the state's return is not prepared here.
    Check,
    /// A loss or nothing from the state, and nothing paid: generally no return.
    NoReturn,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::FileTaxDue => "File — tax due",
            Verdict::FileForRefund => "File to claim a refund",
            Verdict::File => "File",
            Verdict::Check => "Check the state's threshold",
            Verdict::NoReturn => "Generally no return",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateObligation {
    pub state: String,
    /// The state's income from the K-1s (source income, or the allocated share).
    pub income_cents: i64,
    /// Tax the partnerships paid or withheld for you there.
    pub paid_cents: i64,
    /// The computed tax, where the return is prepared here.
    pub tax_cents: Option<i64>,
    /// Positive a refund, negative due, where the return is prepared here.
    pub balance_cents: Option<i64>,
    pub verdict: Verdict,
    pub note: String,
}

/// The year's states, in postal-code order.
pub fn summarise(conn: &Connection, federal: &Form1040) -> Vec<StateObligation> {
    let year = federal.tax_year;
    let mut states: Vec<String> = k1_import_commands::list_state_statements(conn, Some(year))
        .into_iter()
        .map(|s| s.state)
        .collect();
    states.sort();
    states.dedup();
    states
        .into_iter()
        .map(|state| {
            let totals = k1_import_commands::state_totals(conn, year, &state);
            let get = |c: &str| totals.get(c).copied().unwrap_or(0);
            let income = get(sc::SOURCE_INCOME);
            let paid = get(sc::NONRESIDENT_TAX_PAID) + get(sc::WITHHOLDING) + get(sc::PTE_ELECTION_TAX);
            match state.as_str() {
                "MD" => maryland(conn, federal, income, paid),
                "VA" => virginia(conn, federal, income, paid),
                _ => other(state, income, paid),
            }
        })
        .collect()
}

fn from_balance(balance: i64, paid: i64) -> Verdict {
    if balance < 0 {
        Verdict::FileTaxDue
    } else if balance > 0 && paid > 0 {
        Verdict::FileForRefund
    } else {
        Verdict::File
    }
}

fn dollars(cents: i64) -> String {
    let d = md505::round_dollars(cents);
    let s = d.abs().to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    format!("{}${out}", if d < 0 { "-" } else { "" })
}

fn maryland(conn: &Connection, federal: &Form1040, income: i64, paid: i64) -> StateObligation {
    match md505::compute(conn, federal) {
        Ok(r) => {
            let verdict = if r.maryland_agi_cents <= 0 && paid == 0 {
                Verdict::NoReturn
            } else {
                from_balance(r.balance_cents, paid)
            };
            let note = match verdict {
                Verdict::FileTaxDue => format!(
                    "Form 505: tax {} less {} paid by the partnership leaves {} to pay.",
                    dollars(r.total_tax_cents),
                    dollars(r.payments_cents),
                    dollars(-r.balance_cents)
                ),
                Verdict::FileForRefund => format!(
                    "Form 505: tax {}; the partnership paid {} for you, so {} comes back. A \
                     nonresident with Maryland income must file once federal gross income \
                     reaches the federal filing level (2025 instructions, Table 1).",
                    dollars(r.total_tax_cents),
                    dollars(r.payments_cents),
                    dollars(r.balance_cents)
                ),
                _ => "Form 505: nothing due and nothing to claim back.".to_string(),
            };
            StateObligation {
                state: "MD".into(),
                income_cents: income,
                paid_cents: paid,
                tax_cents: Some(r.total_tax_cents),
                balance_cents: Some(r.balance_cents),
                verdict,
                note,
            }
        }
        Err(e) => StateObligation {
            state: "MD".into(),
            income_cents: income,
            paid_cents: paid,
            tax_cents: None,
            balance_cents: None,
            verdict: if income > 0 || paid > 0 { Verdict::Check } else { Verdict::NoReturn },
            note: format!("Form 505 could not be computed: {e}."),
        },
    }
}

fn virginia(conn: &Connection, federal: &Form1040, income: i64, paid: i64) -> StateObligation {
    match va763::compute(conn, federal) {
        Ok(r) => {
            let verdict = if !r.must_file {
                if paid > 0 {
                    Verdict::FileForRefund
                } else {
                    Verdict::NoReturn
                }
            } else {
                from_balance(r.balance_cents, paid)
            };
            let note = match verdict {
                Verdict::FileTaxDue => format!(
                    "Form 763: tax {} less {} withheld leaves {} to pay.",
                    dollars(r.tax_cents),
                    dollars(r.payments_cents),
                    dollars(-r.balance_cents)
                ),
                Verdict::FileForRefund => format!(
                    "Form 763: tax {}; {} was withheld for you, so {} comes back by filing.",
                    dollars(r.tax_cents),
                    dollars(r.payments_cents),
                    dollars(r.balance_cents)
                ),
                Verdict::NoReturn => {
                    "Virginia income is under the filing threshold and nothing was withheld."
                        .to_string()
                }
                _ => "Form 763: nothing due and nothing to claim back.".to_string(),
            };
            StateObligation {
                state: "VA".into(),
                income_cents: income,
                paid_cents: paid,
                tax_cents: Some(r.tax_cents),
                balance_cents: Some(r.balance_cents),
                verdict,
                note,
            }
        }
        Err(e) => StateObligation {
            state: "VA".into(),
            income_cents: income,
            paid_cents: paid,
            tax_cents: None,
            balance_cents: None,
            verdict: if income > 0 || paid > 0 { Verdict::Check } else { Verdict::NoReturn },
            note: format!("Form 763 could not be computed: {e}."),
        },
    }
}

fn other(state: String, income: i64, paid: i64) -> StateObligation {
    let (verdict, note) = if paid > 0 {
        (
            Verdict::Check,
            format!(
                "The partnership paid or withheld {} for you here; filing may get some of it \
                 back. This state's return is not prepared here.",
                dollars(paid)
            ),
        )
    } else if income > 0 {
        (
            Verdict::Check,
            format!(
                "{} of income from this state. Compare it with the state's nonresident filing \
                 threshold; its return is not prepared here.",
                dollars(income)
            ),
        )
    } else {
        (
            Verdict::NoReturn,
            if income < 0 {
                format!(
                    "A loss of {}. A nonresident with no income from the state generally has no \
                     return to file; some states let a loss carry forward only if a return \
                     reports it.",
                    dollars(-income)
                )
            } else {
                "No income from this state.".to_string()
            },
        )
    };
    StateObligation {
        state,
        income_cents: income,
        paid_cents: paid,
        tax_cents: None,
        balance_cents: None,
        verdict,
        note,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_state_not_prepared_here_is_judged_from_its_k1() {
        let loss = other("GA".into(), -36_300, 0);
        assert_eq!(loss.verdict, Verdict::NoReturn);
        assert!(loss.note.contains("$363"), "{}", loss.note);
        assert_eq!(other("SC".into(), 1_100, 0).verdict, Verdict::Check);
        assert_eq!(other("CA".into(), 0, 0).verdict, Verdict::NoReturn);
        assert_eq!(other("NJ".into(), -300, 5_000).verdict, Verdict::Check);
        assert_eq!(dollars(123_456_78), "$123,457");
    }
}
