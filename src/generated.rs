//! Bodies that a case describes as a pattern repeated to a length, instead of holding their bytes.
//!
//! A body is known by its fingerprint: its length and its CRC64NVME. The grader computes the
//! fingerprint of a description without generating the body, and streams an arriving body through
//! the CRC without keeping it, so a body of gigabytes costs no memory and no file.
use crate::Result;
use base64::{Engine, engine::general_purpose::STANDARD};
use crc_fast::{CrcAlgorithm, checksum, checksum_combine};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::Write;

/// The bytes of `pattern` repeated, cut at `length`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GeneratedBody {
    pub pattern_base64: String,
    pub length: u64,
}

/// What identifies a body: its length and its CRC64NVME.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    pub length: u64,
    pub crc64nvme: u64,
}

impl Fingerprint {
    /// The CRC64NVME in base64, big-endian, as S3 writes it in `x-amz-checksum-crc64nvme`.
    pub fn crc64nvme_base64(&self) -> String {
        STANDARD.encode(self.crc64nvme.to_be_bytes())
    }

    /// Reads a fingerprint from the `body_length` and `body_crc64nvme_base64` of a request or a
    /// result, or from `decoded_body_length` and `decoded_body_crc64nvme_base64`: the payload of an
    /// `aws-chunked` request, which is the object's content.
    pub fn from_json(value: &Value) -> Option<Self> {
        let prefix = if value.get("decoded_body_length").is_some() {
            "decoded_body"
        } else {
            "body"
        };
        let length = value.get(format!("{prefix}_length"))?.as_u64()?;
        let crc = STANDARD
            .decode(value.get(format!("{prefix}_crc64nvme_base64"))?.as_str()?)
            .ok()?;
        Some(Self {
            length,
            crc64nvme: u64::from_be_bytes(crc.try_into().ok()?),
        })
    }

    /// Writes the fingerprint into a request or a result.
    pub fn insert_into(&self, value: &mut Value) {
        value["body_length"] = json!(self.length);
        value["body_crc64nvme_base64"] = json!(self.crc64nvme_base64());
    }
}

/// Fingerprints bytes as they pass, without keeping them.
pub struct FingerprintStream {
    digest: crc_fast::Digest,
    length: u64,
}

impl Default for FingerprintStream {
    fn default() -> Self {
        Self {
            digest: crc_fast::Digest::new(CrcAlgorithm::Crc64Nvme),
            length: 0,
        }
    }
}

impl FingerprintStream {
    pub fn update(&mut self, bytes: &[u8]) {
        self.digest.update(bytes);
        self.length += bytes.len() as u64;
    }

    pub fn finish(&self) -> Fingerprint {
        Fingerprint {
            length: self.length,
            crc64nvme: self.digest.finalize(),
        }
    }
}

/// Returns the fingerprint of bytes in memory.
pub fn fingerprint_of(bytes: &[u8]) -> Fingerprint {
    Fingerprint {
        length: bytes.len() as u64,
        crc64nvme: checksum(CrcAlgorithm::Crc64Nvme, bytes),
    }
}

/// Checks that the fingerprint in `value`, a request or a result, is the generated body's.
pub(crate) fn check_generated_body(
    value: &Value,
    expected: &GeneratedBody,
) -> std::result::Result<(), String> {
    let expected = expected.fingerprint().map_err(|error| error.to_string())?;
    let got = Fingerprint::from_json(value).ok_or("no body_length and body_crc64nvme_base64")?;
    if got == expected {
        return Ok(());
    }
    Err(format!(
        "the body is {} bytes with CRC64NVME {}, not the generated {} bytes with {}",
        got.length,
        got.crc64nvme_base64(),
        expected.length,
        expected.crc64nvme_base64()
    ))
}

// Each write of a generated body is one tiled buffer of about this many bytes.
const TILE_BYTES: usize = 1 << 20;

impl GeneratedBody {
    /// Returns the pattern's bytes, which must not be empty.
    pub fn pattern(&self) -> Result<Vec<u8>> {
        let pattern = STANDARD.decode(&self.pattern_base64)?;
        if pattern.is_empty() {
            return Err("a generated body needs a pattern of at least one byte".into());
        }
        Ok(pattern)
    }

    /// Returns the body's fingerprint in O(log length) CRC combinations, without generating it.
    pub fn fingerprint(&self) -> Result<Fingerprint> {
        let pattern = self.pattern()?;
        let pattern_length = pattern.len() as u64;
        let repeats = self.length / pattern_length;
        let remainder = (self.length % pattern_length) as usize;

        // The CRC of the pattern repeated `repeats` times, by doubling: `block` holds the CRC of
        // 2^i repeats, and joins `whole` for each set bit of `repeats`.
        let mut whole = checksum(CrcAlgorithm::Crc64Nvme, &[]);
        let mut block = checksum(CrcAlgorithm::Crc64Nvme, &pattern);
        let mut block_length = pattern_length;
        let mut remaining_repeats = repeats;
        let mut placed_length = 0u64;
        while remaining_repeats > 0 {
            if remaining_repeats & 1 == 1 {
                whole = checksum_combine(CrcAlgorithm::Crc64Nvme, whole, block, block_length);
                placed_length += block_length;
            }
            remaining_repeats >>= 1;
            if remaining_repeats > 0 {
                block = checksum_combine(CrcAlgorithm::Crc64Nvme, block, block, block_length);
                block_length *= 2;
            }
        }
        debug_assert_eq!(placed_length, repeats * pattern_length);
        let tail = checksum(CrcAlgorithm::Crc64Nvme, &pattern[..remainder]);
        Ok(Fingerprint {
            length: self.length,
            crc64nvme: checksum_combine(CrcAlgorithm::Crc64Nvme, whole, tail, remainder as u64),
        })
    }

    /// Returns the body's bytes. Only for a body small enough to hold, such as one sent inline.
    pub fn bytes(&self) -> Result<Vec<u8>> {
        let pattern = self.pattern()?;
        let length = usize::try_from(self.length)?;
        Ok(pattern.iter().copied().cycle().take(length).collect())
    }

    /// Writes the first `limit` bytes of the body, or all of it, one tiled buffer at a time.
    pub fn write_to(&self, writer: &mut impl Write, limit: Option<u64>) -> Result<()> {
        let length = limit.map_or(self.length, |limit| limit.min(self.length));
        self.write_window_to(writer, 0, length)
    }

    /// Writes `length` bytes of the body from `start`, as a ranged read gets them.
    pub fn write_window_to(&self, writer: &mut impl Write, start: u64, length: u64) -> Result<()> {
        let pattern = self.pattern()?;
        // A whole number of patterns, so that a window starts at its offset in the first tile and
        // each later tile starts where the pattern does.
        let repeats_per_tile = TILE_BYTES.div_ceil(pattern.len());
        let tile: Vec<u8> = pattern
            .iter()
            .copied()
            .cycle()
            .take(pattern.len() * repeats_per_tile)
            .collect();
        let mut offset = (start % pattern.len() as u64) as usize;
        let mut remaining = length.min(self.length.saturating_sub(start));
        while remaining > 0 {
            let take = remaining.min((tile.len() - offset) as u64) as usize;
            writer.write_all(&tile[offset..offset + take])?;
            remaining -= take as u64;
            offset = 0;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generated(pattern: &[u8], length: u64) -> GeneratedBody {
        GeneratedBody {
            pattern_base64: STANDARD.encode(pattern),
            length,
        }
    }

    #[test]
    fn a_description_fingerprints_as_its_bytes_do() {
        for (pattern, length) in [
            (&b"0123456789abcdefg"[..], 0),
            (b"0123456789abcdefg", 1),
            (b"0123456789abcdefg", 17),
            (b"0123456789abcdefg", 1000),
            (b"x", 4097),
            (b"0123456789abcdefg", 3 * 1024 * 1024 + 5),
        ] {
            let body = generated(pattern, length);
            assert_eq!(
                body.fingerprint().unwrap(),
                fingerprint_of(&body.bytes().unwrap()),
                "{length} bytes"
            );
        }
    }

    #[test]
    fn written_and_streamed_bytes_agree_with_the_description() {
        let body = generated(b"0123456789abcdefg", 2 * TILE_BYTES as u64 + 123);
        let mut written = Vec::new();
        body.write_to(&mut written, None).unwrap();
        assert_eq!(written, body.bytes().unwrap());
        let mut stream = FingerprintStream::default();
        for piece in written.chunks(7919) {
            stream.update(piece);
        }
        assert_eq!(stream.finish(), body.fingerprint().unwrap());

        let mut truncated = Vec::new();
        body.write_to(&mut truncated, Some(10)).unwrap();
        assert_eq!(truncated, b"0123456789");
    }

    #[test]
    fn a_fingerprint_reads_back_from_json() {
        let fingerprint = generated(b"ab", 5).fingerprint().unwrap();
        let mut value = json!({});
        fingerprint.insert_into(&mut value);
        assert_eq!(Fingerprint::from_json(&value), Some(fingerprint));
        assert!(generated(b"", 1).fingerprint().is_err());
    }
}
