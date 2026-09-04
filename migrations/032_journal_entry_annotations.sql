-- Notes a person adds to a journal entry after it was posted.
--
-- # The gap this fills
--
-- `JournalEntryAnnotated` has existed since the first migration and its projection
-- did nothing — a literal `// For now, we'll skip this`. The command appended the
-- event, returned Ok, and the note was never readable again. Its own test only
-- checked that the call succeeded, which is why nobody noticed: a test that
-- asserts a write returned Ok, and never that the thing written can be read back,
-- passes just as happily against a projection that discards it.
--
-- # Why annotate rather than edit
--
-- An imported entry's memo is what the bank said. A cheque arrives from Plaid as
-- `CHECK#1234` and that string is evidence: it is how the entry is matched to the
-- statement, how a re-import is recognised as a duplicate, and what an examiner
-- sees on the bank's own record. Rewriting it to "January rent" would destroy the
-- only durable link between the ledger and the account it came from, and would do
-- it silently, because the replacement reads perfectly well.
--
-- So the memo stays and the note sits beside it. The entry then carries both what
-- the bank called it and what it was for, which is more than an edit could have
-- said.
--
-- # Why every note is kept
--
-- One row per annotation event rather than one per entry. A note that was
-- corrected is part of the record, the same way a voided entry and a superseded
-- depreciation posting are kept rather than removed — and the reader who needs to
-- know that a cheque was described as one thing and later as another is exactly
-- the reader this table exists for. The current description is the most recent
-- row; the rest is history, and history is cheap.
--
-- Keyed by the event that created it, which makes replay idempotent: the same
-- event applied twice writes the same row rather than a second copy. `entry_id`
-- carries no foreign key to `journal_entries`, deliberately, for the reason
-- migration 025 gives — `Projector::rebuild` truncates projections and replays,
-- and a foreign key would fight the order rows come back in.
CREATE TABLE IF NOT EXISTS journal_entry_annotations (
    event_id INTEGER PRIMARY KEY REFERENCES events(id),
    entry_id TEXT NOT NULL,
    annotation TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_journal_entry_annotations_entry
    ON journal_entry_annotations(entry_id, event_id);
