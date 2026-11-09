ALTER TABLE staged_files ADD COLUMN publication_id TEXT;

-- Match the Unix-second ordering exposed by the API and the file-backed metastore.
ALTER TABLE staged_files ALTER COLUMN create_timestamp
    SET DEFAULT date_trunc('second', CURRENT_TIMESTAMP);
UPDATE staged_files SET create_timestamp = date_trunc('second', create_timestamp)
    WHERE completed_timestamp IS NULL;

CREATE INDEX staged_files_assignment ON staged_files (create_timestamp, file_id)
    WHERE completed_timestamp IS NULL;
CREATE INDEX staged_files_index_assignment ON staged_files (index_uid, create_timestamp, file_id)
    WHERE completed_timestamp IS NULL;
