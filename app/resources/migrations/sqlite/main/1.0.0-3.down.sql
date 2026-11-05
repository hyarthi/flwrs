-- The index must go before the column can be dropped.
DROP INDEX IF EXISTS scenes__enabled_idx;
ALTER TABLE scenes DROP COLUMN enabled;
