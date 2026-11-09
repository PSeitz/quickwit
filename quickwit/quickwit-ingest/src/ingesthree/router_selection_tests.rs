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

use rand::SeedableRng;
use rand::rngs::StdRng;

use super::*;

#[tokio::test]
async fn test_single_index_spreads_across_sampled_destinations() {
    let mut servers = Vec::new();
    for index in 0..8 {
        servers
            .push(destination(&format!("node-{index}"), true, true, "zone-a", None, false).await);
    }
    let nodes: Vec<_> = servers.iter().map(|server| server.node.clone()).collect();
    let zone: AvailabilityZone = "zone-a".into();
    let ordered = destinations(&nodes, "logs", Some(&zone));
    let mut rng = StdRng::seed_from_u64(42);
    let mut counts = vec![0; nodes.len()];
    for _ in 0..10_000 {
        let winner = take_destination(&mut ordered.clone(), Some(&zone), &mut rng).unwrap();
        let rank = ordered
            .iter()
            .position(|node| node.node_id == winner.node_id)
            .unwrap();
        counts[rank] += 1;
    }
    // Equal capacity retains affinity preference, but the best node only wins when sampled.
    assert_eq!(counts.iter().filter(|&&count| count > 0).count(), 7);
    assert!(counts[0] > counts[6], "{counts:?}");
    assert!(counts.iter().all(|&count| count < 3_000), "{counts:?}");

    // Exercise actual RPC dispatch too, so deterministic ranking cannot bypass sampling.
    let metrics = RouterMetrics::for_test("single_index_distribution");
    for _ in 0..128 {
        let response = route_subrequest(
            &subrequest(),
            ordered.clone(),
            ByteSize::mb(1),
            Some(&zone),
            &metrics,
        )
        .await;
        assert_eq!(response.successes.len(), 1);
        assert!(response.failures.is_empty());
    }
    let calls: Vec<_> = servers
        .iter()
        .map(|server| server.calls.load(Ordering::SeqCst))
        .collect();
    assert_eq!(calls.iter().sum::<usize>(), 128);
    assert!(
        calls.iter().filter(|&&count| count > 0).count() >= 3,
        "{calls:?}"
    );
    assert!(calls.iter().all(|&count| count < 96), "{calls:?}");
}

#[tokio::test]
async fn test_selection_preserves_az_capacity_affinity_and_distinct_retries() {
    let preferred = destination("preferred", true, true, "zone-a", None, false).await;
    let other = destination("other", true, true, "zone-a", None, false).await;
    let remote = destination("remote", true, true, "zone-b", None, false).await;
    let low_capacity = update_capacity(&preferred, Some(1)).await;
    let medium_capacity = update_capacity(&other, Some(5)).await;
    let zone: AvailabilityZone = "zone-a".into();
    let mut rng = StdRng::seed_from_u64(42);

    // Even the affinity winner loses to higher capacity within the same AZ.
    let mut candidates = vec![&low_capacity, &medium_capacity, &remote.node];
    assert_eq!(
        take_destination(&mut candidates, Some(&zone), &mut rng)
            .unwrap()
            .node_id,
        other.node.node_id
    );
    // Same AZ still wins over the remote node's higher capacity.
    assert_eq!(
        take_destination(&mut candidates, Some(&zone), &mut rng)
            .unwrap()
            .node_id,
        preferred.node.node_id
    );
    assert_eq!(
        take_destination(&mut candidates, Some(&zone), &mut rng)
            .unwrap()
            .node_id,
        remote.node.node_id
    );
    assert!(take_destination(&mut candidates, Some(&zone), &mut rng).is_none());

    // At equal capacity, retain the ordering produced by index affinity/local bonus.
    let nodes = [preferred.node.clone(), other.node.clone()];
    let ordered = destinations(&nodes, "logs", Some(&zone));
    for _ in 0..100 {
        assert_eq!(
            take_destination(&mut ordered.clone(), Some(&zone), &mut rng)
                .unwrap()
                .node_id,
            ordered[0].node_id
        );
    }
    // No known router AZ: capacity can select from the entire eligible set.
    let mut candidates = vec![&low_capacity, &remote.node];
    assert_eq!(
        take_destination(&mut candidates, None, &mut rng)
            .unwrap()
            .node_id,
        remote.node.node_id
    );
}
