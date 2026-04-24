-- Add payment_meta column to staged transactions for card holder / reference tracking
ALTER TABLE plaid_staged_transactions ADD COLUMN payment_meta TEXT;
