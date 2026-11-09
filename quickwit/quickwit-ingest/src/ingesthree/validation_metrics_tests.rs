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

use std::pin::pin;

use futures::poll;
use quickwit_config::{DocMapping, SearchSettings, build_doc_mapper};
use quickwit_metrics::{Gauge, GaugeGuard, counter, label_values};

use super::*;

fn mapper() -> Arc<DocMapper> {
    let mapping: DocMapping = serde_json::from_str(
        r#"{"mode":"strict","field_mappings":[{"name":"message","type":"text"}]}"#,
    )
    .unwrap();
    build_doc_mapper(&mapping, &SearchSettings::default()).unwrap()
}

fn counts(metrics: &ValidationMetrics) -> (u64, u64, u64, u64) {
    (
        metrics.valid_docs.get(),
        metrics.invalid_docs.get(),
        metrics.valid_bytes.get(),
        metrics.invalid_bytes.get(),
    )
}

#[test]
fn test_validation_metrics_share_existing_counter_schema() {
    use crate::metrics::{DOCS_BYTES_TOTAL, DOCS_TOTAL, VALIDITY};

    let metrics = ValidationMetrics::default();
    assert_eq!(
        metrics.valid_docs,
        counter!(parent: DOCS_TOTAL, labels: [label_values!(VALIDITY => "valid")])
    );
    assert_eq!(
        metrics.invalid_docs,
        counter!(parent: DOCS_TOTAL, labels: [label_values!(VALIDITY => "invalid")])
    );
    assert_eq!(
        metrics.valid_bytes,
        counter!(parent: DOCS_BYTES_TOTAL, labels: [label_values!(VALIDITY => "valid")])
    );
    assert_eq!(
        metrics.invalid_bytes,
        counter!(parent: DOCS_BYTES_TOTAL, labels: [label_values!(VALIDITY => "invalid")])
    );
    assert_eq!(
        metrics.valid_docs.key().name(),
        "quickwit_ingest_docs_total"
    );
    assert_eq!(
        metrics.valid_bytes.key().name(),
        "quickwit_ingest_docs_bytes_total"
    );
}

#[test]
fn test_validation_metrics_count_mixed_and_all_invalid_framed_bytes() {
    let metrics = ValidationMetrics::for_test();
    let valid = r#"{"message":"hello"}"#;
    let input = DocBatchV2::for_test(["", "[]", valid]);
    let original_bytes = input.num_bytes() as u64;
    let valid_bytes = valid.len() as u64 + 4;
    validate_documents(input, Some(mapper()), &metrics).unwrap();
    assert_eq!(
        counts(&metrics),
        (1, 2, valid_bytes, original_bytes - valid_bytes)
    );

    let invalid = DocBatchV2::for_test(["", "[]"]);
    let invalid_bytes = invalid.num_bytes() as u64;
    let (documents, failures) = validate_documents(invalid, Some(mapper()), &metrics).unwrap();
    assert!(documents.is_empty());
    assert_eq!(failures.len(), 2);
    assert_eq!(
        counts(&metrics),
        (
            1,
            4,
            valid_bytes,
            original_bytes - valid_bytes + invalid_bytes
        )
    );
}

#[test]
fn test_validation_metrics_skip_malformed_batches_and_count_unvalidated_documents_as_valid() {
    let metrics = ValidationMetrics::for_test();
    let mut malformed = DocBatchV2::for_test(["invalid JSON"]);
    malformed.doc_lengths[0] += 1;
    assert!(validate_documents(malformed, None, &metrics).is_err());
    assert_eq!(counts(&metrics), (0, 0, 0, 0));
    let input = DocBatchV2::for_test(["invalid JSON", ""]);
    let bytes = input.num_bytes() as u64;
    validate_documents(input, None, &metrics).unwrap();
    assert_eq!(counts(&metrics), (2, 0, bytes, 0));
}

#[tokio::test]
async fn test_validation_keeps_input_charged_after_awaiter_cancellation() {
    const BUDGET: u64 = 1024 * 1024;
    let dir = tempfile::tempdir().unwrap();
    let core = Ingester::open(dir.path(), BUDGET).unwrap();
    let (queue, mut receiver) = WriteQueue::channel(&core, BUDGET);
    let input_bytes = Gauge::local();
    queue.admission.lock().unwrap().metrics.memory_bytes = GaugeGuard::new(&input_bytes, 0.0);
    let mut request = pin!(queue.append(
        IndexUid::for_test("validation", 1),
        DocBatchV2::for_test(["{}"]),
        None
    ));
    assert!(poll!(&mut request).is_pending());
    let WriterRequest::Append(append) = receiver.try_recv().unwrap() else {
        panic!("expected append")
    };
    let charged_bytes = input_bytes.get();
    assert!(charged_bytes > 0.0);
    let metrics = ValidationMetrics::for_test();
    let observed = metrics.clone();
    let (started, ready) = oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let (finished, done) = oneshot::channel();
    let waiter = tokio::spawn(run_cpu_intensive(move || {
        started.send(()).unwrap();
        released.recv().unwrap();
        let result = append.validate(&metrics);
        // Dropping a result after its receiver disappeared must release the same charge.
        drop(result);
        finished.send(()).unwrap();
    }));
    ready.await.unwrap();
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    assert_eq!(input_bytes.get(), charged_bytes);
    release.send(()).unwrap();
    done.await.unwrap();
    assert_eq!(input_bytes.get(), 0.0);
    assert_eq!(observed.valid_docs.get(), 1);
    assert!(matches!(request.await, Err(IngestError::Unavailable)));
    let admission = queue.admission.lock().unwrap();
    assert_eq!(admission.metrics.queue_bytes.delta(), 0.0);
    assert_eq!(admission.metrics.disk_reserved_bytes.delta(), 0.0);
}
