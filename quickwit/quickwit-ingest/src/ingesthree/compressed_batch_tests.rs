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

use std::io::Cursor;

use proptest::prelude::*;

use super::compressed_batch::DecodedBatch;
use super::documents_staged_for_upload::MAGIC_V1;
use super::*;

fn file_header(magic: &[u8; 8], index_uid: &IndexUid) -> Vec<u8> {
    let mut data = magic.to_vec();
    write_record(&mut data, &encode_metadata(index_uid).unwrap()).unwrap();
    data
}

fn payload(num_docs: u64, serialized: &[u8]) -> Vec<u8> {
    let mut data = num_docs.to_le_bytes().to_vec();
    data.extend_from_slice(&(serialized.len() as u64).to_le_bytes());
    data.extend(zstd::bulk::compress(serialized, 1).unwrap());
    data
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn test_batch_roundtrip_and_incompressible_bound(
        documents in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..16384), 1..16),
    ) {
        let document_bytes = documents.iter().map(Vec::len).sum();
        let bound = CompressedBatch::max_encoded_len(document_bytes, documents.len()).unwrap();
        let batch = CompressedBatch::new(&documents).unwrap();
        prop_assert!(batch.data.len() <= bound);
        prop_assert_eq!(batch.num_docs, documents.len() as u64);
        let mut decoded = DecodedBatch::decode(&batch.data).unwrap();
        prop_assert_eq!(decoded.num_docs, batch.num_docs);
        for document in documents {
            let decoded_document = decoded.next_document().unwrap();
            prop_assert_eq!(decoded_document.as_ref(), document.as_slice());
        }
        prop_assert!(decoded.next_document().is_none());
    }
}

#[test]
fn test_batch_bound_rejects_overflow_and_empty_requests() {
    for (bytes, count) in [
        (0, 0),
        (usize::MAX, 1),
        (0, usize::MAX),
        (u32::MAX as usize, 1),
    ] {
        assert!(CompressedBatch::max_encoded_len(bytes, count).is_err());
    }
    assert!(CompressedBatch::new(Vec::<Bytes>::new()).is_err());
    let batch = CompressedBatch::new([b""]).unwrap();
    assert_eq!(
        DecodedBatch::decode(&batch.data).unwrap().next_document(),
        Some(Bytes::new())
    );
}

#[test]
fn test_one_record_per_batch_preserves_opaque_empty_and_duplicate_documents() {
    let directory = tempfile::tempdir().unwrap();
    let index_uid = IndexUid::for_test("batch-records", 1);
    let repeated = Bytes::from(vec![b'x'; 32_768]);
    let documents = [
        Bytes::new(),
        Bytes::from_static(b"\xff\0\n"),
        repeated.clone(),
        repeated,
    ];
    let batch = CompressedBatch::new(&documents).unwrap();
    assert!(batch.data.len() < 512, "compress documents together");
    let mut ingester = Ingester::open(directory.path(), 1_000_000).unwrap();
    let file_id = ingester.append_compressed(&index_uid, &batch).unwrap();
    assert_eq!(
        ingester.append_compressed(&index_uid, &batch).unwrap(),
        file_id
    );
    let input = &ingester.active[&index_uid].input;
    let header_len = file_header(MAGIC, &index_uid).len();
    assert_eq!(
        input.num_bytes,
        header_len as u64 + 2 * record_size(batch.data.len()).unwrap()
    );
    assert_eq!(input.num_docs, 8);
    let contents = fs::read(&input.path).unwrap();
    // Each request has exactly one checksummed outer record.
    let mut framed = Vec::new();
    write_record(&mut framed, &batch.data).unwrap();
    assert_eq!(&contents[header_len..], [framed.clone(), framed].concat());
    let mut reader =
        DocumentFileReader::new(Cursor::new(contents), &index_uid, input.num_bytes).unwrap();
    for document in documents.iter().cycle().take(8) {
        assert_eq!(reader.read_document().unwrap().as_ref(), Some(document));
    }
    assert!(reader.read_document().unwrap().is_none());
    drop(ingester);
    let recovered = Ingester::open(directory.path(), 1_000_000).unwrap();
    assert_eq!(recovered.finished_files()[0].num_docs, 8);
}

#[test]
fn test_malformed_complete_batches_are_fatal_even_with_valid_outer_checksums() {
    let valid = CompressedBatch::new([b"first".as_slice(), b"last".as_slice()])
        .unwrap()
        .data;
    let mut cases = vec![
        Vec::new(),
        valid[..15].to_vec(),
        payload(0, &[]),
        payload(1, &[0, 0, 0]),                // Missing document length byte.
        payload(1, &[5, 0, 0, 0, b'x']),       // Document exceeds decoded size.
        payload(1, &[0, 0, 0, 0, 42]),         // Trailing decoded data.
        payload(2, &[0, 0, 0, 0]),             // Missing document.
        payload(1, &[0, 0, 0, 0, 0, 0, 0, 0]), // Extra document.
    ];
    let mut truncated_zstd = valid.clone();
    truncated_zstd.pop();
    cases.push(truncated_zstd);
    let mut bad_zstd = valid.clone();
    bad_zstd[16] ^= 1;
    cases.push(bad_zstd);
    let mut trailing_compressed = valid.clone();
    trailing_compressed.push(0);
    cases.push(trailing_compressed);
    let mut two_frames = valid.clone();
    two_frames.extend_from_slice(&valid[16..]);
    cases.push(two_frames);
    for count in [0u64, 1, 3, u64::MAX] {
        let mut wrong_count = valid.clone();
        wrong_count[..8].copy_from_slice(&count.to_le_bytes());
        cases.push(wrong_count);
    }
    for size in [0u64, 8, 1000, u64::MAX] {
        let mut wrong_size = valid.clone();
        wrong_size[8..16].copy_from_slice(&size.to_le_bytes());
        cases.push(wrong_size);
    }
    // A syntactically valid frame advertises a 2 GiB window for a 304-byte batch.
    let mut huge_window = CompressedBatch::new([vec![0u8; 300]]).unwrap().data;
    assert_eq!(huge_window[20], 0x60); // Single segment, two-byte content size.
    huge_window[20] = 0x40;
    huge_window.insert(21, 0xa8); // Window_Log = 10 + (0xa8 >> 3) = 31.
    cases.push(huge_window);

    let index_uid = IndexUid::for_test("malformed-batch", 1);
    for (case, batch) in cases.into_iter().enumerate() {
        let mut contents = file_header(MAGIC, &index_uid);
        write_record(&mut contents, &batch).unwrap();
        let mut reader =
            DocumentFileReader::new(Cursor::new(&contents), &index_uid, contents.len() as u64)
                .unwrap();
        assert!(
            reader.read_document().is_err(),
            "case {case} yielded a document"
        );
        for finished in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("input");
            fs::write(&path, &contents).unwrap();
            assert!(
                recover(&path, Ulid::new(), finished).is_err(),
                "case {case}, finished={finished}"
            );
            assert_eq!(
                fs::read(&path).unwrap(),
                contents,
                "case {case} was repaired"
            );
        }
    }
}

#[test]
fn test_decoder_window_limit_is_independent_of_batch_size() {
    // Both headers claim 64 MiB, but the tiny frame must fail before allocating its window.
    let decoded_len = 64 * 1024 * 1024u32;
    let mut data = 1u64.to_le_bytes().to_vec();
    data.extend_from_slice(&u64::from(decoded_len).to_le_bytes());
    data.extend_from_slice(&[0x28, 0xb5, 0x2f, 0xfd, 0x80, 0x80]);
    data.extend_from_slice(&decoded_len.to_le_bytes());
    data.extend_from_slice(&[1, 0, 0]); // Final empty raw block.
    let error = DecodedBatch::decode(&data).err().unwrap();
    assert!(error.to_string().contains("too much memory"), "{error:#}");

    // The window limit still permits batches larger than 8 MiB.
    let document = vec![42u8; 8 * 1024 * 1024 + 1];
    let batch = CompressedBatch::new([&document]).unwrap();
    let mut decoded = DecodedBatch::decode(&batch.data).unwrap();
    assert_eq!(
        decoded.next_document().unwrap().as_ref(),
        document.as_slice()
    );
    assert!(decoded.next_document().is_none());
}

#[test]
fn test_batch_crc_is_checked_before_yielding_any_document() {
    let index_uid = IndexUid::for_test("batch-crc", 1);
    let batch = CompressedBatch::new([b"first", b"other"]).unwrap();
    let mut contents = file_header(MAGIC, &index_uid);
    write_record(&mut contents, &batch.data).unwrap();
    *contents.last_mut().unwrap() ^= 1;
    let mut reader =
        DocumentFileReader::new(Cursor::new(&contents), &index_uid, contents.len() as u64).unwrap();
    assert!(reader.read_document().is_err());
}

#[test]
fn test_v1_recovery_retains_documents_and_never_resumes_legacy_files() {
    let index_uid = IndexUid::for_test("legacy", 1);
    let documents = [b"first".as_slice(), b"", b"first"];
    let mut original = file_header(MAGIC_V1, &index_uid);
    for document in documents {
        write_record(&mut original, document).unwrap();
    }
    let mut last_record = Vec::new();
    write_record(&mut last_record, b"last").unwrap();
    for prefix in 0..=last_record.len() {
        let directory = tempfile::tempdir().unwrap();
        drop(Ingester::open(directory.path(), 1_000_000).unwrap());
        let file_id = Ulid::new();
        let path = directory
            .path()
            .join("ingesthree")
            .join(format!("{file_id}.open"));
        let mut contents = original.clone();
        contents.extend_from_slice(&last_record[..prefix]);
        fs::write(&path, &contents).unwrap();
        let mut recovered = Ingester::open(directory.path(), 1_000_000).unwrap();
        let input = &recovered.finished_files()[0];
        assert_eq!(input.file_id, file_id);
        let complete = prefix == last_record.len();
        assert_eq!(input.num_docs, if complete { 4 } else { 3 });
        assert_eq!(
            fs::read(&input.path).unwrap(),
            if complete { contents } else { original.clone() }
        );
        let mut reader =
            DocumentFileReader::open(&input.path, &index_uid, input.num_bytes).unwrap();
        for document in documents {
            assert_eq!(reader.read_document().unwrap().unwrap(), document);
        }
        if complete {
            assert_eq!(reader.read_document().unwrap().unwrap(), b"last".as_slice());
        }
        assert!(reader.read_document().unwrap().is_none());
        let new_id = recovered.append(&index_uid, documents).unwrap();
        assert_ne!(new_id, file_id);
        let new_contents = fs::read(&recovered.active[&index_uid].input.path).unwrap();
        assert_eq!(&new_contents[..8], MAGIC);
    }
}

#[test]
fn test_v1_corruption_and_finished_truncation_still_fail_recovery() {
    let index_uid = IndexUid::for_test("legacy-corrupt", 1);
    let mut original = file_header(MAGIC_V1, &index_uid);
    let record_offset = original.len();
    write_record(&mut original, &vec![42u8; 20_000]).unwrap();
    for offset in [
        record_offset,
        record_offset + 8,
        record_offset + 8192,
        original.len() - 1,
    ] {
        let mut corrupted = original.clone();
        corrupted[offset] ^= 1;
        for finished in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("input");
            fs::write(&path, &corrupted).unwrap();
            assert!(recover(&path, Ulid::new(), finished).is_err());
            assert_eq!(fs::read(&path).unwrap(), corrupted);
        }
    }
    for magic in [MAGIC_V1, MAGIC] {
        let mut contents = file_header(magic, &index_uid);
        let record_offset = contents.len();
        if magic == MAGIC_V1 {
            write_record(&mut contents, b"last").unwrap();
        } else {
            write_record(
                &mut contents,
                &CompressedBatch::new([b"last"]).unwrap().data,
            )
            .unwrap();
        }
        for end in record_offset + 1..contents.len() {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("input");
            fs::write(&path, &contents[..end]).unwrap();
            assert!(recover(&path, Ulid::new(), true).is_err());
            assert_eq!(fs::read(&path).unwrap(), &contents[..end]);
        }
    }
}
