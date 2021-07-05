/*
 * Copyright (C) 2021 Quickwit Inc.
 *
 * Quickwit is offered under the AGPL v3.0 and as commercial software.
 * For commercial licensing, contact us at hello@quickwit.io.
 *
 * AGPL:
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as
 * published by the Free Software Foundation, either version 3 of the
 * License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU Affero General Public License for more details.
 *
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 */

use std::collections::HashMap;
use std::sync::Arc;

use tokio::task::JoinHandle;
use tracing::*;

use quickwit_metastore::{Metastore, SplitMetadata};
use quickwit_proto::{
    FetchDocsRequest, FetchDocsResult, Hit, LeafSearchRequest, LeafSearchResult, PartialHit,
    SearchRequest, SearchResult,
};

use crate::client_pool::Job;
use crate::list_relevant_splits;
use crate::ClientPool;
use crate::SearchClientPool;

// Measure the cost associated to searching in a given split metadata.
fn compute_split_cost(_split_metadata: &SplitMetadata) -> u32 {
    //TODO: Have a smarter cost, by smoothing the number of docs.
    1
}

/// Merges two partial hits that have already been sorted.
fn merge_partial_hits(
    partial_hits1: Vec<PartialHit>,
    partial_hits2: Vec<PartialHit>,
) -> Vec<PartialHit> {
    let mut merged_partial_hits = Vec::new();

    let mut index1 = 0;
    let mut index2 = 0;

    loop {
        if index1 < partial_hits1.len() && index2 < partial_hits2.len() {
            let value1 = partial_hits1.get(index1).unwrap().clone();
            let value2 = partial_hits2.get(index2).unwrap().clone();

            // Sort by descending order.
            if value1.sorting_field_value > value2.sorting_field_value {
                merged_partial_hits.push(value1);
                index1 += 1;
            } else if value1.sorting_field_value < value2.sorting_field_value {
                merged_partial_hits.push(value2);
                index2 += 1;
            } else {
                merged_partial_hits.push(value1);
                index1 += 1;
                merged_partial_hits.push(value2);
                index2 += 1;
            }
        } else if index1 < partial_hits1.len() {
            let remaining = partial_hits1
                .as_slice()
                .get(index1..partial_hits1.len())
                .unwrap_or(&Vec::new())
                .to_vec();
            merged_partial_hits.extend(remaining);
            break;
        } else if index2 < partial_hits2.len() {
            let remaining = partial_hits2
                .as_slice()
                .get(index2..partial_hits2.len())
                .unwrap_or(&Vec::new())
                .to_vec();
            merged_partial_hits.extend(remaining);
            break;
        } else {
            break;
        }
    }

    merged_partial_hits
}

/// Perform a distributed search.
/// It sends a search request over gRPC to multiple leaf nodes and merges the search results.
pub async fn root_search(
    search_request: &SearchRequest,
    metastore: &dyn Metastore,
    client_pool: &Arc<SearchClientPool>,
) -> anyhow::Result<SearchResult> {
    let start_instant = tokio::time::Instant::now();

    // Create a job for leaf node search and assign the splits that the node is responsible for based on the job.
    let split_metadata_list = list_relevant_splits(search_request, metastore).await?;

    // Create a hash map of SplitMetadata with split id as a key.
    let split_metadata_map: HashMap<String, SplitMetadata> = split_metadata_list
        .into_iter()
        .map(|split_metadata| (split_metadata.split_id.clone(), split_metadata))
        .collect();

    // Create a job for fetching docs and assign the splits that the node is responsible for based on the job.
    let leaf_search_jobs: Vec<Job> = split_metadata_map
        .keys()
        .map(|split_id| {
            // TODO: Change to a better way that does not use unwrap().
            let split_metadata = split_metadata_map.get(split_id).unwrap();
            Job {
                split: split_id.clone(),
                cost: compute_split_cost(split_metadata),
            }
        })
        .collect();

    let assigned_leaf_search_jobs = client_pool.assign_jobs(leaf_search_jobs).await?;

    let clients = client_pool.clients.read().await;

    // Create search request with start offset is 0 that used by leaf node search.
    let mut search_request_with_offset_0 = search_request.clone();
    search_request_with_offset_0.start_offset = 0;
    search_request_with_offset_0.max_hits += search_request.start_offset;

    // Perform the query phese.
    let mut leaf_search_handles: Vec<JoinHandle<anyhow::Result<LeafSearchResult>>> = Vec::new();
    for (addr, jobs) in assigned_leaf_search_jobs {
        let mut search_client = if let Some(client) = clients.get(&addr) {
            client.clone()
        } else {
            return Err(anyhow::anyhow!(
                "There is no search client for {} in the search client pool.",
                addr
            ));
        };

        let leaf_search_request = LeafSearchRequest {
            search_request: Some(search_request_with_offset_0.clone()),
            split_uris: jobs.iter().map(|job| job.split.clone()).collect(),
        };

        let handle = tokio::spawn(async move {
            match search_client.leaf_search(leaf_search_request).await {
                Ok(resp) => Ok(resp.into_inner()),
                Err(err) => Err(anyhow::anyhow!(
                    "Failed to search a leaf node due to {:?}",
                    err
                )),
            }
        });
        leaf_search_handles.push(handle);
    }
    let leaf_search_responses = futures::future::try_join_all(leaf_search_handles).await?;

    // Find the sum of the number of hits and merge multiple partial hits into a single partial hits.
    let mut num_hits = 0;
    let mut partial_hits = Vec::new();
    for response in leaf_search_responses {
        match response {
            Ok(leaf_search_result) => {
                num_hits += leaf_search_result.num_hits;
                partial_hits = merge_partial_hits(partial_hits, leaf_search_result.partial_hits);
            }
            Err(err) => error!(err=?err),
        }
    }
    // Extract the leaf search results for the range specified in the original search request.
    let start_offset = if search_request.start_offset > partial_hits.len() as u64 {
        partial_hits.len() as u64
    } else {
        search_request.start_offset
    };
    let end_offset = if start_offset + search_request.max_hits > partial_hits.len() as u64 {
        partial_hits.len() as u64
    } else {
        start_offset + search_request.max_hits
    };
    partial_hits = partial_hits
        .as_slice()
        .get(start_offset as usize..end_offset as usize)
        .unwrap_or(&Vec::new())
        .to_vec();
    // Make the leaf search results after distributed search.
    let leaf_search_result = LeafSearchResult {
        num_hits,
        partial_hits,
    };

    // Create a hash map of PartialHit with split as a key.
    let mut partial_hits_map: HashMap<String, Vec<PartialHit>> = HashMap::new();
    for partial_hit in leaf_search_result.partial_hits.iter() {
        partial_hits_map
            .entry(partial_hit.split.clone())
            .or_insert_with(Vec::new)
            .push(partial_hit.clone());
    }

    // Create a job for fetching docs and assign the splits that the node is responsible for based on the job.
    let fetch_docs_jobs: Vec<Job> = partial_hits_map
        .keys()
        .map(|split| {
            // TODO: Change to a better way that does not use unwrap().
            let split_metadata = split_metadata_map.get(split).unwrap();
            Job {
                split: split.clone(),
                cost: compute_split_cost(split_metadata),
            }
        })
        .collect();
    let assigned_fetch_docs_jobs = client_pool.assign_jobs(fetch_docs_jobs).await?;

    // Perform the fetch docs phese.
    let mut fetch_docs_handles: Vec<JoinHandle<anyhow::Result<FetchDocsResult>>> = Vec::new();
    for (addr, jobs) in assigned_fetch_docs_jobs {
        for job in jobs {
            let mut search_client = if let Some(client) = clients.get(&addr) {
                client.clone()
            } else {
                return Err(anyhow::anyhow!(
                    "There is no search client for {} in the search client pool.",
                    addr
                ));
            };

            // TODO: Change to a better way that does not use unwrap().
            let partial_hits = partial_hits_map.get(&job.split).unwrap().clone();
            let fetch_docs_request = FetchDocsRequest {
                partial_hits,
                index_id: search_request.index_id.clone(),
            };
            let handle = tokio::spawn(async move {
                match search_client.fetch_docs(fetch_docs_request).await {
                    Ok(resp) => Ok(resp.into_inner()),
                    Err(err) => Err(anyhow::anyhow!("Failed to fetch docs due to {:?}", err)),
                }
            });
            fetch_docs_handles.push(handle);
        }
    }
    let fetch_docs_responses = futures::future::try_join_all(fetch_docs_handles).await?;

    let mut hits: Vec<Hit> = Vec::new();
    for response in fetch_docs_responses {
        match response {
            Ok(fetch_docs_result) => {
                hits.extend(fetch_docs_result.hits);
            }
            Err(err) => error!(err=?err),
        }
    }

    let elapsed = start_instant.elapsed();

    Ok(SearchResult {
        num_hits: leaf_search_result.num_hits,
        hits: hits,
        elapsed_time_micros: elapsed.as_micros() as u64,
    })
}

#[cfg(test)]
mod tests {
    use quickwit_proto::PartialHit;

    use crate::root::merge_partial_hits;

    #[test]
    fn test_root_merge_partial_hits() {
        let p1 = PartialHit {
            sorting_field_value: 9,
            split: "split1".to_string(),
            segment_ord: 1,
            doc_id: 1,
        };
        let p2 = PartialHit {
            sorting_field_value: 7,
            split: "split1".to_string(),
            segment_ord: 2,
            doc_id: 2,
        };
        let p3 = PartialHit {
            sorting_field_value: 5,
            split: "split1".to_string(),
            segment_ord: 3,
            doc_id: 3,
        };
        let p4 = PartialHit {
            sorting_field_value: 3,
            split: "split1".to_string(),
            segment_ord: 4,
            doc_id: 4,
        };
        let p5 = PartialHit {
            sorting_field_value: 1,
            split: "split1".to_string(),
            segment_ord: 5,
            doc_id: 5,
        };

        let p6 = PartialHit {
            sorting_field_value: 8,
            split: "split2".to_string(),
            segment_ord: 6,
            doc_id: 6,
        };
        let p7 = PartialHit {
            sorting_field_value: 6,
            split: "split2".to_string(),
            segment_ord: 7,
            doc_id: 7,
        };
        let p8 = PartialHit {
            sorting_field_value: 4,
            split: "split2".to_string(),
            segment_ord: 8,
            doc_id: 8,
        };
        let p9 = PartialHit {
            sorting_field_value: 2,
            split: "split2".to_string(),
            segment_ord: 9,
            doc_id: 9,
        };
        let p10 = PartialHit {
            sorting_field_value: 0,
            split: "split2".to_string(),
            segment_ord: 10,
            doc_id: 10,
        };

        let partial_hits1 = vec![p1.clone(), p2.clone(), p3.clone(), p4.clone(), p5.clone()];
        let partial_hits2 = vec![p6.clone(), p7.clone(), p8.clone(), p9.clone(), p10.clone()];

        let merged_partial_hits = merge_partial_hits(partial_hits1, partial_hits2);

        assert_eq!(
            merged_partial_hits,
            vec![p1, p6, p2, p7, p3, p8, p4, p9, p5, p10]
        );
    }
}
