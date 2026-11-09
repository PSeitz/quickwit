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

//! S3 emulation and process-crash tests. See src/ingesthree/README.md for setup.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use quickwit_common::tower::BoxService;
use quickwit_common::uri::Uri;
use quickwit_config::{ChecksumAlgorithm, IndexConfig, S3StorageConfig};
use quickwit_ingest::ingesthree::{DocumentsStagedForUpload, Ingester, Uploader, WriteQueue};
use quickwit_metastore::{CreateIndexRequestExt, FileBackedMetastore};
use quickwit_proto::ingest::DocBatchV2;
use quickwit_proto::metastore::{
    CreateIndexRequest, MetastoreError, MetastoreService, MetastoreServiceClient,
    RegisterFileForIndexing, RegisterFileForIndexingResponse,
};
use quickwit_proto::types::IndexUid;
use quickwit_storage::{LocalFileStorage, S3CompatibleObjectStorage, Storage};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, oneshot};
use tokio::time::{sleep, timeout};
use tower::ServiceExt;
use ulid::Ulid;

const BUDGET: u64 = 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(20);
const DOCUMENT: &[u8] = b"opaque\0document\n";

async fn metastore(root: &Path) -> MetastoreServiceClient {
    let directory = root.join("metastore");
    fs::create_dir_all(&directory).unwrap();
    let uri: Uri = format!("file://{}", directory.display()).parse().unwrap();
    let storage = Arc::new(LocalFileStorage::from_uri(&uri).unwrap());
    MetastoreServiceClient::new(FileBackedMetastore::try_new(storage, None).await.unwrap())
}

async fn create_index(root: &Path) -> IndexUid {
    let config = IndexConfig::for_test("logs", "ram:///logs");
    let request = CreateIndexRequest::try_from_index_config(&config).unwrap();
    metastore(root)
        .await
        .create_index(request)
        .await
        .unwrap()
        .index_uid()
        .clone()
}

fn seed(root: &Path, index: &IndexUid, count: usize) -> Vec<DocumentsStagedForUpload> {
    let mut ingester = Ingester::open(root, BUDGET).unwrap();
    for _ in 0..count {
        ingester
            .append(index, &[Bytes::from_static(DOCUMENT)])
            .unwrap();
        ingester.finish_file(index).unwrap();
    }
    ingester.finished_files().to_vec()
}

async fn storage(endpoint: &str, uri: &str) -> Arc<dyn Storage> {
    assert!(
        std::env::var_os("QW_S3_ENDPOINT").is_none(),
        "remove endpoint override"
    );
    assert!(
        endpoint.starts_with("http://127.0.0.1:"),
        "local emulator only"
    );
    Arc::new(
        S3CompatibleObjectStorage::from_uri(
            &S3StorageConfig {
                endpoint: Some(endpoint.to_string()),
                region: Some("us-east-1".into()),
                access_key_id: Some("test".into()),
                secret_access_key: Some("test".into()),
                force_path_style_access: true,
                // Keep the fault proxy's wire protocol to Content-Length HTTP/1.1.
                checksum_algorithm: ChecksumAlgorithm::Disabled,
                ..Default::default()
            },
            &uri.parse().unwrap(),
        )
        .await
        .unwrap(),
    )
}

async fn open(
    root: &Path,
    storage: Arc<dyn Storage>,
    client: MetastoreServiceClient,
) -> WriteQueue {
    let uploader = Uploader::new(storage, client);
    let future = WriteQueue::open(root.into(), BUDGET, BUDGET, Some(uploader));
    timeout(DEADLINE, future)
        .await
        .expect("startup stalled")
        .unwrap()
}

fn destination() -> (String, String) {
    let endpoint = std::env::var("INGESTHREE_S3_ENDPOINT").expect("start a local S3 server");
    let bucket = std::env::var("INGESTHREE_S3_BUCKET").unwrap_or("ingesthree-test".into());
    (endpoint, format!("s3://{bucket}/{}", Ulid::new()))
}

// Read persisted registrations directly: there is no staged-file listing API yet.
fn registered(root: &Path) -> serde_json::Map<String, Value> {
    let value: Value =
        serde_json::from_slice(&fs::read(root.join("metastore/logs/metastore.json")).unwrap())
            .unwrap();
    value["staged_files"]
        .as_object()
        .cloned()
        .unwrap_or_default()
}

async fn wait_for_marker(root: &Path) {
    timeout(DEADLINE, async {
        while !root.join("ready").try_exists().unwrap() {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child never reached crash point");
}

type Registration =
    BoxService<RegisterFileForIndexing, RegisterFileForIndexingResponse, MetastoreError>;

fn gate_registration(
    client: MetastoreServiceClient,
    root: &Path,
    gate: Arc<Semaphore>,
    after_commit: bool,
) -> MetastoreServiceClient {
    let marker = root.join("ready");
    MetastoreServiceClient::tower()
        .stack_register_file_for_indexing_layer(tower::layer::layer_fn(
            move |inner: Registration| {
                let marker = marker.clone();
                let gate = gate.clone();
                tower::service_fn(move |request| {
                    let inner = inner.clone();
                    let marker = marker.clone();
                    let gate = gate.clone();
                    async move {
                        if !after_commit {
                            fs::write(&marker, []).unwrap();
                            gate.acquire().await.unwrap().forget();
                        }
                        let response = inner.oneshot(request).await?;
                        if after_commit {
                            fs::write(&marker, []).unwrap();
                            gate.acquire().await.unwrap().forget();
                        }
                        Ok::<_, MetastoreError>(response)
                    }
                })
            },
        ))
        .build(client)
}

// One real HTTP request, deliberately left unanswered. Optionally forward it to S3
// first, then discard the successful response to model a committed upload/lost reply.
struct UploadStall {
    endpoint: String,
    captured: oneshot::Receiver<(String, Vec<u8>)>,
    disconnected: oneshot::Receiver<()>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for UploadStall {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn http_message(stream: &mut TcpStream) -> (String, Vec<u8>) {
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        assert!(header.len() < 64 * 1024);
        header.push(stream.read_u8().await.unwrap());
    }
    let header = String::from_utf8(header).unwrap();
    let length: usize = header
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .expect("fault proxy requires Content-Length");
    assert!(length <= BUDGET as usize);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await.unwrap();
    (header, body)
}

impl UploadStall {
    async fn start(forward_to: Option<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (captured_tx, captured) = oneshot::channel();
        let (disconnected_tx, disconnected) = oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (header, body) = http_message(&mut socket).await;
            assert!(header.starts_with("PUT "));
            if let Some(endpoint) = forward_to {
                let mut upstream = TcpStream::connect(endpoint.strip_prefix("http://").unwrap())
                    .await
                    .unwrap();
                upstream.write_all(header.as_bytes()).await.unwrap();
                upstream.write_all(&body).await.unwrap();
                let (response, _) = http_message(&mut upstream).await;
                assert!(response.starts_with("HTTP/1.1 200"), "{response}");
            }
            captured_tx.send((header, body)).unwrap();
            // Cancellation must close the actual HTTP exchange, not just stop polling a mock.
            assert_eq!(socket.read(&mut [0]).await.unwrap(), 0);
            let _ = disconnected_tx.send(());
        });
        Self {
            endpoint,
            captured,
            disconnected,
            task,
        }
    }
}

async fn reopen_after_cancellation(root: &Path) -> Ingester {
    timeout(DEADLINE, async {
        loop {
            if let Ok(ingester) = Ingester::open(root, BUDGET) {
                break ingester;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("cancellation retained directory ownership")
}

#[tokio::test]
async fn stalled_upload_cancellation_retains_files() {
    for startup in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let index = create_index(root.path()).await;
        let mut stall = UploadStall::start(None).await;
        let uploader = Uploader::new(
            storage(&stall.endpoint, "s3://unused/stall").await,
            metastore(root.path()).await,
        );
        let (mut worker, queue) = if startup {
            seed(root.path(), &index, 1);
            let path = root.path().to_path_buf();
            (
                tokio::spawn(async move {
                    WriteQueue::open(path, BUDGET, BUDGET, Some(uploader))
                        .await
                        .unwrap();
                }),
                None,
            )
        } else {
            let queue = WriteQueue::open(root.path().into(), BUDGET, BUDGET, Some(uploader))
                .await
                .unwrap();
            queue
                .append(index.clone(), DocBatchV2::for_test(["document"]), None)
                .await
                .unwrap();
            let draining = queue.clone();
            (
                tokio::spawn(async move {
                    draining.drain().await.unwrap();
                }),
                Some(queue),
            )
        };
        let (header, body) = timeout(DEADLINE, &mut stall.captured)
            .await
            .unwrap()
            .unwrap();
        assert!(
            timeout(Duration::from_millis(100), &mut worker)
                .await
                .is_err(),
            "upload completed without an HTTP response"
        );
        assert!(registered(root.path()).is_empty());
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
        timeout(DEADLINE, &mut stall.disconnected)
            .await
            .unwrap()
            .unwrap();
        if let Some(queue) = queue {
            assert!(
                queue
                    .append(index, DocBatchV2::for_test(["late"]), None)
                    .await
                    .is_err()
            );
        }
        let recovered = reopen_after_cancellation(root.path()).await;
        let files = recovered.finished_files();
        assert_eq!(files.len(), 1);
        assert!(header.contains(&format!("/stall/{}", files[0].file_id)));
        assert_eq!(fs::read(&files[0].path).unwrap(), body);
    }
}

#[tokio::test]
#[ignore = "requires local S3-compatible server and bucket"]
async fn s3_startup_registers_and_deletes_recovered_files_before_intake() {
    let root = tempfile::tempdir().unwrap();
    let index = create_index(root.path()).await;
    let files = seed(root.path(), &index, 2);
    let expected: Vec<_> = files
        .iter()
        .map(|file| fs::read(&file.path).unwrap())
        .collect();
    let (endpoint, uri) = destination();
    let storage = storage(&endpoint, &uri).await;
    let gate = Arc::new(Semaphore::new(0));
    let client = gate_registration(
        metastore(root.path()).await,
        root.path(),
        gate.clone(),
        false,
    );
    let mut opening = Box::pin(open(root.path(), storage.clone(), client));
    tokio::select! {
        _ = &mut opening => panic!("startup passed stalled registration"),
        _ = wait_for_marker(root.path()) => {}
    }
    assert!(registered(root.path()).is_empty());
    assert!(files.iter().all(|file| file.path.is_file()));
    gate.add_permits(files.len());
    let queue = opening.await;
    let metadata = registered(root.path());
    assert_eq!(metadata.len(), files.len());
    for (file, bytes) in files.into_iter().zip(expected) {
        let record = &metadata[&file.file_id.to_string()];
        assert_eq!(record["file_uri"], format!("{uri}/{}", file.file_id));
        assert_eq!(record["num_bytes"], file.num_bytes);
        assert_eq!(record["index_uid"], serde_json::to_value(&index).unwrap());
        assert!(!file.path.try_exists().unwrap());
        let key = PathBuf::from(file.file_id.to_string());
        assert_eq!(storage.get_all(&key).await.unwrap().as_slice(), bytes);
        storage.delete(&key).await.unwrap();
    }
    timeout(DEADLINE, queue.drain()).await.unwrap().unwrap();
    let reopened = Ingester::open(root.path(), BUDGET).unwrap();
    assert!(reopened.finished_files().is_empty());
}

struct KillChild(Child);
impl Drop for KillChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
#[ignore = "subprocess entry point, launched by s3_process_crashes_recover_stable_ids"]
async fn handoff_child() {
    let root = PathBuf::from(std::env::var("INGESTHREE_CHILD_ROOT").unwrap());
    let phase = std::env::var("INGESTHREE_CHILD_PHASE").unwrap();
    let mut client = metastore(&root).await;
    if phase == "register" || phase == "registered" {
        client = gate_registration(
            client,
            &root,
            Arc::new(Semaphore::new(0)),
            phase == "registered",
        );
    }
    let storage = storage(
        &std::env::var("INGESTHREE_CHILD_ENDPOINT").unwrap(),
        &std::env::var("INGESTHREE_CHILD_URI").unwrap(),
    )
    .await;
    let queue = open(&root, storage, client).await;
    queue.drain().await.unwrap();
    assert_eq!(phase, "deleted");
    fs::write(root.join("ready"), []).unwrap();
    std::future::pending::<()>().await;
}

#[tokio::test]
#[ignore = "requires local S3-compatible server and bucket"]
async fn s3_process_crashes_recover_stable_ids() {
    for phase in ["upload", "uploaded", "register", "registered", "deleted"] {
        let root = tempfile::tempdir().unwrap();
        let index = create_index(root.path()).await;
        let file = seed(root.path(), &index, 1).pop().unwrap();
        let expected = fs::read(&file.path).unwrap();
        let key = PathBuf::from(file.file_id.to_string());
        let (endpoint, uri) = destination();
        let storage = storage(&endpoint, &uri).await;
        let mut stall = if phase == "upload" || phase == "uploaded" {
            Some(UploadStall::start((phase == "uploaded").then(|| endpoint.clone())).await)
        } else {
            None
        };
        let mut child = KillChild(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "handoff_child", "--ignored", "--nocapture"])
                .env("INGESTHREE_CHILD_ROOT", root.path())
                .env("INGESTHREE_CHILD_PHASE", phase)
                .env("INGESTHREE_CHILD_URI", &uri)
                .env(
                    "INGESTHREE_CHILD_ENDPOINT",
                    stall
                        .as_ref()
                        .map(|stall| &stall.endpoint)
                        .unwrap_or(&endpoint),
                )
                .spawn()
                .unwrap(),
        );
        if let Some(stall) = &mut stall {
            let (header, body) = timeout(DEADLINE, &mut stall.captured)
                .await
                .unwrap()
                .unwrap();
            assert!(header.contains(&format!("/{}", file.file_id)));
            assert_eq!(body, expected);
        } else {
            wait_for_marker(root.path()).await;
        }
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "child exited before {phase}"
        );
        child.0.kill().unwrap();
        assert!(!child.0.wait().unwrap().success());
        let before = registered(root.path());
        assert_eq!(
            before.len(),
            usize::from(phase == "registered" || phase == "deleted"),
            "{phase}"
        );
        assert_eq!(
            file.path.try_exists().unwrap(),
            phase != "deleted",
            "{phase}"
        );
        assert_eq!(
            storage.exists(&key).await.unwrap(),
            phase != "upload",
            "{phase}"
        );
        let queue = open(root.path(), storage.clone(), metastore(root.path()).await).await;
        assert!(!file.path.try_exists().unwrap());
        timeout(DEADLINE, queue.drain()).await.unwrap().unwrap();
        assert_eq!(storage.get_all(&key).await.unwrap().as_slice(), expected);
        let after = registered(root.path());
        assert_eq!(
            after.len(),
            1,
            "recovery created another registration at {phase}"
        );
        let record = &after[&file.file_id.to_string()];
        assert_eq!(record["file_id"], file.file_id.to_string());
        assert_eq!(record["file_uri"], format!("{uri}/{}", file.file_id));
        assert_eq!(record["num_bytes"], expected.len());
        assert_eq!(record["index_uid"], serde_json::to_value(&index).unwrap());
        if !before.is_empty() {
            assert_eq!(before, after, "replay changed committed metadata");
        }
        storage.delete(&key).await.unwrap();
    }
}
