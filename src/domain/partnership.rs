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
        match s
            .trim()
            .to_lowercase()
            .replace(['-', '_', ' '], "")
            .as_str()
        {
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

/// A family tie between two partners, as far as it bears on a return.
///
/// # Why these three and no more
///
/// The only reason the books need to know two partners are related is Schedule
/// B-1's 50% test, which applies the constructive-ownership rules of **IRC
/// §267(c)**. Under §267(c)(2) a person is treated as owning what their *family*
/// owns, and §267(c)(4) defines that family as exactly: a **spouse**, **brothers
/// and sisters**, **ancestors**, and **lineal descendants**. These three kinds
/// span that set — [`Spouse`](Self::Spouse), [`Sibling`](Self::Sibling), and
/// [`ParentOf`](Self::ParentOf), whose chains give ancestors and descendants — and
/// nothing outside it.
///
/// Kinds the statute leaves out are left out on purpose. A spouse's sibling
/// (a sibling-in-law), a sibling's spouse, a cousin, an aunt — none is §267(c)(4)
/// family, so recording one would attribute ownership the law does not, and put a
/// partner on Schedule B-1 who does not belong there. The attribution walk in
/// [`crate::tax::constructive`] therefore reads these kinds by their exact meaning
/// rather than treating the relationships as an undirected "related to" graph, in
/// which those in-laws would leak across.
///
/// # Direction
///
/// [`Spouse`](Self::Spouse) and [`Sibling`](Self::Sibling) are symmetric: the tie
/// is the same read from either partner, so it is stored once, in a canonical
/// order (see [`PartnerRelationship::new`]). [`ParentOf`](Self::ParentOf) is
/// directed — the first partner is the parent of the second — because "ancestor"
/// and "descendant" are opposite directions of the same edge, and a chain of them
/// is what makes a grandparent an ancestor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationshipKind {
    /// Married. Symmetric. §267(c)(4) family.
    Spouse,
    /// Brother or sister, of whole or half blood — §267(c)(4) is explicit that
    /// half blood counts. Symmetric.
    Sibling,
    /// The first partner is the parent of the second. Directed: walked upward it
    /// yields ancestors, downward lineal descendants, and a chain of them reaches
    /// grandparents and grandchildren, who are §267(c)(4) family too.
    ParentOf,
}

impl RelationshipKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RelationshipKind::Spouse => "spouse",
            RelationshipKind::Sibling => "sibling",
            RelationshipKind::ParentOf => "parent_of",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().replace(['-', ' '], "_").as_str() {
            "spouse" | "husband" | "wife" | "married" => Some(RelationshipKind::Spouse),
            "sibling" | "brother" | "sister" => Some(RelationshipKind::Sibling),
            "parent_of" | "parent" | "child_of" | "father_of" | "mother_of" => {
                Some(RelationshipKind::ParentOf)
            }
            _ => None,
        }
    }

    /// How the tie reads on screen, from the first partner to the second.
    pub fn label(self) -> &'static str {
        match self {
            RelationshipKind::Spouse => "spouse of",
            RelationshipKind::Sibling => "sibling of",
            RelationshipKind::ParentOf => "parent of",
        }
    }

    /// Whether the tie reads the same from either partner.
    ///
    /// Spouse and sibling do; parent-of does not. This is what decides whether the
    /// two ids are stored in a canonical order (symmetric — one edge whichever way
    /// it was entered) or kept as given (directed — the order is the meaning).
    pub fn is_symmetric(self) -> bool {
        match self {
            RelationshipKind::Spouse | RelationshipKind::Sibling => true,
            RelationshipKind::ParentOf => false,
        }
    }

    pub const ALL: &'static [RelationshipKind] = &[
        RelationshipKind::Spouse,
        RelationshipKind::Sibling,
        RelationshipKind::ParentOf,
    ];
}

impl std::fmt::Display for RelationshipKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// One family tie between two partners, held so Schedule B-1 can attribute
/// ownership under §267(c). See [`RelationshipKind`] for what the kinds mean, and
/// [`crate::tax::constructive`] for how the attribution is computed from them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartnerRelationship {
    pub partner_id: String,
    pub related_partner_id: String,
    pub kind: RelationshipKind,
}

impl PartnerRelationship {
    /// Build a tie, putting the two ids in the order the books store them in.
    ///
    /// For a symmetric kind the same tie can be entered either way round —
    /// "Alice spouse of Bob" and "Bob spouse of Alice" are one fact — so the ids
    /// are ordered canonically (lexicographically), and both entries land on the
    /// same row rather than two half-duplicates the attribution walk would then
    /// have to reconcile. For a directed kind the order carries the meaning (who
    /// is the parent), so it is kept exactly as given.
    pub fn new(partner_id: &str, related_partner_id: &str, kind: RelationshipKind) -> Self {
        let (a, b) = if kind.is_symmetric() && partner_id > related_partner_id {
            (related_partner_id, partner_id)
        } else {
            (partner_id, related_partner_id)
        };
        PartnerRelationship {
            partner_id: a.to_string(),
            related_partner_id: b.to_string(),
            kind,
        }
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
    if s.is_empty() {
        "0".to_string()
    } else {
        s
    }
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

/// The Illinois-specific choices that shape an IL-1065 for this partnership.
///
/// # Why these are settings rather than asked each time
///
/// Both are the partnership's standing position, not a fact about one year's
/// figures, and they differ between businesses: one of a person's partnerships may
/// operate wholly in Illinois while another sells across state lines, and one may
/// have elected the pass-through entity tax where another has not. Getting either
/// wrong does not blank a box, it fills the *wrong* one — an Illinois-only return
/// carries base income straight to the tax, where a multi-state one must apportion
/// it first — so they are recorded once, per book, and drive which path
/// [`crate::tax::il1065`] takes.
///
/// [`Default`] is Illinois-only with no PTE election, which is the common small
/// partnership and the safer of the two wrong answers to start from: it computes a
/// complete return a reader can see is Illinois-only, rather than an apportioned
/// one with the sales figures silently blank.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Il1065Settings {
    /// Whether any income is earned outside Illinois. `false` checks the form's
    /// "inside Illinois only" box and carries base income straight to Step 7;
    /// `true` checks "outside Illinois" and opens Step 6's apportionment.
    pub apportions_outside_illinois: bool,
    /// Whether the partnership has elected to pay the Illinois Pass-through Entity
    /// tax (4.95%). Checks box I and opens the Step 7 PTE lines.
    pub elects_pte_tax: bool,
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
    /// The percentages as they stand now.
    ///
    /// Kept alongside [`history`] rather than derived from it because almost
    /// every reader wants today's split, and making them all walk a series to
    /// get it would be a lot of code paying for a case most of them do not have.
    /// When `history` is populated this is its last entry.
    ///
    /// [`history`]: Partner::history
    pub shares: Shares,
    /// What the percentages were, and from when, oldest first.
    ///
    /// Empty on books that have never recorded a change, where `shares` has
    /// always been the whole truth — so this deserialises absent and every older
    /// event still replays.
    #[serde(default)]
    pub history: Vec<SharePeriod>,
}

/// One dated step in a partner's percentages.
///
/// A start date and no end: the split in force on a day is the latest step on or
/// before it. A from-and-until pair can be written with a gap or an overlap —
/// two steps both claiming the 3rd of June, or neither — and nothing downstream
/// could resolve that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharePeriod {
    pub effective_from: NaiveDate,
    pub shares: Shares,
}

/// A partner's share of one year's result, fixed in dollars instead of by
/// percentage.
///
/// For an agreement that divides a year by amount — "the departing partner takes
/// what she was paid out, Jinny the rest" — which no set of percentages can
/// express. At most one partner a year takes the remainder (`amount_cents` of
/// `None`). The note is required, because a split that departs from the
/// percentages on file is one somebody may have to show came from the agreement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixedAllocation {
    pub tax_year: i32,
    pub partner_id: String,
    /// The fixed share in cents, or `None` for "whatever the fixed shares leave".
    pub amount_cents: Option<i64>,
    pub note: String,
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

    /// The partner's shares for item J's beginning and ending columns.
    ///
    /// # What the form asks for, which is not what this used to give
    ///
    /// This returned zero for a partner who joined during the year and zero for
    /// one who left, on the reasoning that they held nothing at that end of it.
    /// The instructions for item J say otherwise: for a partner admitted during
    /// the year, enter the percentages **immediately after admission** in the
    /// beginning column; for one whose interest terminated, the percentages
    /// **immediately before termination** in the ending column.
    ///
    /// The old rule produced a K-1 that contradicted itself. A partner who both
    /// joined and left inside one year got 0% in all six item-J boxes on a K-1
    /// carrying a real allocation in Part III — a return asserting that somebody
    /// who owned nothing all year was nonetheless allocated income. Nothing
    /// checked item J against Part III, so it went out looking finished.
    ///
    /// A partner present at both ends shows the same figure twice, which is what
    /// the form expects and not an omission — unless their percentages changed
    /// during the year, in which case the two columns differ, which is the whole
    /// point of the column pair.
    pub fn shares_over(&self, year_start: NaiveDate, year_end: NaiveDate) -> (Shares, Shares) {
        // Clamped into the partner's own tenure, so a joiner is asked about
        // their first day and a leaver about their last, rather than about a
        // date on which `shares_on` would correctly answer "nothing".
        let beginning_on = year_start.max(self.start_date);
        let ending_on = match self.end_date {
            Some(e) if e < year_end => e,
            _ => year_end,
        };
        (self.shares_on(beginning_on), self.shares_on(ending_on))
    }

    /// The percentages in force on a given day.
    ///
    /// # Why this is not simply `self.shares`
    ///
    /// It used to be, and that is what made a prior year's Schedule K-1 show
    /// *this* year's split in item J: a partnership that went from three equal
    /// partners to two at 51/49 filed both years on today's figures, and the
    /// return footed perfectly while describing a partnership that did not exist
    /// in the year being filed.
    ///
    /// With no recorded history the answer is still `self.shares`, because on
    /// books that have never recorded a change that *is* what was true
    /// throughout. Before the first recorded step the earliest one applies: a
    /// history that begins after the date asked about is missing its opening
    /// entry, and the oldest split known is a better answer than today's.
    pub fn shares_on(&self, on: NaiveDate) -> Shares {
        if on < self.start_date || self.end_date.is_some_and(|e| on > e) {
            return Shares::default();
        }
        match self
            .history
            .iter()
            .filter(|p| p.effective_from <= on)
            .max_by_key(|p| p.effective_from)
        {
            Some(p) => p.shares,
            // `min_by_key`, not `first()`. The reader orders by date and the doc
            // on `history` says oldest-first, but a struct literal in a test or a
            // future reader that forgets to sort would make this quietly answer
            // with whichever step happened to be written down first.
            None => self
                .history
                .iter()
                .min_by_key(|p| p.effective_from)
                .map_or(self.shares, |p| p.shares),
        }
    }

    /// Days inside a tax year on which this partner's percentages changed.
    ///
    /// The boundaries a varying-interest allocation splits the year at — see
    /// §706(d). Joining and leaving count: a partner who arrives in March
    /// changes the split for everybody on that day.
    pub fn change_dates_in(&self, year_start: NaiveDate, year_end: NaiveDate) -> Vec<NaiveDate> {
        let mut out: Vec<NaiveDate> = self
            .history
            .iter()
            .map(|p| p.effective_from)
            .chain(std::iter::once(self.start_date))
            .chain(self.end_date.and_then(|e| e.succ_opt()))
            .filter(|d| *d > year_start && *d <= year_end)
            .collect();
        out.sort();
        out.dedup();
        out
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
            history: Vec::new(),
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

    /// A partner admitted during the year shows what they held on arrival.
    ///
    /// Not zero. Item J's instruction for a partner admitted during the year is
    /// to enter the percentages *immediately after admission* in the beginning
    /// column, and a K-1 that says 0% while Part III allocates income to them is
    /// a return that contradicts itself.
    #[test]
    fn joining_midyear_shows_the_share_held_on_arrival() {
        let p = partner(day(2025, 3, 1), None);
        let (begin, end) = p.shares_over(day(2025, 1, 1), day(2025, 12, 31));
        assert_eq!(begin.profit_ppm, 500_000, "what they held on 1 March");
        assert_eq!(end.profit_ppm, 500_000);
    }

    /// A partner who left shows what they held on their last day.
    #[test]
    fn leaving_midyear_shows_the_share_held_immediately_before_leaving() {
        let p = partner(day(2020, 1, 1), Some(day(2025, 6, 30)));
        let (begin, end) = p.shares_over(day(2025, 1, 1), day(2025, 12, 31));
        assert_eq!(begin.profit_ppm, 500_000);
        assert_eq!(end.profit_ppm, 500_000, "what they held on 30 June");
        assert!(p.is_final_for(day(2025, 12, 31)), "their last K-1");
    }

    /// The case that made the old rule indefensible: in and out inside one year.
    ///
    /// All six item-J boxes read zero while Part III carried a real allocation.
    #[test]
    fn joining_and_leaving_in_one_year_still_states_a_real_interest() {
        let p = partner(day(2025, 3, 1), Some(day(2025, 9, 30)));
        let (begin, end) = p.shares_over(day(2025, 1, 1), day(2025, 12, 31));
        assert_eq!(begin.profit_ppm, 500_000);
        assert_eq!(end.profit_ppm, 500_000);
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
        assert_eq!(
            percent_to_ppm(0.00005),
            1,
            "rounds up rather than to nothing"
        );
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
