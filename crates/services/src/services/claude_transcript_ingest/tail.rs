use std::io::BufRead;

use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompleteNativeLine {
    pub line_seq: i64,
    pub start_offset: i64,
    pub end_offset: i64,
    pub raw: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailChunk {
    pub lines: Vec<CompleteNativeLine>,
    pub cursor_offset: i64,
    pub next_line_seq: i64,
    pub trailing_bytes: usize,
    pub last_line_offset: Option<i64>,
    pub last_line_hash: Option<String>,
}

/// Split a read-at-offset byte chunk into newline-terminated records. A
/// trailing partial record is intentionally left behind for the next pass;
/// the returned cursor never advances into it.
#[cfg(test)]
pub fn split_complete_lines(bytes: &[u8], base_offset: i64, first_line_seq: i64) -> TailChunk {
    let mut lines = Vec::new();
    let mut line_start = 0usize;
    let mut line_seq = first_line_seq;
    let mut last_line_offset = None;
    let mut last_line_hash = None;

    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }

        let end = index + 1;
        let line_with_newline = &bytes[line_start..end];
        let mut raw_bytes = &line_with_newline[..line_with_newline.len() - 1];
        if raw_bytes.last() == Some(&b'\r') {
            raw_bytes = &raw_bytes[..raw_bytes.len() - 1];
        }
        let start_offset = base_offset + line_start as i64;
        let end_offset = base_offset + end as i64;
        lines.push(CompleteNativeLine {
            line_seq,
            start_offset,
            end_offset,
            raw: String::from_utf8_lossy(raw_bytes).into_owned(),
        });
        line_seq += 1;
        last_line_offset = Some(start_offset);
        last_line_hash = Some(hash_bytes(line_with_newline));
        line_start = end;
    }

    TailChunk {
        lines,
        cursor_offset: base_offset + line_start as i64,
        next_line_seq: line_seq,
        trailing_bytes: bytes.len() - line_start,
        last_line_offset,
        last_line_hash,
    }
}

/// Read at most `max_lines` newline-terminated records. A trailing partial
/// record is consumed only from this temporary reader and is deliberately not
/// reflected in the returned cursor, so the next scan rereads it from disk.
pub fn read_complete_line_batch<R: BufRead>(
    reader: &mut R,
    base_offset: i64,
    first_line_seq: i64,
    max_lines: usize,
) -> std::io::Result<TailChunk> {
    let mut lines = Vec::with_capacity(max_lines);
    let mut cursor_offset = base_offset;
    let mut line_seq = first_line_seq;
    let mut trailing_bytes = 0;
    let mut last_line_offset = None;
    let mut last_line_hash = None;

    while lines.len() < max_lines {
        let mut line_with_newline = Vec::new();
        let bytes_read = reader.read_until(b'\n', &mut line_with_newline)?;
        if bytes_read == 0 {
            break;
        }
        if line_with_newline.last() != Some(&b'\n') {
            trailing_bytes = bytes_read;
            break;
        }

        let start_offset = cursor_offset;
        cursor_offset += bytes_read as i64;
        let mut raw_bytes = &line_with_newline[..line_with_newline.len() - 1];
        if raw_bytes.last() == Some(&b'\r') {
            raw_bytes = &raw_bytes[..raw_bytes.len() - 1];
        }
        lines.push(CompleteNativeLine {
            line_seq,
            start_offset,
            end_offset: cursor_offset,
            raw: String::from_utf8_lossy(raw_bytes).into_owned(),
        });
        line_seq += 1;
        last_line_offset = Some(start_offset);
        last_line_hash = Some(hash_bytes(&line_with_newline));
    }

    Ok(TailChunk {
        lines,
        cursor_offset,
        next_line_seq: line_seq,
        trailing_bytes,
        last_line_offset,
        last_line_hash,
    })
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredTailState<'a> {
    pub dev: i64,
    pub inode: i64,
    pub cursor_offset: i64,
    pub last_line_hash: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservedFileState<'a> {
    pub dev: i64,
    pub inode: i64,
    pub size: i64,
    pub verified_last_line_hash: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RescanReason {
    WatcherError,
    IdentityChanged,
    Truncated,
    LastLineChanged,
}

pub fn rescan_reason(
    stored: StoredTailState<'_>,
    observed: ObservedFileState<'_>,
    watcher_error: bool,
) -> Option<RescanReason> {
    if watcher_error {
        return Some(RescanReason::WatcherError);
    }
    if stored.dev != observed.dev || stored.inode != observed.inode {
        return Some(RescanReason::IdentityChanged);
    }
    if observed.size < stored.cursor_offset {
        return Some(RescanReason::Truncated);
    }
    if stored.cursor_offset > 0 && stored.last_line_hash != observed.verified_last_line_hash {
        return Some(RescanReason::LastLineChanged);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_line_never_advances_cursor() {
        let chunk = split_complete_lines(b"one\ntwo\npartial", 40, 7);
        assert_eq!(
            chunk
                .lines
                .iter()
                .map(|line| (&line.raw, line.line_seq))
                .collect::<Vec<_>>(),
            vec![(&"one".to_string(), 7), (&"two".to_string(), 8)]
        );
        assert_eq!(chunk.cursor_offset, 48);
        assert_eq!(chunk.next_line_seq, 9);
        assert_eq!(chunk.trailing_bytes, 7);
        assert_eq!(chunk.last_line_offset, Some(44));
    }

    #[test]
    fn a_chunk_with_only_a_partial_line_preserves_state() {
        let chunk = split_complete_lines(b"partial", 12, 3);
        assert!(chunk.lines.is_empty());
        assert_eq!(chunk.cursor_offset, 12);
        assert_eq!(chunk.next_line_seq, 3);
        assert_eq!(chunk.last_line_offset, None);
        assert_eq!(chunk.last_line_hash, None);
    }

    #[test]
    fn line_reader_bounds_batches_and_leaves_partial_cursor_uncommitted() {
        let mut reader = std::io::BufReader::new(&b"one\ntwo\npartial"[..]);
        let first = read_complete_line_batch(&mut reader, 10, 4, 1).unwrap();
        assert_eq!(first.lines.len(), 1);
        assert_eq!(first.cursor_offset, 14);
        assert_eq!(first.next_line_seq, 5);

        let second =
            read_complete_line_batch(&mut reader, first.cursor_offset, first.next_line_seq, 2)
                .unwrap();
        assert_eq!(second.lines.len(), 1);
        assert_eq!(second.lines[0].raw, "two");
        assert_eq!(second.cursor_offset, 18);
        assert_eq!(second.trailing_bytes, 7);
    }

    #[test]
    fn rescan_decisions_cover_identity_truncate_and_equal_size_rewrite() {
        let stored = StoredTailState {
            dev: 1,
            inode: 2,
            cursor_offset: 10,
            last_line_hash: Some("old"),
        };
        assert_eq!(
            rescan_reason(
                stored,
                ObservedFileState {
                    dev: 1,
                    inode: 3,
                    size: 10,
                    verified_last_line_hash: Some("old"),
                },
                false,
            ),
            Some(RescanReason::IdentityChanged)
        );
        assert_eq!(
            rescan_reason(
                stored,
                ObservedFileState {
                    dev: 1,
                    inode: 2,
                    size: 9,
                    verified_last_line_hash: Some("old"),
                },
                false,
            ),
            Some(RescanReason::Truncated)
        );
        assert_eq!(
            rescan_reason(
                stored,
                ObservedFileState {
                    dev: 1,
                    inode: 2,
                    // Same size, different bytes at the last-line offset.
                    size: 10,
                    verified_last_line_hash: Some("new"),
                },
                false,
            ),
            Some(RescanReason::LastLineChanged)
        );
    }
}
