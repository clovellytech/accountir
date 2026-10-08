//! Section 199A: what each partner needs to figure their qualified business
//! income deduction.
//!
//! # What the deduction is
//!
//! A partner who is an individual, trust or estate can deduct up to 20% of their
//! share of the partnership's qualified business income. The deduction is taken
//! on the partner's own return, not the partnership's, so the partnership's job is
//! to report the pieces: the partner's share of QBI, of W-2 wages paid by the
//! business, and of the unadjusted basis immediately after acquisition (UBIA) of
//! its qualified property. Above the taxable-income threshold the deduction is
//! capped by those last two, which is why they have to be reported at all. The
//! K-1 carries them as box 20 code Z, with a statement.
//!
//! # How the pieces are split
//!
//! QBI follows each partner's ordinary business income (box 1) less their share
//! of §179, because that is what it is. W-2 wages and UBIA are apportioned in
//! proportion to box 1: a partner's share of W-2 wages follows their share of wage
//! expense, and their share of UBIA follows their share of depreciation, and both
//! expenses sit inside the ordinary income the partnership divided. A partnership
//! whose agreement allocates wage expense or depreciation separately from the
//! bottom line has to adjust the statements by hand, and the return says so.

use chrono::Datelike;
use lopdf::Document;

use super::acroform::FormError;
use super::depreciation::YearSchedule;
use super::lines::{cents_to_dollars, format_dollars, Form1065Lines};
use super::statement::{build_table, Column, TableLine, TableStatement};
use crate::domain::BusinessProfile;

/// The partnership's Section 199A figures for the year, in whole dollars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Totals {
    /// Ordinary business income less §179 — Schedule K line 1 less line 12.
    pub qbi: i64,
    /// Page 1 line 9: wages paid to employees, which is what Form W-3 reports.
    pub w2_wages: i64,
    /// UBIA of qualified property held at the end of the year.
    pub ubia: i64,
}

/// One partner's Section 199A figures, in whole dollars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Share {
    /// Their box 1.
    pub ordinary: i64,
    /// Their box 12.
    pub section_179: i64,
    pub w2_wages: i64,
    pub ubia: i64,
}

impl Share {
    /// Qualified business income: box 1 less §179.
    pub fn qbi(&self) -> i64 {
        self.ordinary - self.section_179
    }

    /// Whether there is anything to report.
    pub fn is_empty(&self) -> bool {
        self.qbi() == 0 && self.w2_wages == 0 && self.ubia == 0
    }
}

/// Total the year's Section 199A figures from the return and the asset register.
pub fn totals(lines: &Form1065Lines, schedule: &YearSchedule<'_>) -> Totals {
    Totals {
        qbi: lines.k_line_1() - lines.get("k12"),
        w2_wages: lines.get("l9"),
        ubia: cents_to_dollars(ubia_cents(schedule)),
    }
}

/// UBIA of the qualified property on the register at the end of the year.
///
/// Qualified property is tangible depreciable property used in the business,
/// still held at the close of the year, and still within its depreciable period —
/// which runs to the later of ten years after it was placed in service or the
/// last full year of its recovery period. UBIA is the basis when placed in
/// service, before depreciation, §179 or bonus; the register's adjusted cost is
/// used so a basis reduced by a reimbursing grant is reported reduced.
pub fn ubia_cents(schedule: &YearSchedule<'_>) -> i64 {
    let year = schedule.tax_year;
    schedule
        .rows
        .iter()
        .filter(|r| !r.disposed)
        .filter(|r| {
            let placed = r.asset.placed_in_service.year();
            let life = r.asset.class.recovery_years(r.asset.system).ceil() as i32;
            year <= (placed + 9).max(placed + life - 1)
        })
        .map(|r| r.adjusted_cost_cents.max(0))
        .sum()
}

/// Split the totals across the partners filing a K-1.
///
/// `ordinary` and `section_179` are each partner's box 1 and box 12, in the order
/// the K-1s are built. W-2 wages and UBIA are apportioned in proportion to box 1,
/// exactly — the shares always add back to the totals.
pub fn split(totals: Totals, ordinary: &[i64], section_179: &[i64]) -> Vec<Share> {
    let wages = apportion(totals.w2_wages, ordinary);
    let ubia = apportion(totals.ubia, ordinary);
    ordinary
        .iter()
        .enumerate()
        .map(|(i, o)| Share {
            ordinary: *o,
            section_179: section_179.get(i).copied().unwrap_or(0),
            w2_wages: wages[i],
            ubia: ubia[i],
        })
        .collect()
}

/// `amount` in proportion to `weights`, by largest remainder, so the parts sum to
/// `amount` exactly. Nothing is apportioned when the weights total zero.
fn apportion(amount: i64, weights: &[i64]) -> Vec<i64> {
    let total: i128 = weights.iter().map(|w| *w as i128).sum();
    if total == 0 || amount == 0 {
        return vec![0; weights.len()];
    }
    let mut out = Vec::with_capacity(weights.len());
    let mut remainders = Vec::with_capacity(weights.len());
    for (i, w) in weights.iter().enumerate() {
        let exact = amount as i128 * *w as i128;
        out.push((exact / total) as i64);
        remainders.push(((exact % total).abs(), i));
    }
    let mut left = amount - out.iter().sum::<i64>();
    remainders.sort_by(|a, b| b.0.cmp(&a.0));
    let step = left.signum();
    for (_, i) in remainders {
        if left == 0 {
            break;
        }
        out[i] += step;
        left -= step;
    }
    out
}

/// The box 20 code Z statement behind one partner's K-1, or `None` when they have
/// nothing to report.
pub fn statement(
    profile: &BusinessProfile,
    year: i32,
    partner: &str,
    share: &Share,
) -> Result<Option<Document>, FormError> {
    if share.is_empty() {
        return Ok(None);
    }
    let money = |d: i64| {
        if d < 0 {
            format!("({})", format_dollars(-d))
        } else {
            format_dollars(d)
        }
    };
    let row = |label: &str, value: String| TableLine::Cells(vec![label.to_string(), value]);
    let mut lines = vec![
        row(
            "Trade or business",
            format!("{} (EIN {})", profile.legal_name, profile.ein),
        ),
        row("Publicly traded partnership (PTP)", "No".to_string()),
        row("Aggregated with another trade or business", "No".to_string()),
        row(
            "Specified service trade or business (SSTB)",
            "No".to_string(),
        ),
        row(
            "Ordinary business income (loss) — box 1",
            money(share.ordinary),
        ),
    ];
    if share.section_179 != 0 {
        lines.push(row(
            "Less section 179 deduction — box 12",
            money(-share.section_179),
        ));
    }
    lines.extend([
        row("Qualified business income (loss)", money(share.qbi())),
        row("W-2 wages", money(share.w2_wages)),
        row(
            "Unadjusted basis immediately after acquisition (UBIA) of qualified property",
            money(share.ubia),
        ),
        row("Section 199A dividends", money(0)),
    ]);

    build_table(&TableStatement {
        legal_name: &profile.legal_name,
        id_label: "EIN",
        ein: &profile.ein,
        heading: format!(
            "Schedule K-1 (Form 1065) {year} — box 20, code Z: Section 199A information"
        ),
        subheading: format!("Partner: {partner}"),
        columns: vec![
            Column {
                title: "Item".to_string(),
                x: 54.0,
                right: false,
            },
            Column {
                title: "Amount".to_string(),
                x: 738.0,
                right: true,
            },
        ],
        lines,
        footnotes: vec![
            "Qualified business income follows this partner's ordinary business income less their \
             section 179 deduction. W-2 wages and UBIA are this partner's share in proportion to \
             their ordinary business income, the way wage and depreciation expense travel with \
             the partnership's income. Use these figures with Form 8995 or 8995-A."
                .to_string(),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wages_and_ubia_follow_box_1_and_add_back_exactly() {
        // A year divided in fixed amounts: one partner the remainder, one a sum,
        // one nothing.
        let totals = Totals {
            qbi: 41_077,
            w2_wages: 28_174,
            ubia: 86_310,
        };
        let shares = split(totals, &[39_233, 1_844, 0], &[0, 0, 0]);
        assert_eq!(shares.iter().map(|s| s.w2_wages).sum::<i64>(), 28_174);
        assert_eq!(shares.iter().map(|s| s.ubia).sum::<i64>(), 86_310);
        assert_eq!(shares.iter().map(|s| s.qbi()).sum::<i64>(), 41_077);
        assert_eq!(shares[2], Share::default(), "no income, no share");
        assert_eq!(shares[1].w2_wages, (28_174.0_f64 * 1_844.0 / 41_077.0).round() as i64);
    }

    #[test]
    fn nothing_is_apportioned_over_a_year_with_no_income() {
        let shares = split(
            Totals {
                qbi: 0,
                w2_wages: 5_000,
                ubia: 10_000,
            },
            &[0, 0],
            &[0, 0],
        );
        assert!(shares.iter().all(|s| s.is_empty()));
    }
}
