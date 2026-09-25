//! Bounded HTTP/1.1 exchanges on loopback. Connections close after each response.
use crate::{Result, Session, model::*, normalize_http_request};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    sync::Mutex,
    time::{Duration, SystemTime},
};

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

fn read_http_line(reader: &mut impl BufRead, limit: usize) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    reader
        .take((limit + 1) as u64)
        .read_until(b'\n', &mut line)?;
    if line.len() > limit || !line.ends_with(b"\r\n") {
        return Err("HTTP line is oversized, truncated, or missing CRLF".into());
    }
    Ok(line)
}

fn read_chunked_body(reader: &mut impl BufRead) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let line = read_http_line(reader, MAX_HEADER_BYTES)?;
        let httparse::Status::Complete((_, length)) =
            httparse::parse_chunk_size(&line).map_err(|_| "invalid HTTP chunk size")?
        else {
            return Err("incomplete HTTP chunk size".into());
        };
        let length = usize::try_from(length)?;
        if length == 0 {
            if read_http_line(reader, MAX_HEADER_BYTES)? != b"\r\n" {
                return Err("HTTP trailers are not supported by this transport".into());
            }
            return Ok(body);
        }
        let previous_length = body.len();
        let total_length = previous_length.checked_add(length).ok_or("body overflow")?;
        if total_length > MAX_BODY_BYTES {
            return Err("request exceeds 16 MiB".into());
        }
        body.resize(total_length, 0);
        reader.read_exact(&mut body[previous_length..])?;
        let mut terminator = [0; 2];
        reader.read_exact(&mut terminator)?;
        if terminator != *b"\r\n" {
            return Err("invalid HTTP chunk terminator".into());
        }
    }
}

fn read_http_request(reader: &mut BufReader<impl Read + Write>) -> Result<Value> {
    let mut header_bytes = Vec::new();
    loop {
        let line = read_http_line(reader, MAX_HEADER_BYTES - header_bytes.len())?;
        let finished = line == b"\r\n";
        header_bytes.extend(line);
        if finished {
            break;
        }
    }
    let mut header_storage = [httparse::EMPTY_HEADER; 256];
    let mut request = httparse::Request::new(&mut header_storage);
    if request.parse(&header_bytes)?.is_partial() {
        return Err("incomplete HTTP request".into());
    }
    let mut headers = Vec::new();
    let mut content_length = None;
    let mut chunked = false;
    let mut expect_continue = false;
    for header in request.headers.iter() {
        let value = std::str::from_utf8(header.value)?;
        match header.name.to_ascii_lowercase().as_str() {
            "content-length" => {
                if content_length.is_some() {
                    return Err("duplicate Content-Length".into());
                }
                content_length = Some(value.parse::<usize>()?);
            }
            "transfer-encoding" => {
                if chunked || !value.eq_ignore_ascii_case("chunked") {
                    return Err("unsupported or duplicate Transfer-Encoding".into());
                }
                chunked = true;
            }
            "expect" => {
                if !value.eq_ignore_ascii_case("100-continue") {
                    return Err("unsupported HTTP expectation".into());
                }
                expect_continue = true;
            }
            _ => {}
        }
        headers.push((header.name.to_owned(), value.to_owned()));
    }
    if chunked && content_length.is_some() {
        return Err("ambiguous HTTP body framing".into());
    }
    let content_length = content_length.unwrap_or(0);
    if content_length > MAX_BODY_BYTES {
        return Err("request exceeds 16 MiB".into());
    }
    if expect_continue {
        reader
            .get_mut()
            .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
        reader.get_mut().flush()?;
    }
    let body = if chunked {
        read_chunked_body(reader)?
    } else {
        let mut body = vec![0; content_length];
        reader.read_exact(&mut body)?;
        body
    };
    normalize_http_request(
        request.method.ok_or("missing HTTP method")?,
        request.path.ok_or("missing HTTP target")?,
        &headers,
        &body,
    )
}

fn write_http_response(
    writer: &mut impl Write,
    request: &Value,
    response: Response,
    now: SystemTime,
) -> Result<()> {
    let body = response.body.bytes()?;
    let is_head = request["method"] == "HEAD";
    let mut content_length = body.len();
    write!(writer, "HTTP/1.1 {} Fixture\r\n", response.status)?;
    for (name, header) in response.headers {
        let value = match header {
            Header::Literal(value) => Some(value),
            Header::Dynamic(DynamicHeader::Now) => Some(httpdate::fmt_http_date(now)),
            Header::Dynamic(DynamicHeader::Request { name }) => request["headers"]
                [name.to_ascii_lowercase()]
            .as_str()
            .map(str::to_owned),
        };
        if name.eq_ignore_ascii_case("content-length") {
            if is_head {
                content_length = value.ok_or("missing HEAD length")?.parse()?;
            }
        } else if !name.eq_ignore_ascii_case("connection")
            && let Some(value) = value
        {
            write!(writer, "{name}: {value}\r\n")?;
        }
    }
    write!(writer, "Connection: close\r\n")?;
    if response.status != 204 && response.status != 304 {
        write!(writer, "Content-Length: {content_length}\r\n")?;
    }
    write!(writer, "\r\n")?;
    if !is_head && response.status != 204 && response.status != 304 {
        writer.write_all(&body)?;
    }
    writer.flush()?;
    Ok(())
}

pub(crate) fn serve_connection(stream: TcpStream, state: &Mutex<Session>) {
    let exchange = || -> Result<()> {
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let mut connection = BufReader::new(stream);
        let request = read_http_request(&mut connection)?;
        let now = SystemTime::now();
        let response = state.lock().unwrap().respond(&request, now);
        if let Some(response) = response {
            write_http_response(connection.get_mut(), &request, response, now)?;
        } else {
            connection.get_mut().write_all(
                b"HTTP/1.1 400 Fixture\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            )?;
        }
        Ok(())
    };
    if let Err(error) = exchange() {
        state.lock().unwrap().failures.push(json!({
            "error": format!("HTTP transport: {error}"),
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct TestConnection {
        request: Cursor<Vec<u8>>,
        response: Vec<u8>,
    }

    impl Read for TestConnection {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.request.read(buffer)
        }
    }

    impl Write for TestConnection {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.response.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn make_test_connection(request: &[u8]) -> BufReader<TestConnection> {
        BufReader::new(TestConnection {
            request: Cursor::new(request.to_vec()),
            response: Vec::new(),
        })
    }

    #[test]
    fn metadata_and_absolute_targets_reach_the_grader_unchanged() {
        let mut connection = make_test_connection(
            "PUT http://fixture.invalid/a/../caf%C3%A9?name=a+b HTTP/1.1\r\n\
             Host: fixture.invalid\r\n\
             X-Amz-Meta-Value: café\r\n\
             X-Test: first\r\nX-Test: second\r\n\
             Content-Length: 1\r\n\r\nx"
                .as_bytes(),
        );
        let request = read_http_request(&mut connection).unwrap();
        assert_eq!(request["headers"]["x-amz-meta-value"], "café");
        assert_eq!(request["headers"]["x-test"], json!(["first", "second"]));
        assert_eq!(request["path"], "/a/../café");
        assert_eq!(request["query"]["name"], "a+b");
        assert_eq!(request["body_text"], "x");
    }

    #[test]
    fn continue_and_chunked_uploads_preserve_binary_bytes() {
        let mut connection = make_test_connection(
            b"PUT /object HTTP/1.1\r\nHost: fixture\r\n\
              Transfer-Encoding: chunked\r\nExpect: 100-continue\r\n\r\n\
              2\r\n\x00\xff\r\n1;extension=value\r\nx\r\n0\r\n\r\n",
        );
        let request = read_http_request(&mut connection).unwrap();
        assert_eq!(request["body_base64"], "AP94");
        assert_eq!(
            connection.get_ref().response,
            b"HTTP/1.1 100 Continue\r\n\r\n"
        );
    }

    #[test]
    fn ambiguous_and_oversized_bodies_fail_before_allocation() {
        for headers in [
            "Content-Length: 1\r\nContent-Length: 1\r\n",
            "Content-Length: 1\r\nTransfer-Encoding: chunked\r\n",
            "Content-Length: 16777217\r\n",
            "Transfer-Encoding: gzip, chunked\r\n",
        ] {
            let request = format!("PUT /object HTTP/1.1\r\n{headers}\r\n");
            assert!(read_http_request(&mut make_test_connection(request.as_bytes())).is_err());
        }
        let request = b"PUT /object HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n1000001\r\n";
        assert!(read_http_request(&mut make_test_connection(request)).is_err());
    }
}
