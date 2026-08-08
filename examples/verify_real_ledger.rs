//! One-off: re-derive every event hash in a real ledger with the current code.
//!
//! Run against a ledger written by an older build to prove a change to an event's
//! shape did not disturb events already on disk. A chained log cannot tolerate a
//! serialization change: every event after the altered one would fail to verify.
//!
//!     cargo run --example verify_real_ledger -- /path/to/ledger.db
use accountir::events::payload::compute_event_hash;
use accountir::events::types::Event;

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: verify_real_ledger <db>");
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open");

    let mut stmt = conn
        .prepare("SELECT id, event_type, payload, hash, user_id, timestamp FROM events ORDER BY id")
        .unwrap();
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Vec<u8>>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
            ))
        })
        .unwrap();

    let (mut ok, mut bad, mut unparsed) = (0u32, 0u32, 0u32);
    for row in rows {
        let (id, kind, payload, hash, user, ts) = row.unwrap();
        let event: Event = match serde_json::from_str(&payload) {
            Ok(e) => e,
            Err(e) => {
                println!("event {id} ({kind}): DOES NOT PARSE with current code: {e}");
                unparsed += 1;
                continue;
            }
        };
        match compute_event_hash(&event, &ts, &user) {
            Ok(h) if h.as_slice() == hash.as_slice() => ok += 1,
            Ok(_) => {
                println!("event {id} ({kind}): HASH MISMATCH — serialization changed");
                bad += 1;
            }
            Err(e) => {
                println!("event {id} ({kind}): hash error {e}");
                bad += 1;
            }
        }
    }
    println!("{path}: {ok} verified, {bad} mismatched, {unparsed} unparsable");
    if bad > 0 || unparsed > 0 {
        std::process::exit(1);
    }
}
