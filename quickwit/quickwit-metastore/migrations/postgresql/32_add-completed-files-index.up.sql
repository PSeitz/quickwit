CREATE INDEX staged_files_completed ON staged_files (file_id)
    WHERE completed_timestamp IS NOT NULL;
