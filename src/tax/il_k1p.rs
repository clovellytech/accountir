//! Illinois Schedule K-1-P, Partner's or Shareholder's Share of Income,
//! Deductions, Credits, and Recapture — one per partner.
//!
//! What each partner needs to carry the partnership onto their own Illinois
//! return: their share of ordinary income (Step 4), and their share of the
//! Illinois additions and subtractions the IL-1065 made (Step 5). The shares are
//! [`super::il1065::member_shares`], the same figures Illinois Schedule B column
//! E adds up, so the schedules agree with the return they come from.
//!
//! Column B is Column A times the apportionment factor. For a partnership whose
//! income is all Illinois' the factor is one and the columns match; for one that
//! apportions, the factor is not known to the books and Column B is left blank.
//!
//! The schedules go to the partners, with Schedule K-1-P(2), by the IL-1065's due
//! date. They are not mailed with the return.

use super::acroform::{field_map, set_check, set_text, strip_xfa, FormError};
use super::form1065::PartnerFiling;
use super::il1065::MemberShares;
use super::lines::format_dollars;
use crate::domain::BusinessProfile;
use lopdf::Document;

/// The tax year the vendored blank (R-12/25) is for.
pub const FORM_YEAR: i32 = 2025;

const K1P: &[u8] = include_bytes!("../../assets/il/schedule-k-1-p.pdf");

/// The blank's field names, exactly as the PDF spells them.
pub mod f {
    pub const MONTH: &str = "Month - 1 - Year ending";
    pub const YEAR: &str = "Year - Year ending";
    pub const PARTNERSHIP_BOX: &str = "Step 1 - Line 1 - Check your business type - Partnership";
    pub const NAME: &str = "Step 1 - Line 2 - Enter your name as shown on your Form IL-1065 or Form IL-1120-ST";
    pub const FEIN_2: &str = "Step 1 - Line 3 - Enter your federal employer identification number (FEIN) - Enter the first two numbers";
    pub const FEIN_7: &str = "Step 1 - Line 3 - Enter your federal employer identification number (FEIN) - Enter the last seven digits of FEIN";
    pub const FACTOR: &str = "Step 1 - Line 4 - Enter the apportionment factor from Form IL-1065 or Form IL-1120-ST, Line 42.  Otherwise, enter \"1\"";
    pub const MEMBER_NAME: &str = "Step 2 - Line 5 - Name";
    pub const MEMBER_ADDRESS: &str = "Step 2 - Line 6 - Mailing address";
    pub const MEMBER_CITY: &str = "Step 2 - Line 6 - City";
    pub const MEMBER_STATE: &str = "Step 2 - Line 6 - State";
    pub const MEMBER_ZIP: &str = "Step 2 - Line 6 - ZIP";
    pub const MEMBER_TIN: &str = "Step 2 - Line 7 - Social Security number or FEIN";
    pub const MEMBER_SHARE: &str = "Step 2 - Line 8 - Share (%)";
    pub const TYPE_INDIVIDUAL: &str = "Step 2 - Line 9a - Check the appropriate box.  See instructions.  Individual";
    pub const TYPE_CORPORATION: &str = "Step 2 - Line 9a - Check the appropriate box.  See instructions.  Corporation";
    pub const TYPE_TRUST: &str = "Step 2 - Line 9a - Check the appropriate box.  See instructions.  Trust";
    pub const TYPE_PARTNERSHIP: &str = "Step 2 - Line 9a - Check the appropriate box.  See instructions.  Partnership";
    pub const TYPE_S_CORPORATION: &str = "Step 2 - Line 9a - Check the appropriate box.  See instructions.  S corporation";
    pub const TYPE_ESTATE: &str = "Step 2 - Line 9a - Check the appropriate box.  See instructions.  Estate";
    pub const L20_A: &str = "Step 4 - Line 20 - Column A - Member's share from U.S. Schedule K-1, less non business income -  Ordinary income or loss from trade or business activity";
    pub const L20_B: &str = "Step 4 - Line 20 - Column B - Member's share apportioned to Illinois -  Ordinary income or loss from trade or business activity";
    pub const L29_A: &str = "Step 4 - Line 29 - Column A - Member's share from U.S. Schedule K-1, less non business income - Guaranteed payments to partner (U.S. Form 1065 only)";
    pub const L29_B: &str = "Step 4 - Line 29 - Column B - Member's share apportioned to Illinois - Guaranteed payments to partner (U.S. Form 1065 only)";
    pub const L33_A: &str = "Step 5 - Line 33 - Column A - Member's share from Form IL-1065 or IL-1120-ST - Illinois replacement tax and surcharge deducted";
    pub const L33_B: &str = "Step 5 - Line 33 - Column B - Member's share apportioned or allocated to Illinois - Illinois replacement tax and surcharge deducted";
    pub const L34_A: &str = "Step 5 - Line 34 - Column A - Member's share from Form IL-1065 or IL-1120-ST - Illinois Special Depreciation addition";
    pub const L34_B: &str = "Step 5 - Line 34 - Column B - Member's share apportioned or allocated to Illinois - Illinois Special Depreciation addition";
    pub const L44_A: &str = "Step 5 - Line 44 - Column A - Member's share from Form IL-1065 or IL-1120-ST - Illinois Special Depreciation subtraction";
    pub const L44_B: &str = "Step 5 - Line 44 - Column B - Member's share apportioned or allocated to Illinois - Illinois Special Depreciation subtraction";
}

/// Fill one partner's Schedule K-1-P.
///
/// `apportions` is whether the partnership apportions income outside Illinois:
/// then line 4 and Column B are left for a person.
pub fn build(
    profile: &BusinessProfile,
    year: i32,
    filing: &PartnerFiling,
    shares: &MemberShares,
    apportions: bool,
) -> Result<(Document, Vec<String>), FormError> {
    let mut warnings = Vec::new();
    let mut doc = Document::load_mem(K1P)?;
    strip_xfa(&mut doc);
    let map = field_map(&doc);
    let p = &filing.partner;

    set_text(&mut doc, &map, f::MONTH, "12")?;
    set_text(&mut doc, &map, f::YEAR, &year.to_string())?;
    set_check(&mut doc, &map, f::PARTNERSHIP_BOX, "Partnership")?;
    set_text(&mut doc, &map, f::NAME, &profile.legal_name)?;
    let (fein2, fein7) = super::il1065::split_fein(&profile.ein);
    set_text(&mut doc, &map, f::FEIN_2, &fein2)?;
    set_text(&mut doc, &map, f::FEIN_7, &fein7)?;
    if !apportions {
        set_text(&mut doc, &map, f::FACTOR, "1")?;
    }

    set_text(&mut doc, &map, f::MEMBER_NAME, &p.name)?;
    let street = match p.address.suite.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(suite) => format!("{} {}", p.address.street, suite),
        None => p.address.street.clone(),
    };
    set_text(&mut doc, &map, f::MEMBER_ADDRESS, &street)?;
    set_text(&mut doc, &map, f::MEMBER_CITY, &p.address.city)?;
    set_text(&mut doc, &map, f::MEMBER_STATE, &p.address.state)?;
    set_text(&mut doc, &map, f::MEMBER_ZIP, &p.address.postal_code)?;
    match filing.tin.as_deref() {
        Some(tin) => set_text(&mut doc, &map, f::MEMBER_TIN, tin)?,
        None => warnings.push(format!(
            "Schedule K-1-P for {}: no identifying number is held on this machine, so line 7 is \
             blank.",
            p.name
        )),
    }
    set_text(
        &mut doc,
        &map,
        f::MEMBER_SHARE,
        &format!("{:.2}", shares.share_ppm as f64 / 10_000.0),
    )?;
    let type_box = match super::il1065::illinois_member_type(p) {
        Some("I") => Some((f::TYPE_INDIVIDUAL, "Individual")),
        Some("P") => Some((f::TYPE_PARTNERSHIP, "Partnership")),
        Some("C") => Some((f::TYPE_CORPORATION, "Corporation")),
        Some("S") => Some((f::TYPE_S_CORPORATION, "S corporation")),
        Some("T") => Some((f::TYPE_TRUST, "Trust")),
        Some("M") => Some((f::TYPE_ESTATE, "Estate")),
        _ => None,
    };
    match type_box {
        Some((field, on)) => set_check(&mut doc, &map, field, on)?,
        None => warnings.push(format!(
            "Schedule K-1-P for {}: the entity type {:?} does not say which box on line 9a it is, \
             so none is checked.",
            p.name, p.entity_type
        )),
    }

    // Steps 4 and 5: Column A, and Column B at a factor of one.
    for (a, b, amount, always) in [
        (f::L20_A, f::L20_B, shares.ordinary, true),
        (f::L29_A, f::L29_B, shares.guaranteed, false),
        (f::L33_A, f::L33_B, shares.illinois_taxes, false),
        (f::L34_A, f::L34_B, shares.special_addition, false),
        (f::L44_A, f::L44_B, shares.special_subtraction, false),
    ] {
        if !always && amount == 0 {
            continue;
        }
        set_text(&mut doc, &map, a, &format_dollars(amount))?;
        if !apportions {
            set_text(&mut doc, &map, b, &format_dollars(amount))?;
        }
    }
    Ok((doc, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Address, Partner, PartnerType, Residency, Shares};
    use crate::tax::acroform::get_value;

    #[test]
    fn every_field_named_is_on_the_blank() {
        let mut doc = Document::load_mem(K1P).unwrap();
        strip_xfa(&mut doc);
        let map = field_map(&doc);
        for name in [
            f::MONTH, f::YEAR, f::PARTNERSHIP_BOX, f::NAME, f::FEIN_2, f::FEIN_7, f::FACTOR,
            f::MEMBER_NAME, f::MEMBER_ADDRESS, f::MEMBER_CITY, f::MEMBER_STATE, f::MEMBER_ZIP,
            f::MEMBER_TIN, f::MEMBER_SHARE, f::TYPE_INDIVIDUAL, f::TYPE_CORPORATION,
            f::TYPE_TRUST, f::TYPE_PARTNERSHIP, f::TYPE_S_CORPORATION, f::TYPE_ESTATE,
            f::L20_A, f::L20_B, f::L29_A, f::L29_B, f::L33_A, f::L33_B, f::L34_A, f::L34_B,
            f::L44_A, f::L44_B,
        ] {
            assert!(map.find(name).is_some(), "no field {name:?}");
        }
    }

    #[test]
    fn a_partners_schedule_carries_their_shares_in_both_columns() {
        let filing = PartnerFiling {
            partner: Partner {
                history: Vec::new(),
                partner_id: "active".into(),
                name: "Active Partner".into(),
                partner_type: PartnerType::General,
                residency: Residency::Domestic,
                entity_type: "Individual".into(),
                address: Address {
                    street: "1 Main St".into(),
                    suite: None,
                    city: "Chicago".into(),
                    state: "IL".into(),
                    postal_code: "60601".into(),
                    country: None,
                },
                start_date: chrono::NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(),
                end_date: None,
                shares: Shares::from_percents(51.0, 0.0, 51.0),
            },
            tin: Some("123-45-6789".into()),
        };
        let shares = MemberShares {
            ordinary: 97_629,
            guaranteed: 0,
            illinois_taxes: 326,
            special_addition: 15_333,
            special_subtraction: 1_631,
            base_income: 111_657,
            share_ppm: 510_000,
        };
        let profile = BusinessProfile {
            legal_name: "Prairie Partners LLC".into(),
            ein: "37-1234567".into(),
            ..Default::default()
        };
        let (doc, warnings) = build(&profile, FORM_YEAR, &filing, &shares, false).unwrap();
        let map = field_map(&doc);
        let v = |name: &str| get_value(&doc, &map, name);
        assert_eq!(v(f::L20_A).as_deref(), Some("97,629"));
        assert_eq!(v(f::L20_B).as_deref(), Some("97,629"));
        assert_eq!(v(f::L33_A).as_deref(), Some("326"));
        assert_eq!(v(f::L34_B).as_deref(), Some("15,333"));
        assert_eq!(v(f::L44_A).as_deref(), Some("1,631"));
        assert_eq!(v(f::L29_A), None, "no guaranteed payments, no line 29");
        assert_eq!(v(f::MEMBER_SHARE).as_deref(), Some("51.00"));
        assert_eq!(v(f::FACTOR).as_deref(), Some("1"));
        assert_eq!(v(f::TYPE_INDIVIDUAL).as_deref(), Some("/Individual"));
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// Two partners' schedules in one document keep every box apart: the blank
    /// keeps its fields at the top level, and namespacing must not give them all
    /// one name, or a viewer shows one value in every box.
    #[test]
    fn two_schedules_in_one_document_keep_their_own_values() {
        use crate::tax::acroform::{append_document, get_value_in, namespace_fields};
        let blank = {
            let mut doc = Document::load_mem(K1P).unwrap();
            strip_xfa(&mut doc);
            doc
        };
        let per_copy = field_map(&blank).len();

        let fill = |name: &str, tin: &str| {
            let mut doc = blank.clone();
            let map = field_map(&doc);
            set_text(&mut doc, &map, f::MEMBER_NAME, name).unwrap();
            set_text(&mut doc, &map, f::MEMBER_TIN, tin).unwrap();
            doc
        };
        let mut bundle = fill("First Partner", "111-11-1111");
        namespace_fields(&mut bundle, "K1P_1");
        let mut second = fill("Second Partner", "222-22-2222");
        namespace_fields(&mut second, "K1P_2");
        append_document(&mut bundle, second).unwrap();

        let map = field_map(&bundle);
        assert_eq!(map.len(), per_copy * 2, "every box keeps a name of its own");
        assert_eq!(
            get_value_in(&bundle, &map, "K1P_1", f::MEMBER_NAME).as_deref(),
            Some("First Partner")
        );
        assert_eq!(
            get_value_in(&bundle, &map, "K1P_2", f::MEMBER_TIN).as_deref(),
            Some("222-22-2222")
        );
    }
}
