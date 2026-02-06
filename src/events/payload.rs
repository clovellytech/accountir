use crate::events::types::Event;
use serde_json;
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum PayloadError {
    #[error("Serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),
    #[error("Invalid hash length")]
    InvalidHashLength,
}

/// Serialize an event to JSON
pub fn serialize_event(event: &Event) -> Result<String, PayloadError> {
    Ok(serde_json::to_string(event)?)
}

/// Deserialize an event from JSON
pub fn deserialize_event(json: &str) -> Result<Event, PayloadError> {
    Ok(serde_json::from_str(json)?)
}

/// Serialize an event to pretty-printed JSON (for debugging/display)
pub fn serialize_event_pretty(event: &Event) -> Result<String, PayloadError> {
    Ok(serde_json::to_string_pretty(event)?)
}

/// Compute the hash of an event for the Merkle tree
/// Hash = SHA-256(event_type | payload | timestamp | user_id)
pub fn compute_event_hash(
    event: &Event,
    timestamp: &str,
    user_id: &str,
) -> Result<[u8; 32], PayloadError> {
    let payload = serialize_event(event)?;

    let mut hasher = Sha256::new();
    hasher.update(event.event_type().as_bytes());
    hasher.update(b"|");
    hasher.update(payload.as_bytes());
    hasher.update(b"|");
    hasher.update(timestamp.as_bytes());
    hasher.update(b"|");
    hasher.update(user_id.as_bytes());

    let result = hasher.finalize();
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&result);
    Ok(hash)
}

/// Convert a hash to hex string for display
pub fn hash_to_hex(hash: &[u8]) -> String {
    hex::encode(hash)
}

/// Convert a hex string back to hash bytes
pub fn hex_to_hash(hex_str: &str) -> Result<Vec<u8>, PayloadError> {
    hex::decode(hex_str).map_err(|_| PayloadError::InvalidHashLength)
}

/// Verify that a hash matches the expected value
pub fn verify_hash(hash: &[u8], expected: &[u8]) -> bool {
    hash == expected
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::types::EventAccountType;
    use chrono::NaiveDate;

    #[test]
    fn test_serialize_deserialize() {
        let event = Event::AccountCreated {
            account_id: "acc-001".to_string(),
            account_type: EventAccountType::Asset,
            account_number: "1000".to_string(),
            name: "Cash".to_string(),
            parent_id: None,
            currency: Some("USD".to_string()),
            description: None,
        };

        let json = serialize_event(&event).unwrap();
        let parsed = deserialize_event(&json).unwrap();

        assert_eq!(event.event_type(), parsed.event_type());
    }

    #[test]
    fn test_compute_hash() {
        let event = Event::AccountCreated {
            account_id: "acc-001".to_string(),
            account_type: EventAccountType::Asset,
            account_number: "1000".to_string(),
            name: "Cash".to_string(),
            parent_id: None,
            currency: Some("USD".to_string()),
            description: None,
        };

        let hash1 = compute_event_hash(&event, "2024-01-15T10:00:00Z", "user-001").unwrap();
        let hash2 = compute_event_hash(&event, "2024-01-15T10:00:00Z", "user-001").unwrap();

        // Same inputs should produce same hash
        assert_eq!(hash1, hash2);

        // Different timestamp should produce different hash
        let hash3 = compute_event_hash(&event, "2024-01-15T10:00:01Z", "user-001").unwrap();
        assert_ne!(hash1, hash3);

        // Different user should produce different hash
        let hash4 = compute_event_hash(&event, "2024-01-15T10:00:00Z", "user-002").unwrap();
        assert_ne!(hash1, hash4);
    }

    #[test]
    fn test_hash_to_hex() {
        let event = Event::CompanyCreated {
            company_id: "test-id".to_string(),
            name: "Test".to_string(),
            base_currency: "USD".to_string(),
            fiscal_year_start: 1,
        };

        let hash = compute_event_hash(&event, "2024-01-15T10:00:00Z", "user-001").unwrap();
        let hex = hash_to_hex(&hash);

        assert_eq!(hex.len(), 64); // SHA-256 produces 32 bytes = 64 hex chars

        let decoded = hex_to_hash(&hex).unwrap();
        assert_eq!(hash.to_vec(), decoded);
    }

    #[test]
    fn test_journal_entry_hash() {
        use crate::events::types::{JournalEntrySource, JournalLineData};

        let event = Event::JournalEntryPosted {
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

        let hash = compute_event_hash(&event, "2024-01-15T10:00:00Z", "user-001").unwrap();
        assert_eq!(hash.len(), 32);
    }
}
