-- A fixed allocation that is a preferred share: the partner takes the first
-- `amount_cents` of the year's income, and what is left is divided on the
-- percentages with everybody, that partner included.
--
-- For an agreement like "the active member takes the first $90,000 of profit
-- each year, then 51/49". A loss year ignores it and follows the loss
-- percentages, because a preference in profit is not a guarantee against loss.
ALTER TABLE partner_fixed_allocations ADD COLUMN preferred INTEGER NOT NULL DEFAULT 0;
