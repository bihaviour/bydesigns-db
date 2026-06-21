//! Binary encodings for the object-storage backend's durable objects.
//!
//! The backend hand-rolls these codecs (no serde / JSON dependency — the rule
//! is to keep dependencies deliberate). Every object is immutable once written
//! and self-describing: a magic prefix, a body, and a trailing CRC32 so a torn
//! object (should one ever become visible) is rejected rather than misread.
//!
//! Three object shapes live here:
//!
//! * **log segment** — one CAS-appended commit-log slot, holding the ordered
//!   items of a single `append_wal` / `put_page` call. Replaying segments in
//!   sequence order reconstructs the gap-free LSN stream.
//! * **delta layer** — the immutable object a memtable flush produces: the
//!   `(page_id, lsn) -> image` records over a bounded LSN span.
//! * **image layer** — the immutable full-page snapshot compaction produces:
//!   one materialized version per `page_id` as of an image LSN. The read floor.

use crate::types::{StorageError, PAGE_SIZE};

const SEG_MAGIC: &[u8; 6] = b"BDLOG1";
const DELTA_MAGIC: &[u8; 6] = b"BDDEL1";
const IMAGE_MAGIC: &[u8; 6] = b"BDIMG1";

// Log-item tags.
const ITEM_WAL: u8 = 1;
const ITEM_PAGE: u8 = 2;

/// One durable item inside a commit-log segment. Each item consumes exactly one
/// LSN on replay, keeping the LSN stream gap-free across both write paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogItem {
    /// An opaque engine WAL record. The backend never interprets the bytes.
    Wal(Vec<u8>),
    /// A versioned page image written via `put_page`.
    Page { page_id: u64, image: Vec<u8> },
}

/// A `(page_id, version_lsn, image)` triple — the unit a delta/image layer holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageRecord {
    pub page_id: u64,
    pub lsn: u64,
    pub image: Vec<u8>,
}

// ---- log segment ----------------------------------------------------------

/// Serialize one commit-log segment payload (CRC-checked).
pub fn encode_segment(items: &[LogItem]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(SEG_MAGIC);
    body.extend_from_slice(&(items.len() as u32).to_le_bytes());
    for item in items {
        match item {
            LogItem::Wal(bytes) => {
                body.push(ITEM_WAL);
                body.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                body.extend_from_slice(bytes);
            }
            LogItem::Page { page_id, image } => {
                body.push(ITEM_PAGE);
                body.extend_from_slice(&page_id.to_le_bytes());
                body.extend_from_slice(&(image.len() as u32).to_le_bytes());
                body.extend_from_slice(image);
            }
        }
    }
    finish(body)
}

/// Parse a commit-log segment payload, rejecting a torn / corrupt object.
pub fn decode_segment(bytes: &[u8]) -> Result<Vec<LogItem>, StorageError> {
    let mut r = Reader::open(bytes, SEG_MAGIC, "log segment")?;
    let count = r.u32()? as usize;
    let mut items = Vec::with_capacity(count);
    for _ in 0..count {
        match r.u8()? {
            ITEM_WAL => {
                let len = r.u32()? as usize;
                items.push(LogItem::Wal(r.take(len)?.to_vec()));
            }
            ITEM_PAGE => {
                let page_id = r.u64()?;
                let len = r.u32()? as usize;
                if len > PAGE_SIZE {
                    return Err(StorageError::Corruption(format!(
                        "log page image {len} exceeds PAGE_SIZE {PAGE_SIZE}"
                    )));
                }
                items.push(LogItem::Page {
                    page_id,
                    image: r.take(len)?.to_vec(),
                });
            }
            other => {
                return Err(StorageError::Corruption(format!(
                    "log segment: unknown item tag {other}"
                )))
            }
        }
    }
    Ok(items)
}

// ---- delta / image layers -------------------------------------------------

/// Serialize a delta layer (the records flushed from a memtable).
pub fn encode_delta(records: &[PageRecord]) -> Vec<u8> {
    encode_layer(DELTA_MAGIC, records)
}

/// Parse a delta layer.
pub fn decode_delta(bytes: &[u8]) -> Result<Vec<PageRecord>, StorageError> {
    decode_layer(bytes, DELTA_MAGIC, "delta layer")
}

/// Serialize an image layer (one materialized version per page from compaction).
pub fn encode_image(records: &[PageRecord]) -> Vec<u8> {
    encode_layer(IMAGE_MAGIC, records)
}

/// Parse an image layer.
pub fn decode_image(bytes: &[u8]) -> Result<Vec<PageRecord>, StorageError> {
    decode_layer(bytes, IMAGE_MAGIC, "image layer")
}

fn encode_layer(magic: &[u8; 6], records: &[PageRecord]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(magic);
    body.extend_from_slice(&(records.len() as u32).to_le_bytes());
    for rec in records {
        body.extend_from_slice(&rec.page_id.to_le_bytes());
        body.extend_from_slice(&rec.lsn.to_le_bytes());
        body.extend_from_slice(&(rec.image.len() as u32).to_le_bytes());
        body.extend_from_slice(&rec.image);
    }
    finish(body)
}

fn decode_layer(
    bytes: &[u8],
    magic: &[u8; 6],
    what: &str,
) -> Result<Vec<PageRecord>, StorageError> {
    let mut r = Reader::open(bytes, magic, what)?;
    let count = r.u32()? as usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let page_id = r.u64()?;
        let lsn = r.u64()?;
        let len = r.u32()? as usize;
        if len > PAGE_SIZE {
            return Err(StorageError::Corruption(format!(
                "{what} page image {len} exceeds PAGE_SIZE {PAGE_SIZE}"
            )));
        }
        out.push(PageRecord {
            page_id,
            lsn,
            image: r.take(len)?.to_vec(),
        });
    }
    Ok(out)
}

// ---- framing helpers ------------------------------------------------------

/// Append the CRC32 over the body and return the finished object bytes.
fn finish(mut body: Vec<u8>) -> Vec<u8> {
    let crc = crc32(&body);
    body.extend_from_slice(&crc.to_le_bytes());
    body
}

/// A bounds- and CRC-checked cursor over a decoded object body.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn open(bytes: &'a [u8], magic: &[u8; 6], what: &str) -> Result<Reader<'a>, StorageError> {
        if bytes.len() < magic.len() + 4 {
            return Err(StorageError::Corruption(format!(
                "{what}: object too short"
            )));
        }
        let (body, crc_bytes) = bytes.split_at(bytes.len() - 4);
        let stored = u32::from_le_bytes(crc_bytes.try_into().unwrap());
        if crc32(body) != stored {
            return Err(StorageError::Corruption(format!("{what}: CRC mismatch")));
        }
        if &body[..magic.len()] != magic {
            return Err(StorageError::Corruption(format!("{what}: bad magic")));
        }
        Ok(Reader {
            buf: body,
            pos: magic.len(),
        })
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], StorageError> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|e| *e <= self.buf.len())
            .ok_or_else(|| StorageError::Corruption("object: truncated field".into()))?;
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, StorageError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, StorageError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, StorageError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}

// ---- CRC32 (IEEE 802.3) ---------------------------------------------------

fn crc32_table() -> &'static [u32; 256] {
    use std::sync::OnceLock;
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        let mut i = 0;
        while i < 256 {
            let mut c = i as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
                k += 1;
            }
            t[i] = c;
            i += 1;
        }
        t
    })
}

/// CRC32 over a byte slice. Shared shape with `local.rs`; small enough to keep
/// duplicated rather than couple the two backends through a shared module.
pub fn crc32(data: &[u8]) -> u32 {
    let table = crc32_table();
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_roundtrip() {
        let items = vec![
            LogItem::Wal(b"hello".to_vec()),
            LogItem::Page {
                page_id: 42,
                image: vec![0xAB; 100],
            },
        ];
        let bytes = encode_segment(&items);
        assert_eq!(decode_segment(&bytes).unwrap(), items);
    }

    #[test]
    fn layer_roundtrip() {
        let recs = vec![
            PageRecord {
                page_id: 1,
                lsn: 10,
                image: vec![1; 8],
            },
            PageRecord {
                page_id: 2,
                lsn: 11,
                image: vec![2; 8],
            },
        ];
        assert_eq!(decode_delta(&encode_delta(&recs)).unwrap(), recs);
        assert_eq!(decode_image(&encode_image(&recs)).unwrap(), recs);
    }

    #[test]
    fn corrupt_object_rejected() {
        let mut bytes = encode_segment(&[LogItem::Wal(b"x".to_vec())]);
        let n = bytes.len();
        bytes[n - 6] ^= 0xFF; // flip a body byte; CRC no longer matches
        assert!(matches!(
            decode_segment(&bytes),
            Err(StorageError::Corruption(_))
        ));
    }

    #[test]
    fn truncated_object_rejected() {
        let bytes = encode_delta(&[PageRecord {
            page_id: 1,
            lsn: 1,
            image: vec![9; 16],
        }]);
        assert!(matches!(
            decode_delta(&bytes[..bytes.len() / 2]),
            Err(StorageError::Corruption(_))
        ));
    }
}
