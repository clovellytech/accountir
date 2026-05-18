-- Customers, Invoices, Invoice Lines, Payments
-- All scoped per-tenant with RLS.

CREATE TABLE customers (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    company_id UUID NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    email TEXT,
    phone TEXT,
    address_line1 TEXT,
    address_line2 TEXT,
    city TEXT,
    state TEXT,
    postal_code TEXT,
    country TEXT NOT NULL DEFAULT 'US',
    default_terms TEXT NOT NULL DEFAULT 'net_30',
    notes TEXT,
    is_active BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_customers_company ON customers(company_id, is_active);
CREATE INDEX idx_customers_company_name ON customers(company_id, name);

CREATE TYPE invoice_status AS ENUM ('draft', 'sent', 'partial', 'paid', 'overdue', 'void');

CREATE TABLE invoices (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    company_id UUID NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    customer_id UUID NOT NULL REFERENCES customers(id),
    invoice_number TEXT NOT NULL,
    status invoice_status NOT NULL DEFAULT 'draft',
    issue_date DATE NOT NULL,
    due_date DATE NOT NULL,
    terms TEXT NOT NULL DEFAULT 'net_30',
    currency TEXT NOT NULL DEFAULT 'USD',
    subtotal_cents BIGINT NOT NULL DEFAULT 0,
    tax_cents BIGINT NOT NULL DEFAULT 0,
    total_cents BIGINT NOT NULL DEFAULT 0,
    paid_cents BIGINT NOT NULL DEFAULT 0,
    memo TEXT,
    customer_notes TEXT,
    public_token TEXT NOT NULL,
    posted_entry_id UUID REFERENCES journal_entries(id),
    sent_at TIMESTAMPTZ,
    last_sent_to TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (company_id, invoice_number),
    UNIQUE (public_token)
);

CREATE INDEX idx_invoices_company_status ON invoices(company_id, status);
CREATE INDEX idx_invoices_company_due ON invoices(company_id, due_date);
CREATE INDEX idx_invoices_company_customer ON invoices(company_id, customer_id);

CREATE TABLE invoice_lines (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    company_id UUID NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    invoice_id UUID NOT NULL REFERENCES invoices(id) ON DELETE CASCADE,
    sort_order INTEGER NOT NULL DEFAULT 0,
    description TEXT NOT NULL,
    quantity NUMERIC(14,4) NOT NULL DEFAULT 1,
    unit_price_cents BIGINT NOT NULL,
    amount_cents BIGINT NOT NULL,
    tax_rate_pct NUMERIC(6,3) NOT NULL DEFAULT 0,
    tax_cents BIGINT NOT NULL DEFAULT 0,
    revenue_account_id UUID NOT NULL REFERENCES accounts(id)
);

CREATE INDEX idx_invoice_lines_invoice ON invoice_lines(invoice_id);
CREATE INDEX idx_invoice_lines_company ON invoice_lines(company_id);

CREATE TYPE invoice_payment_method AS ENUM ('cash', 'check', 'ach', 'wire', 'card', 'other');

CREATE TABLE invoice_payments (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    company_id UUID NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    invoice_id UUID NOT NULL REFERENCES invoices(id) ON DELETE CASCADE,
    payment_date DATE NOT NULL,
    amount_cents BIGINT NOT NULL,
    method invoice_payment_method NOT NULL DEFAULT 'check',
    reference TEXT,
    deposit_account_id UUID NOT NULL REFERENCES accounts(id),
    entry_id UUID REFERENCES journal_entries(id),
    memo TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_invoice_payments_invoice ON invoice_payments(invoice_id);
CREATE INDEX idx_invoice_payments_company ON invoice_payments(company_id);

-- Per-company invoice numbering sequence.
CREATE TABLE invoice_number_seq (
    company_id UUID PRIMARY KEY REFERENCES companies(id) ON DELETE CASCADE,
    prefix TEXT NOT NULL DEFAULT 'INV-',
    next_number BIGINT NOT NULL DEFAULT 1001
);

-- ---------- RLS ----------------------------------------------------------------

ALTER TABLE customers           ENABLE ROW LEVEL SECURITY;
ALTER TABLE invoices            ENABLE ROW LEVEL SECURITY;
ALTER TABLE invoice_lines       ENABLE ROW LEVEL SECURITY;
ALTER TABLE invoice_payments    ENABLE ROW LEVEL SECURITY;
ALTER TABLE invoice_number_seq  ENABLE ROW LEVEL SECURITY;

ALTER TABLE customers           FORCE ROW LEVEL SECURITY;
ALTER TABLE invoices            FORCE ROW LEVEL SECURITY;
ALTER TABLE invoice_lines       FORCE ROW LEVEL SECURITY;
ALTER TABLE invoice_payments    FORCE ROW LEVEL SECURITY;
ALTER TABLE invoice_number_seq  FORCE ROW LEVEL SECURITY;

DO $$
DECLARE t TEXT;
BEGIN
    FOR t IN SELECT unnest(ARRAY[
        'customers', 'invoices', 'invoice_lines', 'invoice_payments', 'invoice_number_seq'
    ])
    LOOP
        EXECUTE format(
            'CREATE POLICY tenant_isolation ON %I USING (company_id = current_company_id()) WITH CHECK (company_id = current_company_id())',
            t
        );
    END LOOP;
END $$;

-- Ensure non-superuser app role has access to the new tables.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'accountir_app') THEN
        GRANT SELECT, INSERT, UPDATE, DELETE, REFERENCES ON
            customers, invoices, invoice_lines, invoice_payments, invoice_number_seq
            TO accountir_app;
    END IF;
END $$;
