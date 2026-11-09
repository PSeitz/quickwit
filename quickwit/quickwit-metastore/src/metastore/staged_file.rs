// Copyright 2021-Present Datadog, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashSet;

use quickwit_common::uri::Uri;
use quickwit_proto::metastore::{
    AssignFilesForIndexing, DocumentsStagedForUpload, ListCompletedFilesRequest, MetastoreError,
    MetastoreResult, PublishFileSplits, RegisterFileForIndexing,
};
use ulid::Ulid;

/// Validate before using generated accessors, which panic on missing message fields.
/// Both backends accept the same sizes even though PostgreSQL uses signed BIGINT.
pub(super) fn validate_registration(request: &RegisterFileForIndexing) -> MetastoreResult<()> {
    let index_uid = request
        .index_uid
        .as_ref()
        .ok_or_else(|| invalid("missing index UID"))?;
    if index_uid.index_id.is_empty() {
        return Err(invalid("empty index ID"));
    }
    let file_id = request
        .file_id
        .parse::<Ulid>()
        .map_err(|_| invalid("invalid staged file ULID"))?;
    if file_id.to_string() != request.file_id {
        return Err(invalid("staged file ID must be a canonical ULID"));
    }
    if request.num_bytes == 0 || request.num_bytes > i64::MAX as u64 {
        return Err(invalid(
            "staged file size must be between 1 and INT64_MAX bytes",
        ));
    }
    if !request.file_uri.contains("://") {
        return Err(invalid("staged file URI must include a storage protocol"));
    }
    let uri = request
        .file_uri
        .parse::<Uri>()
        .map_err(|_| invalid("invalid staged file URI"))?;
    if !(uri.protocol().is_object_storage() || uri.protocol().is_file_storage())
        || uri.parent().is_none()
    {
        return Err(invalid("staged file URI must identify a file in storage"));
    }
    Ok(())
}

pub(super) fn validate_assignment(request: &AssignFilesForIndexing) -> MetastoreResult<()> {
    if request.max_num_files == 0 || request.max_num_files > 1_000 {
        return Err(invalid("assignment file limit must be between 1 and 1000"));
    }
    if request.max_num_bytes == 0 || request.max_num_bytes > i64::MAX as u64 {
        return Err(invalid(
            "assignment byte target must be between 1 and INT64_MAX",
        ));
    }
    if request.retry_after_secs == 0 {
        return Err(invalid("assignment retry delay must be positive"));
    }
    Ok(())
}

pub(super) fn validate_completed_files_request(
    request: &ListCompletedFilesRequest,
) -> MetastoreResult<()> {
    if request.max_num_files == 0 || request.max_num_files > 1_000 {
        return Err(invalid("completed file limit must be between 1 and 1000"));
    }
    Ok(())
}

pub(super) fn validate_publication(request: &PublishFileSplits) -> MetastoreResult<()> {
    let index_uid = request
        .index_uid
        .as_ref()
        .ok_or_else(|| invalid("missing index UID"))?;
    if index_uid.index_id.is_empty() {
        return Err(invalid("empty index ID"));
    }
    if request.file_ids.is_empty() || request.file_ids.len() > 1_000 {
        return Err(invalid(
            "publication requires between 1 and 1000 input files",
        ));
    }
    if request.staged_split_ids.len() > 1_000 {
        return Err(invalid("publication accepts at most 1000 output splits"));
    }
    for file_id in &request.file_ids {
        validate_ulid(file_id)?;
    }
    validate_ulid(&request.publication_id)?;
    for ids in [&request.file_ids, &request.staged_split_ids] {
        let mut unique_ids = HashSet::with_capacity(ids.len());
        for id in ids {
            if id.is_empty() || !unique_ids.insert(id) {
                return Err(invalid(
                    "publication requires distinct nonempty input and output IDs",
                ));
            }
        }
    }
    Ok(())
}

fn invalid(message: &str) -> MetastoreError {
    MetastoreError::InvalidArgument {
        message: message.to_string(),
    }
}

fn validate_ulid(value: &str) -> MetastoreResult<()> {
    let ulid = value
        .parse::<Ulid>()
        .map_err(|_| invalid("invalid file or publication ULID"))?;
    if ulid.to_string() != value {
        return Err(invalid("file and publication IDs must be canonical ULIDs"));
    }
    Ok(())
}

pub(super) fn is_eligible(file: &DocumentsStagedForUpload, retry_before: i64) -> bool {
    file.completed_timestamp.is_none()
        && file
            .processing_started_timestamp
            .map(|started| started < retry_before)
            .unwrap_or(true)
}

/// Candidates are sorted, count-capped, and belong to one index. Always take the oldest
/// file so a file larger than the byte target can still make progress.
pub(super) fn assignment_file_count(
    mut file_sizes: impl Iterator<Item = u64>,
    max_num_bytes: u64,
) -> usize {
    let Some(first_size) = file_sizes.next() else {
        return 0;
    };
    let mut remaining = max_num_bytes.saturating_sub(first_size);
    let mut count = 1;
    for num_bytes in file_sizes {
        if num_bytes > remaining {
            break;
        }
        remaining -= num_bytes;
        count += 1;
    }
    count
}

/// The backend must first verify that every requested input exists in the requested
/// index. Publication IDs identify immutable input/output sets, never assignments.
/// Retained completion evidence makes retries independent of output split lifetime.
pub(super) fn publication_outcome(
    files: &[DocumentsStagedForUpload],
    publication_id: &str,
) -> Option<bool> {
    if !files.iter().any(|file| file.completed_timestamp.is_some()) {
        return None;
    }
    Some(files.iter().all(|file| {
        file.completed_timestamp.is_some() && file.publication_id.as_deref() == Some(publication_id)
    }))
}
