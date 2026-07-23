-- Accounts Payable / Accounts Receivable tables

CREATE TABLE IF NOT EXISTS bills (
    id TEXT PRIMARY KEY,
    vendor TEXT NOT NULL,
    amount INTEGER NOT NULL,
    currency TEXT NOT NULL DEFAULT 'USD',
    amount_paid INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'open',
    due_date TEXT NOT NULL,
    terms TEXT,
    memo TEXT,
    entry_id TEXT NOT NULL,
    posted_at_event INTEGER REFERENCES events(id),
    updated_at_event INTEGER REFERENCES events(id)
);

CREATE TABLE IF NOT EXISTS bill_payments (
    bill_id TEXT NOT NULL REFERENCES bills(id),
    payment_entry_id TEXT NOT NULL,
    amount_applied INTEGER NOT NULL,
    applied_at_event INTEGER REFERENCES events(id),
    PRIMARY KEY (bill_id, payment_entry_id)
);

CREATE TABLE IF NOT EXISTS invoices (
    id TEXT PRIMARY KEY,
    customer TEXT NOT NULL,
    amount INTEGER NOT NULL,
    currency TEXT NOT NULL DEFAULT 'USD',
    amount_paid INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'open',
    due_date TEXT NOT NULL,
    terms TEXT,
    memo TEXT,
    entry_id TEXT NOT NULL,
    posted_at_event INTEGER REFERENCES events(id),
    updated_at_event INTEGER REFERENCES events(id)
);

CREATE TABLE IF NOT EXISTS invoice_payments (
    invoice_id TEXT NOT NULL REFERENCES invoices(id),
    payment_entry_id TEXT NOT NULL,
    amount_applied INTEGER NOT NULL,
    applied_at_event INTEGER REFERENCES events(id),
    PRIMARY KEY (invoice_id, payment_entry_id)
);

CREATE INDEX IF NOT EXISTS idx_bills_status ON bills(status);
CREATE INDEX IF NOT EXISTS idx_bills_due_date ON bills(due_date);
CREATE INDEX IF NOT EXISTS idx_invoices_status ON invoices(status);
CREATE INDEX IF NOT EXISTS idx_invoices_due_date ON invoices(due_date);
