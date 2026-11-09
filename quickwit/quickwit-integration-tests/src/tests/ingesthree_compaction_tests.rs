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
use std::time::Duration;

use quickwit_common::test_utils::wait_until_predicate;
use quickwit_config::NodeConfig;
use quickwit_config::service::QuickwitService;
use quickwit_metastore::{Split, SplitState};
use quickwit_proto::types::SplitId;
use quickwit_serve::ListSplitsQueryParams;

use super::{accept, assert_no_shards, create_index, start_cluster, wait_for_hits};
use crate::test_utils::ClusterSandbox;

pub(super) async fn wait_for_splits(
    sandbox: &ClusterSandbox,
    index_id: &str,
    num_unmerged: usize,
    num_merged: usize,
) -> Vec<Split> {
    let client = sandbox.rest_client(QuickwitService::Metastore);
    let result = wait_until_predicate(
        || async {
            let splits = client
                .splits(index_id)
                .list(ListSplitsQueryParams {
                    split_states: Some(vec![SplitState::Published]),
                    ..Default::default()
                })
                .await
                .unwrap();
            splits
                .iter()
                .filter(|split| split.split_metadata.num_merge_ops == 0)
                .count()
                == num_unmerged
                && splits
                    .iter()
                    .filter(|split| split.split_metadata.num_merge_ops == 1)
                    .count()
                    == num_merged
                && splits.len() == num_unmerged + num_merged
        },
        Duration::from_secs(45),
        Duration::from_millis(200),
    )
    .await;
    let splits = client
        .splits(index_id)
        .list(Default::default())
        .await
        .unwrap();
    assert!(
        result.is_ok(),
        "expected {num_unmerged} unmerged and {num_merged} merged published splits: {splits:?}"
    );
    splits
}

pub(super) fn assert_compacted(splits: &[Split], compactor: &NodeConfig) -> HashSet<SplitId> {
    assert_eq!(
        splits.len(),
        3,
        "expected two inputs and one merged output: {splits:?}"
    );
    let inputs: Vec<_> = splits
        .iter()
        .filter(|split| split.split_metadata.num_merge_ops == 0)
        .collect();
    assert_eq!(inputs.len(), 2);
    for input in &inputs {
        assert_eq!(input.split_state, SplitState::MarkedForDeletion);
        assert_eq!(input.split_metadata.num_docs, 1);
        assert_ne!(input.split_metadata.node_id, compactor.node_id.as_str());
    }
    let input_ids: HashSet<_> = inputs
        .iter()
        .map(|split| split.split_metadata.split_id.clone())
        .collect();
    assert_eq!(input_ids.len(), 2);
    let outputs: Vec<_> = splits
        .iter()
        .filter(|split| split.split_state == SplitState::Published)
        .collect();
    assert_eq!(outputs.len(), 1);
    let output = &outputs[0].split_metadata;
    assert_eq!(output.num_merge_ops, 1);
    assert_eq!(output.num_docs, 2);
    assert_eq!(output.node_id, compactor.node_id.as_str());
    assert!(!input_ids.contains(&output.split_id));
    for input in inputs {
        assert_eq!(output.index_uid, input.split_metadata.index_uid);
        assert_eq!(output.source_id, input.split_metadata.source_id);
        assert_eq!(output.doc_mapping_uid, input.split_metadata.doc_mapping_uid);
    }
    input_ids
}

async fn stop_node(sandbox: &mut ClusterSandbox, service: QuickwitService) -> NodeConfig {
    let config = sandbox
        .node_configs
        .iter()
        .find(|(_, services)| services.contains(&service))
        .unwrap()
        .0
        .clone();
    let shutdown = sandbox.remove_node(&config.node_id).unwrap();
    tokio::time::timeout(Duration::from_secs(30), shutdown.shutdown())
        .await
        .unwrap()
        .unwrap();
    config
}

#[tokio::test]
async fn test_ingesthree_compaction_recovers_outputs_after_planner_restart() {
    let mut sandbox = start_cluster(&[true]).await;
    let compactor = stop_node(&mut sandbox, QuickwitService::Compactor).await;
    let planner = stop_node(&mut sandbox, QuickwitService::Janitor).await;
    let index_id = "ingesthree-compaction-restart";
    create_index(&sandbox, index_id).await;
    accept(&sandbox, index_id, r#"{"body":"before"}"#).await;
    wait_for_splits(&sandbox, index_id, 1, 0).await;
    accept(&sandbox, index_id, r#"{"body":"restart"}"#).await;
    let inputs = wait_for_splits(&sandbox, index_id, 2, 0).await;
    assert_eq!(inputs.len(), 2);
    for input in &inputs {
        assert_eq!(input.split_state, SplitState::Published);
        assert_eq!(input.split_metadata.num_merge_ops, 0);
        assert_eq!(input.split_metadata.num_docs, 1);
    }
    let input_ids: HashSet<_> = inputs
        .into_iter()
        .map(|split| split.split_metadata.split_id)
        .collect();
    wait_for_hits(&sandbox, index_id, 2).await;

    // Stop the only indexer as well. Discovery and merging must use durable metastore state,
    // without another ingest request, worker publication, or local merge notification.
    stop_node(&mut sandbox, QuickwitService::Indexer).await;
    sandbox.restart_node(planner).await;
    sandbox.restart_node(compactor.clone()).await;
    // Planner startup gives existing workers ten seconds to report in-progress work, then
    // scans every five seconds. This poll observes actual replacement, not just visibility.
    let splits = wait_for_splits(&sandbox, index_id, 0, 1).await;
    assert_eq!(assert_compacted(&splits, &compactor), input_ids);
    wait_for_hits(&sandbox, index_id, 2).await;
    sandbox.assert_hit_count(index_id, "body:before", 1).await;
    sandbox.assert_hit_count(index_id, "body:restart", 1).await;
    assert_no_shards(&sandbox, index_id).await;
    sandbox.shutdown().await.unwrap();
}
