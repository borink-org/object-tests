//! Runs one adapter process per case and serves the case's loopback endpoint.
use crate::{Result, Session, generated::GeneratedBody, model::*};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::TcpListener,
    process::{Command, Stdio},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

// Each case serves its loopback endpoint from this many threads.
const HTTP_WORKERS: usize = 2;

// A generated call body up to this size reaches the adapter as `body_base64`, so an adapter that
// cannot generate a body still runs the cases with small ones.
const INLINE_GENERATED_BYTES: u64 = 1 << 20;

// An adapter gets this long, and a second more for every 64 MiB of generated body a case moves.
const ADAPTER_SECONDS: u64 = 30;
const GENERATED_BYTES_PER_SECOND: u64 = 64 << 20;

/// Returns the call an adapter receives: a small generated body comes as its bytes.
fn adapter_call(call: &Value) -> Result<Value> {
    let mut call = call.clone();
    if call["body"]["encoding"] == "repeat" {
        let generated: GeneratedBody = serde_json::from_value(call["body"]["data"].clone())?;
        if generated.length <= INLINE_GENERATED_BYTES {
            let object = call.as_object_mut().ok_or("call")?;
            object.remove("body");
            object.insert(
                "body_base64".into(),
                json!(STANDARD.encode(generated.bytes()?)),
            );
        }
    }
    Ok(call)
}

/// Returns the bytes of generated body that a case moves: in its call, its request checks and
/// its responses.
fn generated_byte_count(case: &Case) -> u64 {
    let call_bytes = serde_json::from_value::<GeneratedBody>(case.call["body"]["data"].clone())
        .map_or(0, |generated| generated.length);
    let mut exchange_bytes = 0;
    let mut exchanges: Vec<&Exchange> = case.exchanges.iter().collect();
    while let Some(exchange) = exchanges.pop() {
        for alternative in &exchange.alternatives {
            exchanges.extend(&alternative.then);
            if let Body::Repeat(generated) = &alternative.response.body {
                exchange_bytes += generated.length;
            }
            for check in &alternative.request {
                if let Rule::GeneratedBody { length, .. } = check.rule {
                    exchange_bytes += length;
                }
            }
        }
    }
    call_bytes + exchange_bytes
}

// The proxy variables that HTTP clients read, such as ureq and the AWS CRT.
const PROXY_VARIABLES: &[&str] = &[
    "ALL_PROXY",
    "all_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "NO_PROXY",
    "no_proxy",
];

// An offline adapter must reach only the grader. It inherits no proxy, and where the endpoint
// names the grader as its proxy, it gets that proxy in HTTP_PROXY too, so that a client that
// reads its proxy from the environment needs no configuration. A live adapter keeps the
// caller's proxy, which may be the way to the service.
fn set_proxy_environment(adapter: &mut Command, message: &Value) {
    if message["mode"] != "offline" {
        return;
    }
    for name in PROXY_VARIABLES {
        adapter.env_remove(name);
    }
    if let Some(proxy_url) = message["endpoint"]["proxy_url"].as_str() {
        adapter
            .env("HTTP_PROXY", proxy_url)
            .env("http_proxy", proxy_url);
    }
}

fn invoke_adapter_process(command: &[String], message: &Value, timeout: Duration) -> Result<Value> {
    let mut input_bytes = serde_json::to_vec(message)?;
    input_bytes.push(b'\n');
    if input_bytes.len() > MAX_MESSAGE_BYTES {
        return Err("adapter input exceeds 16 MiB".into());
    }
    let deadline = Instant::now() + timeout;
    let mut adapter = Command::new(&command[0]);
    set_proxy_environment(&mut adapter, message);
    let mut child = adapter
        .args(&command[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut input = child.stdin.take().expect("piped stdin");
    let output = child.stdout.take().expect("piped stdout");
    // Both pipes must progress while the parent enforces the process deadline.
    let (input_sender, input_receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let result = input.write_all(&input_bytes);
        drop(input);
        let _ = input_sender.send(result);
    });
    let (output_sender, output_receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut bytes = vec![];
        let result = output
            .take((MAX_MESSAGE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = output_sender.send(result);
    });
    let result = (|| {
        input_receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| "adapter input deadline exceeded")??;
        let bytes = output_receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| "adapter output deadline exceeded")??;
        if bytes.len() > MAX_MESSAGE_BYTES {
            return Err("adapter output exceeds 16 MiB".into());
        }
        loop {
            if let Some(status) = child.try_wait()? {
                if !status.success() {
                    return Err(format!("adapter exited {status}").into());
                }
                break;
            }
            if Instant::now() >= deadline {
                return Err("adapter process deadline exceeded".into());
            }
            // The adapter closed its output, so it exits within moments.
            thread::sleep(Duration::from_millis(1));
        }
        let value: Value = serde_json::from_slice(&bytes)?;
        if !value.is_object() {
            return Err("adapter result must be a JSON object".into());
        }
        Ok(value)
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

/// Grades one adapter invocation against a case's requests and result assertions.
///
/// Starts a fresh process and, for offline HTTP cases, a loopback endpoint.
///
/// # Errors
/// Returns an error if the adapter command is empty or the loopback listener cannot start.
/// An adapter that crashes, times out or prints malformed output gets a report
/// with the `failed` verdict.
pub fn grade_case(
    case: &Case,
    profile: &Profile,
    command: &[String],
    live_endpoint: Option<&Value>,
) -> Result<Value> {
    if command.is_empty() {
        return Err("missing adapter command after --".into());
    }
    let session = Mutex::new(Session::new(case.clone(), profile.clone()));
    let mut endpoint = live_endpoint.unwrap_or(&profile.endpoint).clone();
    let server = if live_endpoint.is_none() && !case.exchanges.is_empty() {
        let server = TcpListener::bind("127.0.0.1:0")?;
        let loopback_url = json!(format!("http://{}", server.local_addr()?));
        if endpoint.get("url").is_some() {
            endpoint["proxy_url"] = loopback_url;
        } else {
            endpoint["url"] = loopback_url;
        }
        Some(server)
    } else {
        None
    };
    let message = json!({
        "version": 1,
        "mode": if live_endpoint.is_some() {"live"} else {"offline"},
        "provider": profile.provider,
        "endpoint": endpoint,
        "call": adapter_call(&case.call)?,
    });
    let adapter_time = Duration::from_secs(
        ADAPTER_SECONDS + generated_byte_count(case) / GENERATED_BYTES_PER_SECOND,
    );
    let stop = AtomicBool::new(false);
    let result = thread::scope(|scope| {
        if let Some(server) = &server {
            for _ in 0..HTTP_WORKERS {
                scope.spawn(|| {
                    loop {
                        let accepted = server.accept();
                        if stop.load(Ordering::Acquire) {
                            break;
                        }
                        match accepted {
                            Ok((stream, _)) => {
                                crate::http_transport::serve_connection(stream, &session)
                            }
                            Err(error) => {
                                session
                                    .lock()
                                    .unwrap()
                                    .failures
                                    .push(json!({"error": error.to_string()}));
                                break;
                            }
                        }
                    }
                });
            }
        }
        let result = invoke_adapter_process(command, &message, adapter_time);
        stop.store(true, Ordering::Release);
        // A worker blocks in accept, so one connection per worker wakes it to see the stop.
        if let Some(server) = &server
            && let Ok(address) = server.local_addr()
        {
            for _ in 0..HTTP_WORKERS {
                let _ = std::net::TcpStream::connect(address);
            }
        }
        result
    });
    let session = session.into_inner().unwrap();
    Ok(match result {
        Ok(result) => session.finish(&result, SystemTime::now()),
        Err(error) => {
            json!({
                "id": case.id,
                "lane": case.lane,
                "verdict": "failed",
                "purpose": case.purpose,
                "reason": error.to_string(),
                "request_failures": session.failures,
            })
        }
    })
}

/// Grades `cases` on up to `worker_count` threads and returns their reports in the order of `cases`.
///
/// # Errors
/// Returns the first error from [`grade_case`] in the order of `cases`.
pub fn grade_cases(
    cases: &[&Case],
    profiles: &BTreeMap<String, Profile>,
    command: &[String],
    live_endpoints: Option<&BTreeMap<String, Value>>,
    worker_count: usize,
) -> Result<Vec<Value>> {
    let next_case_index = AtomicUsize::new(0);
    let reports: Vec<Mutex<Option<Result<Value>>>> =
        cases.iter().map(|_| Mutex::new(None)).collect();
    thread::scope(|scope| {
        for _ in 0..worker_count.min(cases.len()) {
            scope.spawn(|| {
                loop {
                    let case_index = next_case_index.fetch_add(1, Ordering::Relaxed);
                    let Some(case) = cases.get(case_index) else {
                        break;
                    };
                    let live_endpoint = live_endpoints.map(|endpoints| &endpoints[&case.profile]);
                    let report = grade_case(case, &profiles[&case.profile], command, live_endpoint);
                    *reports[case_index].lock().unwrap() = Some(report);
                }
            });
        }
    });
    reports
        .into_iter()
        .map(|report| {
            report
                .into_inner()
                .unwrap()
                .expect("a worker grades every case")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxy_environment(message: Value) -> Vec<(String, Option<String>)> {
        let mut adapter = Command::new("adapter");
        set_proxy_environment(&mut adapter, &message);
        let mut environment: Vec<_> = adapter
            .get_envs()
            .map(|(name, value)| {
                let text = |text: &std::ffi::OsStr| text.to_string_lossy().into_owned();
                (text(name), value.map(text))
            })
            .collect();
        environment.sort();
        environment
    }

    #[test]
    fn an_offline_adapter_gets_the_grader_as_its_only_proxy() {
        let proxied = proxy_environment(json!({
            "mode": "offline",
            "endpoint": {"proxy_url": "http://127.0.0.1:1234"},
        }));
        let proxy = Some("http://127.0.0.1:1234".to_owned());
        assert!(proxied.contains(&("HTTP_PROXY".to_owned(), proxy.clone())));
        assert!(proxied.contains(&("http_proxy".to_owned(), proxy)));
        assert!(proxied.contains(&("ALL_PROXY".to_owned(), None)));
        assert!(proxied.contains(&("NO_PROXY".to_owned(), None)));

        let direct = proxy_environment(json!({"mode": "offline", "endpoint": {}}));
        assert!(direct.iter().all(|(_, value)| value.is_none()));
        assert_eq!(direct.len(), PROXY_VARIABLES.len());

        let live = proxy_environment(json!({
            "mode": "live",
            "endpoint": {"proxy_url": "http://127.0.0.1:1234"},
        }));
        assert!(live.is_empty());
    }

    #[test]
    fn deadline_includes_an_adapter_that_never_reads_stdin() {
        let command = crate::test_support::adapter_command("never-read-stdin");

        // Exceed pipe capacity so writing stdin cannot finish without a reader.
        const INPUT_SIZE: usize = 1024 * 1024;
        let input = json!({"body": "x".repeat(INPUT_SIZE)});

        let started = Instant::now();
        let error =
            invoke_adapter_process(&command, &input, Duration::from_millis(100)).unwrap_err();
        assert!(error.to_string().contains("deadline"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
