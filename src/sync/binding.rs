//! Which group server a ledger file is a replica of.
//!
//! A binding answers one question — *whose log is this file a copy of?* — and it
//! is stored **inside that file** (`group_binding`, migration 017) rather than in
//! the machine's registry. Two failures motivate that:
//!
//! - **A moved file must keep its identity.** Ledgers get copied to a new laptop,
//!   restored from a backup, or pulled out of a sync folder. If the binding lived
//!   on the machine, the restored copy would either lose it (and silently become
//!   a stale local ledger that people keep typing into) or inherit whatever this
//!   machine was last pointed at.
//! - **A replica of group A must never be fed group B's log.** Two groups' logs
//!   both start at seq 1, so grafting one onto the other produces a file that
//!   looks contiguous and is complete nonsense. [`bind`] therefore refuses to
//!   replace a *different* existing binding, and refuses to bind at all if the
//!   file already has events.
//!
//! By construction this module has no credential fields. The binding says
//! *where*; who you are lives in memory only (see the desktop `session` module).

use crate::store::event_store::{EventStore, EventStoreError};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum BindingError {
    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),
    /// The file already holds events of its own. Binding it would mean grafting a
    /// server log onto an unrelated local one: the ids would collide and the
    /// hashes are unrelated, so the result could never be a prefix of the
    /// server's log. Connecting starts from a fresh, empty ledger.
    #[error(
        "this ledger already has {count} local event(s), so it cannot become a replica; \
         connect a new, empty ledger to the group instead"
    )]
    NotEmpty { count: i64 },
    /// Already a replica of some *other* group. Rebinding would silently
    /// reinterpret every event in the file as belonging to a different set of
    /// books.
    #[error("this ledger is already bound to group \"{existing}\" and cannot be rebound to \"{requested}\"")]
    AlreadyBound { existing: String, requested: String },
    #[error("stored binding is corrupt: {0}")]
    Corrupt(String),
}

/// Where this ledger's authoritative log lives, and how far we have followed it.
///
/// `last_server_head` / `last_synced_at` are **display state**, not the sync
/// cursor. The cursor is `MAX(events.id)` — derived from the data itself — so
/// there is no second number that can drift out of step with what was actually
/// applied. See [`super::replica::local_cursor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupBinding {
    pub group_id: String,
    pub instance_url: String,
    pub control_plane_url: String,
    pub bound_at: DateTime<Utc>,
    pub last_server_head: i64,
    pub last_synced_at: Option<DateTime<Utc>>,
}

/// Read the binding, if this ledger has one. `None` means a plain local ledger —
/// the normal state for a solo user, not a fault.
pub fn get(conn: &Connection) -> Result<Option<GroupBinding>, BindingError> {
    // A database that predates migration 017 simply isn't bound. Treating a
    // missing table as "no binding" keeps `get` callable on any ledger, which
    // matters because every business-open path calls it before deciding whether
    // local writes are allowed.
    if !table_exists(conn)? {
        return Ok(None);
    }
    let row = conn
        .query_row(
            "SELECT group_id, instance_url, control_plane_url, bound_at, last_server_head, last_synced_at
             FROM group_binding WHERE id = 1",
            [],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, Option<String>>(5)?,
                ))
            },
        )
        .optional()?;

    let Some((group_id, instance_url, control_plane_url, bound_at, head, synced_at)) = row else {
        return Ok(None);
    };
    Ok(Some(GroupBinding {
        group_id,
        instance_url,
        control_plane_url,
        bound_at: parse_time(&bound_at)?,
        last_server_head: head,
        last_synced_at: synced_at.as_deref().map(parse_time).transpose()?,
    }))
}

/// Bind this ledger to a group, refusing every case where the file cannot
/// honestly be a replica of it.
///
/// Re-binding to the *same* group is allowed and idempotent (a user who
/// re-pastes the same invite, or whose instance host moved, should not have to
/// start over). Anything else is refused rather than reconciled: there is no
/// merge in this design (SPEC §4.1), so a wrong binding is not something a later
/// sync can repair.
pub fn bind(
    conn: &Connection,
    group_id: &str,
    instance_url: &str,
    control_plane_url: &str,
) -> Result<GroupBinding, BindingError> {
    if let Some(existing) = get(conn)? {
        if existing.group_id != group_id {
            return Err(BindingError::AlreadyBound {
                existing: existing.group_id,
                requested: group_id.to_string(),
            });
        }
        // Same group: refresh where it lives, keep how far we have followed it.
        conn.execute(
            "UPDATE group_binding SET instance_url = ?1, control_plane_url = ?2 WHERE id = 1",
            params![instance_url, control_plane_url],
        )?;
        return get(conn)?.ok_or_else(|| BindingError::Corrupt("binding vanished".into()));
    }

    let count: i64 = conn.query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))?;
    if count > 0 {
        return Err(BindingError::NotEmpty { count });
    }

    let bound_at = Utc::now();
    conn.execute(
        "INSERT INTO group_binding (id, group_id, instance_url, control_plane_url, bound_at, last_server_head, last_synced_at)
         VALUES (1, ?1, ?2, ?3, ?4, 0, NULL)",
        params![group_id, instance_url, control_plane_url, bound_at.to_rfc3339()],
    )?;
    Ok(GroupBinding {
        group_id: group_id.to_string(),
        instance_url: instance_url.to_string(),
        control_plane_url: control_plane_url.to_string(),
        bound_at,
        last_server_head: 0,
        last_synced_at: None,
    })
}

/// Record that we have seen the server at `head`, for the UI's "synced 12s ago".
/// Never load-bearing: losing this write costs a stale label, not correctness.
pub fn record_sync(conn: &Connection, head: i64) -> Result<(), BindingError> {
    conn.execute(
        "UPDATE group_binding SET last_server_head = ?1, last_synced_at = ?2 WHERE id = 1",
        params![head, Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

/// Detach this ledger from its group.
///
/// The file keeps every event it has pulled, which is the point: disconnecting
/// leaves a readable, frozen copy of the books as of the last sync. It does not
/// turn back into a writable local ledger, because the ids in it belong to the
/// server's sequence and appending to them locally would fabricate history that
/// the group never agreed to.
pub fn unbind(conn: &Connection) -> Result<(), BindingError> {
    conn.execute("DELETE FROM group_binding WHERE id = 1", [])?;
    Ok(())
}

/// Convenience for the common `EventStore` caller.
pub fn get_for(store: &EventStore) -> Result<Option<GroupBinding>, BindingError> {
    get(store.connection())
}

/// Is this store a replica? The single question every local write path should be
/// asking before it appends.
pub fn is_bound(store: &EventStore) -> Result<bool, BindingError> {
    Ok(get_for(store)?.is_some())
}

fn table_exists(conn: &Connection) -> Result<bool, BindingError> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'group_binding'",
        [],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

fn parse_time(s: &str) -> Result<DateTime<Utc>, BindingError> {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| BindingError::Corrupt(e.to_string()))
}

impl From<EventStoreError> for BindingError {
    fn from(e: EventStoreError) -> Self {
        match e {
            EventStoreError::DatabaseError(e) => BindingError::Database(e),
            other => BindingError::Corrupt(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::types::{Event, EventEnvelope};
    use crate::store::migrations::SchemaStore;

    fn store() -> EventStore {
        let mut s = EventStore::in_memory().unwrap();
        s.init_schema().unwrap();
        s.run_migrations().unwrap();
        s
    }

    fn company_event() -> EventEnvelope {
        EventEnvelope::new(
            Event::CompanyCreated {
                company_id: "c1".into(),
                name: "Acme".into(),
                base_currency: "USD".into(),
                fiscal_year_start: 1,
            },
            "tester".into(),
        )
    }

    #[test]
    fn an_unbound_ledger_reports_no_binding() {
        let s = store();
        assert!(get(s.connection()).unwrap().is_none());
        assert!(!is_bound(&s).unwrap());
    }

    #[test]
    fn binding_round_trips_through_the_ledger_file() {
        let s = store();
        let b = bind(
            s.connection(),
            "acme",
            "https://acme.accountir.com",
            "https://app.accountir.com",
        )
        .unwrap();
        let read = get(s.connection()).unwrap().unwrap();
        assert_eq!(read, b);
        assert_eq!(read.group_id, "acme");
        assert_eq!(read.last_server_head, 0);
        assert!(read.last_synced_at.is_none());
    }

    /// The regression: a ledger someone has been using locally being turned into
    /// a replica. Its ids and hashes are unrelated to the server's, so the result
    /// could never be a prefix of the server's log — and the corruption would only
    /// show up later, as balances nobody else can reproduce.
    #[test]
    fn binding_is_refused_on_a_database_that_already_has_events() {
        let mut s = store();
        s.append(company_event()).unwrap();
        let err = bind(s.connection(), "acme", "https://i", "https://cp").unwrap_err();
        assert!(matches!(err, BindingError::NotEmpty { count: 1 }), "{err}");
        assert!(get(s.connection()).unwrap().is_none());
    }

    /// The regression: pointing an existing replica at a different group, which
    /// would reinterpret every event already in the file as another group's.
    #[test]
    fn rebinding_to_a_different_group_is_refused() {
        let s = store();
        bind(s.connection(), "acme", "https://i", "https://cp").unwrap();
        let err = bind(s.connection(), "other", "https://i2", "https://cp").unwrap_err();
        assert!(
            matches!(err, BindingError::AlreadyBound { .. }),
            "expected AlreadyBound, got {err}"
        );
        assert_eq!(get(s.connection()).unwrap().unwrap().group_id, "acme");
    }

    /// Re-pasting the same invite (or an admin moving the instance host) must not
    /// force the user to start over, and must not rewind how far we have synced.
    #[test]
    fn rebinding_to_the_same_group_updates_the_address_and_keeps_progress() {
        let s = store();
        bind(s.connection(), "acme", "https://old", "https://cp").unwrap();
        record_sync(s.connection(), 42).unwrap();
        let b = bind(s.connection(), "acme", "https://new", "https://cp").unwrap();
        assert_eq!(b.instance_url, "https://new");
        assert_eq!(b.last_server_head, 42);
        assert!(b.last_synced_at.is_some());
    }

    #[test]
    fn unbinding_leaves_the_events_in_place_as_a_frozen_copy() {
        let s = store();
        bind(s.connection(), "acme", "https://i", "https://cp").unwrap();
        unbind(s.connection()).unwrap();
        assert!(get(s.connection()).unwrap().is_none());
    }

    /// Guards against a credential column being added to the table that a plain
    /// file copy carries around.
    #[test]
    fn the_binding_table_has_no_credential_columns() {
        let s = store();
        let cols: Vec<String> = s
            .connection()
            .prepare("SELECT name FROM pragma_table_info('group_binding')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        for forbidden in ["token", "password", "secret", "api_key", "refresh"] {
            assert!(
                !cols.iter().any(|c| c.contains(forbidden)),
                "group_binding must never hold {forbidden}: {cols:?}"
            );
        }
    }
}
