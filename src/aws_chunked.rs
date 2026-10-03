//! Decodes an `aws-chunked` request body as it arrives, as S3 does: the payload inside the chunk
//! framing, and the trailing headers after the last chunk.
//!
//! A chunk is `size[;chunk-signature=…]` in hexadecimal and CRLF, its bytes, and CRLF. A chunk of
//! size zero ends the payload. Trailing headers, such as `x-amz-checksum-crc32`, follow it, one
//! per line, up to an empty line. A chunk signature is not checked: it needs the client's secret.
use crate::generated::FingerprintStream;
use serde_json::{Map, Value, json};

// A size line or a trailer line is short; one longer than this is malformed.
const MAX_LINE_BYTES: usize = 4096;

enum State {
    SizeLine,
    Data { remaining: u64 },
    DataEnd { seen: usize },
    Trailer,
    Done,
    Failed(String),
}

/// The state of an `aws-chunked` body that has partly arrived.
pub(crate) struct AwsChunkedDecoder {
    state: State,
    line: Vec<u8>,
    payload: FingerprintStream,
    chunk_count: u64,
    trailers: Map<String, Value>,
}

impl Default for AwsChunkedDecoder {
    fn default() -> Self {
        Self {
            state: State::SizeLine,
            line: Vec::new(),
            payload: FingerprintStream::default(),
            chunk_count: 0,
            trailers: Map::new(),
        }
    }
}

impl AwsChunkedDecoder {
    pub(crate) fn absorb(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            match &mut self.state {
                State::Done => return self.fail("bytes after the end of the aws-chunked body"),
                State::Failed(_) => return,
                State::Data { remaining } => {
                    let take = bytes
                        .len()
                        .min(usize::try_from(*remaining).unwrap_or(usize::MAX));
                    self.payload.update(&bytes[..take]);
                    *remaining -= take as u64;
                    if *remaining == 0 {
                        self.state = State::DataEnd { seen: 0 };
                    }
                    bytes = &bytes[take..];
                }
                State::DataEnd { seen } => {
                    if bytes[0] != b"\r\n"[*seen] {
                        return self.fail("a chunk does not end in CRLF");
                    }
                    *seen += 1;
                    bytes = &bytes[1..];
                    if *seen == 2 {
                        self.state = State::SizeLine;
                    }
                }
                State::SizeLine | State::Trailer => {
                    let end = bytes.iter().position(|byte| *byte == b'\n');
                    let take = end.map_or(bytes.len(), |end| end + 1);
                    self.line.extend_from_slice(&bytes[..take]);
                    bytes = &bytes[take..];
                    if self.line.len() > MAX_LINE_BYTES {
                        return self.fail("an aws-chunked line is too long");
                    }
                    if end.is_some() {
                        let line = std::mem::take(&mut self.line);
                        self.finish_line(&line);
                    }
                }
            }
        }
    }

    fn finish_line(&mut self, line: &[u8]) {
        let Some(text) = line
            .strip_suffix(b"\r\n")
            .and_then(|text| std::str::from_utf8(text).ok())
        else {
            return self.fail("an aws-chunked line does not end in CRLF");
        };
        match self.state {
            State::SizeLine => {
                let size = text.split(';').next().unwrap_or("");
                let Ok(size) = u64::from_str_radix(size, 16) else {
                    return self.fail("an aws-chunked size is not hexadecimal");
                };
                self.chunk_count += 1;
                self.state = if size == 0 {
                    State::Trailer
                } else {
                    State::Data { remaining: size }
                };
            }
            State::Trailer if text.is_empty() => self.state = State::Done,
            State::Trailer => {
                let Some((name, value)) = text.split_once(':') else {
                    return self.fail("an aws-chunked trailer is not a header");
                };
                self.trailers
                    .insert(name.trim().to_ascii_lowercase(), json!(value.trim()));
            }
            _ => unreachable!("only size and trailer lines are read as lines"),
        }
    }

    fn fail(&mut self, reason: &str) {
        self.state = State::Failed(reason.to_owned());
    }

    /// Writes what the body decoded to into a request: the payload's fingerprint as
    /// `decoded_body_length` and `decoded_body_crc64nvme_base64`, and the framing as `aws_chunked`.
    pub(crate) fn insert_into(&self, request: &mut Value) {
        match &self.state {
            State::Done => {
                let payload = self.payload.finish();
                request["decoded_body_length"] = json!(payload.length);
                request["decoded_body_crc64nvme_base64"] = json!(payload.crc64nvme_base64());
                request["aws_chunked"] = json!({
                    "chunks": self.chunk_count,
                    "trailers": self.trailers,
                });
            }
            State::Failed(reason) => request["aws_chunked"] = json!({"error": reason}),
            _ => request["aws_chunked"] = json!({"error": "the aws-chunked body ends early"}),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::fingerprint_of;

    fn decode(pieces: &[&[u8]]) -> Value {
        let mut decoder = AwsChunkedDecoder::default();
        for piece in pieces {
            decoder.absorb(piece);
        }
        let mut request = json!({});
        decoder.insert_into(&mut request);
        request
    }

    #[test]
    fn chunks_decode_to_their_payload_in_any_split() {
        let body = b"5\r\nhello\r\n6;chunk-signature=abc\r\n world\r\n0\r\nx-amz-checksum-crc32:DUoRhQ==\r\n\r\n";
        let expected = fingerprint_of(b"hello world");
        for split in 0..body.len() {
            let request = decode(&[&body[..split], &body[split..]]);
            assert_eq!(request["decoded_body_length"], 11, "split at {split}");
            assert_eq!(
                request["decoded_body_crc64nvme_base64"],
                expected.crc64nvme_base64()
            );
            assert_eq!(request["aws_chunked"]["chunks"], 3);
            assert_eq!(
                request["aws_chunked"]["trailers"]["x-amz-checksum-crc32"],
                "DUoRhQ=="
            );
        }
    }

    #[test]
    fn malformed_or_short_bodies_are_reported() {
        for body in [
            &b"5\r\nhello\r\n"[..],
            b"5\r\nhelloXX0\r\n\r\n",
            b"zz\r\n",
            b"0\r\nnot a header\r\n\r\n",
            b"0\r\n\r\nextra",
        ] {
            let request = decode(&[body]);
            assert!(request["aws_chunked"]["error"].is_string(), "{body:?}");
            assert!(request.get("decoded_body_length").is_none());
        }
    }
}
