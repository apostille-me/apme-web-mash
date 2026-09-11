use scintilla_route_lambdas::FunctionResponse;
use std::{
    collections::BTreeMap,
    fmt,
    io::{self, BufRead, Read, Write},
    os::unix::net::UnixStream,
};

const ADAPTER_VERSION: &str = "scintilla-rust/0.3.0";
const MAX_HEADER_LINES: usize = 128;
const MAX_HEADER_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub(super) struct ParsedRequest {
    pub(super) headers: BTreeMap<String, String>,
    pub(super) body: Vec<u8>,
}

#[derive(Debug)]
pub(super) enum ReadRequestError {
    Io(io::Error),
    BadRequest(&'static str),
    TooLarge,
}

impl fmt::Display for ReadRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::BadRequest(error) => write!(formatter, "{error}"),
            Self::TooLarge => write!(formatter, "event_too_large"),
        }
    }
}

impl From<io::Error> for ReadRequestError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub(super) fn read_request<R: BufRead>(
    reader: &mut R,
    max_body_bytes: usize,
) -> Result<Option<ParsedRequest>, ReadRequestError> {
    let Some(request_line) = read_http_line(reader)? else {
        return Ok(None);
    };
    let request_parts: Vec<&str> = request_line.split_whitespace().collect();
    if request_parts.len() != 3 || !request_parts[2].starts_with("HTTP/1.") {
        return Err(ReadRequestError::BadRequest("invalid_http_request_line"));
    }

    let mut headers = BTreeMap::new();
    let mut header_bytes = request_line.len();
    if header_bytes > MAX_HEADER_BYTES {
        return Err(ReadRequestError::BadRequest("headers_too_large"));
    }
    let mut headers_complete = false;
    for _ in 0..MAX_HEADER_LINES {
        let line = read_http_line(reader)?
            .ok_or(ReadRequestError::BadRequest("unexpected_eof_in_headers"))?;
        header_bytes = header_bytes.saturating_add(line.len());
        if header_bytes > MAX_HEADER_BYTES {
            return Err(ReadRequestError::BadRequest("headers_too_large"));
        }
        if line.is_empty() {
            headers_complete = true;
            break;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or(ReadRequestError::BadRequest("invalid_http_header"))?;
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(ReadRequestError::BadRequest("invalid_http_header_name"));
        }
        let value = value.trim();
        if value.chars().any(char::is_control) {
            return Err(ReadRequestError::BadRequest("invalid_http_header_value"));
        }
        headers
            .entry(name)
            .and_modify(|existing: &mut String| {
                existing.push(',');
                existing.push_str(value);
            })
            .or_insert_with(|| value.to_owned());
    }
    if !headers_complete {
        return Err(ReadRequestError::BadRequest("too_many_headers"));
    }

    let body = if headers
        .get("transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
    {
        read_chunked_body(reader, max_body_bytes)?
    } else if let Some(value) = headers.get("content-length") {
        let length = value
            .trim()
            .parse::<usize>()
            .map_err(|_| ReadRequestError::BadRequest("invalid_content_length"))?;
        if length > max_body_bytes {
            return Err(ReadRequestError::TooLarge);
        }
        let mut body = vec![0_u8; length];
        reader.read_exact(&mut body)?;
        body
    } else {
        Vec::new()
    };

    Ok(Some(ParsedRequest { headers, body }))
}

fn read_chunked_body<R: BufRead>(
    reader: &mut R,
    max_body_bytes: usize,
) -> Result<Vec<u8>, ReadRequestError> {
    let mut body = Vec::new();
    loop {
        let size_line = read_http_line(reader)?
            .ok_or(ReadRequestError::BadRequest("unexpected_eof_in_chunk_size"))?;
        let size_text = size_line.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| ReadRequestError::BadRequest("invalid_chunk_size"))?;
        if size == 0 {
            loop {
                let trailer = read_http_line(reader)?
                    .ok_or(ReadRequestError::BadRequest("unexpected_eof_in_trailers"))?;
                if trailer.is_empty() {
                    break;
                }
            }
            break;
        }
        if body.len().saturating_add(size) > max_body_bytes {
            return Err(ReadRequestError::TooLarge);
        }
        let start = body.len();
        body.resize(start + size, 0);
        reader.read_exact(&mut body[start..])?;
        let mut terminator = [0_u8; 2];
        reader.read_exact(&mut terminator)?;
        if terminator != *b"\r\n" {
            return Err(ReadRequestError::BadRequest("invalid_chunk_terminator"));
        }
    }
    Ok(body)
}

fn read_http_line<R: BufRead>(
    reader: &mut R,
) -> Result<Option<String>, ReadRequestError> {
    let mut line = String::new();
    let count = reader.read_line(&mut line)?;
    if count == 0 {
        return Ok(None);
    }
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
    Ok(Some(line))
}

pub(super) fn write_fn_response(
    stream: &mut UnixStream,
    outer_status: u16,
    response: FunctionResponse,
) -> io::Result<()> {
    let body = serde_json::to_vec(&response.body)
        .unwrap_or_else(|_| b"{\"error\":\"serialization_failed\"}".to_vec());
    let mut headers = response.headers;
    headers.remove("content-length");
    headers.remove("connection");
    headers
        .entry("content-type".to_owned())
        .or_insert_with(|| "application/json".to_owned());
    headers.insert("fn-http-status".to_owned(), response.status_code.to_string());
    headers.insert("fn-fdk-version".to_owned(), ADAPTER_VERSION.to_owned());
    write_http_response(stream, outer_status, headers, &body)
}

fn write_http_response(
    stream: &mut UnixStream,
    status: u16,
    headers: BTreeMap<String, String>,
    body: &[u8],
) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        502 => "Bad Gateway",
        _ => "Response",
    };
    write!(stream, "HTTP/1.1 {status} {reason}\r\n")?;
    for (name, value) in headers {
        if valid_header_name(&name) && valid_header_value(&value) {
            write!(stream, "{name}: {value}\r\n")?;
        }
    }
    write!(
        stream,
        "content-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

fn valid_header_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn valid_header_value(value: &str) -> bool {
    !value.chars().any(|character| character == '\r' || character == '\n')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parses_content_length_request() {
        let input = b"POST /call HTTP/1.1\r\nFn-Call-Id: call-1\r\nContent-Length: 19\r\n\r\n{\"document_id\":\"1\"}";
        let mut cursor = Cursor::new(input);
        let request = read_request(&mut cursor, 1024).unwrap().unwrap();
        assert_eq!(request.headers["fn-call-id"], "call-1");
        assert_eq!(request.body, br#"{"document_id":"1"}"#);
    }

    #[test]
    fn parses_chunked_request() {
        let input = b"POST /call HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n3\r\n:1}\r\n0\r\n\r\n";
        let mut cursor = Cursor::new(input);
        let request = read_request(&mut cursor, 1024).unwrap().unwrap();
        assert_eq!(request.body, br#"{"a":1}"#);
    }

    #[test]
    fn rejects_excessive_header_count() {
        let mut input = b"POST /call HTTP/1.1\r\n".to_vec();
        for index in 0..MAX_HEADER_LINES {
            input.extend_from_slice(format!("X-{index}: value\r\n").as_bytes());
        }
        input.extend_from_slice(b"\r\n");
        let mut cursor = Cursor::new(input);
        assert!(matches!(
            read_request(&mut cursor, 1024),
            Err(ReadRequestError::BadRequest("too_many_headers"))
        ));
    }
}
