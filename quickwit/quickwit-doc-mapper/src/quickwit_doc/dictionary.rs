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

use std::hash::BuildHasher;
use std::net::Ipv6Addr;
use std::ops::Range;

use hashbrown::HashTable;
use rustc_hash::FxBuildHasher;

/// String values longer than this are appended without hash-table deduplication.
/// Object keys are always interned so duplicate-key resolution remains exact.
const MAX_INTERNED_VALUE_LEN: usize = 256;

/// Stores the bytes for a batch. Field names, short text, decoded binary data, and IP addresses
/// share an ID when their bytes match.
#[derive(Clone, Debug, Default)]
pub struct ByteDictionary {
    bytes: Vec<u8>,
    // Exclusive end offset in `bytes` for each dictionary ID.
    end_offsets: Vec<u32>,
    // Indexes every entry while building a batch. Freezing drops its allocation.
    lookup: HashTable<u32>,
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    FxBuildHasher.hash_one(bytes)
}

fn byte_range(end_offsets: &[u32], id: u32) -> Range<usize> {
    let id = id as usize;
    let start_offset = if id == 0 { 0 } else { end_offsets[id - 1] };
    start_offset as usize..end_offsets[id] as usize
}

impl ByteDictionary {
    /// Borrows the bytes for an ID from this dictionary.
    pub fn get_bytes(&self, id: u32) -> &[u8] {
        &self.bytes[byte_range(&self.end_offsets, id)]
    }

    /// Borrows an entry as UTF-8 text without re-validating.
    ///
    /// Contract: `id` must refer to a UTF-8 entry inserted via `intern_str` / `intern_value_str`
    /// (object keys and `QuickwitLeafValue::String`).
    #[inline]
    pub fn get_str(&self, id: u32) -> &str {
        // SAFETY: callers only pass IDs originating from `&str` dictionary insertions.
        unsafe { std::str::from_utf8_unchecked(self.get_bytes(id)) }
    }

    /// Reads the 16 network-order octets of a mapped IP address.
    pub fn get_ip(&self, id: u32) -> Ipv6Addr {
        let octets: [u8; 16] = self
            .get_bytes(id)
            .try_into()
            .expect("dictionary IP must have 16 octets");
        Ipv6Addr::from(octets)
    }

    pub(super) fn len(&self) -> usize {
        self.end_offsets.len()
    }

    pub(super) fn intern_str(&mut self, text: &str) -> Result<u32, &'static str> {
        self.intern_entry(text.as_bytes())
    }

    pub(super) fn intern_value_str(&mut self, text: &str) -> Result<u32, &'static str> {
        if text.len() <= MAX_INTERNED_VALUE_LEN {
            self.intern_entry(text.as_bytes())
        } else {
            self.append_entry(text.as_bytes())
        }
    }

    pub(crate) fn intern(&mut self, bytes: &[u8]) -> Result<u32, &'static str> {
        self.intern_entry(bytes)
    }

    fn append_entry(&mut self, bytes: &[u8]) -> Result<u32, &'static str> {
        if bytes.len() > u32::MAX as usize - self.bytes.len() {
            return Err("document batch dictionary exceeds 4 GiB");
        }
        let id = self.len() as u32;
        self.bytes.extend_from_slice(bytes);
        self.end_offsets.push(self.bytes.len() as u32);
        Ok(id)
    }

    fn intern_entry(&mut self, bytes: &[u8]) -> Result<u32, &'static str> {
        let hash = hash_bytes(bytes);
        if let Some(&id) = self.lookup.find(hash, |id| self.get_bytes(*id) == bytes) {
            return Ok(id);
        }
        let id = self.append_entry(bytes)?;
        self.lookup.insert_unique(hash, id, |id| {
            hash_bytes(&self.bytes[byte_range(&self.end_offsets, *id)])
        });
        Ok(id)
    }

    /// Drop entries introduced since the start of a rejected parse or mapping.
    pub(super) fn truncate(&mut self, len: usize) {
        for i in len..self.end_offsets.len() {
            let hash = hash_bytes(&self.bytes[byte_range(&self.end_offsets, i as u32)]);
            if let Ok(entry) = self.lookup.find_entry(hash, |id| *id == i as u32) {
                entry.remove();
            }
        }
        self.end_offsets.truncate(len);
        self.bytes
            .truncate(self.end_offsets.last().copied().unwrap_or(0) as usize);
    }

    pub(crate) fn freeze(&mut self) {
        self.lookup = HashTable::new();
        self.bytes.shrink_to_fit();
        self.end_offsets.shrink_to_fit();
    }

    pub(crate) fn mem_usage(&self) -> usize {
        self.bytes.capacity() + self.end_offsets.capacity() * std::mem::size_of::<u32>()
    }
}
