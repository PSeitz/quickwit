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

use std::io::Read;

use anyhow::{Context, ensure};
use bytes::{Buf, Bytes};

use super::documents_staged_for_upload::record_size;

const BATCH_HEADER_LEN: usize = 16;

/// One request's documents, serialized as repeated u32 LE lengths and opaque bytes,
/// compressed together at level 1. The outer record payload starts with a u64 LE
/// document count and u64 LE uncompressed length, followed by exactly one zstd frame.
/// The outer record checksum covers this entire payload.
pub(crate) struct CompressedBatch {
    pub(super) num_docs: u64,
    pub(super) data: Vec<u8>,
}

impl CompressedBatch {
    /// Cloning the iterator must preserve its documents. Serialization scratch is
    /// document_bytes + 4 * num_docs. The compressed allocation is bounded below.
    pub(super) fn new(
        documents: impl IntoIterator<Item: AsRef<[u8]>, IntoIter: Clone>,
    ) -> anyhow::Result<Self> {
        let documents = documents.into_iter();
        let mut document_bytes = 0usize;
        let mut num_docs = 0usize;
        for document in documents.clone() {
            u32::try_from(document.as_ref().len()).context("document exceeds u32 length")?;
            document_bytes = document_bytes
                .checked_add(document.as_ref().len())
                .context("batch size overflow")?;
            num_docs = num_docs.checked_add(1).context("document count overflow")?;
        }
        let max_len = Self::max_encoded_len(document_bytes, num_docs)?;
        let serialized_len = document_bytes + 4 * num_docs;
        let mut serialized = Vec::new();
        serialized.try_reserve_exact(serialized_len)?;
        for document in documents {
            let document = document.as_ref();
            let len = u32::try_from(document.len()).context("document exceeds u32 length")?;
            serialized.extend_from_slice(&len.to_le_bytes());
            serialized.extend_from_slice(document);
        }
        ensure!(
            serialized.len() == serialized_len,
            "document iterator changed"
        );
        let mut data = Vec::new();
        data.try_reserve_exact(max_len)?;
        // Compress into the final allocation, avoiding a second compressed copy.
        data.resize(max_len, 0);
        data[..8].copy_from_slice(&(num_docs as u64).to_le_bytes());
        data[8..16].copy_from_slice(&(serialized_len as u64).to_le_bytes());
        let compressed_len = zstd::bulk::Compressor::new(1)?
            .compress_to_buffer(&serialized, &mut data[BATCH_HEADER_LEN..])?;
        data.truncate(BATCH_HEADER_LEN + compressed_len);
        Ok(Self {
            num_docs: num_docs as u64,
            data,
        })
    }

    /// Conservative payload bound, including metadata and incompressible zstd
    /// overhead. The caller separately reserves the outer frame and file header.
    pub(super) fn max_encoded_len(document_bytes: usize, num_docs: usize) -> anyhow::Result<usize> {
        ensure!(num_docs > 0, "empty document batch");
        let serialized_len = num_docs
            .checked_mul(4)
            .and_then(|lengths| document_bytes.checked_add(lengths))
            .context("batch size overflow")?;
        // The bound itself must fit our u32 record length. Checking the input
        // first also keeps compress_bound's arithmetic inside its supported range.
        record_size(serialized_len)?;
        let max_len = zstd::zstd_safe::compress_bound(serialized_len)
            .checked_add(BATCH_HEADER_LEN)
            .context("batch size overflow")?;
        record_size(max_len)?;
        Ok(max_len)
    }
}

/// Holds one fully validated batch. Documents are slices of the decoded allocation.
#[derive(Default)]
pub(super) struct DecodedBatch {
    data: Bytes,
    pub(super) num_docs: u64,
}

impl DecodedBatch {
    pub(super) fn decode(payload: &[u8]) -> anyhow::Result<Self> {
        ensure!(payload.len() >= BATCH_HEADER_LEN, "incomplete batch header");
        let num_docs = u64::from_le_bytes(payload[..8].try_into().unwrap());
        let decoded_len = u64::from_le_bytes(payload[8..16].try_into().unwrap());
        ensure!(
            num_docs > 0 && num_docs <= decoded_len / 4,
            "invalid batch document count"
        );
        let decoded_len = usize::try_from(decoded_len).context("batch size overflow")?;
        let document_bytes = decoded_len - usize::try_from(num_docs)? * 4;
        CompressedBatch::max_encoded_len(document_bytes, usize::try_from(num_docs)?)?;
        let compressed = &payload[BATCH_HEADER_LEN..];
        ensure!(
            zstd::zstd_safe::get_frame_content_size(compressed)
                .map_err(|error| anyhow::anyhow!("invalid zstd content size: {error}"))?
                == Some(decoded_len as u64),
            "batch decompressed size mismatch"
        );
        let frame_len =
            zstd::zstd_safe::find_frame_compressed_size(compressed).map_err(|code| {
                anyhow::anyhow!(
                    "invalid zstd frame: {}",
                    zstd::zstd_safe::get_error_name(code)
                )
            })?;
        ensure!(
            frame_len == compressed.len(),
            "trailing batch compressed data"
        );
        // Allocate in proportion to decoded bytes, rather than trusting a corrupt
        // size declaration. Limit both the zstd window and total decoded output.
        let mut decoder = zstd::stream::read::Decoder::with_buffer(compressed)?;
        // Level-1 frames need at most 512 KiB; cap decoder windows at 8 MiB regardless of size
        // claims.
        let window_log = (usize::BITS - (decoded_len - 1).leading_zeros()).clamp(10, 23);
        decoder.window_log_max(window_log)?;
        let mut data = Vec::new();
        let mut buffer = [0u8; 8192];
        loop {
            let len = decoder.read(&mut buffer)?;
            if len == 0 {
                break;
            }
            ensure!(
                len <= decoded_len - data.len(),
                "batch exceeds decompressed size"
            );
            data.try_reserve(len)?;
            data.extend_from_slice(&buffer[..len]);
        }
        ensure!(
            data.len() == decoded_len,
            "batch decompressed size mismatch"
        );
        // Validate all lengths/counts before exposing even the first document.
        let mut remaining = data.as_slice();
        for _ in 0..num_docs {
            ensure!(remaining.len() >= 4, "incomplete batch document length");
            let len = u32::from_le_bytes(remaining[..4].try_into().unwrap()) as usize;
            remaining = &remaining[4..];
            ensure!(
                len <= remaining.len(),
                "batch document exceeds decoded size"
            );
            remaining = &remaining[len..];
        }
        ensure!(remaining.is_empty(), "trailing batch document data");
        Ok(Self {
            data: Bytes::from(data),
            num_docs,
        })
    }

    pub(super) fn next_document(&mut self) -> Option<Bytes> {
        if self.num_docs == 0 {
            return None;
        }
        let len = self.data.get_u32_le() as usize;
        self.num_docs -= 1;
        Some(self.data.split_to(len))
    }
}
