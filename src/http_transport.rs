//! Bounded HTTP/1.1 exchanges on loopback. Connections close after each response.
use crate::{
    Result, Session,
    aws_chunked::AwsChunkedDecoder,
    generated::{FingerprintStream, GeneratedBody},
    model::*,
    request::normalize_request_with_fingerprint,
};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    sync::Mutex,
    time::{Duration, SystemTime},
};

const MAX_HEADER_BYTES: usize = 64 * 1024;

// A request body up to this size is kept for checks on its bytes. A larger one, which only a
// large case sends, is fingerprinted as it arrives and not kept.
const KEPT_BODY_BYTES: usize = 16 * 1024 * 1024;

// The largest request body the transport reads, beyond every service limit a case tests.
const MAX_BODY_BYTES: u64 = 64 * 1024 * 1024 * 1024;

// A request body arrives in reads of at most this many bytes.
const READ_BYTES: usize = 1 << 20;

/// A request body as it arrives: its fingerprint, its bytes while they are few enough to keep,
/// and, for an `aws-chunked` body, the payload it decodes to.
struct ArrivingBody {
    kept: Option<Vec<u8>>,
    fingerprint: FingerprintStream,
    aws_chunked: Option<AwsChunkedDecoder>,
}

impl ArrivingBody {
    fn new(aws_chunked: bool) -> Self {
        Self {
            kept: Some(Vec::new()),
            fingerprint: FingerprintStream::default(),
            aws_chunked: aws_chunked.then(AwsChunkedDecoder::default),
        }
    }

    fn absorb(&mut self, bytes: &[u8]) -> Result<()> {
        self.fingerprint.update(bytes);
        if let Some(decoder) = &mut self.aws_chunked {
            decoder.absorb(bytes);
        }
        if self.fingerprint.finish().length > MAX_BODY_BYTES {
            return Err("request body exceeds 64 GiB".into());
        }
        if let Some(kept) = &mut self.kept {
            if kept.len() + bytes.len() <= KEPT_BODY_BYTES {
                kept.extend_from_slice(bytes);
            } else {
                self.kept = None;
            }
        }
        Ok(())
    }

    /// Reads `length` bytes of the body from `reader`.
    fn read_from(&mut self, reader: &mut impl Read, length: u64) -> Result<()> {
        let mut buffer = vec![0; READ_BYTES.min(usize::try_from(length).unwrap_or(READ_BYTES))];
        let mut remaining = length;
        while remaining > 0 {
            let take = remaining.min(buffer.len() as u64) as usize;
            reader.read_exact(&mut buffer[..take])?;
            self.absorb(&buffer[..take])?;
            remaining -= take as u64;
        }
        Ok(())
    }
}

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

fn read_chunked_body(reader: &mut impl BufRead, body: &mut ArrivingBody) -> Result<()> {
    loop {
        let line = read_http_line(reader, MAX_HEADER_BYTES)?;
        let httparse::Status::Complete((_, length)) =
            httparse::parse_chunk_size(&line).map_err(|_| "invalid HTTP chunk size")?
        else {
            return Err("incomplete HTTP chunk size".into());
        };
        if length == 0 {
            if read_http_line(reader, MAX_HEADER_BYTES)? != b"\r\n" {
                return Err("HTTP trailers are not supported by this transport".into());
            }
            return Ok(());
        }
        body.read_from(reader, length)?;
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
    let mut aws_chunked = false;
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
                content_length = Some(value.parse::<u64>()?);
            }
            "transfer-encoding" => {
                if chunked || !value.eq_ignore_ascii_case("chunked") {
                    return Err("unsupported or duplicate Transfer-Encoding".into());
                }
                chunked = true;
            }
            // S3 decodes a body whose content encoding names aws-chunked, as `aws-chunked` or
            // `aws-chunked,gzip`.
            "content-encoding" => {
                aws_chunked |= value
                    .split(',')
                    .any(|coding| coding.trim().eq_ignore_ascii_case("aws-chunked"));
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
        return Err("request body exceeds 64 GiB".into());
    }
    // Like Azure, send no interim response when no content follows (RFC 9110, section 10.1.1).
    if expect_continue && (chunked || content_length > 0) {
        reader
            .get_mut()
            .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
        reader.get_mut().flush()?;
    }
    let mut body = ArrivingBody::new(aws_chunked);
    if chunked {
        read_chunked_body(reader, &mut body)?;
    } else {
        body.read_from(reader, content_length)?;
    }
    let mut normalized = normalize_request_with_fingerprint(
        request.method.ok_or("missing HTTP method")?,
        request.path.ok_or("missing HTTP target")?,
        &headers,
        body.kept.as_deref(),
        body.fingerprint.finish(),
    )?;
    if let Some(decoder) = &body.aws_chunked {
        decoder.insert_into(&mut normalized);
    }
    Ok(normalized)
}

/// Formats a time as ISO 8601 in UTC with whole seconds, as `2024-01-02T03:04:05Z`.
fn iso8601(time: SystemTime) -> String {
    let seconds = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64);
    let (days, second_of_day) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    // The civil date of a day count, after Howard Hinnant's days_from_civil inverse.
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        second_of_day / 3600,
        second_of_day / 60 % 60,
        second_of_day % 60
    )
}

/// Replaces the body time's text with the time it names, counted from `now`.
fn fill_body_time(body: Vec<u8>, time: Option<&BodyTime>, now: SystemTime) -> Vec<u8> {
    let Some(BodyTime {
        text,
        offset_seconds,
    }) = time
    else {
        return body;
    };
    let offset = Duration::from_secs(offset_seconds.unsigned_abs());
    let moment = if *offset_seconds < 0 {
        now - offset
    } else {
        now + offset
    };
    let replacement = iso8601(moment).into_bytes();
    let mut filled = Vec::with_capacity(body.len());
    let mut rest = &body[..];
    while let Some(at) = rest
        .windows(text.len())
        .position(|window| window == text.as_bytes())
    {
        filled.extend_from_slice(&rest[..at]);
        filled.extend_from_slice(&replacement);
        rest = &rest[at + text.len()..];
    }
    filled.extend_from_slice(rest);
    filled
}

/// Frames each write as one chunk of a chunked body.
struct ChunkedWriter<'a, W: Write>(&'a mut W);

impl<W: Write> Write for ChunkedWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if !bytes.is_empty() {
            write!(self.0, "{:x}\r\n", bytes.len())?;
            self.0.write_all(bytes)?;
            write!(self.0, "\r\n")?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// A response body to send: bytes the case holds, or a body generated as it is sent.
enum OutgoingBody {
    Held(Vec<u8>),
    Generated(GeneratedBody),
}

impl OutgoingBody {
    fn length(&self) -> u64 {
        match self {
            Self::Held(bytes) => bytes.len() as u64,
            Self::Generated(generated) => generated.length,
        }
    }

    fn write_window(&self, writer: &mut impl Write, start: u64, length: u64) -> Result<()> {
        match self {
            Self::Held(bytes) => {
                writer.write_all(&bytes[start as usize..(start + length) as usize])?
            }
            Self::Generated(generated) => generated.write_window_to(writer, start, length)?,
        }
        Ok(())
    }
}

/// The window a request asks for of a body of `total` bytes, from its `x-ms-range` or `Range`:
/// `Some(Ok((start, length)))`, `Some(Err(()))` for a range past the end, or `None` for the
/// whole body.
fn requested_window(request: &Value, total: u64) -> Option<std::result::Result<(u64, u64), ()>> {
    let range = ["x-ms-range", "range"]
        .iter()
        .find_map(|name| request["headers"][name].as_str())?;
    let (first, last) = range.strip_prefix("bytes=")?.split_once('-')?;
    let (start, end) = match (first.parse::<u64>().ok(), last.parse::<u64>().ok()) {
        (Some(start), Some(last)) => (start, last.min(total.saturating_sub(1))),
        (Some(start), None) if last.is_empty() => (start, total.saturating_sub(1)),
        (None, Some(suffix)) if first.is_empty() => {
            (total.saturating_sub(suffix), total.saturating_sub(1))
        }
        _ => return None,
    };
    if start >= total || end < start {
        return Some(Err(()));
    }
    Some(Ok((start, end - start + 1)))
}

fn write_http_response(
    writer: &mut impl Write,
    request: &Value,
    response: Response,
    now: SystemTime,
) -> Result<()> {
    let body = match &response.body {
        Body::Repeat(generated) => OutgoingBody::Generated(generated.clone()),
        held => OutgoingBody::Held(fill_body_time(
            held.bytes()?,
            response.body_time.as_ref(),
            now,
        )),
    };
    let is_head = request["method"] == "HEAD";
    let mut content_length = body.length();
    // A response that serves ranges answers a ranged read with that window of its body.
    let window = if response.serves_ranges && !is_head {
        requested_window(request, body.length())
    } else {
        None
    };
    let status = match window {
        Some(Ok(_)) => 206,
        Some(Err(())) => 416,
        None => response.status,
    };
    write!(writer, "HTTP/1.1 {status} Fixture\r\n")?;
    match window {
        Some(Ok((start, length))) => {
            write!(
                writer,
                "Content-Range: bytes {start}-{}/{}\r\n",
                start + length - 1,
                body.length()
            )?;
            content_length = length;
        }
        Some(Err(())) => {
            write!(writer, "Content-Range: bytes */{}\r\n", body.length())?;
            content_length = 0;
        }
        None => {}
    }
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
    let has_body = status != 204 && status != 304;
    let chunked = has_body && !is_head && window.is_none() && response.framing == Framing::Chunked;
    if chunked {
        write!(writer, "Transfer-Encoding: chunked\r\n\r\n")?;
        match &body {
            // Two chunks, so a client must join them.
            OutgoingBody::Held(bytes) => {
                let (first_chunk, second_chunk) = bytes.split_at(bytes.len() / 2);
                let mut chunks = ChunkedWriter(writer);
                chunks.write_all(first_chunk)?;
                chunks.write_all(second_chunk)?;
            }
            OutgoingBody::Generated(generated) => {
                generated.write_to(&mut ChunkedWriter(writer), None)?;
            }
        }
        write!(writer, "0\r\n\r\n")?;
    } else {
        if has_body {
            write!(writer, "Content-Length: {content_length}\r\n")?;
        }
        write!(writer, "\r\n")?;
        if let Some(Ok((start, length))) = window {
            body.write_window(writer, start, length)?;
        } else if !is_head && has_body && window.is_none() {
            let sent_length = response.truncate_body_after.map(|length| length as u64);
            match &body {
                OutgoingBody::Held(bytes) => {
                    let sent_length = sent_length.unwrap_or(bytes.len() as u64) as usize;
                    writer.write_all(&bytes[..sent_length])?;
                }
                OutgoingBody::Generated(generated) => generated.write_to(writer, sent_length)?,
            }
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

    #[test]
    fn a_body_time_is_written_relative_to_the_response() {
        assert_eq!(iso8601(SystemTime::UNIX_EPOCH), "1970-01-01T00:00:00Z");
        let leap_day = SystemTime::UNIX_EPOCH + Duration::from_secs(1_709_164_805);
        assert_eq!(iso8601(leap_day), "2024-02-29T00:00:05Z");

        let expiration = BodyTime {
            text: "EXPIRATION".into(),
            offset_seconds: 300,
        };
        let body = b"<Expiration>EXPIRATION</Expiration><Again>EXPIRATION</Again>".to_vec();
        assert_eq!(
            fill_body_time(body, Some(&expiration), leap_day),
            b"<Expiration>2024-02-29T00:05:05Z</Expiration><Again>2024-02-29T00:05:05Z</Again>"
        );
        let past = BodyTime {
            text: "T".into(),
            offset_seconds: -5,
        };
        assert_eq!(
            fill_body_time(b"T".to_vec(), Some(&past), leap_day),
            b"2024-02-29T00:00:00Z"
        );
        assert_eq!(fill_body_time(b"T".to_vec(), None, leap_day), b"T");
    }

    fn written_response(framing: Framing, truncate_body_after: Option<usize>) -> String {
        let response = Response {
            status: 200,
            headers: Default::default(),
            body: Body::Utf8("hello".into()),
            framing,
            truncate_body_after,
            serves_ranges: false,
            body_time: None,
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
            "Content-Length: 68719476737\r\n",
            "Transfer-Encoding: gzip, chunked\r\n",
        ] {
            let request = format!("PUT /object HTTP/1.1\r\n{headers}\r\n");
            assert!(read_http_request(&mut make_test_connection(request.as_bytes())).is_err());
        }
        let request = b"PUT /object HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n1000000001\r\n";
        assert!(read_http_request(&mut make_test_connection(request)).is_err());
    }

    #[test]
    fn a_ranged_read_gets_its_window() {
        let total = 10;
        let window = |range: &str| requested_window(&json!({"headers": {"range": range}}), total);
        assert_eq!(window("bytes=2-4"), Some(Ok((2, 3))));
        assert_eq!(window("bytes=8-"), Some(Ok((8, 2))));
        assert_eq!(window("bytes=-3"), Some(Ok((7, 3))));
        assert_eq!(window("bytes=5-99"), Some(Ok((5, 5))));
        assert_eq!(window("bytes=10-12"), Some(Err(())));
        assert_eq!(window("items=1-2"), None);
        let azure_first = json!({"headers": {"range": "bytes=0-1", "x-ms-range": "bytes=3-4"}});
        assert_eq!(requested_window(&azure_first, total), Some(Ok((3, 2))));

        let generated = GeneratedBody {
            pattern_base64: "MDEyMzQ1Njc4OWFiY2RlZmc=".into(),
            length: 3 * (1 << 20) + 5,
        };
        let whole = generated.bytes().unwrap();
        for (start, length) in [(0, 17), (5, 40), ((1 << 20) - 3, 9), (3 << 20, 5)] {
            let response = Response {
                status: 200,
                headers: Default::default(),
                body: Body::Repeat(generated.clone()),
                framing: Framing::Chunked,
                truncate_body_after: None,
                serves_ranges: true,
                body_time: None,
            };
            let request = json!({
                "method": "GET",
                "headers": {"range": format!("bytes={start}-{}", start + length - 1)},
            });
            let mut written = Vec::new();
            write_http_response(&mut written, &request, response, SystemTime::now()).unwrap();
            let split = written
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .unwrap();
            let head = String::from_utf8_lossy(&written[..split]);
            assert!(head.starts_with("HTTP/1.1 206"), "{head}");
            assert!(head.contains(&format!(
                "Content-Range: bytes {start}-{}/{}",
                start + length - 1,
                generated.length
            )));
            assert!(
                head.contains(&format!("Content-Length: {length}")),
                "{head}"
            );
            assert_eq!(
                &written[split + 4..],
                &whole[start as usize..(start + length) as usize]
            );
        }
    }

    #[test]
    fn a_body_over_16_mib_arrives_as_its_fingerprint_alone() {
        let generated = GeneratedBody {
            pattern_base64: "MDEyMzQ1Njc4OWFiY2RlZmc=".into(),
            length: KEPT_BODY_BYTES as u64 + 1,
        };
        for chunked in [false, true] {
            let mut request = if chunked {
                b"PUT /object HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec()
            } else {
                format!(
                    "PUT /object HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
                    generated.length
                )
                .into_bytes()
            };
            if chunked {
                generated
                    .write_to(&mut ChunkedWriter(&mut request), None)
                    .unwrap();
                request.extend_from_slice(b"0\r\n\r\n");
            } else {
                generated.write_to(&mut request, None).unwrap();
            }
            let request = read_http_request(&mut make_test_connection(&request)).unwrap();
            assert!(request.get("body_base64").is_none());
            assert!(crate::generated::check_generated_body(&request, &generated).is_ok());
        }

        let small = read_http_request(&mut make_test_connection(
            b"PUT /object HTTP/1.1\r\nContent-Length: 2\r\n\r\nhi",
        ))
        .unwrap();
        assert_eq!(small["body_text"], "hi");
        assert_eq!(small["body_length"], 2);
    }

    #[test]
    fn a_generated_response_is_written_as_its_bytes() {
        let generated = GeneratedBody {
            pattern_base64: "YWJj".into(),
            length: 3 * (1 << 20) + 2,
        };
        let expected = generated.bytes().unwrap();
        for framing in [Framing::ContentLength, Framing::Chunked] {
            let response = Response {
                status: 200,
                headers: Default::default(),
                body: Body::Repeat(generated.clone()),
                framing,
                truncate_body_after: None,
                serves_ranges: false,
                body_time: None,
            };
            let mut written = Vec::new();
            write_http_response(
                &mut written,
                &json!({"method": "GET"}),
                response,
                SystemTime::now(),
            )
            .unwrap();
            let split = written
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .unwrap();
            let (head, mut rest) = (&written[..split], &written[split + 4..]);
            let body = if framing == Framing::Chunked {
                let mut body = Vec::new();
                loop {
                    let line_end = rest
                        .windows(2)
                        .position(|window| window == b"\r\n")
                        .unwrap();
                    let length =
                        usize::from_str_radix(std::str::from_utf8(&rest[..line_end]).unwrap(), 16)
                            .unwrap();
                    if length == 0 {
                        break body;
                    }
                    body.extend_from_slice(&rest[line_end + 2..line_end + 2 + length]);
                    rest = &rest[line_end + 2 + length + 2..];
                }
            } else {
                assert!(String::from_utf8_lossy(head).contains("Content-Length: 3145730"));
                rest.to_vec()
            };
            assert!(body == expected, "{framing:?}");
        }
    }
}
