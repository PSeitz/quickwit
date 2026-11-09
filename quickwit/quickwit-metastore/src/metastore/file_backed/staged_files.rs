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

use quickwit_proto::metastore::{
    AssignFilesForIndexing, DocumentsStagedForUpload, MetastoreResult,
};
use quickwit_proto::types::{IndexId, IndexUid};
use time::OffsetDateTime;

use super::{FileBackedMetastore, LazyIndexStatus};

impl FileBackedMetastore {
    pub(super) async fn active_index_ids(&self) -> Vec<IndexId> {
        self.state
            .read()
            .await
            .indexes
            .iter()
            .filter(|(_, status)| matches!(status, LazyIndexStatus::Active(_)))
            .map(|(index_id, _)| index_id.clone())
            .collect()
    }

    pub(super) async fn assign_files_for_indexing_inner(
        &self,
        request: &AssignFilesForIndexing,
    ) -> MetastoreResult<Vec<DocumentsStagedForUpload>> {
        // Freeze registration, publication, deletion and other claims across the global scan
        // and persistence. Use the same clock/cutoff for selection and claiming.
        let guard = self.mutation_lock.clone().write_owned().await;
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let retry_before = now - i64::from(request.retry_after_secs);
        let mut oldest: Option<((i64, String), IndexUid)> = None;
        for index_id in self.active_index_ids().await {
            let candidate = self
                .read_any(&index_id, None, |index| {
                    Ok(index
                        .oldest_eligible_file(retry_before)
                        .map(|candidate| (candidate, index.index_uid().clone())))
                })
                .await?;
            if let Some(candidate) = candidate
                && oldest
                    .as_ref()
                    .map(|oldest| candidate.0 < oldest.0)
                    .unwrap_or(true)
            {
                oldest = Some(candidate);
            }
        }
        let Some((_, index_uid)) = oldest else {
            return Ok(Vec::new());
        };
        self.mutate_with_guard(
            &index_uid,
            |index| Ok(index.assign_files_for_indexing(request, now, retry_before)),
            guard,
        )
        .await
    }
}
