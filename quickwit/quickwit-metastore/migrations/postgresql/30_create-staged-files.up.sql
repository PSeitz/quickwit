CREATE TABLE staged_files (
    index_uid VARCHAR(282) NOT NULL,
    file_id VARCHAR(26) NOT NULL,
    file_uri TEXT NOT NULL,
    num_bytes BIGINT NOT NULL CHECK (num_bytes > 0),
    create_timestamp TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    processing_started_timestamp TIMESTAMPTZ,
    completed_timestamp TIMESTAMPTZ,
    PRIMARY KEY (file_id),
    FOREIGN KEY (index_uid) REFERENCES indexes (index_uid) ON DELETE CASCADE
);
