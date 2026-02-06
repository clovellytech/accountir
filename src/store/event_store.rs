use crate::events::payload::{compute_event_hash, serialize_event};
use crate::events::types::{Event, EventEnvelope, StoredEvent};
use crate::events::validation::validate_event;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum EventStoreError {
    #[error("Database error: {0}")]
    DatabaseError(#[from] rusqlite::Error),
    #[error("Serialization error: {0}")]
    SerializationError(String),
    #[error("Validation error: {0}")]
    ValidationError(#[from] crate::events::validation::ValidationError),
    #[error("Event not found: {0}")]
    NotFound(i64),
    #[error("Duplicate event hash")]
    DuplicateHash,
}

/// The event store manages the append-only event log
pub struct EventStore {
    conn: Connection,
}

impl EventStore {
    /// Open an existing database or create a new one
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, EventStoreError> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        Ok(Self { conn })
    }

    /// Create an in-memory event store (for testing)
    pub fn in_memory() -> Result<Self, EventStoreError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        Ok(Self { conn })
    }

    /// Get the underlying connection (for migrations, etc.)
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Get a mutable reference to the connection
    pub fn connection_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// Append a new event to the store
    pub fn append(&mut self, envelope: EventEnvelope) -> Result<StoredEvent, EventStoreError> {
        // Validate the event
        validate_event(&envelope.event)?;

        // Serialize the event
        let payload = serialize_event(&envelope.event)
            .map_err(|e| EventStoreError::SerializationError(e.to_string()))?;

        // Compute the hash
        let timestamp_str = envelope.timestamp.to_rfc3339();
        let hash = compute_event_hash(&envelope.event, &timestamp_str, &envelope.user_id)
            .map_err(|e| EventStoreError::SerializationError(e.to_string()))?;

        // Insert the event
        let result = self.conn.execute(
            "INSERT INTO events (event_type, payload, hash, user_id, timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                envelope.event.event_type(),
                payload,
                hash.as_slice(),
                envelope.user_id,
                timestamp_str,
            ],
        );

        match result {
            Ok(_) => {
                let id = self.conn.last_insert_rowid();
                Ok(StoredEvent::new(
                    id,
                    envelope.event,
                    hash.to_vec(),
                    envelope.user_id,
                    envelope.timestamp,
                ))
            }
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Err(EventStoreError::DuplicateHash)
            }
            Err(e) => Err(EventStoreError::DatabaseError(e)),
        }
    }

    /// Get an event by ID
    pub fn get(&self, id: i64) -> Result<StoredEvent, EventStoreError> {
        let row = self
            .conn
            .query_row(
                "SELECT id, event_type, payload, hash, user_id, timestamp
                 FROM events WHERE id = ?1",
                [id],
                |row| {
                    let payload: String = row.get(2)?;
                    let hash: Vec<u8> = row.get(3)?;
                    let user_id: String = row.get(4)?;
                    let timestamp_str: String = row.get(5)?;

                    Ok((payload, hash, user_id, timestamp_str))
                },
            )
            .optional()?;

        match row {
            Some((payload, hash, user_id, timestamp_str)) => {
                let event: Event = serde_json::from_str(&payload)
                    .map_err(|e| EventStoreError::SerializationError(e.to_string()))?;
                let timestamp = DateTime::parse_from_rfc3339(&timestamp_str)
                    .map_err(|e| EventStoreError::SerializationError(e.to_string()))?
                    .with_timezone(&Utc);

                Ok(StoredEvent::new(id, event, hash, user_id, timestamp))
            }
            None => Err(EventStoreError::NotFound(id)),
        }
    }

    /// Get all events in order
    pub fn get_all(&self) -> Result<Vec<StoredEvent>, EventStoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, event_type, payload, hash, user_id, timestamp
             FROM events ORDER BY id ASC",
        )?;

        let events = stmt
            .query_map([], |row| {
                let id: i64 = row.get(0)?;
                let payload: String = row.get(2)?;
                let hash: Vec<u8> = row.get(3)?;
                let user_id: String = row.get(4)?;
                let timestamp_str: String = row.get(5)?;

                Ok((id, payload, hash, user_id, timestamp_str))
            })?
            .filter_map(|r| r.ok())
            .filter_map(|(id, payload, hash, user_id, timestamp_str)| {
                let event: Event = serde_json::from_str(&payload).ok()?;
                let timestamp = DateTime::parse_from_rfc3339(&timestamp_str)
                    .ok()?
                    .with_timezone(&Utc);
                Some(StoredEvent::new(id, event, hash, user_id, timestamp))
            })
            .collect();

        Ok(events)
    }

    /// Get events by type
    pub fn get_by_type(&self, event_type: &str) -> Result<Vec<StoredEvent>, EventStoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, event_type, payload, hash, user_id, timestamp
             FROM events WHERE event_type = ?1 ORDER BY id ASC",
        )?;

        let events = stmt
            .query_map([event_type], |row| {
                let id: i64 = row.get(0)?;
                let payload: String = row.get(2)?;
                let hash: Vec<u8> = row.get(3)?;
                let user_id: String = row.get(4)?;
                let timestamp_str: String = row.get(5)?;

                Ok((id, payload, hash, user_id, timestamp_str))
            })?
            .filter_map(|r| r.ok())
            .filter_map(|(id, payload, hash, user_id, timestamp_str)| {
                let event: Event = serde_json::from_str(&payload).ok()?;
                let timestamp = DateTime::parse_from_rfc3339(&timestamp_str)
                    .ok()?
                    .with_timezone(&Utc);
                Some(StoredEvent::new(id, event, hash, user_id, timestamp))
            })
            .collect();

        Ok(events)
    }

    /// Get events after a specific ID (for sync)
    pub fn get_after(&self, after_id: i64) -> Result<Vec<StoredEvent>, EventStoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, event_type, payload, hash, user_id, timestamp
             FROM events WHERE id > ?1 ORDER BY id ASC",
        )?;

        let events = stmt
            .query_map([after_id], |row| {
                let id: i64 = row.get(0)?;
                let payload: String = row.get(2)?;
                let hash: Vec<u8> = row.get(3)?;
                let user_id: String = row.get(4)?;
                let timestamp_str: String = row.get(5)?;

                Ok((id, payload, hash, user_id, timestamp_str))
            })?
            .filter_map(|r| r.ok())
            .filter_map(|(id, payload, hash, user_id, timestamp_str)| {
                let event: Event = serde_json::from_str(&payload).ok()?;
                let timestamp = DateTime::parse_from_rfc3339(&timestamp_str)
                    .ok()?
                    .with_timezone(&Utc);
                Some(StoredEvent::new(id, event, hash, user_id, timestamp))
            })
            .collect();

        Ok(events)
    }

    /// Get the count of events
    pub fn count(&self) -> Result<i64, EventStoreError> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
        Ok(count)
    }

    /// Get the latest event ID
    pub fn latest_id(&self) -> Result<Option<i64>, EventStoreError> {
        let id: Option<i64> = self
            .conn
            .query_row("SELECT MAX(id) FROM events", [], |row| row.get(0))?;
        Ok(id)
    }

    /// Get all event hashes (for Merkle tree building)
    pub fn get_all_hashes(&self) -> Result<Vec<Vec<u8>>, EventStoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT hash FROM events ORDER BY id ASC")?;

        let hashes: Vec<Vec<u8>> = stmt
            .query_map([], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();

        Ok(hashes)
    }

    /// Get event hash by ID
    pub fn get_hash(&self, id: i64) -> Result<Vec<u8>, EventStoreError> {
        let hash: Vec<u8> = self
            .conn
            .query_row("SELECT hash FROM events WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()?
            .ok_or(EventStoreError::NotFound(id))?;
        Ok(hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::types::{EventAccountType, JournalEntrySource, JournalLineData};
    use crate::store::migrations::init_schema;
    use chrono::NaiveDate;

    fn setup_store() -> EventStore {
        let store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        store
    }

    #[test]
    fn test_append_and_get() {
        let mut store = setup_store();

        let event = Event::AccountCreated {
            account_id: "acc-001".to_string(),
            account_type: EventAccountType::Asset,
            account_number: "1000".to_string(),
            name: "Cash".to_string(),
            parent_id: None,
            currency: Some("USD".to_string()),
            description: None,
        };

        let envelope = EventEnvelope::new(event, "user-001".to_string());
        let stored = store.append(envelope).unwrap();

        assert_eq!(stored.id, 1);
        assert_eq!(stored.user_id, "user-001");

        let retrieved = store.get(1).unwrap();
        assert_eq!(retrieved.id, stored.id);
    }

    #[test]
    fn test_duplicate_hash_rejected() {
        let mut store = setup_store();

        let event = Event::AccountCreated {
            account_id: "acc-001".to_string(),
            account_type: EventAccountType::Asset,
            account_number: "1000".to_string(),
            name: "Cash".to_string(),
            parent_id: None,
            currency: None,
            description: None,
        };

        let timestamp = Utc::now();
        let envelope1 =
            EventEnvelope::with_timestamp(event.clone(), "user-001".to_string(), timestamp);
        let envelope2 = EventEnvelope::with_timestamp(event, "user-001".to_string(), timestamp);

        store.append(envelope1).unwrap();
        let result = store.append(envelope2);

        assert!(matches!(result, Err(EventStoreError::DuplicateHash)));
    }

    #[test]
    fn test_get_all() {
        let mut store = setup_store();

        // Add multiple events
        for i in 1..=5 {
            let event = Event::AccountCreated {
                account_id: format!("acc-{:03}", i),
                account_type: EventAccountType::Asset,
                account_number: format!("{}", 1000 + i),
                name: format!("Account {}", i),
                parent_id: None,
                currency: None,
                description: None,
            };
            let envelope = EventEnvelope::new(event, "user-001".to_string());
            store.append(envelope).unwrap();
        }

        let all = store.get_all().unwrap();
        assert_eq!(all.len(), 5);
    }

    #[test]
    fn test_get_by_type() {
        let mut store = setup_store();

        // Add account events
        let event1 = Event::AccountCreated {
            account_id: "acc-001".to_string(),
            account_type: EventAccountType::Asset,
            account_number: "1000".to_string(),
            name: "Cash".to_string(),
            parent_id: None,
            currency: None,
            description: None,
        };
        store
            .append(EventEnvelope::new(event1, "user-001".to_string()))
            .unwrap();

        // Add journal entry event
        let event2 = Event::JournalEntryPosted {
            entry_id: "entry-001".to_string(),
            date: NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
            memo: "Test".to_string(),
            lines: vec![
                JournalLineData {
                    line_id: "line-001".to_string(),
                    account_id: "acc-001".to_string(),
                    amount: 10000,
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
                JournalLineData {
                    line_id: "line-002".to_string(),
                    account_id: "acc-002".to_string(),
                    amount: -10000,
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
            ],
            reference: None,
            source: Some(JournalEntrySource::Manual),
        };
        store
            .append(EventEnvelope::new(event2, "user-001".to_string()))
            .unwrap();

        let accounts = store.get_by_type("account_created").unwrap();
        assert_eq!(accounts.len(), 1);

        let entries = store.get_by_type("journal_entry_posted").unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn test_get_after() {
        let mut store = setup_store();

        // Add 5 events
        for i in 1..=5 {
            let event = Event::AccountCreated {
                account_id: format!("acc-{:03}", i),
                account_type: EventAccountType::Asset,
                account_number: format!("{}", 1000 + i),
                name: format!("Account {}", i),
                parent_id: None,
                currency: None,
                description: None,
            };
            store
                .append(EventEnvelope::new(event, "user-001".to_string()))
                .unwrap();
        }

        let after_3 = store.get_after(3).unwrap();
        assert_eq!(after_3.len(), 2);
        assert_eq!(after_3[0].id, 4);
        assert_eq!(after_3[1].id, 5);
    }

    #[test]
    fn test_count() {
        let mut store = setup_store();

        assert_eq!(store.count().unwrap(), 0);

        let event = Event::AccountCreated {
            account_id: "acc-001".to_string(),
            account_type: EventAccountType::Asset,
            account_number: "1000".to_string(),
            name: "Cash".to_string(),
            parent_id: None,
            currency: None,
            description: None,
        };
        store
            .append(EventEnvelope::new(event, "user-001".to_string()))
            .unwrap();

        assert_eq!(store.count().unwrap(), 1);
    }
}
