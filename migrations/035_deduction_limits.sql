-- What proportion of an account's balance the law actually lets you deduct.
--
-- # Why this is not a column on `tax_line_mappings`
--
-- They are two different facts, set at different times by different reasoning.
-- *Which line* an expense reports on is a question about the form: staff meals
-- are "other deductions", line 21, and always will be. *How much of it is
-- deductible* is a question about §274 and changes with the law — meals were
-- 50% limited, then 100% for 2021-22, then 50% again. Keeping them apart means
-- a rate change is one row, and an account that has no limit carries no row at
-- all rather than a 100 that has to be written and maintained everywhere.
--
-- # Why a percentage rather than two accounts
--
-- The alternative is to split the account and post a year-end entry moving the
-- disallowed half across. That works, but it puts a tax rule inside the books:
-- the ledger stops showing what the meals cost. The disallowance is not a
-- bookkeeping fact, it is an adjustment made on the way to a return, and this is
-- the way to it.
CREATE TABLE IF NOT EXISTS tax_deduction_limits (
    account_id TEXT PRIMARY KEY REFERENCES accounts(id),
    -- 0-100. The rest of the balance is not deducted anywhere: on Form 1065 it
    -- is reported on Schedule K line 18c so it reduces each partner's basis.
    deductible_pct INTEGER NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at_event INTEGER REFERENCES events(id)
);
