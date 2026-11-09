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

use quickwit_common::ring_buffer::RingBuffer;

/// Tracks disk and queue headroom with the v2 scoring formula, independently of WAL state.
/// Seven samples compare the current reading with six intervals ago (30 seconds).
/// The disk and queue readings always share the same sampling cadence.
pub(super) struct CapacityTracker {
    capacities: [u64; 2],
    readings: RingBuffer<[f64; 2], 7>,
}

impl CapacityTracker {
    pub(super) fn new(disk_capacity: u64, queue_capacity: u64) -> Self {
        assert!(disk_capacity > 0 && queue_capacity > 0);
        Self {
            capacities: [disk_capacity, queue_capacity],
            readings: RingBuffer::default(),
        }
    }

    pub(super) fn record_and_score(&mut self, disk_used: u64, queue_used: u64) -> usize {
        let used = [disk_used, queue_used];
        // Subtract bytes first so exactly 5% remaining stays at the cutoff.
        let remaining = [0, 1]
            .map(|i| self.capacities[i].saturating_sub(used[i]) as f64 / self.capacities[i] as f64);
        self.readings.push_back(remaining);
        let oldest = self.readings.front().expect("just recorded capacity");
        let scores = [0, 1].map(|i| {
            if remaining[i] <= 0.05 {
                return 0;
            }
            // Eight points for headroom. Two for stability, falling to zero when
            // headroom shrinks by at least 10% over the lookback window.
            let drain = (oldest[i] - remaining[i]).clamp(0.0, 0.10);
            (8.0 * remaining[i] + 2.0 * (1.0 - drain / 0.10)).clamp(0.0, 10.0) as usize
        });
        scores[0].min(scores[1])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capacity_cutoff_and_recovery() {
        for used in [95, 96, 100, 200] {
            let mut tracker = CapacityTracker::new(100, 100);
            assert_eq!(tracker.record_and_score(used, 0), 0);
            assert_eq!(tracker.record_and_score(0, used), 0);
            assert_eq!(tracker.record_and_score(0, 0), 10);
        }
    }

    #[test]
    fn test_capacity_uses_worse_resource() {
        for (disk, queue) in [(10, 90), (90, 10)] {
            let mut tracker = CapacityTracker::new(100, 100);
            assert_eq!(tracker.record_and_score(disk, queue), 2);
        }
    }

    #[test]
    fn test_capacity_trend_spans_six_intervals() {
        let mut tracker = CapacityTracker::new(100, 100);
        assert_eq!(tracker.record_and_score(0, 0), 10);
        for _ in 0..6 {
            assert_eq!(tracker.record_and_score(90, 0), 0);
        }
        // Once the initial empty sample ages out, stable 10% headroom scores two.
        assert_eq!(tracker.record_and_score(90, 0), 2);
    }
}
