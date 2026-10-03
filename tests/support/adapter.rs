//! Native process fixture for the grader tests.
use base64::{Engine, engine::general_purpose::STANDARD};
use object_tests::generated::{FingerprintStream, GeneratedBody};
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    net::TcpStream,
    process::ExitCode,
    thread,
    time::Duration,
};

/// Reads a response head and returns its status, `Content-Length` and the total that its
/// `Content-Range` names.
fn read_response_head(reader: &mut impl BufRead) -> (u16, u64, Option<u64>) {
    let mut status = 0;
    let mut length = 0;
    let mut total = None;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            return (status, length, total);
        }
        if let Some(rest) = line.strip_prefix("HTTP/1.1 ") {
            status = rest[..3].parse().unwrap();
        } else if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap();
            } else if name.eq_ignore_ascii_case("content-range") {
                total = value.trim().rsplit('/').next().unwrap().parse().ok();
            }
        }
    }
}

/// Sends a GET and feeds the body it gets into `fingerprint`; returns the object's total size.
fn read_into(address: &str, range: Option<(u64, u64)>, fingerprint: &mut FingerprintStream) -> u64 {
    let mut stream = TcpStream::connect(address).unwrap();
    let range_header = range.map_or(String::new(), |(start, end)| {
        format!("Range: bytes={start}-{end}\r\n")
    });
    write!(
        stream,
        "GET /fixture/object HTTP/1.1\r\nHost: {address}\r\nx-ms-version: 2023-11-03\r\n{range_header}\r\n"
    )
    .unwrap();
    let mut reader = BufReader::with_capacity(1 << 20, stream);
    let (_, mut remaining, total) = read_response_head(&mut reader);
    let whole = remaining;
    while remaining > 0 {
        let buffer = reader.fill_buf().unwrap();
        let take = buffer.len().min(remaining as usize);
        fingerprint.update(&buffer[..take]);
        reader.consume(take);
        remaining -= take as u64;
    }
    total.unwrap_or(whole)
}

/// How the fixture sends and reads a generated body.
#[derive(Clone, Copy, PartialEq)]
enum Transfer {
    Plain,
    /// The last byte sent is changed.
    Corrupt,
    /// Read back in ranges of 7 MiB.
    Ranged,
    /// Sent `aws-chunked`, in chunks of 1 MiB with a trailing checksum.
    AwsChunked,
}

/// PUTs the call's body, generated or held, then GETs the object back and reports the
/// fingerprint of what it read.
fn put_and_get_generated(input: &Value, transfer: Transfer) -> Value {
    let corrupt = transfer == Transfer::Corrupt;
    let address = input["endpoint"]["url"]
        .as_str()
        .unwrap()
        .strip_prefix("http://")
        .unwrap()
        .to_owned();
    let call = &input["call"];
    let generated: Option<GeneratedBody> = (call["body"]["encoding"] == "repeat")
        .then(|| serde_json::from_value(call["body"]["data"].clone()).unwrap());
    let held = call["body_base64"]
        .as_str()
        .map(|encoded| STANDARD.decode(encoded).unwrap());
    let length = generated
        .as_ref()
        .map_or_else(|| held.as_ref().unwrap().len() as u64, |body| body.length);

    let mut stream = TcpStream::connect(&address).unwrap();
    if transfer == Transfer::AwsChunked {
        let mut body = Vec::new();
        match (&generated, &held) {
            (Some(generated), _) => generated.write_to(&mut body, None).unwrap(),
            (None, Some(bytes)) => body.extend_from_slice(bytes),
            (None, None) => panic!("the call names no body"),
        }
        let mut framed = Vec::new();
        for chunk in body.chunks(1 << 20) {
            write!(framed, "{:x};chunk-signature=fixture\r\n", chunk.len()).unwrap();
            framed.extend_from_slice(chunk);
            framed.extend_from_slice(b"\r\n");
        }
        framed.extend_from_slice(b"0\r\nx-amz-checksum-crc32:fixture==\r\n\r\n");
        write!(
            stream,
            "PUT /fixture/object HTTP/1.1\r\nHost: {address}\r\nx-ms-version: 2023-11-03\r\n\
             Content-Encoding: aws-chunked\r\nx-amz-decoded-content-length: {length}\r\n\
             Content-Length: {}\r\n\r\n",
            framed.len()
        )
        .unwrap();
        stream.write_all(&framed).unwrap();
    } else {
        write!(
            stream,
            "PUT /fixture/object HTTP/1.1\r\nHost: {address}\r\nx-ms-version: 2023-11-03\r\n\
         Content-Length: {length}\r\n\r\n"
        )
        .unwrap();
        let mut writer = io::BufWriter::with_capacity(1 << 20, &mut stream);
        match (&generated, &held) {
            (Some(body), _) if corrupt => {
                body.write_to(&mut writer, Some(length - 1)).unwrap();
                writer.write_all(b"!").unwrap();
            }
            (Some(body), _) => body.write_to(&mut writer, None).unwrap(),
            (None, Some(bytes)) if corrupt => {
                writer.write_all(&bytes[..bytes.len() - 1]).unwrap();
                writer.write_all(b"!").unwrap();
            }
            (None, Some(bytes)) => writer.write_all(bytes).unwrap(),
            (None, None) => panic!("the call names no body"),
        }
        writer.flush().unwrap();
        drop(writer);
    }
    let (status, _, _) = read_response_head(&mut BufReader::new(&mut stream));
    if status != 201 {
        return json!({"outcome": "error", "kind": "other", "status": status});
    }

    let mut fingerprint = FingerprintStream::default();
    if transfer == Transfer::Ranged {
        let window = 7 << 20;
        let total = read_into(&address, Some((0, window - 1)), &mut fingerprint);
        let mut start = window;
        while start < total {
            read_into(
                &address,
                Some((start, start + window - 1)),
                &mut fingerprint,
            );
            start += window;
        }
    } else {
        read_into(&address, None, &mut fingerprint);
    }
    let mut value = json!({});
    fingerprint.finish().insert_into(&mut value);
    json!({"outcome": "ok", "value": value})
}

fn perform_http_request(input: &Value) -> Value {
    let endpoint = input["endpoint"]["url"].as_str().unwrap();
    let address = endpoint.strip_prefix("http://").unwrap();
    let is_head = input["call"]["op"] == "head";
    let method = if is_head { "HEAD" } else { "GET" };

    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "{method} /fixture/object HTTP/1.1\r\n\
         Host: {address}\r\n\
         Connection: close\r\n\
         x-ms-version: 2023-11-03\r\n\r\n"
    )
    .unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (headers, body) = response.split_once("\r\n\r\n").unwrap();
    assert!(headers.starts_with("HTTP/1.1 200"));

    let value = if is_head {
        assert!(body.is_empty());
        let size = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .unwrap()
            .1
            .trim()
            .parse::<u64>()
            .unwrap();
        json!({"size": size})
    } else {
        json!({"body_base64": STANDARD.encode(body)})
    };

    json!({"outcome": "ok", "value": value})
}

fn main() -> ExitCode {
    let mode = std::env::args().nth(1).expect("fixture mode");

    // Leave stdin unread so a full pipe exercises the parent's write deadline.
    if mode == "never-read-stdin" {
        loop {
            thread::park();
        }
    }

    let input: Value = serde_json::from_reader(io::stdin()).unwrap();

    match mode.as_str() {
        "malformed-output" => println!("not JSON"),
        "unsuccessful-exit" => {
            println!("{{}}");
            return ExitCode::from(3);
        }
        "check-input" => {
            let fields: Vec<_> = input
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(fields, ["call", "endpoint", "mode", "provider", "version"]);
            println!(
                "{}",
                json!({"outcome": "unsupported", "reason": "test fixture"})
            );
        }
        "multiple-results" => println!("{{}}\n{{}}"),
        "non-object-result" => println!("[]"),
        "http" => println!("{}", perform_http_request(&input)),
        "generated" => println!("{}", put_and_get_generated(&input, Transfer::Plain)),
        "generated-corrupt" => println!("{}", put_and_get_generated(&input, Transfer::Corrupt)),
        "generated-ranged" => println!("{}", put_and_get_generated(&input, Transfer::Ranged)),
        "generated-aws-chunked" => {
            println!("{}", put_and_get_generated(&input, Transfer::AwsChunked))
        }
        _ => panic!("unknown fixture mode {mode}"),
    }

    ExitCode::SUCCESS
}
