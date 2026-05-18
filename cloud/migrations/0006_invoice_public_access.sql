-- Allow public token-based read of a single invoice.
-- Replaces the tenant_isolation policy on invoices with one that ALSO permits
-- SELECT when app.invoice_public_token matches. Writes still require tenant scope.

DROP POLICY IF EXISTS tenant_isolation ON invoices;

CREATE POLICY tenant_isolation ON invoices
USING (
    company_id = current_company_id()
    OR public_token = NULLIF(current_setting('app.invoice_public_token', true), '')
)
WITH CHECK (company_id = current_company_id());

-- Same trick for invoice_lines + customers + invoice_payments so the public
-- page can render line items + customer block. Read-only.
DROP POLICY IF EXISTS tenant_isolation ON invoice_lines;
CREATE POLICY tenant_isolation ON invoice_lines
USING (
    company_id = current_company_id()
    OR EXISTS (
        SELECT 1 FROM invoices i
        WHERE i.id = invoice_lines.invoice_id
        AND i.public_token = NULLIF(current_setting('app.invoice_public_token', true), '')
    )
)
WITH CHECK (company_id = current_company_id());

DROP POLICY IF EXISTS tenant_isolation ON customers;
CREATE POLICY tenant_isolation ON customers
USING (
    company_id = current_company_id()
    OR EXISTS (
        SELECT 1 FROM invoices i
        WHERE i.customer_id = customers.id
        AND i.public_token = NULLIF(current_setting('app.invoice_public_token', true), '')
    )
)
WITH CHECK (company_id = current_company_id());

DROP POLICY IF EXISTS tenant_isolation ON invoice_payments;
CREATE POLICY tenant_isolation ON invoice_payments
USING (
    company_id = current_company_id()
    OR EXISTS (
        SELECT 1 FROM invoices i
        WHERE i.id = invoice_payments.invoice_id
        AND i.public_token = NULLIF(current_setting('app.invoice_public_token', true), '')
    )
)
WITH CHECK (company_id = current_company_id());

-- Note: the `accounts` table is not exposed publicly. Public invoice rendering
-- omits internal account names (customers see only line description + amount).
