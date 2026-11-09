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
use std::process::Command;

use proptest::prelude::*;

use super::*;

const DISK_BUDGET: u64 = 1024 * 1024;

fn documents() -> Vec<Bytes> {
    vec![
        Bytes::from_static(br#"{"body":"hello"}"#),
        Bytes::from_static(b"second\nrecord"),
    ]
}

pub(super) fn read_input(path: &Path) -> (IndexUid, Vec<Vec<u8>>) {
    let bytes = fs::read(path).unwrap();
    assert_eq!(&bytes[..8], MAGIC);
    let mut remaining = &bytes[8..];
    let mut records = Vec::new();
    while !remaining.is_empty() {
        let len_bytes: [u8; 4] = remaining[..4].try_into().unwrap();
        let len = u32::from_le_bytes(len_bytes) as usize;
        let length_crc = u32::from_le_bytes(remaining[4..8].try_into().unwrap());
        assert_eq!(crc32fast::hash(&len_bytes), length_crc);
        let payload = &remaining[8..8 + len];
        let payload_crc = u32::from_le_bytes(remaining[8 + len..12 + len].try_into().unwrap());
        assert_eq!(crc32fast::hash(payload), payload_crc);
        records.push(payload.to_vec());
        remaining = &remaining[12 + len..];
    }
    let payload = records.remove(0);
    let index_uid = std::str::from_utf8(&payload).unwrap().parse().unwrap();
    (index_uid, records)
}

#[test]
fn test_append_flushes_and_groups_by_index_incarnation() {
    let dir = tempfile::tempdir().unwrap();
    let mut ingester = Ingester::open(dir.path(), DISK_BUDGET).unwrap();
    let first = IndexUid::for_test("logs", 1);
    let second = IndexUid::for_test("logs", 2);
    let first_id = ingester.append(&first, &documents()).unwrap();
    assert_eq!(ingester.append(&first, &documents()).unwrap(), first_id);
    let second_id = ingester.append(&second, &documents()).unwrap();
    assert_ne!(first_id, second_id);
    // Read through another file descriptor before dropping the writer or finishing the file.
    let active = &ingester.active[&first];
    let (index_uid, records) = read_input(&active.input.path);
    assert_eq!(index_uid, first);
    assert_eq!(
        active.input.path.file_stem().unwrap(),
        first_id.to_string().as_str()
    );
    let header = fs::read(&active.input.path).unwrap();
    let metadata_len = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
    assert_eq!(&header[16..16 + metadata_len], first.to_string().as_bytes());
    assert_eq!(records, [documents(), documents()].concat());
    assert_eq!(active.input.num_docs, 4);
    let bytes_on_disk: u64 = ingester
        .active
        .values()
        .map(|active| {
            let len = fs::metadata(&active.input.path).unwrap().len();
            assert_eq!(len, active.input.num_bytes);
            len
        })
        .sum();
    assert_eq!(bytes_on_disk, ingester.disk_bytes);
}

#[test]
fn test_finish_file_preserves_bytes_and_next_append_gets_new_id() {
    let dir = tempfile::tempdir().unwrap();
    let index = IndexUid::for_test("logs", 1);
    let mut ingester = Ingester::open(dir.path(), DISK_BUDGET).unwrap();
    let first_id = ingester.append(&index, &documents()).unwrap();
    let original = fs::read(&ingester.active[&index].input.path).unwrap();
    ingester.finish_file(&index).unwrap();
    ingester.finish_file(&index).unwrap();
    assert_eq!(ingester.finished_files().len(), 1);
    assert_eq!(
        fs::read(&ingester.finished_files()[0].path).unwrap(),
        original
    );
    let next_id = ingester.append(&index, &documents()).unwrap();
    assert_ne!(first_id, next_id);
    let inputs = ingester.finish().unwrap();
    assert_eq!(inputs.len(), 2);
    assert!(
        inputs
            .iter()
            .all(|input| input.path.extension().unwrap() == "finished")
    );
    let recovered = Ingester::open(dir.path(), DISK_BUDGET).unwrap();
    assert_eq!(recovered.finished_files().len(), 2);
    assert_eq!(fs::read(&inputs[0].path).unwrap(), original);
}

#[test]
fn test_recovery_preserves_open_and_finished_files() {
    let dir = tempfile::tempdir().unwrap();
    let mut ingester = Ingester::open(dir.path(), DISK_BUDGET).unwrap();
    let mut originals = HashMap::new();
    for incarnation in 0..32 {
        let index = IndexUid::for_test("logs", incarnation);
        let file_id = ingester.append(&index, &documents()).unwrap();
        originals.insert(
            file_id,
            fs::read(&ingester.active[&index].input.path).unwrap(),
        );
        if incarnation % 2 == 0 {
            ingester.finish_file(&index).unwrap();
        }
    }
    let ids: HashSet<_> = originals.keys().copied().collect();
    let disk_bytes = ingester.disk_bytes;
    drop(ingester);
    let mut recovered = Ingester::open(dir.path(), DISK_BUDGET).unwrap();
    assert_eq!(recovered.finished_files().len(), ids.len());
    assert_eq!(
        recovered
            .finished_files()
            .iter()
            .map(|input| input.file_id)
            .collect::<HashSet<_>>(),
        ids
    );
    assert_eq!(recovered.disk_bytes, disk_bytes);
    assert!(recovered.active.is_empty());
    assert_eq!(recovered.next_deadline(), None);
    for input in recovered.finished_files() {
        let original = &originals[&input.file_id];
        assert_eq!(input.path.extension().unwrap(), "finished");
        assert_eq!(&fs::read(&input.path).unwrap(), original);
        assert_eq!(input.num_bytes, original.len() as u64);
        assert_eq!(input.num_docs, documents().len() as u64);
    }
    let new_id = recovered
        .append(&IndexUid::for_test("logs", 1), &documents())
        .unwrap();
    assert!(!ids.contains(&new_id));
    assert_eq!(recovered.finish().unwrap().len(), ids.len() + 1);
}

#[test]
fn test_recovery_truncates_incomplete_final_frames_and_keeps_complete_frames() {
    let index = IndexUid::for_test("logs", 1);
    let unacknowledged = Bytes::from_static(b"unacknowledged document");
    let mut frame = Vec::new();
    write_record(&mut frame, &unacknowledged).unwrap();
    for prefix_len in 0..=frame.len() {
        let dir = tempfile::tempdir().unwrap();
        let mut ingester = Ingester::open(dir.path(), DISK_BUDGET).unwrap();
        let file_id = ingester.append(&index, &documents()).unwrap();
        let path = ingester.active[&index].input.path.clone();
        let original = fs::read(&path).unwrap();
        drop(ingester);
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&frame[..prefix_len])
            .unwrap();
        let mut expected_bytes = original;
        let mut expected_documents = documents();
        if prefix_len == frame.len() {
            expected_bytes.extend_from_slice(&frame);
            expected_documents.push(unacknowledged.clone());
        }
        let recovered = Ingester::open(dir.path(), DISK_BUDGET).unwrap();
        assert_eq!(recovered.finished_files().len(), 1);
        let input = &recovered.finished_files()[0];
        assert_eq!(input.file_id, file_id);
        assert_eq!(input.num_docs, expected_documents.len() as u64);
        assert_eq!(input.num_bytes, expected_bytes.len() as u64);
        assert_eq!(recovered.disk_bytes, expected_bytes.len() as u64);
        assert_eq!(read_input(&input.path).1, expected_documents);
        assert_eq!(
            fs::read(&input.path).unwrap(),
            expected_bytes,
            "prefix {prefix_len}"
        );
    }
}

#[test]
fn test_recovery_rejects_invalid_metadata() {
    for payload in [
        Vec::new(),
        vec![0; 16],
        vec![0xff],       // Invalid UTF-8.
        b"logs".to_vec(), // Missing incarnation.
        b"logs:not-a-ulid".to_vec(),
        vec![0; 16 * 1024 + 1],
    ] {
        let dir = tempfile::tempdir().unwrap();
        drop(Ingester::open(dir.path(), DISK_BUDGET).unwrap());
        let path = dir
            .path()
            .join("ingesthree")
            .join(format!("{}.open", Ulid::nil()));
        let mut bytes = MAGIC.to_vec();
        // Valid framing/checksums ensure recovery rejects the metadata itself.
        write_record(&mut bytes, &payload).unwrap();
        fs::write(&path, &bytes).unwrap();
        assert!(Ingester::open(dir.path(), DISK_BUDGET).is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
}

#[test]
fn test_recovery_distinguishes_incomplete_metadata_from_missing_documents() {
    let file_id = Ulid::new();
    let index_uid = IndexUid::for_test("logs", 1);
    let mut bytes = MAGIC.to_vec();
    write_record(&mut bytes, &encode_metadata(&index_uid).unwrap()).unwrap();
    let header_len = bytes.len();
    write_record(&mut bytes, b"first document").unwrap();
    for prefix_len in [header_len - 1, header_len, bytes.len() - 1] {
        let dir = tempfile::tempdir().unwrap();
        drop(Ingester::open(dir.path(), DISK_BUDGET).unwrap());
        let path = dir
            .path()
            .join("ingesthree")
            .join(format!("{file_id}.open"));
        fs::write(&path, &bytes[..prefix_len]).unwrap();
        let result = Ingester::open(dir.path(), DISK_BUDGET);
        if prefix_len < header_len {
            assert!(result.is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes[..prefix_len]);
        } else {
            let recovered = result.unwrap();
            assert!(recovered.finished_files().is_empty());
            assert_eq!(recovered.disk_bytes, 0);
            assert!(!path.try_exists().unwrap());
            assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1); // LOCK only
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn test_recovery_checks_payloads_across_chunks(
        payload in proptest::collection::vec(any::<u8>(), 8193..32769),
        corruption_offset in any::<usize>(),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let index = IndexUid::for_test("logs", 1);
        let documents = [Bytes::new(), Bytes::from(payload), Bytes::from_static(b"last")];
        let mut ingester = Ingester::open(dir.path(), DISK_BUDGET).unwrap();
        let file_id = ingester.append(&index, &documents).unwrap();
        let original = fs::read(&ingester.active[&index].input.path).unwrap();
        drop(ingester);

        let recovered = Ingester::open(dir.path(), DISK_BUDGET).unwrap();
        let input = &recovered.finished_files()[0];
        prop_assert_eq!(input.file_id, file_id);
        prop_assert_eq!(input.num_docs, documents.len() as u64);
        prop_assert_eq!(input.num_bytes, original.len() as u64);
        prop_assert_eq!(read_input(&input.path).1, documents.to_vec());
        prop_assert_eq!(&fs::read(&input.path).unwrap(), &original);
        let path = input.path.clone();
        drop(recovered);

        // Skip metadata, the empty document, and the next frame's length/CRC.
        let metadata_len = u32::from_le_bytes(original[8..12].try_into().unwrap()) as usize;
        let payload_start = MAGIC.len() + 12 + metadata_len + 12 + 8;
        let position = payload_start + 8192 + corruption_offset % (documents[1].len() - 8192);
        let mut corrupted = original;
        corrupted[position] ^= 1;
        fs::write(&path, &corrupted).unwrap();
        prop_assert!(Ingester::open(dir.path(), DISK_BUDGET).is_err());
        prop_assert_eq!(fs::read(&path).unwrap(), corrupted);
    }
}

#[test]
fn test_corruption_fails_recovery_without_changing_file() {
    for corrupt_length in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let index = IndexUid::for_test("logs", 1);
        let mut ingester = Ingester::open(dir.path(), DISK_BUDGET).unwrap();
        ingester.append(&index, &documents()).unwrap();
        let path = ingester.active[&index].input.path.clone();
        drop(ingester);
        let mut bytes = fs::read(&path).unwrap();
        let position = if corrupt_length {
            // Skip magic and the metadata frame to reach the first document length.
            8 + 12 + u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize
        } else {
            bytes.len() - 1
        };
        bytes[position] ^= 1;
        fs::write(&path, &bytes).unwrap();
        assert!(Ingester::open(dir.path(), DISK_BUDGET).is_err());
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}

#[test]
fn test_finished_file_is_never_repaired() {
    let dir = tempfile::tempdir().unwrap();
    let mut ingester = Ingester::open(dir.path(), DISK_BUDGET).unwrap();
    ingester
        .append(&IndexUid::for_test("logs", 1), &documents())
        .unwrap();
    let files = ingester.finish().unwrap();
    let path = &files[0].path;
    let mut truncated = fs::read(path).unwrap();
    truncated.pop();
    fs::write(path, &truncated).unwrap();
    assert!(Ingester::open(dir.path(), DISK_BUDGET).is_err());
    assert_eq!(fs::read(path).unwrap(), truncated);
}

#[test]
fn test_budget_includes_framing_and_recovered_files() {
    let dir = tempfile::tempdir().unwrap();
    let index = IndexUid::for_test("logs", 1);
    let mut ingester = Ingester::open(dir.path(), DISK_BUDGET).unwrap();
    ingester.append(&index, &documents()).unwrap();
    let used = ingester.disk_bytes;
    let extra = record_size(1).unwrap();
    ingester.max_disk_bytes = used + extra - 1;
    assert!(
        ingester
            .append(&index, &[Bytes::from_static(b"x")])
            .is_err()
    );
    assert_eq!(ingester.disk_bytes, used);
    ingester.max_disk_bytes = used + extra;
    ingester
        .append(&index, &[Bytes::from_static(b"x")])
        .unwrap();
    assert_eq!(ingester.disk_bytes, used + extra);
    drop(ingester);
    let mut recovered = Ingester::open(dir.path(), used).unwrap();
    assert_eq!(recovered.disk_bytes, used + extra);
    assert!(recovered.append(&index, &documents()).is_err());
    assert!(recovered.active.is_empty());
    assert_eq!(recovered.finished_files()[0].num_docs, 3);
}

#[test]
fn test_rejections_create_no_input_files() {
    let dir = tempfile::tempdir().unwrap();
    let index = IndexUid::for_test("logs", 1);
    let mut ingester = Ingester::open(dir.path(), 0).unwrap();
    assert!(ingester.append(&index, &[]).is_err());
    assert!(ingester.append(&index, &documents()).is_err());
    assert_eq!(
        fs::read_dir(dir.path().join("ingesthree")).unwrap().count(),
        1
    ); // LOCK only
    let invalid_index = IndexUid::for_test(&"x".repeat(20_000), 1);
    ingester.max_disk_bytes = DISK_BUDGET;
    assert!(ingester.append(&invalid_index, &documents()).is_err());
    // Rejections do not poison subsequent valid writes.
    ingester.append(&index, &documents()).unwrap();
}

#[test]
fn test_flush_failure_stops_further_writes_until_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let index = IndexUid::for_test("logs", 1);
    let mut ingester = Ingester::open(dir.path(), DISK_BUDGET).unwrap();
    ingester.append(&index, &documents()).unwrap();
    let active = ingester.active.get_mut(&index).unwrap();
    // Small writes enter the userspace buffer, then fail when flush reaches the
    // read-only descriptor. This specifically checks the acknowledgment boundary.
    active.writer = BufWriter::new(File::open(&active.input.path).unwrap());
    assert!(ingester.append(&index, &documents()).is_err());
    let error = ingester.append(&index, &documents()).unwrap_err();
    assert!(error.to_string().contains("requires recovery"));
    drop(ingester);
    let recovered = Ingester::open(dir.path(), DISK_BUDGET).unwrap();
    assert_eq!(recovered.finished_files()[0].num_docs, 2);
}

#[test]
fn test_directory_has_one_owner() {
    let dir = tempfile::tempdir().unwrap();
    let ingester = Ingester::open(dir.path(), DISK_BUDGET).unwrap();
    assert!(Ingester::open(dir.path(), DISK_BUDGET).is_err());
    drop(ingester);
    assert!(Ingester::open(dir.path(), DISK_BUDGET).is_ok());
}

#[test]
fn test_incomplete_creation_is_removed_but_unknown_files_are_errors() {
    let dir = tempfile::tempdir().unwrap();
    drop(Ingester::open(dir.path(), DISK_BUDGET).unwrap());
    let directory = dir.path().join("ingesthree");
    let creating = directory.join(format!("{}.creating", Ulid::new()));
    fs::write(&creating, b"partial header").unwrap();
    let recovered = Ingester::open(dir.path(), DISK_BUDGET).unwrap();
    assert!(!creating.try_exists().unwrap());
    assert!(recovered.finished_files().is_empty());
    drop(recovered);
    let unknown = directory.join("unexpected-file");
    fs::write(&unknown, b"keep me").unwrap();
    assert!(Ingester::open(dir.path(), DISK_BUDGET).is_err());
    assert_eq!(fs::read(unknown).unwrap(), b"keep me");
}

#[test]
fn test_acknowledged_data_survives_process_exit_without_destructors() {
    const CHILD_DIR: &str = "QW_INGESTHREE_CRASH_TEST_DIR";
    if let Some(path) = std::env::var_os(CHILD_DIR) {
        let path = Path::new(&path);
        let mut ingester = Ingester::open(path, DISK_BUDGET).unwrap();
        let file_id = ingester
            .append(&IndexUid::for_test("logs", 1), &documents())
            .unwrap();
        fs::write(path.join("acknowledged-file-id"), file_id.to_string()).unwrap();
        // Exit bypasses BufWriter and Ingester destructors. The parent checks
        // this exact status so a panic or failed setup cannot pass the test.
        std::process::exit(42);
    }
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "ingesthree::tests::test_acknowledged_data_survives_process_exit_without_destructors",
        ])
        .env(CHILD_DIR, dir.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(42), "{output:?}");
    let expected_id: Ulid = fs::read_to_string(dir.path().join("acknowledged-file-id"))
        .unwrap()
        .parse()
        .unwrap();
    let recovered = Ingester::open(dir.path(), DISK_BUDGET).unwrap();
    let input = &recovered.finished_files()[0];
    assert_eq!(input.file_id, expected_id);
    assert_eq!(input.num_docs, 2);
    assert_eq!(read_input(&input.path).1, documents());
}
