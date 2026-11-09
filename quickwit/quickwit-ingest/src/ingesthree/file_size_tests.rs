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

use std::process::Command;

use super::*;

#[test]
fn test_file_size_environment() {
    const CHILD: &str = "QW_INGESTHREE_FILE_SIZE_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let directory = tempfile::tempdir().unwrap();
        let value = std::env::var(TARGET_FILE_SIZE_ENV_KEY).ok();
        let expected = match value.as_deref() {
            None => Some(100_000_000),
            Some("1B") => Some(1),
            Some("2 MiB") => Some(2 * 1024 * 1024),
            Some("2000000") => Some(2_000_000),
            Some("" | "0B" | "not-a-size") => None,
            _ => panic!("unexpected test value: {value:?}"),
        };
        let result = Ingester::open(directory.path(), 10_000_000);
        let Some(expected) = expected else {
            let error = result
                .err()
                .expect("invalid file target should fail startup");
            assert!(error.to_string().contains(TARGET_FILE_SIZE_ENV_KEY));
            assert!(!directory.path().join("ingesthree").try_exists().unwrap());
            return;
        };
        let mut ingester = result.unwrap();
        assert_eq!(ingester.file_limits.target_file_bytes, expected);
        assert_eq!(ingester.file_limits.max_file_age, Duration::from_secs(10));
        assert_eq!(ingester.file_limits.max_open_files, 1_024);
        let index_uid = IndexUid::for_test("file-size", 1);
        let documents = vec![Bytes::from_static(b"first"), Bytes::from_static(b"second")];
        let first_id = ingester.append(&index_uid, &documents).unwrap();
        let second_id = ingester.append(&index_uid, &documents).unwrap();
        if expected == 1 {
            assert_ne!(first_id, second_id);
            assert!(ingester.active.is_empty());
            assert_eq!(ingester.finished_files().len(), 2);
            for input in ingester.finished_files() {
                assert!(input.num_bytes > expected);
                assert_eq!(input.num_docs, 2);
                let mut reader =
                    DocumentFileReader::open(&input.path, &index_uid, input.num_bytes).unwrap();
                assert_eq!(reader.read_document().unwrap(), Some(documents[0].clone()));
                assert_eq!(reader.read_document().unwrap(), Some(documents[1].clone()));
                assert!(reader.read_document().unwrap().is_none());
            }
        } else {
            assert_eq!(first_id, second_id);
            assert_eq!(ingester.active.len(), 1);
            assert!(ingester.finished_files().is_empty());
        }
        return;
    }
    // Each case gets its own process, avoiding mutation of a multithreaded test's environment.
    for value in [
        None,
        Some("1B"),
        Some("2 MiB"),
        Some("2000000"),
        Some(""),
        Some("0B"),
        Some("not-a-size"),
    ] {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "ingesthree::file_size_tests::test_file_size_environment",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env_remove(TARGET_FILE_SIZE_ENV_KEY);
        if let Some(value) = value {
            command.env(TARGET_FILE_SIZE_ENV_KEY, value);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "file-size value {value:?}: {output:?}"
        );
    }
}
