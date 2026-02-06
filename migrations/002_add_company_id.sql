-- Add company_id column to company table for database identity
ALTER TABLE company ADD COLUMN company_id TEXT;

-- Generate a UUID for any existing company rows
UPDATE company SET company_id = lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)),2) || '-' || substr('89ab', abs(random()) % 4 + 1, 1) || substr(hex(randomblob(2)),2) || '-' || hex(randomblob(6))) WHERE company_id IS NULL;
