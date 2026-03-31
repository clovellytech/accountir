pub mod import;

use chrono::{DateTime, NaiveDate, Utc};
use flate2::read::GzDecoder;
use quick_xml::events::{BytesStart, Event as XmlEvent};
use quick_xml::Reader;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum GnuCashError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("XML error: {0}")]
    Xml(#[from] quick_xml::Error),
    #[error("Parse error: {0}")]
    Parse(String),
    #[error("Import error: {0}")]
    Import(String),
    #[error("Event store error: {0}")]
    EventStore(String),
}

#[derive(Debug, Clone)]
pub struct GncCommodity {
    pub space: String,
    pub id: String,
}

#[derive(Debug, Clone)]
pub struct GncAccount {
    pub guid: String,
    pub name: String,
    pub account_type: String,
    pub commodity: Option<GncCommodity>,
    pub description: String,
    pub parent_guid: Option<String>,
    pub is_placeholder: bool,
}

#[derive(Debug, Clone)]
pub struct GncSplit {
    pub guid: String,
    pub reconciled_state: String,
    pub value_num: i64,
    pub value_denom: i64,
    pub quantity_num: i64,
    pub quantity_denom: i64,
    pub account_guid: String,
    pub memo: String,
}

#[derive(Debug, Clone)]
pub struct GncTransaction {
    pub guid: String,
    pub currency: GncCommodity,
    pub date_posted: NaiveDate,
    pub date_entered: DateTime<Utc>,
    pub description: String,
    pub num: String,
    pub splits: Vec<GncSplit>,
}

#[derive(Debug, Clone)]
pub struct GncBook {
    pub commodities: Vec<GncCommodity>,
    pub accounts: Vec<GncAccount>,
    pub transactions: Vec<GncTransaction>,
}

/// Parse a GnuCash fraction string like "10000000/100" into (num, denom)
pub fn parse_fraction(s: &str) -> Result<(i64, i64), GnuCashError> {
    let parts: Vec<&str> = s.trim().split('/').collect();
    if parts.len() != 2 {
        return Err(GnuCashError::Parse(format!("Invalid fraction: {}", s)));
    }
    let num: i64 = parts[0]
        .parse()
        .map_err(|_| GnuCashError::Parse(format!("Invalid numerator: {}", parts[0])))?;
    let denom: i64 = parts[1]
        .parse()
        .map_err(|_| GnuCashError::Parse(format!("Invalid denominator: {}", parts[1])))?;
    Ok((num, denom))
}

/// Convert a fraction (num/denom) to cents (hundredths)
pub fn fraction_to_cents(num: i64, denom: i64) -> i64 {
    if denom == 0 {
        return 0;
    }
    // num/denom * 100 = num * 100 / denom
    // Use i128 to avoid overflow
    let result = (num as i128 * 100) / denom as i128;
    result as i64
}

/// Parse a GnuCash date string like "2025-04-15 10:59:00 +0000" into NaiveDate
pub fn parse_gnucash_date(s: &str) -> Result<NaiveDate, GnuCashError> {
    let s = s.trim();
    // Try full datetime format first
    if s.len() >= 10 {
        NaiveDate::parse_from_str(&s[..10], "%Y-%m-%d")
            .map_err(|e| GnuCashError::Parse(format!("Invalid date '{}': {}", s, e)))
    } else {
        Err(GnuCashError::Parse(format!("Date too short: {}", s)))
    }
}

/// Parse a GnuCash datetime string into DateTime<Utc>
pub fn parse_gnucash_datetime(s: &str) -> Result<DateTime<Utc>, GnuCashError> {
    let s = s.trim();
    // Format: "2025-04-15 10:59:00 +0000"
    let dt = chrono::NaiveDateTime::parse_from_str(
        &s.replace(" +0000", "").replace(" -0000", ""),
        "%Y-%m-%d %H:%M:%S",
    )
    .map_err(|e| GnuCashError::Parse(format!("Invalid datetime '{}': {}", s, e)))?;
    Ok(dt.and_utc())
}

/// Check if tag name (possibly with namespace prefix) matches expected local name
fn tag_matches(tag: &[u8], local_name: &[u8]) -> bool {
    // Tag might be "act:name" or just "name"
    if tag == local_name {
        return true;
    }
    // Check for namespace:localname pattern
    if let Some(pos) = tag.iter().position(|&b| b == b':') {
        &tag[pos + 1..] == local_name
    } else {
        false
    }
}

/// Check if tag matches namespace:localname exactly
fn tag_matches_full(tag: &[u8], expected: &[u8]) -> bool {
    tag == expected
}

/// Read text content until the matching end tag
fn read_text<R: BufRead>(
    reader: &mut Reader<R>,
    buf: &mut Vec<u8>,
) -> Result<String, GnuCashError> {
    let mut text = String::new();
    loop {
        match reader.read_event_into(buf)? {
            XmlEvent::Text(e) => {
                text.push_str(&String::from_utf8_lossy(e.as_ref()));
            }
            XmlEvent::CData(e) => {
                text.push_str(&String::from_utf8_lossy(e.as_ref()));
            }
            XmlEvent::End(_) => break,
            XmlEvent::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(text)
}

/// Get the value of the "value" attribute from a start tag
fn get_attr_value(tag: &BytesStart, attr_name: &[u8]) -> Option<String> {
    for attr in tag.attributes().flatten() {
        if attr.key.as_ref() == attr_name {
            return Some(String::from_utf8_lossy(&attr.value).to_string());
        }
    }
    None
}

fn parse_commodity<R: BufRead>(
    reader: &mut Reader<R>,
    buf: &mut Vec<u8>,
) -> Result<GncCommodity, GnuCashError> {
    let mut space = String::new();
    let mut id = String::new();

    loop {
        match reader.read_event_into(buf)? {
            XmlEvent::Start(ref e) => {
                let tag = e.name().as_ref().to_vec();
                if tag_matches(&tag, b"space") {
                    space = read_text(reader, buf)?;
                } else if tag_matches(&tag, b"id") {
                    id = read_text(reader, buf)?;
                } else {
                    // Skip unknown element
                    read_text(reader, buf)?;
                }
            }
            XmlEvent::End(ref e) => {
                let tag = e.name().as_ref().to_vec();
                if tag_matches_full(&tag, b"gnc:commodity")
                    || tag_matches_full(&tag, b"act:commodity")
                    || tag_matches_full(&tag, b"trn:currency")
                {
                    break;
                }
            }
            XmlEvent::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(GncCommodity { space, id })
}

fn parse_account<R: BufRead>(
    reader: &mut Reader<R>,
    buf: &mut Vec<u8>,
) -> Result<GncAccount, GnuCashError> {
    let mut guid = String::new();
    let mut name = String::new();
    let mut account_type = String::new();
    let mut commodity: Option<GncCommodity> = None;
    let mut description = String::new();
    let mut parent_guid: Option<String> = None;
    let mut is_placeholder = false;
    let mut in_slots = false;
    let mut current_slot_key = String::new();

    loop {
        match reader.read_event_into(buf)? {
            XmlEvent::Start(ref e) => {
                let tag = e.name().as_ref().to_vec();
                if in_slots {
                    if tag_matches(&tag, b"key") {
                        current_slot_key = read_text(reader, buf)?;
                    } else if tag_matches(&tag, b"value") {
                        let val = read_text(reader, buf)?;
                        if current_slot_key == "placeholder" && val == "true" {
                            is_placeholder = true;
                        }
                    } else if tag_matches(&tag, b"slot") {
                        // Nested slot, just continue
                    } else {
                        read_text(reader, buf)?;
                    }
                } else if tag_matches_full(&tag, b"act:id") {
                    guid = read_text(reader, buf)?;
                } else if tag_matches_full(&tag, b"act:name") {
                    name = read_text(reader, buf)?;
                } else if tag_matches_full(&tag, b"act:type") {
                    account_type = read_text(reader, buf)?;
                } else if tag_matches_full(&tag, b"act:commodity") {
                    commodity = Some(parse_commodity(reader, buf)?);
                } else if tag_matches_full(&tag, b"act:description") {
                    description = read_text(reader, buf)?;
                } else if tag_matches_full(&tag, b"act:parent") {
                    parent_guid = Some(read_text(reader, buf)?);
                } else if tag_matches_full(&tag, b"act:slots") {
                    in_slots = true;
                } else {
                    read_text(reader, buf)?;
                }
            }
            XmlEvent::End(ref e) => {
                let tag = e.name().as_ref().to_vec();
                if tag_matches_full(&tag, b"act:slots") {
                    in_slots = false;
                } else if tag_matches_full(&tag, b"gnc:account") {
                    break;
                }
            }
            XmlEvent::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(GncAccount {
        guid,
        name,
        account_type,
        commodity,
        description,
        parent_guid,
        is_placeholder,
    })
}

fn parse_split<R: BufRead>(
    reader: &mut Reader<R>,
    buf: &mut Vec<u8>,
) -> Result<GncSplit, GnuCashError> {
    let mut guid = String::new();
    let mut reconciled_state = String::new();
    let mut value_num: i64 = 0;
    let mut value_denom: i64 = 1;
    let mut quantity_num: i64 = 0;
    let mut quantity_denom: i64 = 1;
    let mut account_guid = String::new();
    let mut memo = String::new();

    loop {
        match reader.read_event_into(buf)? {
            XmlEvent::Start(ref e) => {
                let tag = e.name().as_ref().to_vec();
                if tag_matches_full(&tag, b"split:id") {
                    guid = read_text(reader, buf)?;
                } else if tag_matches_full(&tag, b"split:reconciled-state") {
                    reconciled_state = read_text(reader, buf)?;
                } else if tag_matches_full(&tag, b"split:value") {
                    let val = read_text(reader, buf)?;
                    let (n, d) = parse_fraction(&val)?;
                    value_num = n;
                    value_denom = d;
                } else if tag_matches_full(&tag, b"split:quantity") {
                    let val = read_text(reader, buf)?;
                    let (n, d) = parse_fraction(&val)?;
                    quantity_num = n;
                    quantity_denom = d;
                } else if tag_matches_full(&tag, b"split:account") {
                    account_guid = read_text(reader, buf)?;
                } else if tag_matches_full(&tag, b"split:memo") {
                    memo = read_text(reader, buf)?;
                } else {
                    read_text(reader, buf)?;
                }
            }
            XmlEvent::End(ref e) => {
                let tag = e.name().as_ref().to_vec();
                if tag_matches_full(&tag, b"trn:split") {
                    break;
                }
            }
            XmlEvent::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(GncSplit {
        guid,
        reconciled_state,
        value_num,
        value_denom,
        quantity_num,
        quantity_denom,
        account_guid,
        memo,
    })
}

fn parse_date_element<R: BufRead>(
    reader: &mut Reader<R>,
    buf: &mut Vec<u8>,
) -> Result<String, GnuCashError> {
    let mut date_str = String::new();

    loop {
        match reader.read_event_into(buf)? {
            XmlEvent::Start(ref e) => {
                let tag = e.name().as_ref().to_vec();
                if tag_matches_full(&tag, b"ts:date") {
                    date_str = read_text(reader, buf)?;
                } else {
                    read_text(reader, buf)?;
                }
            }
            XmlEvent::End(ref e) => {
                let tag = e.name().as_ref().to_vec();
                if tag_matches_full(&tag, b"trn:date-posted")
                    || tag_matches_full(&tag, b"trn:date-entered")
                {
                    break;
                }
            }
            XmlEvent::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(date_str)
}

fn parse_transaction<R: BufRead>(
    reader: &mut Reader<R>,
    buf: &mut Vec<u8>,
) -> Result<GncTransaction, GnuCashError> {
    let mut guid = String::new();
    let mut currency = GncCommodity {
        space: String::new(),
        id: String::new(),
    };
    let mut date_posted = chrono::NaiveDate::from_ymd_opt(2000, 1, 1).unwrap();
    let mut date_entered = Utc::now();
    let mut description = String::new();
    let mut num = String::new();
    let mut splits = Vec::new();
    let mut in_splits = false;

    loop {
        match reader.read_event_into(buf)? {
            XmlEvent::Start(ref e) => {
                let tag = e.name().as_ref().to_vec();
                if in_splits {
                    if tag_matches_full(&tag, b"trn:split") {
                        splits.push(parse_split(reader, buf)?);
                    } else {
                        read_text(reader, buf)?;
                    }
                } else if tag_matches_full(&tag, b"trn:id") {
                    guid = read_text(reader, buf)?;
                } else if tag_matches_full(&tag, b"trn:currency") {
                    currency = parse_commodity(reader, buf)?;
                } else if tag_matches_full(&tag, b"trn:date-posted") {
                    let s = parse_date_element(reader, buf)?;
                    date_posted = parse_gnucash_date(&s)?;
                } else if tag_matches_full(&tag, b"trn:date-entered") {
                    let s = parse_date_element(reader, buf)?;
                    date_entered = parse_gnucash_datetime(&s)?;
                } else if tag_matches_full(&tag, b"trn:description") {
                    description = read_text(reader, buf)?;
                } else if tag_matches_full(&tag, b"trn:num") {
                    num = read_text(reader, buf)?;
                } else if tag_matches_full(&tag, b"trn:splits") {
                    in_splits = true;
                } else {
                    read_text(reader, buf)?;
                }
            }
            XmlEvent::End(ref e) => {
                let tag = e.name().as_ref().to_vec();
                if tag_matches_full(&tag, b"trn:splits") {
                    in_splits = false;
                } else if tag_matches_full(&tag, b"gnc:transaction") {
                    break;
                }
            }
            XmlEvent::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(GncTransaction {
        guid,
        currency,
        date_posted,
        date_entered,
        description,
        num,
        splits,
    })
}

/// Parse a GnuCash XML file (gzip-compressed or plain XML)
pub fn parse_gnucash_file(path: &Path) -> Result<GncBook, GnuCashError> {
    let file = File::open(path)?;
    let mut header = [0u8; 2];
    let mut file = BufReader::new(file);
    file.read_exact(&mut header)?;

    // Reopen to reset position
    let file = File::open(path)?;
    let file = BufReader::new(file);

    // Check gzip magic bytes
    let is_gzip = header[0] == 0x1f && header[1] == 0x8b;

    if is_gzip {
        let decoder = GzDecoder::new(file);
        let reader = BufReader::new(decoder);
        parse_gnucash_xml(reader)
    } else {
        parse_gnucash_xml(file)
    }
}

fn parse_gnucash_xml<R: BufRead>(reader: R) -> Result<GncBook, GnuCashError> {
    let mut xml_reader = Reader::from_reader(reader);
    xml_reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    let mut commodities = Vec::new();
    let mut accounts = Vec::new();
    let mut transactions = Vec::new();

    loop {
        match xml_reader.read_event_into(&mut buf)? {
            XmlEvent::Start(ref e) => {
                let tag = e.name().as_ref().to_vec();
                if tag_matches_full(&tag, b"gnc:commodity") {
                    // Check if this is a commodity definition (has version attribute)
                    // vs a commodity reference inside an account
                    if get_attr_value(e, b"version").is_some() {
                        commodities.push(parse_commodity(&mut xml_reader, &mut buf)?);
                    }
                } else if tag_matches_full(&tag, b"gnc:account") {
                    // Skip the count-data versions (they have a "type" attribute with value "new")
                    if get_attr_value(e, b"version").is_some() {
                        accounts.push(parse_account(&mut xml_reader, &mut buf)?);
                    }
                } else if tag_matches_full(&tag, b"gnc:transaction")
                    && get_attr_value(e, b"version").is_some()
                {
                    transactions.push(parse_transaction(&mut xml_reader, &mut buf)?);
                }
            }
            XmlEvent::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(GncBook {
        commodities,
        accounts,
        transactions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_fraction() {
        assert_eq!(parse_fraction("10000000/100").unwrap(), (10000000, 100));
        assert_eq!(parse_fraction("-5000/100").unwrap(), (-5000, 100));
        assert_eq!(parse_fraction("0/100").unwrap(), (0, 100));
    }

    #[test]
    fn test_fraction_to_cents() {
        assert_eq!(fraction_to_cents(10000, 100), 10000);
        assert_eq!(fraction_to_cents(-5000, 100), -5000);
        assert_eq!(fraction_to_cents(0, 100), 0);
        assert_eq!(fraction_to_cents(1050, 100), 1050);
        assert_eq!(fraction_to_cents(100, 1), 10000);
    }

    #[test]
    fn test_parse_gnucash_date() {
        let d = parse_gnucash_date("2025-04-15 10:59:00 +0000").unwrap();
        assert_eq!(d, NaiveDate::from_ymd_opt(2025, 4, 15).unwrap());
    }

    #[test]
    fn test_parse_gnucash_datetime() {
        let dt = parse_gnucash_datetime("2025-04-15 10:59:00 +0000").unwrap();
        assert_eq!(
            dt.date_naive(),
            NaiveDate::from_ymd_opt(2025, 4, 15).unwrap()
        );
    }

    #[test]
    fn test_tag_matches() {
        assert!(tag_matches(b"act:name", b"name"));
        assert!(tag_matches(b"name", b"name"));
        assert!(!tag_matches(b"act:id", b"name"));
    }

    #[test]
    fn test_tag_matches_full() {
        assert!(tag_matches_full(b"act:name", b"act:name"));
        assert!(!tag_matches_full(b"trn:name", b"act:name"));
    }
}
