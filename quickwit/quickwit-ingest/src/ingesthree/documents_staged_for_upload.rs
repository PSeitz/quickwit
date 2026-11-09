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

use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, Write};
use std::path::{Path, PathBuf};

use anyhow::ensure;
use bytes::Bytes;
use quickwit_proto::types::IndexUid;
use ulid::Ulid;

/// Documents staged for upload in a local file belonging to one index.
#[derive(Clone, Debug)]
pub struct DocumentsStagedForUpload {
    pub file_id: Ulid,
    pub index_uid: IndexUid,
    pub path: PathBuf,
    pub num_bytes: u64,
    pub num_docs: u64,
}

// Versioned magic, one framed metadata record, then framed opaque documents.
// Metadata: UTF-8 index UID. The file ID comes from the filename.
// Each frame: u32 LE length, CRC32(length), payload, CRC32(payload).
// Checking the length separately prevents a damaged length from being mistaken
// for an interrupted payload. CRC32 detects accidental corruption, not tampering.
pub(super) const MAGIC: &[u8; 8] = b"QWINPUT\x01";
pub(super) const FRAME_OVERHEAD: u64 = 12;
const MAX_METADATA_BYTES: usize = 16 * 1024;

pub(super) fn encode_metadata(index_uid: &IndexUid) -> anyhow::Result<Vec<u8>> {
    let payload = index_uid.to_string().into_bytes();
    ensure!(
        payload.len() <= MAX_METADATA_BYTES,
        "input metadata too large"
    );
    Ok(payload)
}

pub(super) fn record_size(len: usize) -> io::Result<u64> {
    let len = u32::try_from(len).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "input record exceeds u32 length",
        )
    })?;
    Ok(u64::from(len) + FRAME_OVERHEAD)
}

pub(super) fn write_record(writer: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    record_size(payload.len())?;
    let len_bytes = (payload.len() as u32).to_le_bytes();
    writer.write_all(&len_bytes)?;
    writer.write_all(&crc32fast::hash(&len_bytes).to_le_bytes())?;
    writer.write_all(payload)?;
    writer.write_all(&crc32fast::hash(payload).to_le_bytes())
}

fn invalid_data(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn read_length(reader: &mut impl Read) -> io::Result<u32> {
    let mut header = [0u8; 8];
    reader.read_exact(&mut header)?;
    let expected_crc = u32::from_le_bytes(header[4..].try_into().unwrap());
    if crc32fast::hash(&header[..4]) != expected_crc {
        return Err(invalid_data("input record length checksum mismatch"));
    }
    Ok(u32::from_le_bytes(header[..4].try_into().unwrap()))
}

fn check_payload_crc(reader: &mut impl Read, actual_crc: u32) -> io::Result<()> {
    let mut crc_bytes = [0u8; 4];
    reader.read_exact(&mut crc_bytes)?;
    if u32::from_le_bytes(crc_bytes) != actual_crc {
        return Err(invalid_data("input record payload checksum mismatch"));
    }
    Ok(())
}

fn read_metadata(reader: &mut impl Read) -> anyhow::Result<IndexUid> {
    let mut magic = [0u8; 8];
    reader.read_exact(&mut magic)?;
    ensure!(&magic == MAGIC, "unsupported input file format");
    let len = read_length(reader)? as usize;
    ensure!(len <= MAX_METADATA_BYTES, "input metadata too large");
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;
    check_payload_crc(reader, crc32fast::hash(&payload))?;
    Ok(std::str::from_utf8(&payload)?.parse()?)
}

/// Reads documents from a finished file, checking its size, index incarnation, and checksums.
/// Reading never repairs or truncates the file. Run filesystem operations on a blocking thread.
pub struct DocumentFileReader {
    reader: BufReader<File>,
    remaining_bytes: u64,
}

impl DocumentFileReader {
    pub fn open(path: &Path, index_uid: &IndexUid, num_bytes: u64) -> anyhow::Result<Self> {
        let file = File::open(path)?;
        ensure!(
            file.metadata()?.len() == num_bytes,
            "input file size mismatch"
        );
        let mut reader = BufReader::new(file);
        ensure!(
            read_metadata(&mut reader)? == *index_uid,
            "input index UID mismatch"
        );
        let remaining_bytes = num_bytes
            .checked_sub(reader.stream_position()?)
            .ok_or_else(|| invalid_data("input metadata exceeds file size"))?;
        Ok(Self {
            reader,
            remaining_bytes,
        })
    }

    /// Returns the next document, or `None` at the end of the file.
    /// After an error, discard the reader and leave the input unfinished.
    pub fn read_document(&mut self) -> anyhow::Result<Option<Bytes>> {
        if self.remaining_bytes == 0 {
            return Ok(None);
        }
        let len = read_length(&mut self.reader)?;
        let frame_bytes = u64::from(len) + FRAME_OVERHEAD;
        ensure!(
            frame_bytes <= self.remaining_bytes,
            "input record exceeds file size"
        );
        let mut payload = vec![0u8; len as usize];
        self.reader.read_exact(&mut payload)?;
        check_payload_crc(&mut self.reader, crc32fast::hash(&payload))?;
        self.remaining_bytes -= frame_bytes;
        Ok(Some(Bytes::from(payload)))
    }
}

// Scan with fixed memory, regardless of document length or backlog size.
fn scan_record(reader: &mut impl Read) -> io::Result<u64> {
    let len = read_length(reader)?;
    let mut remaining = u64::from(len);
    let mut hasher = crc32fast::Hasher::new();
    let mut buffer = [0u8; 8192];
    while remaining > 0 {
        let chunk_len = remaining.min(buffer.len() as u64) as usize;
        reader.read_exact(&mut buffer[..chunk_len])?;
        hasher.update(&buffer[..chunk_len]);
        remaining -= chunk_len as u64;
    }
    check_payload_crc(reader, hasher.finalize())?;
    Ok(u64::from(len) + FRAME_OVERHEAD)
}

/// Only an incomplete final record in an open file may be truncated. Bad
/// checksums/metadata and any damage to a finished file fail recovery, preserving
/// the file. An incomplete header in `.open` is also an error: headers are
/// flushed before the `.creating` -> `.open` rename and before any append.
pub(super) fn recover(
    path: &Path,
    file_id: Ulid,
    finished: bool,
) -> anyhow::Result<DocumentsStagedForUpload> {
    let file = OpenOptions::new().read(true).write(!finished).open(path)?;
    let file_len = file.metadata()?.len();
    let mut reader = BufReader::new(file);
    let index_uid = read_metadata(&mut reader)?;
    let mut valid_end = reader.stream_position()?;
    let mut num_docs = 0;
    while valid_end < file_len {
        match scan_record(&mut reader) {
            Ok(num_bytes) => {
                valid_end += num_bytes;
                num_docs += 1;
            }
            Err(error) if !finished && error.kind() == io::ErrorKind::UnexpectedEof => {
                reader.get_ref().set_len(valid_end)?;
                break;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(DocumentsStagedForUpload {
        file_id,
        index_uid,
        path: path.to_path_buf(),
        num_bytes: valid_end,
        num_docs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_document_reader_checks_frames_and_identity() {
        let index_uid = IndexUid::for_test("reader", 1);
        let mut contents = MAGIC.to_vec();
        write_record(&mut contents, &encode_metadata(&index_uid).unwrap()).unwrap();
        let document_offset = contents.len();
        write_record(&mut contents, b"first\0document\n").unwrap();
        write_record(&mut contents, b"").unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("finished");
        std::fs::write(&path, &contents).unwrap();
        let mut reader =
            DocumentFileReader::open(&path, &index_uid, contents.len() as u64).unwrap();
        assert_eq!(
            reader.read_document().unwrap().unwrap(),
            &b"first\0document\n"[..]
        );
        assert_eq!(reader.read_document().unwrap().unwrap(), Bytes::new());
        assert!(reader.read_document().unwrap().is_none());
        assert!(
            DocumentFileReader::open(
                &path,
                &IndexUid::for_test("reader", 2),
                contents.len() as u64
            )
            .is_err()
        );
        assert!(DocumentFileReader::open(&path, &index_uid, contents.len() as u64 + 1).is_err());

        // Finished inputs are never repaired, including truncated or corrupt document frames.
        let mut bad_length = contents.clone();
        bad_length[document_offset] ^= 1;
        let mut bad_payload = contents.clone();
        bad_payload[document_offset + 8] ^= 1;
        let mut huge_length = contents.clone();
        huge_length[document_offset..document_offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        huge_length[document_offset + 4..document_offset + 8]
            .copy_from_slice(&crc32fast::hash(&u32::MAX.to_le_bytes()).to_le_bytes());
        for damaged in [
            bad_length,
            bad_payload,
            huge_length,
            contents[..contents.len() - 1].to_vec(),
        ] {
            std::fs::write(&path, &damaged).unwrap();
            let mut reader =
                DocumentFileReader::open(&path, &index_uid, damaged.len() as u64).unwrap();
            let error = loop {
                match reader.read_document() {
                    Ok(Some(_)) => continue,
                    result => break result,
                }
            };
            assert!(error.is_err());
            assert_eq!(std::fs::read(&path).unwrap(), damaged);
        }
    }
}
