-- One tax year's filing facts: status, household, state of residence, estimated
-- payments, and which accounts are which rental property.
--
-- # Why a JSON column
--
-- The profile is read whole, by one year, and written whole by one event
-- (`PersonalTaxProfileSet`). Nothing queries inside it, and a column per field would
-- be a migration per field for no reader. The event is the schema.
--
-- A projection: truncated and replayed by `Projector::rebuild`.

CREATE TABLE IF NOT EXISTS personal_tax_profiles (
    tax_year         INTEGER PRIMARY KEY,
    -- `PersonalTaxProfileData`, as JSON.
    profile          TEXT NOT NULL,
    updated_at_event INTEGER REFERENCES events(id)
);
