//! Runs one adapter process per case and serves the case's loopback endpoint.
use crate::{Result, Session, model::*};
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

fn invoke_adapter_process(command: &[String], message: &Value, timeout: Duration) -> Result<Value> {
    let mut input_bytes = serde_json::to_vec(message)?;
    input_bytes.push(b'\n');
    if input_bytes.len() > MAX_MESSAGE_BYTES {
        return Err("adapter input exceeds 16 MiB".into());
    }
    let deadline = Instant::now() + timeout;
    let mut child = Command::new(&command[0])
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
        "call": case.call,
    });
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
        let result = invoke_adapter_process(command, &message, Duration::from_secs(30));
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
