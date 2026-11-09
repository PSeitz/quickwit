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

use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};

use ulid::Ulid;

use super::{Admission, DocumentsStagedForUpload, Ingester};

// Closes admission after the file writer drops, including during unwinding.
// Blocking work retains the file writer and its admission together.
pub(super) struct FileWriter {
    ingester: Ingester,
    admission: CloseAdmission,
}

impl FileWriter {
    pub fn new(ingester: Ingester, admission: Arc<Mutex<Admission>>) -> Self {
        Self {
            ingester,
            admission: CloseAdmission(admission),
        }
    }

    pub fn finish(self) -> anyhow::Result<Vec<DocumentsStagedForUpload>> {
        let Self {
            ingester,
            admission,
        } = self;
        let result = ingester.finish();
        drop(admission);
        result
    }

    pub fn delete_finished_file(&mut self, file_id: Ulid) -> anyhow::Result<u64> {
        let num_bytes = self.ingester.delete_finished_file(file_id)?;
        // Deletion and its metric update stay together if the awaiting task is cancelled.
        let mut admission = self.admission.0.lock().expect("admission mutex poisoned");
        admission.disk_bytes -= num_bytes;
        admission
            .metrics
            .disk_bytes
            .set(admission.disk_bytes as f64);
        Ok(num_bytes)
    }
}

impl Deref for FileWriter {
    type Target = Ingester;

    fn deref(&self) -> &Self::Target {
        &self.ingester
    }
}

impl DerefMut for FileWriter {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.ingester
    }
}

struct CloseAdmission(Arc<Mutex<Admission>>);

impl Drop for CloseAdmission {
    fn drop(&mut self) {
        self.0.lock().expect("admission mutex poisoned").close();
    }
}
