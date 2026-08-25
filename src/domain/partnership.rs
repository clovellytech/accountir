//! The partnership itself, and the partners a Form 1065 and its K-1s are about.
//!
//! # Why this is not just "company settings"
//!
//! The ledger already knows a company name and a base currency, which is enough
//! to head a balance sheet. A return needs more and needs it exactly: an EIN the
//! IRS matches against its own record, a NAICS code, the date the business
//! started, and a legal name that is the name on the SS-4 rather than the name
//! over the door. Getting one of them wrong does not produce a wrong report, it
//! produces a rejected filing, so they are their own record with their own
//! validation rather than free-text settings.
//!
//! # Why percentages are integers
//!
//! A partner's share is divided by nothing and multiplied by everything: it
//! allocates income, loss, and capital to a human being who pays tax on the
//! result. Three partners at "a third each" in floating point sum to 99.999999%,
//! and the K-1s then disagree with the 1065 by a cent that somebody has to
//! explain. Shares are held in parts per million of the whole — 100% is
//! [`FULL_SHARE`] — so a third is 333_333 ppm, the shortfall is visible, and
//! [`Shares::sums_to_whole`] can say plainly whether the books add up.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

/// 100% expressed in parts per million, the unit every share is held in.
pub const FULL_SHARE: i64 = 1_000_000;

/// Which box is ticked in item G of a Schedule K-1.
///
/// The form offers exactly these two and no third, so this is an enum rather
/// than a string: a partner is one or the other on the day the return is filed,
/// and "neither" is not a state the IRS accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PartnerType {
    /// "General partner or LLC member-manager" — K-1 item G, first box.
    General,
    /// "Limited partner or other LLC member" — K-1 item G, second box.
    Limited,
}

impl PartnerType {
    pub fn as_str(self) -> &'static str {
        match self {
            PartnerType::General => "general",
            PartnerType::Limited => "limited",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().replace(['-', '_', ' '], "").as_str() {
            "general" | "generalpartner" | "membermanager" | "gp" => Some(PartnerType::General),
            "limited" | "limitedpartner" | "member" | "lp" => Some(PartnerType::Limited),
            _ => None,
        }
    }

    /// The label as the K-1 itself words it.
    pub fn label(self) -> &'static str {
        match self {
            PartnerType::General => "General partner or LLC member-manager",
            PartnerType::Limited => "Limited partner or other LLC member",
        }
    }

    pub const ALL: &'static [PartnerType] = &[PartnerType::General, PartnerType::Limited];
}

impl std::fmt::Display for PartnerType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Which box is ticked in item H1 of a Schedule K-1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Residency {
    Domestic,
    Foreign,
}

impl Residency {
    pub fn as_str(self) -> &'static str {
        match self {
            Residency::Domestic => "domestic",
            Residency::Foreign => "foreign",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "domestic" | "us" | "usa" | "d" => Some(Residency::Domestic),
            "foreign" | "f" | "non-us" | "nonus" => Some(Residency::Foreign),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Residency::Domestic => "Domestic partner",
            Residency::Foreign => "Foreign partner",
        }
    }

    pub const ALL: &'static [Residency] = &[Residency::Domestic, Residency::Foreign];
}

impl std::fmt::Display for Residency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// A postal address, in the shape the 1065 header and K-1 item F ask for.
///
/// Split into fields rather than kept as a block of text because the 1065 header
/// has a separate box for each one, and re-splitting a blob on commas guesses
/// wrongly the first time a street name contains one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Address {
    pub street: String,
    /// "Room or suite no." on the 1065 header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suite: Option<String>,
    pub city: String,
    /// State, or province for a foreign address.
    pub state: String,
    /// ZIP, or foreign postal code.
    pub postal_code: String,
    /// Left empty for a US address, which is what the form expects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
}

impl Address {
    /// One line per element, as K-1 item F wants the partner's address.
    ///
    /// The K-1 gives a single multi-line box rather than the 1065's separate
    /// ones, so the parts are joined here instead of being placed individually.
    pub fn as_block(&self, name: &str) -> String {
        let mut out = String::new();
        if !name.is_empty() {
            out.push_str(name);
            out.push('\n');
        }
        out.push_str(&self.street);
        if let Some(suite) = self.suite.as_deref().filter(|s| !s.trim().is_empty()) {
            out.push(' ');
            out.push_str(suite);
        }
        out.push('\n');
        out.push_str(&self.city);
        if !self.state.is_empty() {
            out.push_str(", ");
            out.push_str(&self.state);
        }
        if !self.postal_code.is_empty() {
            out.push(' ');
            out.push_str(&self.postal_code);
        }
        if let Some(country) = self.country.as_deref().filter(|c| !c.trim().is_empty()) {
            out.push('\n');
            out.push_str(country);
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.street.trim().is_empty() && self.city.trim().is_empty()
    }
}

/// A partner's share of profit, loss, and capital, in parts per million.
///
/// Three separate figures because they genuinely differ: a partner can take 50%
/// of profits, be allocated 50% of losses, and hold 40% of capital, and the K-1
/// has a row for each. Collapsing them to one "ownership" number is the mistake
/// that makes item J impossible to fill in honestly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shares {
    pub profit_ppm: i64,
    pub loss_ppm: i64,
    pub capital_ppm: i64,
}

impl Shares {
    /// Build from percentages written the way a person says them — `50.0`, `33.3333`.
    pub fn from_percents(profit: f64, loss: f64, capital: f64) -> Self {
        Shares {
            profit_ppm: percent_to_ppm(profit),
            loss_ppm: percent_to_ppm(loss),
            capital_ppm: percent_to_ppm(capital),
        }
    }

    /// Every share is between nothing and the whole.
    ///
    /// A negative share would allocate income away from the partnership, and one
    /// over 100% would allocate more than exists; both are arithmetic that no
    /// later step checks, so they are refused here.
    pub fn is_in_range(&self) -> bool {
        self.out_of_range().is_none()
    }

    /// The first share that is not between nothing and the whole, named.
    ///
    /// The single definition of that rule. The event validator calls this rather
    /// than restating the bounds, because two copies of one rule are two rules
    /// the day somebody edits one — and the one that would drift is the one
    /// guarding the log.
    pub fn out_of_range(&self) -> Option<(&'static str, i64)> {
        [
            ("profit", self.profit_ppm),
            ("loss", self.loss_ppm),
            ("capital", self.capital_ppm),
        ]
        .into_iter()
        .find(|&(_, ppm)| !(0..=FULL_SHARE).contains(&ppm))
    }

    /// Whether a set of partners' shares each add up to exactly the whole.
    ///
    /// Returned per column rather than as one boolean because the columns fail
    /// independently and separately, and "the capital column is 2 ppm short" is
    /// a fixable statement where "the shares are wrong" is not.
    pub fn sums_to_whole(partners: &[Shares]) -> ShareTotals {
        ShareTotals {
            profit_ppm: partners.iter().map(|s| s.profit_ppm).sum(),
            loss_ppm: partners.iter().map(|s| s.loss_ppm).sum(),
            capital_ppm: partners.iter().map(|s| s.capital_ppm).sum(),
        }
    }
}

/// What a set of partners' shares actually add up to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShareTotals {
    pub profit_ppm: i64,
    pub loss_ppm: i64,
    pub capital_ppm: i64,
}

impl ShareTotals {
    pub fn is_whole(&self) -> bool {
        self.profit_ppm == FULL_SHARE
            && self.loss_ppm == FULL_SHARE
            && self.capital_ppm == FULL_SHARE
    }

    /// The columns that do not add to 100%, named and with their totals, ready
    /// to put in front of somebody about to file.
    pub fn discrepancies(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (name, total) in [
            ("profit", self.profit_ppm),
            ("loss", self.loss_ppm),
            ("capital", self.capital_ppm),
        ] {
            if total != FULL_SHARE {
                out.push(format!("{name} totals {}%", format_ppm(total)));
            }
        }
        out
    }
}

/// Percent → parts per million, rounded half away from zero.
///
/// Rounded rather than truncated so that 33.3333% is 333_333 ppm and not
/// 333_332: truncation biases every share downward, and three of them then sum
/// to visibly less than the whole.
/// # Why a non-finite percentage becomes a deliberately impossible share
///
/// Rust's float-to-integer cast saturates, and it maps `NaN` to **zero**. So a
/// `--profit nan` typed at a prompt, or a `0.0 / 0.0` computed upstream, would
/// otherwise admit a partner at 0% — and every check downstream would wave it
/// through, because zero is a perfectly legal share. Worse, if the other partner
/// holds the remaining 100%, the shares still total the whole and the "these do
/// not add up" warning never fires. A partner ends up allocated nothing, and
/// nothing anywhere says so.
///
/// Mapping non-finite input to [`i64::MIN`] instead puts it outside `0..=100%`,
/// where [`Shares::out_of_range`] and the event validator both already refuse it
/// — the same route `inf` takes today by saturating to [`i64::MAX`]. The refusal
/// is thus enforced at the choke point every writer passes through, local and
/// server alike, rather than at whichever caller remembered to check.
pub fn percent_to_ppm(percent: f64) -> i64 {
    if !percent.is_finite() {
        return i64::MIN;
    }
    (percent * 10_000.0).round() as i64
}

/// Parts per million → the percentage string the K-1 carries.
///
/// Trailing zeros are trimmed so a plain half reads `50` rather than `50.0000`,
/// but a third keeps every digit it needs.
pub fn format_ppm(ppm: i64) -> String {
    let s = format!("{:.4}", ppm as f64 / 10_000.0);
    // `-0` cannot survive the trim to a bare "-", so only the empty case needs
    // guarding — "0.0000" trims to "0", not to nothing.
    let s = s.trim_end_matches('0').trim_end_matches('.').to_string();
    if s.is_empty() { "0".to_string() } else { s }
}

/// The partnership, as the head of Form 1065 describes it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BusinessProfile {
    /// The name on the SS-4, which is what the IRS matches the EIN against.
    pub legal_name: String,
    pub address: Address,
    /// Employer identification number, `NN-NNNNNNN`.
    pub ein: String,
    /// Six-digit NAICS code — box C, "Business code number".
    pub naics_code: String,
    /// Box E, "Date business started".
    pub formation_date: NaiveDate,
    /// Box A, "Principal business activity" — optional, free text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_activity: Option<String>,
    /// Box B, "Principal product or service" — optional, free text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_product: Option<String>,
}

/// One partner, and everything a Schedule K-1 needs to name them.
///
/// The taxpayer identification number is deliberately **not** here — see
/// [`crate::commands::partnership_commands`] for where it lives and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Partner {
    pub partner_id: String,
    pub name: String,
    pub partner_type: PartnerType,
    pub residency: Residency,
    /// K-1 item I1, "What type of entity is this partner?" — free text because
    /// the form's own answer is free text: "Individual", "S Corporation",
    /// "Estate", and a dozen more the IRS adds to without warning.
    pub entity_type: String,
    pub address: Address,
    /// Defaults to the partnership's formation date when the partner was there
    /// from the start, which is the common case and a tedious one to retype.
    pub start_date: NaiveDate,
    /// `None` while the partner is still in. Set on the day they leave, which is
    /// what makes their K-1 a final one.
    pub end_date: Option<NaiveDate>,
    pub shares: Shares,
}

impl Partner {
    /// Whether the partner held an interest at any point in a tax year.
    ///
    /// A partner who joined in March and one who left in March both get a K-1
    /// for that year; only somebody who was outside the year at both ends does
    /// not. Overlap rather than containment, for exactly that reason.
    pub fn was_partner_during(&self, year_start: NaiveDate, year_end: NaiveDate) -> bool {
        let started_by_end = self.start_date <= year_end;
        let not_gone_before_start = self.end_date.is_none_or(|e| e >= year_start);
        started_by_end && not_gone_before_start
    }

    /// Whether this year's K-1 is the partner's last — K-1 checkbox "Final K-1".
    pub fn is_final_for(&self, year_end: NaiveDate) -> bool {
        self.end_date.is_some_and(|e| e <= year_end)
    }

    /// The partner's shares at the start and end of a tax year.
    ///
    /// Item J has a beginning and an ending column, and they differ precisely
    /// when a partner joined or left mid-year: somebody who joined in March
    /// began the year holding nothing, and somebody who left in March ends it
    /// holding nothing. A partner present throughout shows the same figure
    /// twice, which is what the form expects and not an omission.
    pub fn shares_over(&self, year_start: NaiveDate, year_end: NaiveDate) -> (Shares, Shares) {
        let joined_midyear = self.start_date > year_start;
        let left_by_year_end = self.end_date.is_some_and(|e| e <= year_end);

        let beginning = if joined_midyear {
            Shares::default()
        } else {
            self.shares
        };
        let ending = if left_by_year_end {
            Shares::default()
        } else {
            self.shares
        };
        (beginning, ending)
    }
}

/// An EIN as the IRS writes it: two digits, a hyphen, seven digits.
pub fn is_valid_ein(ein: &str) -> bool {
    let b = ein.as_bytes();
    b.len() == 10
        && b[2] == b'-'
        && b[..2].iter().all(u8::is_ascii_digit)
        && b[3..].iter().all(u8::is_ascii_digit)
}

/// An SSN as written on a K-1: three digits, hyphen, two, hyphen, four.
pub fn is_valid_ssn(ssn: &str) -> bool {
    let b = ssn.as_bytes();
    b.len() == 11
        && b[3] == b'-'
        && b[6] == b'-'
        && b[..3].iter().all(u8::is_ascii_digit)
        && b[4..6].iter().all(u8::is_ascii_digit)
        && b[7..].iter().all(u8::is_ascii_digit)
}

/// A partner's TIN is an SSN or an EIN — item E takes either.
pub fn is_valid_tin(tin: &str) -> bool {
    is_valid_ssn(tin) || is_valid_ein(tin)
}

/// NAICS codes are six digits, always.
pub fn is_valid_naics(code: &str) -> bool {
    code.len() == 6 && code.as_bytes().iter().all(u8::is_ascii_digit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn partner(start: NaiveDate, end: Option<NaiveDate>) -> Partner {
        Partner {
            partner_id: "p1".into(),
            name: "A Partner".into(),
            partner_type: PartnerType::General,
            residency: Residency::Domestic,
            entity_type: "Individual".into(),
            address: Address::default(),
            start_date: start,
            end_date: end,
            shares: Shares::from_percents(50.0, 50.0, 50.0),
        }
    }

    /// Thirds must not silently lose the remainder.
    ///
    /// This is the whole reason shares are integers: in floating point the three
    /// sum to something that is not one, and the K-1s then disagree with the
    /// 1065 by an amount nobody can point at.
    #[test]
    fn three_equal_partners_are_two_ppm_short_and_say_so() {
        let third = Shares::from_percents(33.3333, 33.3333, 33.3333);
        assert_eq!(third.profit_ppm, 333_333);

        let totals = Shares::sums_to_whole(&[third, third, third]);
        assert_eq!(totals.profit_ppm, 999_999);
        assert!(!totals.is_whole(), "999_999 ppm is not the whole");
        assert_eq!(totals.discrepancies().len(), 3, "every column is short");
    }

    #[test]
    fn two_equal_partners_add_up_exactly() {
        let half = Shares::from_percents(50.0, 50.0, 50.0);
        let totals = Shares::sums_to_whole(&[half, half]);
        assert!(totals.is_whole());
        assert!(totals.discrepancies().is_empty());
    }

    #[test]
    fn a_share_outside_nothing_to_everything_is_refused() {
        assert!(Shares::from_percents(50.0, 50.0, 50.0).is_in_range());
        assert!(Shares::from_percents(100.0, 100.0, 100.0).is_in_range());
        assert!(!Shares::from_percents(-1.0, 50.0, 50.0).is_in_range());
        assert!(!Shares::from_percents(50.0, 101.0, 50.0).is_in_range());
    }

    /// A partner who joined mid-year began it holding nothing.
    #[test]
    fn joining_midyear_shows_a_beginning_share_of_nothing() {
        let p = partner(day(2025, 3, 1), None);
        let (begin, end) = p.shares_over(day(2025, 1, 1), day(2025, 12, 31));
        assert_eq!(begin.profit_ppm, 0, "was not a partner on 1 January");
        assert_eq!(end.profit_ppm, 500_000);
    }

    /// A partner who left mid-year ends it holding nothing.
    #[test]
    fn leaving_midyear_shows_an_ending_share_of_nothing() {
        let p = partner(day(2020, 1, 1), Some(day(2025, 6, 30)));
        let (begin, end) = p.shares_over(day(2025, 1, 1), day(2025, 12, 31));
        assert_eq!(begin.profit_ppm, 500_000);
        assert_eq!(end.profit_ppm, 0);
        assert!(p.is_final_for(day(2025, 12, 31)), "their last K-1");
    }

    #[test]
    fn a_partner_present_all_year_shows_the_same_share_twice() {
        let p = partner(day(2020, 1, 1), None);
        let (begin, end) = p.shares_over(day(2025, 1, 1), day(2025, 12, 31));
        assert_eq!(begin, end);
        assert!(!p.is_final_for(day(2025, 12, 31)));
    }

    /// Both a joiner and a leaver get a K-1; only somebody outside the year does not.
    #[test]
    fn anyone_who_held_an_interest_during_the_year_gets_a_k1() {
        let (ys, ye) = (day(2025, 1, 1), day(2025, 12, 31));
        assert!(partner(day(2025, 12, 31), None).was_partner_during(ys, ye));
        assert!(partner(day(2020, 1, 1), Some(day(2025, 1, 1))).was_partner_during(ys, ye));
        assert!(partner(day(2020, 1, 1), None).was_partner_during(ys, ye));

        assert!(
            !partner(day(2026, 1, 1), None).was_partner_during(ys, ye),
            "joined after the year ended"
        );
        assert!(
            !partner(day(2020, 1, 1), Some(day(2024, 12, 31))).was_partner_during(ys, ye),
            "left before the year began"
        );
    }

    /// `NaN` must not become a legal share.
    ///
    /// Rust maps it to zero on cast, and zero is a share the books accept. Two
    /// partners, one entered as `nan`: that one holds nothing, the other holds
    /// the whole, the totals come to exactly 100%, and the warning that exists
    /// to catch bad splits stays silent. The K-1 allocating a partner nothing is
    /// the only evidence, months later.
    #[test]
    fn a_share_that_is_not_a_number_is_refused_rather_than_read_as_nothing() {
        assert_eq!(
            percent_to_ppm(f64::NAN),
            i64::MIN,
            "NaN must land outside the permitted range, not on zero"
        );

        let nan = Shares::from_percents(f64::NAN, 50.0, 50.0);
        assert!(!nan.is_in_range(), "a NaN share passed the range check");
        assert_eq!(nan.out_of_range().map(|(n, _)| n), Some("profit"));

        // The failure this prevents: paired with a 100% partner the totals are
        // whole, so nothing downstream would have objected.
        let whole = Shares::from_percents(100.0, 50.0, 50.0);
        assert_eq!(
            Shares::sums_to_whole(&[nan, whole]).profit_ppm,
            i64::MIN + FULL_SHARE,
            "if NaN read as 0 this would total exactly 100% and look correct"
        );

        // Infinities already fell outside by saturating; keep it that way.
        assert!(!Shares::from_percents(f64::INFINITY, 50.0, 50.0).is_in_range());
        assert!(!Shares::from_percents(f64::NEG_INFINITY, 50.0, 50.0).is_in_range());
    }

    #[test]
    fn percentages_round_rather_than_truncate() {
        assert_eq!(percent_to_ppm(33.3333), 333_333);
        assert_eq!(percent_to_ppm(0.00005), 1, "rounds up rather than to nothing");
        assert_eq!(percent_to_ppm(100.0), FULL_SHARE);
    }

    #[test]
    fn a_share_reads_back_as_it_was_written() {
        assert_eq!(format_ppm(500_000), "50");
        assert_eq!(format_ppm(333_333), "33.3333");
        assert_eq!(format_ppm(FULL_SHARE), "100");
        assert_eq!(format_ppm(0), "0");
    }

    #[test]
    fn identifiers_are_checked_against_the_shape_the_irs_uses() {
        assert!(is_valid_ein("88-1234567"));
        assert!(!is_valid_ein("881234567"), "no hyphen");
        assert!(!is_valid_ein("8-81234567"), "hyphen misplaced");
        assert!(!is_valid_ein("88-123456X"));

        assert!(is_valid_ssn("123-45-6789"));
        assert!(!is_valid_ssn("123456789"));

        assert!(is_valid_tin("123-45-6789"), "a TIN may be an SSN");
        assert!(is_valid_tin("88-1234567"), "or an EIN");

        assert!(is_valid_naics("541511"));
        assert!(!is_valid_naics("54151"), "five digits is not a NAICS code");
    }

    #[test]
    fn an_address_reads_as_a_block_on_the_k1() {
        let addr = Address {
            street: "1 Example Street".into(),
            suite: Some("Suite 4".into()),
            city: "Cape Town".into(),
            state: "WC".into(),
            postal_code: "8001".into(),
            country: None,
        };
        assert_eq!(
            addr.as_block("Alice Example"),
            "Alice Example\n1 Example Street Suite 4\nCape Town, WC 8001"
        );
    }

    #[test]
    fn partner_type_and_residency_parse_the_words_people_type() {
        assert_eq!(PartnerType::parse("general"), Some(PartnerType::General));
        assert_eq!(PartnerType::parse("LP"), Some(PartnerType::Limited));
        assert_eq!(
            PartnerType::parse("member-manager"),
            Some(PartnerType::General)
        );
        assert_eq!(PartnerType::parse("nonsense"), None);

        assert_eq!(Residency::parse("foreign"), Some(Residency::Foreign));
        assert_eq!(Residency::parse("US"), Some(Residency::Domestic));
        assert_eq!(Residency::parse(""), None);
    }
}
