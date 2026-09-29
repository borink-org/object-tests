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

// Accepts a proxy tunnel, which ureq opens even to an http target. Every target of the grader is
// plain HTTP, so the tunnel carries the request as it is, and the grader reads it from the same
// connection after answering CONNECT. The request inside names its host, which the case checks.
fn accept_proxy_tunnel(connection: &mut BufReader<impl Read + Write>) -> Result<()> {
    if !connection.fill_buf()?.starts_with(b"CONNECT ") {
        return Ok(());
    }
    let mut header_length = 0;
    loop {
        let line = read_http_line(connection, MAX_HEADER_BYTES - header_length)?;
        header_length += line.len();
        if line == b"\r\n" {
            break;
        }
    }
    let stream = connection.get_mut();
    stream.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")?;
    stream.flush()?;
    Ok(())
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
        // A header value is bytes. Each byte becomes the character of the same number, as
        // ISO-8859-1 reads it, so a case can tell `é` sent as `e9` from `é` sent as UTF-8.
        let value: String = header.value.iter().copied().map(char::from).collect();
        let value = value.as_str();
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
    // Like Azure, send no interim response when no content follows (RFC 9110, section 10.1.1).
    if expect_continue && (chunked || content_length > 0) {
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
            // An echoed request header goes back as the bytes it arrived as.
            let value: Vec<u8> = value.chars().map(|character| character as u8).collect();
            write!(writer, "{name}: ")?;
            writer.write_all(&value)?;
            write!(writer, "\r\n")?;
        }
    }
    write!(writer, "Connection: close\r\n")?;
    let has_body = response.status != 204 && response.status != 304;
    let chunked = has_body && !is_head && response.framing == Framing::Chunked;
    if chunked {
        write!(writer, "Transfer-Encoding: chunked\r\n\r\n")?;
        // Two chunks, so a client must join them.
        let (first_chunk, second_chunk) = body.split_at(body.len() / 2);
        for chunk in [first_chunk, second_chunk]
            .into_iter()
            .filter(|chunk| !chunk.is_empty())
        {
            write!(writer, "{:x}\r\n", chunk.len())?;
            writer.write_all(chunk)?;
            write!(writer, "\r\n")?;
        }
        write!(writer, "0\r\n\r\n")?;
    } else {
        if has_body {
            write!(writer, "Content-Length: {content_length}\r\n")?;
        }
        write!(writer, "\r\n")?;
        if !is_head && has_body {
            let sent_length = response.truncate_body_after.unwrap_or(body.len());
            writer.write_all(&body[..sent_length])?;
        }
    }
    writer.flush()?;
    Ok(())
}

pub(crate) fn serve_connection(stream: TcpStream, state: &Mutex<Session>) {
    let exchange = || -> Result<()> {
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let mut connection = BufReader::new(stream);
        // A client may open a connection and never use it, such as a pool connection. One that
        // closes or times out before its first byte sent no request, so it is no failure. How
        // many such connections a client opens depends on its timing.
        match connection.fill_buf() {
            Ok([]) => return Ok(()),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(());
            }
            _ => {}
        }
        accept_proxy_tunnel(&mut connection)?;
        let request = read_http_request(&mut connection)?;
        let now = SystemTime::now();
        let response = state.lock().unwrap().respond(&request, now);
        if let Some(response) = response {
            // One write per response. The Azure C++ SDK loses a chunked body whose head arrives
            // in a segment of its own, and a service sends both together.
            write_http_response(
                &mut std::io::BufWriter::new(connection.get_mut()),
                &request,
                response,
                now,
            )?;
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
        let request_bytes = [
            "PUT http://fixture.invalid/a/../caf%C3%A9?name=a+b HTTP/1.1\r\n\
             Host: fixture.invalid\r\n\
             X-Amz-Meta-Value: café\r\n\
             X-Amz-Meta-Latin: caf"
                .as_bytes(),
            b"\xe9\r\n",
            b"X-Test: first\r\nX-Test: second\r\n\
              Content-Length: 1\r\n\r\nx",
        ]
        .concat();
        let mut connection = make_test_connection(&request_bytes);
        let request = read_http_request(&mut connection).unwrap();
        assert_eq!(request["headers"]["x-amz-meta-value"], "cafÃ©");
        assert_eq!(request["headers"]["x-amz-meta-latin"], "café");
        assert_eq!(request["headers"]["x-test"], json!(["first", "second"]));
        assert_eq!(request["path"], "/a/../café");
        assert_eq!(request["query"]["name"], "a+b");
        assert_eq!(request["body_text"], "x");
    }

    #[test]
    fn a_request_through_a_proxy_tunnel_reaches_the_grader() {
        let mut connection = make_test_connection(
            b"CONNECT bucket.fixture.invalid:80 HTTP/1.1\r\nHost: bucket.fixture.invalid:80\r\n\r\n\
              GET /key HTTP/1.1\r\nHost: bucket.fixture.invalid\r\nContent-Length: 0\r\n\r\n",
        );
        accept_proxy_tunnel(&mut connection).unwrap();
        let request = read_http_request(&mut connection).unwrap();
        assert_eq!(request["method"], "GET");
        assert_eq!(request["path"], "/key");
        assert_eq!(request["headers"]["host"], "bucket.fixture.invalid");
        assert!(
            connection
                .get_ref()
                .response
                .starts_with(b"HTTP/1.1 200 Connection established\r\n\r\n")
        );
    }

    #[test]
    fn a_plain_request_needs_no_tunnel() {
        let mut connection = make_test_connection(
            b"GET /key HTTP/1.1\r\nHost: fixture\r\nContent-Length: 0\r\n\r\n",
        );
        accept_proxy_tunnel(&mut connection).unwrap();
        assert!(connection.get_ref().response.is_empty());
        assert_eq!(read_http_request(&mut connection).unwrap()["path"], "/key");
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

    fn written_response(framing: Framing, truncate_body_after: Option<usize>) -> String {
        let response = Response {
            status: 200,
            headers: Default::default(),
            body: Body::Utf8("hello".into()),
            framing,
            truncate_body_after,
        };
        let mut written = Vec::new();
        let request = json!({"method": "GET"});
        write_http_response(&mut written, &request, response, SystemTime::now()).unwrap();
        String::from_utf8(written).unwrap()
    }

    #[test]
    fn chunked_responses_split_the_body_and_truncated_ones_stop_short() {
        let chunked = written_response(Framing::Chunked, None);
        assert!(
            chunked.contains("Transfer-Encoding: chunked\r\n\r\n2\r\nhe\r\n3\r\nllo\r\n0\r\n\r\n")
        );
        assert!(!chunked.contains("Content-Length"));

        let truncated = written_response(Framing::ContentLength, Some(2));
        assert!(truncated.ends_with("Content-Length: 5\r\n\r\nhe"));
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
