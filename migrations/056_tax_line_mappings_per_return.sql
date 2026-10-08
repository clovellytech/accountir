-- Each return keeps its own account-to-line assignments.
--
-- # What was wrong with one row per account and year
--
-- Form 1065 and Schedule C shared `tax_line_mappings`, keyed by account and
-- year, so mapping an account on the Schedule C page overwrote its Form 1065
-- line. A business set to the wrong type for a while, or one that changes what
-- it files, lost the partnership mapping it had built. Worse, inheritance down
-- the account tree read both vocabularies at once: a child still carrying a
-- 1065 key stopped its parent's Schedule C line from reaching it, because the
-- nearest answer in the tree was a line of the other form.
--
-- `form` is '1065' or 'schedule_c' (`tax::ReturnForm::as_str`). Defaulted to
-- '1065' so a row written by code that does not name a form — the adoption of
-- pre-027 rows, tests building a mapping without a log — lands where every such
-- writer meant it to.
--
-- # Why this rebuilds rather than adding a column
--
-- The primary key widens, which SQLite cannot do in place — see migration 038.
-- The form is derived rather than selected, so this works whether or not the old
-- table already had the column.
SAVEPOINT m056;

DROP TABLE IF EXISTS tax_line_mappings_per_return;
CREATE TABLE tax_line_mappings_per_return (
    account_id       TEXT NOT NULL,
    form             TEXT NOT NULL DEFAULT '1065',
    effective_from   INTEGER NOT NULL,
    line_key         TEXT NOT NULL,
    updated_at       TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at_event INTEGER REFERENCES events(id),
    PRIMARY KEY (account_id, form, effective_from)
);

-- Every row that is here now stays, filed under the return its key belongs to.
-- Schedule C keys are prefixed `sc` and no Form 1065 key is (both catalogues'
-- tests hold them to it); `off` was only ever written for Form 1065.
INSERT INTO tax_line_mappings_per_return
        (account_id, form, effective_from, line_key, updated_at, updated_at_event)
    SELECT account_id,
           CASE WHEN line_key LIKE 'sc%' THEN 'schedule_c' ELSE '1065' END,
           effective_from, line_key, updated_at, updated_at_event
      FROM tax_line_mappings;

-- And the assignments the shared row lost, recovered from the log.
--
-- Where somebody mapped an account for one return and later for the other, the
-- second overwrote the first. The log still says what the first was. For each
-- account, return and year, the most recent set survives unless a clear came
-- after it — and a clear from before this migration names no form, because it
-- cleared the one shared row, so it counts against both. This is exactly what
-- replaying the log through the new projector produces, so a later rebuild
-- agrees with it.
--
-- `INSERT OR IGNORE`: a row copied above is the projection's own answer for its
-- return and wins.
INSERT OR IGNORE INTO tax_line_mappings_per_return
        (account_id, form, effective_from, line_key, updated_at_event)
    WITH ev AS (
        SELECT id,
               event_type,
               json_extract(payload, '$.account_id') AS account_id,
               COALESCE(json_extract(payload, '$.effective_from'), 0) AS effective_from,
               json_extract(payload, '$.line_key') AS line_key,
               json_extract(payload, '$.form') AS form
          FROM events
         WHERE event_type IN ('tax_line_mapping_set', 'tax_line_mapping_cleared')
           AND json_valid(payload)
    ),
    sets AS (
        SELECT id, account_id, effective_from, line_key,
               COALESCE(form,
                        CASE WHEN line_key LIKE 'sc%' THEN 'schedule_c' ELSE '1065' END)
                   AS form
          FROM ev WHERE event_type = 'tax_line_mapping_set'
    ),
    latest AS (
        SELECT *, ROW_NUMBER() OVER (
                   PARTITION BY account_id, form, effective_from ORDER BY id DESC) AS rn
          FROM sets
    )
    SELECT s.account_id, s.form, s.effective_from, s.line_key, s.id
      FROM latest s
     WHERE s.rn = 1
       AND s.account_id IS NOT NULL
       AND s.line_key IS NOT NULL
       AND NOT EXISTS (
           SELECT 1 FROM ev c
            WHERE c.event_type = 'tax_line_mapping_cleared'
              AND c.account_id = s.account_id
              AND c.effective_from = s.effective_from
              AND c.id > s.id
              AND (c.form IS NULL OR c.form = s.form)
       );

DROP TABLE tax_line_mappings;
ALTER TABLE tax_line_mappings_per_return RENAME TO tax_line_mappings;

CREATE INDEX IF NOT EXISTS idx_tax_line_mappings_line ON tax_line_mappings(line_key);
CREATE INDEX IF NOT EXISTS idx_tax_line_mappings_year ON tax_line_mappings(effective_from);

RELEASE m056;
