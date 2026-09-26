//! Native process fixture for the grader tests.
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    io::{self, Read, Write},
    net::TcpStream,
    process::ExitCode,
    thread,
    time::Duration,
};

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
        _ => panic!("unknown fixture mode {mode}"),
    }

    ExitCode::SUCCESS
}
