ALTER TABLE shadow_blocks ADD COLUMN reorged_out BOOL DEFAULT false;
ALTER TABLE shadow_blocks ADD COLUMN canonical_hash TEXT;
