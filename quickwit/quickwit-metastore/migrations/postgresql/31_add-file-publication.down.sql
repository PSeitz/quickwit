DROP INDEX staged_files_index_assignment;
DROP INDEX staged_files_assignment;
ALTER TABLE staged_files DROP COLUMN publication_id;
ALTER TABLE staged_files ALTER COLUMN create_timestamp SET DEFAULT CURRENT_TIMESTAMP;
