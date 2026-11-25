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

use std::collections::BTreeMap;

use bytes::Bytes;
use quickwit_config::{SourceInputFormat, TransformConfig};
use quickwit_doc_mapper::QuickwitDocBatch;
use tracing::warn;
use vrl::compiler::runtime::Runtime;
pub use vrl::compiler::runtime::Terminate as VrlTerminate;
use vrl::compiler::state::RuntimeState;
use vrl::compiler::{Program, TargetValueRef, TimeZone};
pub use vrl::value::{Secrets as VrlSecrets, Value as VrlValue};

use super::doc_processor::DocProcessorError;

pub(super) struct VrlProgram {
    program: Program,
    timezone: TimeZone,
    runtime: Runtime,
    metadata: VrlValue,
    secrets: VrlSecrets,
}

impl VrlProgram {
    pub(super) fn parse_into_batch(
        &mut self,
        format: SourceInputFormat,
        raw_doc: Bytes,
        batch: &mut QuickwitDocBatch,
    ) -> Result<(), DocProcessorError> {
        let num_bytes = raw_doc.len();
        let mut vrl_value = match format {
            SourceInputFormat::Json => serde_json::from_slice(&raw_doc)?,
            SourceInputFormat::PlainText => VrlValue::Object(BTreeMap::from([(
                "plain_text".into(),
                VrlValue::Bytes(raw_doc),
            )])),
            SourceInputFormat::OtlpLogsJson
            | SourceInputFormat::OtlpLogsProtobuf
            | SourceInputFormat::OtlpTracesJson
            | SourceInputFormat::OtlpTracesProtobuf => {
                panic!("OTLP logs or traces do not support VRL transforms")
            }
        };

        let mut target = TargetValueRef {
            value: &mut vrl_value,
            metadata: &mut self.metadata,
            secrets: &mut self.secrets,
        };
        let runtime_res = self
            .runtime
            .resolve(&mut target, &self.program, &self.timezone)
            .map_err(|transform_error| {
                warn!(transform_error=?transform_error);
                DocProcessorError::Transform(transform_error)
            });

        if let VrlValue::Object(metadata) = target.metadata {
            metadata.clear();
        }
        self.runtime.clear();

        let output = runtime_res?;
        batch.push_serialize(&output, num_bytes)?;
        Ok(())
    }

    pub fn try_from_transform_config(transform_config: TransformConfig) -> anyhow::Result<Self> {
        let (program, timezone) = transform_config.compile_vrl_script()?;
        let state = RuntimeState::default();
        let runtime = Runtime::new(state);

        Ok(VrlProgram {
            program,
            runtime,
            timezone,
            metadata: VrlValue::Object(BTreeMap::new()),
            secrets: VrlSecrets::default(),
        })
    }
}
