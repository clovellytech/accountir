-- Invitation links for adding members to a company without sending email.
-- Owner generates a link, recipient opens it (signing up if needed) and is added.

CREATE TABLE company_invitations (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    token TEXT NOT NULL UNIQUE,
    company_id UUID NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    role membership_role NOT NULL,
    invited_by UUID NOT NULL REFERENCES auth_users(id),
    expires_at TIMESTAMPTZ NOT NULL,
    accepted_at TIMESTAMPTZ,
    accepted_by UUID REFERENCES auth_users(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_company_invitations_company ON company_invitations(company_id);
