//! Rules over a whole request that a pattern cannot state well: an Azure Blob Batch body, and a
//! checksum header that must hold the checksum of the body the client sent.
//!
//! Each returns why the request fails, for the report.
use crate::model::{BatchSubrequest, ChecksumAlgorithm};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::Value;
use std::collections::BTreeSet;

fn request_header<'a>(request: &'a Value, name: &str) -> Option<&'a str> {
    request["headers"][name].as_str()
}

fn request_body(request: &Value) -> Result<Vec<u8>, String> {
    let encoded = request["body_base64"]
        .as_str()
        .ok_or("the request has no body")?;
    STANDARD
        .decode(encoded)
        .map_err(|_| "the request body is not base64".to_owned())
}

/// Returns a header's value from lines of `Name: value`, matching the name without case.
fn header_in<'a>(lines: &[&'a str], name: &str) -> Option<&'a str> {
    lines.iter().find_map(|line| {
        let (line_name, value) = line.split_once(':')?;
        line_name
            .trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim())
    })
}

/// Returns the boundary that a `multipart/mixed` content type names.
fn multipart_boundary(content_type: &str) -> Option<&str> {
    let (media_type, parameters) = content_type.split_once(';')?;
    if !media_type.trim().eq_ignore_ascii_case("multipart/mixed") {
        return None;
    }
    parameters.split(';').find_map(|parameter| {
        let (name, value) = parameter.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("boundary")
            .then(|| value.trim().trim_matches('"'))
    })
}

/// Checks an Azure Blob Batch request: a `multipart/mixed` body whose boundary the content type
/// names, with lines ending in CRLF and a closing boundary. Each part is `application/http` in
/// `binary` transfer encoding with a Content-ID of its own, and holds one subrequest without a
/// body, which names `x-ms-date` and `Authorization`. The subrequests are the expected ones, each
/// once, in any order, with a path or an absolute URL whose path decodes to the expected one.
pub(crate) fn check_blob_batch(
    request: &Value,
    expected: &[BatchSubrequest],
) -> Result<(), String> {
    let content_type = request_header(request, "content-type").ok_or("no Content-Type")?;
    let boundary = multipart_boundary(content_type).ok_or_else(|| {
        format!("Content-Type {content_type:?} names no multipart/mixed boundary")
    })?;
    let body = String::from_utf8(request_body(request)?).map_err(|_| "the body is not UTF-8")?;
    if body.replace("\r\n", "").contains('\n') {
        return Err("a line of the body ends in LF without CR".into());
    }

    let closing = format!("--{boundary}--");
    let without_closing = body
        .strip_suffix("\r\n")
        .unwrap_or(&body)
        .strip_suffix(&closing)
        .ok_or_else(|| format!("the body does not end with {closing}"))?;
    let delimiter = format!("--{boundary}\r\n");
    let parts: Vec<&str> = without_closing
        .strip_prefix(&delimiter)
        .ok_or_else(|| format!("the body does not start with --{boundary}"))?
        .split(&format!("\r\n{delimiter}"))
        .map(|part| part.strip_suffix("\r\n").unwrap_or(part))
        .collect();
    if parts.len() != expected.len() {
        return Err(format!(
            "{} parts, where {} were expected",
            parts.len(),
            expected.len()
        ));
    }

    let mut content_ids = BTreeSet::new();
    let mut sent = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        let (part_headers, subrequest) = part
            .split_once("\r\n\r\n")
            .ok_or_else(|| format!("part {index} has no blank line after its headers"))?;
        let part_headers: Vec<&str> = part_headers.split("\r\n").collect();
        if !header_in(&part_headers, "content-type")
            .is_some_and(|value| value.eq_ignore_ascii_case("application/http"))
        {
            return Err(format!(
                "part {index} is not Content-Type: application/http"
            ));
        }
        if !header_in(&part_headers, "content-transfer-encoding")
            .is_some_and(|value| value.eq_ignore_ascii_case("binary"))
        {
            return Err(format!(
                "part {index} is not Content-Transfer-Encoding: binary"
            ));
        }
        let content_id = header_in(&part_headers, "content-id")
            .ok_or_else(|| format!("part {index} has no Content-ID"))?;
        if !content_ids.insert(content_id.to_owned()) {
            return Err(format!("Content-ID {content_id} appears twice"));
        }

        let subrequest = subrequest.strip_suffix("\r\n").unwrap_or(subrequest);
        let (subrequest_head, subrequest_body) = subrequest
            .split_once("\r\n\r\n")
            .unwrap_or((subrequest, ""));
        if !subrequest_body.is_empty() {
            return Err(format!("the subrequest of part {index} has a body"));
        }
        let mut lines = subrequest_head.split("\r\n");
        let request_line = lines.next().unwrap_or("");
        let headers: Vec<&str> = lines.collect();
        let mut words = request_line.split(' ');
        let (Some(method), Some(target), Some("HTTP/1.1"), None) =
            (words.next(), words.next(), words.next(), words.next())
        else {
            return Err(format!(
                "part {index} starts with {request_line:?}, not METHOD TARGET HTTP/1.1"
            ));
        };
        for required in ["x-ms-date", "authorization"] {
            if header_in(&headers, required).is_none() {
                return Err(format!("the subrequest of part {index} has no {required}"));
            }
        }
        if header_in(&headers, "content-length").is_some_and(|length| length != "0") {
            return Err(format!("the subrequest of part {index} declares a body"));
        }
        let path = target
            .strip_prefix("https://")
            .or_else(|| target.strip_prefix("http://"))
            .map(|rest| rest.find('/').map_or("/", |start| &rest[start..]))
            .unwrap_or(target);
        let path = path.split('?').next().unwrap_or(path);
        let decoded = percent_encoding::percent_decode_str(path)
            .decode_utf8()
            .map_err(|_| format!("the path of part {index} does not decode to UTF-8"))?;
        sent.push((method.to_owned(), decoded.into_owned()));
    }

    for subrequest in expected {
        let position = sent
            .iter()
            .position(|(method, path)| *method == subrequest.method && *path == subrequest.path)
            .ok_or_else(|| format!("no subrequest {} {}", subrequest.method, subrequest.path))?;
        sent.remove(position);
    }
    Ok(())
}

/// Checks that a header holds the checksum of the request body, base64-encoded big-endian.
pub(crate) fn check_body_checksum(
    request: &Value,
    header: &str,
    algorithm: &ChecksumAlgorithm,
) -> Result<(), String> {
    let body = request_body(request)?;
    let expected = match algorithm {
        ChecksumAlgorithm::Crc32 => STANDARD.encode(crc32(&body, 0xEDB8_8320).to_be_bytes()),
        ChecksumAlgorithm::Crc32c => STANDARD.encode(crc32(&body, 0x82F6_3B78).to_be_bytes()),
        ChecksumAlgorithm::Crc64nvme => STANDARD.encode(crc64nvme(&body).to_be_bytes()),
    };
    match request_header(request, header) {
        Some(value) if value == expected => Ok(()),
        Some(value) => Err(format!(
            "{header} is {value}, and the body's checksum is {expected}"
        )),
        None => Err(format!("no {header}; the body's checksum is {expected}")),
    }
}

/// A reflected CRC-32 with the given reversed polynomial: ISO-HDLC or Castagnoli.
fn crc32(bytes: &[u8], reversed_polynomial: u32) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ reversed_polynomial
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// CRC-64/NVME, the CRC64NVME of S3.
fn crc64nvme(bytes: &[u8]) -> u64 {
    let mut crc = u64::MAX;
    for byte in bytes {
        crc ^= u64::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0x9A6C_9329_AC4B_C9B5
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn checksums_give_the_standard_check_values() {
        assert_eq!(crc32(b"123456789", 0xEDB8_8320), 0xCBF4_3926);
        assert_eq!(crc32(b"123456789", 0x82F6_3B78), 0xE306_9283);
        assert_eq!(crc64nvme(b"123456789"), 0xAE8B_1486_0A79_9888);
    }

    fn batch_request(body: &str) -> Value {
        json!({
            "headers": {"content-type": "multipart/mixed; boundary=batch_x"},
            "body_base64": STANDARD.encode(body),
        })
    }

    fn deletion(content_id: u32, path: &str) -> String {
        format!(
            "--batch_x\r\nContent-Type: application/http\r\nContent-Transfer-Encoding: binary\r\n\
             Content-ID: {content_id}\r\n\r\nDELETE {path} HTTP/1.1\r\nx-ms-date: Mon, 01 Jan 2024 \
             00:00:00 GMT\r\nAuthorization: Bearer t\r\nContent-Length: 0\r\n\r\n"
        )
    }

    #[test]
    fn a_blob_batch_needs_every_subrequest_in_a_well_formed_part() {
        let expected = [
            BatchSubrequest {
                method: "DELETE".into(),
                path: "/c/a b".into(),
            },
            BatchSubrequest {
                method: "DELETE".into(),
                path: "/c/b".into(),
            },
        ];
        let body = format!(
            "{}{}--batch_x--\r\n",
            deletion(0, "/c/a%20b"),
            deletion(1, "https://h/c/b")
        );
        assert_eq!(check_blob_batch(&batch_request(&body), &expected), Ok(()));

        // In another order, which Content-IDs still tell apart.
        let reordered = format!(
            "{}{}--batch_x--",
            deletion(1, "/c/b"),
            deletion(0, "/c/a%20b")
        );
        assert_eq!(
            check_blob_batch(&batch_request(&reordered), &expected),
            Ok(())
        );

        for broken in [
            format!("{}{}", deletion(0, "/c/a%20b"), deletion(1, "/c/b")),
            format!(
                "{}{}--batch_x--",
                deletion(0, "/c/a%20b"),
                deletion(0, "/c/b")
            ),
            format!(
                "{}{}--batch_x--",
                deletion(0, "/c/a%20b"),
                deletion(1, "/c/other")
            ),
            format!(
                "{}{}--batch_x--",
                deletion(0, "/c/a%20b"),
                deletion(1, "/c/b")
            )
            .replace("\r\n", "\n"),
            format!(
                "{}{}--batch_x--",
                deletion(0, "/c/a%20b"),
                deletion(1, "/c/b")
            )
            .replace("x-ms-date", "x-ms-other"),
            format!(
                "{}{}--batch_x--",
                deletion(0, "/c/a%20b"),
                deletion(1, "/c/b")
            )
            .replace("Content-ID: 1\r\n", ""),
        ] {
            assert!(
                check_blob_batch(&batch_request(&broken), &expected).is_err(),
                "{broken:?}"
            );
        }
    }

    #[test]
    fn a_body_checksum_must_be_the_checksum_of_the_body() {
        let request = |value: &str| {
            json!({
                "headers": {"x-amz-checksum-crc64nvme": value},
                "body_base64": STANDARD.encode("123456789"),
            })
        };
        let right = STANDARD.encode(0xAE8B_1486_0A79_9888_u64.to_be_bytes());
        let reversed = STANDARD.encode(0xAE8B_1486_0A79_9888_u64.to_le_bytes());
        let algorithm = ChecksumAlgorithm::Crc64nvme;
        assert_eq!(
            check_body_checksum(&request(&right), "x-amz-checksum-crc64nvme", &algorithm),
            Ok(())
        );
        assert!(
            check_body_checksum(&request(&reversed), "x-amz-checksum-crc64nvme", &algorithm)
                .is_err()
        );
    }
}
