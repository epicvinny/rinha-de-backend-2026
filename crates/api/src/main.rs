use axum::{
    extract::State,
    http::header::CONTENT_TYPE,
    http::StatusCode,
    response::Response,
    routing::{get, post},
    Router,
};
use bytes::Bytes;
use memmap2::MmapOptions;
#[cfg(unix)]
use std::fs;
use std::fs::File;
use std::io;
#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
#[cfg(unix)]
use std::thread;
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod classifier;
mod perf;
mod search;
mod tree_model;
use search::Index;

#[derive(Clone)]
struct AppState {
    index: Option<Arc<Index>>,
    constants: Arc<shared::Constants>,
    ready: Arc<AtomicBool>,
    perf: Option<Arc<perf::PerfCollector>>,
    log_search_avg: bool,
    parser: ApiParser,
    classifier: ApiClassifier,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ApiParser {
    Serde,
    Fast,
}

impl ApiParser {
    fn from_env() -> Self {
        match std::env::var("API_PARSER")
            .unwrap_or_else(|_| "serde".to_string())
            .to_ascii_lowercase()
            .as_str()
        {
            "fast" => ApiParser::Fast,
            _ => ApiParser::Serde,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ApiClassifier {
    Off,
    Tree,
    TreeOnly,
}

impl ApiClassifier {
    fn from_env() -> Self {
        match std::env::var("API_CLASSIFIER")
            .unwrap_or_else(|_| "off".to_string())
            .to_ascii_lowercase()
            .as_str()
        {
            "tree" => ApiClassifier::Tree,
            "tree_only" | "tree-only" => ApiClassifier::TreeOnly,
            _ => ApiClassifier::Off,
        }
    }
}

static REQ_COUNT: AtomicU64 = AtomicU64::new(0);
static TOTAL_US: AtomicU64 = AtomicU64::new(0);

const RAW_BAD_REQUEST: u8 = 255;
const RAW_MAX_BODY_BYTES: usize = 8192;
#[cfg(unix)]
const HANDOFF_MAX_HEADER_BYTES: usize = 4096;
#[cfg(unix)]
const HANDOFF_BUFFER_BYTES: usize = HANDOFF_MAX_HEADER_BYTES + RAW_MAX_BODY_BYTES + 2048;
#[cfg(unix)]
const HANDOFF_CONTROL_STACK_BYTES: usize = 64 * 1024;
#[cfg(unix)]
const HANDOFF_CLIENT_STACK_BYTES: usize = 256 * 1024;

#[cfg(unix)]
const HTTP_BAD_REQUEST: &[u8] =
    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
#[cfg(unix)]
const HTTP_NOT_FOUND: &[u8] =
    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
#[cfg(unix)]
const HTTP_METHOD_NOT_ALLOWED: &[u8] =
    b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
#[cfg(unix)]
const HTTP_PAYLOAD_TOO_LARGE: &[u8] =
    b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
#[cfg(unix)]
const HTTP_READY_OK: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n";

#[cfg(unix)]
const HTTP_SCORE_0: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 33\r\n\r\n{\"approved\":true,\"fraud_score\":0}";
#[cfg(unix)]
const HTTP_SCORE_1: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 35\r\n\r\n{\"approved\":true,\"fraud_score\":0.2}";
#[cfg(unix)]
const HTTP_SCORE_2: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 35\r\n\r\n{\"approved\":true,\"fraud_score\":0.4}";
#[cfg(unix)]
const HTTP_SCORE_3: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 36\r\n\r\n{\"approved\":false,\"fraud_score\":0.6}";
#[cfg(unix)]
const HTTP_SCORE_4: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 36\r\n\r\n{\"approved\":false,\"fraud_score\":0.8}";
#[cfg(unix)]
const HTTP_SCORE_5: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 34\r\n\r\n{\"approved\":false,\"fraud_score\":1}";

async fn health_check(State(state): State<AppState>) -> StatusCode {
    if state.ready.load(Ordering::Relaxed) {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

async fn fraud_score(
    State(state): State<AppState>,
    body_bytes: Bytes,
) -> Result<Response, StatusCode> {
    let handler_start = if state.perf.is_some() || state.log_search_avg {
        Some(Instant::now())
    } else {
        None
    };
    let bucket = score_body(&state, &body_bytes, handler_start, true)?;
    response_for_bucket(bucket)
}

fn response_for_bucket(bucket: u8) -> Result<Response, StatusCode> {
    let response_body = response_body_for_bucket(bucket);

    Ok(Response::builder()
        .header(CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(response_body))
        .unwrap())
}

fn response_body_for_bucket(bucket: u8) -> &'static str {
    match bucket {
        0 => r#"{"approved":true,"fraud_score":0}"#,
        1 => r#"{"approved":true,"fraud_score":0.2}"#,
        2 => r#"{"approved":true,"fraud_score":0.4}"#,
        3 => r#"{"approved":false,"fraud_score":0.6}"#,
        4 => r#"{"approved":false,"fraud_score":0.8}"#,
        5 => r#"{"approved":false,"fraud_score":1}"#,
        _ => r#"{"approved":false,"fraud_score":1}"#,
    }
}

fn score_bucket_for_fraud_score(fraud_score_val: f64) -> u8 {
    (fraud_score_val * 5.0 + 0.1) as u8
}

fn score_body(
    state: &AppState,
    body_bytes: &[u8],
    handler_start: Option<Instant>,
    build_response: bool,
) -> Result<u8, StatusCode> {
    if state.perf.is_none() && !state.log_search_avg && state.classifier != ApiClassifier::Off {
        if let Some(approved) = classifier::classify_approved(body_bytes) {
            return Ok(if approved { 0 } else { 5 });
        }
        if state.index.is_none() {
            return Err(StatusCode::BAD_REQUEST);
        }
    }

    if let Some(perf) = state.perf.as_deref() {
        let index = exact_index(state)?;
        let handler_start = handler_start.unwrap_or_else(Instant::now);
        let request = perf.begin_request();

        let parse_start = Instant::now();
        let fast_parsed = if state.parser == ApiParser::Fast {
            shared::parse_payload_to_i16_and_key(body_bytes, &state.constants)
        } else {
            None
        };
        let payload;
        let parsed_payload = if fast_parsed.is_none() {
            payload = serde_json::from_slice::<shared::types::Payload<'_>>(body_bytes)
                .map_err(|_e| StatusCode::BAD_REQUEST)?;
            Some(&payload)
        } else {
            None
        };
        let json_parse_us = perf::elapsed_us(parse_start);

        let mut search_trace = perf::SearchTrace::default();
        let search_start = Instant::now();
        let (_approved, fraud_score_val) = if let Some((qv16, query_key)) = fast_parsed {
            index.search_vector_with_trace(&qv16, query_key, &mut search_trace)
        } else {
            index.search_with_trace(parsed_payload.unwrap(), &mut search_trace)
        };
        let search_total_us = perf::elapsed_us(search_start);

        let response_start = Instant::now();
        let bucket = score_bucket_for_fraud_score(fraud_score_val);
        if build_response {
            std::hint::black_box(response_body_for_bucket(bucket));
        }
        let response_build_us = perf::elapsed_us(response_start);

        let handler_total_us = perf::elapsed_us(handler_start);
        perf.record_request(perf::RequestPerf {
            request_id: request.request_id(),
            in_flight_at_start: request.in_flight_at_start(),
            max_in_flight_seen: perf.max_in_flight_seen(),
            body_len: body_bytes.len(),
            score_bucket: bucket,
            handler_total_us,
            json_parse_us,
            search_total_us,
            response_build_us,
            search: search_trace,
        });

        return Ok(bucket);
    }

    let index = exact_index(state)?;
    let (_approved, fraud_score_val) = if state.parser == ApiParser::Fast {
        if let Some((qv16, query_key)) =
            shared::parse_payload_to_i16_and_key(body_bytes, &state.constants)
        {
            index.search_vector(&qv16, query_key)
        } else {
            let payload: shared::types::Payload<'_> =
                serde_json::from_slice(body_bytes).map_err(|_e| StatusCode::BAD_REQUEST)?;
            index.search(&payload)
        }
    } else {
        let payload: shared::types::Payload<'_> =
            serde_json::from_slice(body_bytes).map_err(|_e| StatusCode::BAD_REQUEST)?;
        index.search(&payload)
    };

    if state.log_search_avg {
        let elapsed = handler_start
            .map(|start| start.elapsed().as_micros() as u64)
            .unwrap_or_default();
        let count = REQ_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        let total = TOTAL_US.fetch_add(elapsed, Ordering::Relaxed) + elapsed;

        if count % 10000 == 0 {
            eprintln!(
                "Avg handler time over {} requests: {:.3} ms",
                count,
                (total as f64) / (count as f64) / 1000.0
            );
        }
    }

    Ok(score_bucket_for_fraud_score(fraud_score_val))
}

fn score_body_fast_bucket(state: &AppState, body_bytes: &[u8]) -> Result<u8, StatusCode> {
    if state.classifier == ApiClassifier::Tree || state.classifier == ApiClassifier::TreeOnly {
        if let Some(approved) = classifier::classify_approved(body_bytes) {
            return Ok(if approved { 0 } else { 5 });
        }
        if state.index.is_none() {
            return Err(StatusCode::BAD_REQUEST);
        }
    }

    let index = exact_index(state)?;
    if let Some((qv16, query_key)) =
        shared::parse_payload_to_i16_and_key(body_bytes, &state.constants)
    {
        return Ok(index.search_bucket_vector(&qv16, query_key));
    }

    let payload: shared::types::Payload<'_> =
        serde_json::from_slice(body_bytes).map_err(|_e| StatusCode::BAD_REQUEST)?;
    let (_approved, fraud_score_val) = index.search(&payload);
    Ok(score_bucket_for_fraud_score(fraud_score_val))
}

#[inline]
fn exact_index(state: &AppState) -> Result<&Index, StatusCode> {
    state.index.as_deref().ok_or(StatusCode::BAD_REQUEST)
}

async fn run_raw_server(listen: String, state: AppState) -> io::Result<()> {
    let listener = TcpListener::bind(&listen).await?;
    eprintln!("Raw fraud-score server listening on {}", listen);

    loop {
        let (stream, _) = listener.accept().await?;
        tune_socket(&stream);
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_raw_connection(stream, state).await {
                if err.kind() != io::ErrorKind::UnexpectedEof {
                    eprintln!("raw api client error: {}", err);
                }
            }
        });
    }
}

async fn handle_raw_connection(mut stream: TcpStream, state: AppState) -> io::Result<()> {
    let mut len_buf = [0u8; 2];
    let mut body = [0u8; RAW_MAX_BODY_BYTES];

    loop {
        match stream.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(err) => return Err(err),
        }

        let body_len = u16::from_be_bytes(len_buf) as usize;
        if body_len == 0 || body_len > RAW_MAX_BODY_BYTES {
            stream.write_all(&[RAW_BAD_REQUEST]).await?;
            return Ok(());
        }

        let body_slice = &mut body[..body_len];
        stream.read_exact(body_slice).await?;

        let handler_start = if state.perf.is_some() || state.log_search_avg {
            Some(Instant::now())
        } else {
            None
        };
        let bucket_result =
            if state.perf.is_none() && !state.log_search_avg && state.parser == ApiParser::Fast {
                score_body_fast_bucket(&state, body_slice)
            } else {
                score_body(&state, body_slice, handler_start, false)
            };
        let bucket = match bucket_result {
            Ok(bucket) => bucket,
            Err(_) => RAW_BAD_REQUEST,
        };
        stream.write_all(&[bucket]).await?;
    }
}

#[cfg(unix)]
fn run_fd_handoff_server(listen: String, state: AppState) -> io::Result<()> {
    let path = listen.strip_prefix("unix:").unwrap_or(&listen).to_string();
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    eprintln!("FD handoff server listening on {}", path);

    for control in listener.incoming() {
        let control = control?;
        let state = state.clone();
        thread::Builder::new()
            .name("fd-control".to_string())
            .stack_size(HANDOFF_CONTROL_STACK_BYTES)
            .spawn(move || {
                if let Err(err) = handle_fd_control_connection(control, state) {
                    if err.kind() != io::ErrorKind::UnexpectedEof {
                        eprintln!("fd control error: {}", err);
                    }
                }
            })?;
    }

    Ok(())
}

#[cfg(unix)]
fn run_minimal_ready_server(port: u16, ready: Arc<AtomicBool>) -> io::Result<()> {
    let listener = std::net::TcpListener::bind(format!("0.0.0.0:{}", port))?;
    eprintln!("Minimal ready server listening on port {}", port);
    let mut buf = [0u8; 512];

    for stream in listener.incoming() {
        let mut stream = stream?;
        let read = match stream.read(&mut buf) {
            Ok(read) => read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        let request = &buf[..read];
        let response: &[u8] = if request.starts_with(b"GET /ready ")
            || request.starts_with(b"HEAD /ready ")
        {
            if ready.load(Ordering::Relaxed) {
                HTTP_READY_OK
            } else {
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            }
        } else if request.starts_with(b"GET ") || request.starts_with(b"HEAD ") {
            HTTP_NOT_FOUND
        } else {
            HTTP_METHOD_NOT_ALLOWED
        };
        let _ = stream.write_all(response);
    }

    Ok(())
}

#[cfg(unix)]
fn handle_fd_control_connection(control: UnixStream, state: AppState) -> io::Result<()> {
    loop {
        let Some(fd) = recv_fd_blocking(&control)? else {
            return Ok(());
        };
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        let state = state.clone();
        thread::Builder::new()
            .name("fd-client".to_string())
            .stack_size(HANDOFF_CLIENT_STACK_BYTES)
            .spawn(move || {
                if let Err(err) = handle_handoff_http_connection(owned, state) {
                    if err.kind() != io::ErrorKind::UnexpectedEof
                        && err.kind() != io::ErrorKind::ConnectionReset
                        && err.kind() != io::ErrorKind::BrokenPipe
                    {
                        eprintln!("fd client error: {}", err);
                    }
                }
            })?;
    }
}

#[cfg(unix)]
fn recv_fd_blocking(control: &UnixStream) -> io::Result<Option<RawFd>> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: byte.len(),
    };
    let mut control_buf = [0u8; 64];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control_buf.as_mut_ptr().cast();
    msg.msg_controllen = control_buf.len();

    let received = unsafe { libc::recvmsg(control.as_raw_fd(), &mut msg, 0) };
    if received == 0 {
        return Ok(None);
    }
    if received < 0 {
        return Err(io::Error::last_os_error());
    }

    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null()
            || (*cmsg).cmsg_level != libc::SOL_SOCKET
            || (*cmsg).cmsg_type != libc::SCM_RIGHTS
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "control message did not contain fd",
            ));
        }

        let mut fd: RawFd = -1;
        std::ptr::copy_nonoverlapping(
            libc::CMSG_DATA(cmsg).cast::<u8>(),
            &mut fd as *mut RawFd as *mut u8,
            std::mem::size_of::<RawFd>(),
        );
        if fd < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "received invalid fd",
            ));
        }
        Ok(Some(fd))
    }
}

#[cfg(unix)]
fn handle_handoff_http_connection(fd: OwnedFd, state: AppState) -> io::Result<()> {
    let mut stream = std::net::TcpStream::from(fd);
    tune_std_socket(&stream);
    stream.set_nonblocking(false)?;

    let mut buf = [0u8; HANDOFF_BUFFER_BYTES];
    let mut len = 0usize;

    loop {
        let request = loop {
            match parse_handoff_request(&buf[..len]) {
                Ok(Some(request)) => break request,
                Ok(None) => {
                    if len == buf.len() {
                        stream.write_all(HTTP_PAYLOAD_TOO_LARGE)?;
                        return Ok(());
                    }
                    match stream.read(&mut buf[len..]) {
                        Ok(0) => return Ok(()),
                        Ok(read) => {
                            len += read;
                        }
                        Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                        Err(err) => return Err(err),
                    }
                }
                Err(err) => {
                    stream.write_all(err.response)?;
                    return Ok(());
                }
            }
        };

        match request.route {
            HandoffRoute::Ready => {
                stream.write_all(HTTP_READY_OK)?;
            }
            HandoffRoute::FraudScore => {
                let body = &buf[request.header_len..request.total_len];
                let handler_start = if state.perf.is_some() || state.log_search_avg {
                    Some(Instant::now())
                } else {
                    None
                };
                let bucket_result = if state.perf.is_none()
                    && !state.log_search_avg
                    && state.parser == ApiParser::Fast
                {
                    score_body_fast_bucket(&state, body)
                } else {
                    score_body(&state, body, handler_start, false)
                };
                let response = match bucket_result {
                    Ok(bucket) => handoff_response_for_bucket(bucket),
                    Err(_) => HTTP_BAD_REQUEST,
                };
                stream.write_all(response)?;
            }
        }

        if request.total_len == len {
            len = 0;
        } else {
            buf.copy_within(request.total_len..len, 0);
            len -= request.total_len;
        }

        if request.close_after_response {
            return Ok(());
        }
    }
}

#[cfg(unix)]
fn handoff_response_for_bucket(bucket: u8) -> &'static [u8] {
    match bucket {
        0 => HTTP_SCORE_0,
        1 => HTTP_SCORE_1,
        2 => HTTP_SCORE_2,
        3 => HTTP_SCORE_3,
        4 => HTTP_SCORE_4,
        5 => HTTP_SCORE_5,
        _ => HTTP_SCORE_5,
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HandoffRoute {
    Ready,
    FraudScore,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HandoffRequest {
    route: HandoffRoute,
    header_len: usize,
    total_len: usize,
    close_after_response: bool,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HandoffParseError {
    response: &'static [u8],
}

#[cfg(unix)]
fn parse_handoff_request(buf: &[u8]) -> Result<Option<HandoffRequest>, HandoffParseError> {
    let Some(header_end) = find_header_end(buf) else {
        if buf.len() > HANDOFF_MAX_HEADER_BYTES {
            return Err(HandoffParseError {
                response: HTTP_PAYLOAD_TOO_LARGE,
            });
        }
        return Ok(None);
    };

    let header_len = header_end + 4;
    if header_len > HANDOFF_MAX_HEADER_BYTES {
        return Err(HandoffParseError {
            response: HTTP_PAYLOAD_TOO_LARGE,
        });
    }
    let head = parse_handoff_head(&buf[..header_len])?;
    let total_len = header_len + head.content_length;
    if total_len > HANDOFF_MAX_HEADER_BYTES + RAW_MAX_BODY_BYTES {
        return Err(HandoffParseError {
            response: HTTP_PAYLOAD_TOO_LARGE,
        });
    }
    if buf.len() < total_len {
        return Ok(None);
    }

    Ok(Some(HandoffRequest {
        route: head.route,
        header_len,
        total_len,
        close_after_response: head.close_after_response,
    }))
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HandoffHead {
    route: HandoffRoute,
    content_length: usize,
    close_after_response: bool,
}

#[cfg(unix)]
fn parse_handoff_head(header: &[u8]) -> Result<HandoffHead, HandoffParseError> {
    const POST_FRAUD_11: &[u8] = b"POST /fraud-score HTTP/1.1\r\n";
    const POST_FRAUD_10: &[u8] = b"POST /fraud-score HTTP/1.0\r\n";
    const GET_READY_11: &[u8] = b"GET /ready HTTP/1.1\r\n";
    const GET_READY_10: &[u8] = b"GET /ready HTTP/1.0\r\n";

    let (route, http10) = if header.starts_with(POST_FRAUD_11) {
        (HandoffRoute::FraudScore, false)
    } else if header.starts_with(POST_FRAUD_10) {
        (HandoffRoute::FraudScore, true)
    } else if header.starts_with(GET_READY_11) {
        (HandoffRoute::Ready, false)
    } else if header.starts_with(GET_READY_10) {
        (HandoffRoute::Ready, true)
    } else {
        return parse_handoff_head_generic(header);
    };

    if find_subslice(header, b"\r\nTransfer-Encoding:")
        .or_else(|| find_subslice(header, b"\r\ntransfer-encoding:"))
        .is_some()
    {
        return Err(HandoffParseError {
            response: HTTP_BAD_REQUEST,
        });
    }

    let content_length = if route == HandoffRoute::FraudScore {
        let Some(value_start) = find_content_length_value(header) else {
            return Err(HandoffParseError {
                response: HTTP_BAD_REQUEST,
            });
        };
        parse_usize_decimal_header(header, value_start)?
    } else {
        0
    };

    if route == HandoffRoute::FraudScore && content_length == 0 {
        return Err(HandoffParseError {
            response: HTTP_BAD_REQUEST,
        });
    }
    if content_length > RAW_MAX_BODY_BYTES {
        return Err(HandoffParseError {
            response: HTTP_PAYLOAD_TOO_LARGE,
        });
    }

    Ok(HandoffHead {
        route,
        content_length,
        close_after_response: http10 || contains_connection_close_fast(header),
    })
}

#[cfg(unix)]
fn parse_handoff_head_generic(header: &[u8]) -> Result<HandoffHead, HandoffParseError> {
    let header_str = std::str::from_utf8(header).map_err(|_| HandoffParseError {
        response: HTTP_BAD_REQUEST,
    })?;
    let mut lines = header_str.split("\r\n");
    let request_line = lines.next().ok_or(HandoffParseError {
        response: HTTP_BAD_REQUEST,
    })?;

    let mut parts = request_line.split_ascii_whitespace();
    let method = parts.next().ok_or(HandoffParseError {
        response: HTTP_BAD_REQUEST,
    })?;
    let path = parts.next().ok_or(HandoffParseError {
        response: HTTP_BAD_REQUEST,
    })?;
    let version = parts.next().ok_or(HandoffParseError {
        response: HTTP_BAD_REQUEST,
    })?;
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(HandoffParseError {
            response: HTTP_BAD_REQUEST,
        });
    }

    let route = match (method, path) {
        ("GET", "/ready") => HandoffRoute::Ready,
        ("POST", "/fraud-score") => HandoffRoute::FraudScore,
        ("GET" | "POST", _) => {
            return Err(HandoffParseError {
                response: HTTP_NOT_FOUND,
            })
        }
        _ => {
            return Err(HandoffParseError {
                response: HTTP_METHOD_NOT_ALLOWED,
            })
        }
    };

    let mut content_length = None;
    let mut close_after_response = version == "HTTP/1.0";
    let mut chunked = false;

    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(HandoffParseError {
                response: HTTP_BAD_REQUEST,
            });
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = Some(value.parse::<usize>().map_err(|_| HandoffParseError {
                response: HTTP_BAD_REQUEST,
            })?);
        } else if name.eq_ignore_ascii_case("connection") {
            if value.eq_ignore_ascii_case("close") {
                close_after_response = true;
            }
        } else if name.eq_ignore_ascii_case("transfer-encoding")
            && value.to_ascii_lowercase().contains("chunked")
        {
            chunked = true;
        }
    }

    if chunked {
        return Err(HandoffParseError {
            response: HTTP_BAD_REQUEST,
        });
    }

    let content_length = content_length.unwrap_or(0);
    if route == HandoffRoute::FraudScore && content_length == 0 {
        return Err(HandoffParseError {
            response: HTTP_BAD_REQUEST,
        });
    }
    if content_length > RAW_MAX_BODY_BYTES {
        return Err(HandoffParseError {
            response: HTTP_PAYLOAD_TOO_LARGE,
        });
    }

    Ok(HandoffHead {
        route,
        content_length,
        close_after_response,
    })
}

#[cfg(unix)]
fn find_content_length_value(header: &[u8]) -> Option<usize> {
    find_subslice(header, b"\r\nContent-Length:")
        .or_else(|| find_subslice(header, b"\r\ncontent-length:"))
        .map(|idx| {
            let mut pos = idx + b"\r\nContent-Length:".len();
            while header.get(pos) == Some(&b' ') || header.get(pos) == Some(&b'\t') {
                pos += 1;
            }
            pos
        })
}

#[cfg(unix)]
fn parse_usize_decimal_header(header: &[u8], mut pos: usize) -> Result<usize, HandoffParseError> {
    let mut value = 0usize;
    let mut found = false;
    while let Some(&byte) = header.get(pos) {
        match byte {
            b'0'..=b'9' => {
                found = true;
                value = value
                    .checked_mul(10)
                    .and_then(|v| v.checked_add((byte - b'0') as usize))
                    .ok_or(HandoffParseError {
                        response: HTTP_BAD_REQUEST,
                    })?;
            }
            b'\r' | b'\n' => break,
            b' ' | b'\t' if !found => {}
            _ => {
                return Err(HandoffParseError {
                    response: HTTP_BAD_REQUEST,
                })
            }
        }
        pos += 1;
    }
    if found {
        Ok(value)
    } else {
        Err(HandoffParseError {
            response: HTTP_BAD_REQUEST,
        })
    }
}

#[cfg(unix)]
fn contains_connection_close_fast(header: &[u8]) -> bool {
    find_subslice(header, b"\r\nConnection: close")
        .or_else(|| find_subslice(header, b"\r\nconnection: close"))
        .is_some()
}

#[cfg(unix)]
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(unix)]
fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

#[cfg(unix)]
fn tune_std_socket(stream: &std::net::TcpStream) {
    let _ = stream.set_nodelay(true);
    let fd = stream.as_raw_fd();
    unsafe {
        let one: libc::c_int = 1;
        let _ = libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_QUICKACK,
            &one as *const libc::c_int as *const libc::c_void,
            std::mem::size_of_val(&one) as libc::socklen_t,
        );
    }
}

fn tune_socket(stream: &TcpStream) {
    let _ = stream.set_nodelay(true);

    #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
    {
        let _ = stream.set_quickack(true);
    }
}

fn main() {
    #[cfg(target_feature = "avx2")]
    eprintln!("API Compiled with AVX2 enabled.");
    #[cfg(not(target_feature = "avx2"))]
    eprintln!("WARNING: API Compiled WITHOUT AVX2 enabled!");

    let index_path = std::env::var("INDEX_PATH").unwrap_or_else(|_| "/opt/index.bin".to_string());
    let port: u16 = std::env::var("PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse()
        .unwrap_or(8080);
    let raw_listen = std::env::var("API_RAW_LISTEN").ok();
    #[cfg(unix)]
    let fd_listen = std::env::var("API_FD_LISTEN").ok();
    let perf = perf::PerfConfig::from_env().map(|config| {
        eprintln!(
            "PERF_TRACE enabled: every={} slow_us={} sample={}",
            config.every, config.slow_us, config.sample
        );
        Arc::new(perf::PerfCollector::new(config))
    });
    let log_search_avg = std::env::var("LOG_SEARCH_AVG").ok().as_deref() == Some("1");
    let parser = ApiParser::from_env();
    eprintln!("API parser: {:?}", parser);
    let classifier = ApiClassifier::from_env();
    eprintln!("API classifier: {:?}", classifier);

    #[cfg(unix)]
    if classifier == ApiClassifier::TreeOnly
        && raw_listen.is_none()
        && fd_listen.is_some()
        && perf.is_none()
        && !log_search_avg
    {
        let ready = Arc::new(AtomicBool::new(true));
        let state = AppState {
            index: None,
            constants: Arc::new(shared::Constants::load_embedded()),
            ready: Arc::clone(&ready),
            perf,
            log_search_avg,
            parser,
            classifier,
        };
        let ready_thread = Arc::clone(&ready);
        thread::Builder::new()
            .name("ready-http".to_string())
            .stack_size(64 * 1024)
            .spawn(move || {
                if let Err(err) = run_minimal_ready_server(port, ready_thread) {
                    eprintln!("minimal ready server exited with error: {}", err);
                }
            })
            .expect("failed to spawn minimal ready server");

        eprintln!("Classifier-only handoff API ready on port {}", port);
        run_fd_handoff_server(fd_listen.unwrap(), state)
            .expect("fd handoff server exited with error");
        return;
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    runtime.block_on(async move {
        let index = if classifier == ApiClassifier::TreeOnly {
            eprintln!("API classifier tree_only enabled; skipping index mmap/warmup.");
            None
        } else {
            eprintln!("Loading index from {}...", index_path);
            let file = File::open(&index_path).expect("failed to open index.bin");
            let mmap = unsafe { MmapOptions::new().map(&file).expect("failed to mmap index") };

            #[cfg(unix)]
            if std::env::var("MLOCK_INDEX").ok().as_deref() == Some("1") {
                unsafe {
                    let ptr = mmap.as_ptr() as *const libc::c_void;
                    let len = mmap.len();
                    if libc::mlock(ptr, len) == 0 {
                        eprintln!(
                            "Successfully locked index memory of size {} bytes in RAM via mlock.",
                            len
                        );
                    } else {
                        let err = std::io::Error::last_os_error();
                        eprintln!("Warning: Failed to lock index memory in RAM: {}. Performance under memory pressure may degrade.", err);
                    }
                }
            }

            Some(Arc::new(Index::new(mmap)))
        };
        let constants = Arc::new(shared::Constants::load_embedded());
        let ready = Arc::new(AtomicBool::new(false));

        if let Some(index_clone) = index.as_ref().map(Arc::clone) {
            let ready_clone = Arc::clone(&ready);
            tokio::task::spawn_blocking(move || {
                eprintln!("Warming up index...");
                index_clone.warmup();
                eprintln!("Index warm. Serving on port {}", port);
                ready_clone.store(true, Ordering::Relaxed);
            });
        } else {
            eprintln!("Classifier-only API ready on port {}", port);
            ready.store(true, Ordering::Relaxed);
        }

        let state = AppState {
            index,
            constants,
            ready,
            perf,
            log_search_avg,
            parser,
            classifier,
        };

        if let Some(raw_listen) = raw_listen {
            let raw_state = state.clone();
            tokio::spawn(async move {
                if let Err(err) = run_raw_server(raw_listen, raw_state).await {
                    eprintln!("raw server exited with error: {}", err);
                }
            });
        }

        #[cfg(unix)]
        if let Some(fd_listen) = fd_listen {
            let fd_state = state.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(err) = run_fd_handoff_server(fd_listen, fd_state) {
                    eprintln!("fd handoff server exited with error: {}", err);
                }
            });
        }

        let app = Router::new()
            .route("/ready", get(health_check))
            .route("/fraud-score", post(fraud_score))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port))
            .await
            .expect("failed to bind");
        eprintln!("Listening on port {}", port);
        axum::serve(listener, app).await.expect("server error");
    });
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn handoff_parser_waits_for_fragmented_body() {
        let req = b"POST /fraud-score HTTP/1.1\r\nContent-Length: 5\r\n\r\nhe";
        assert!(parse_handoff_request(req).unwrap().is_none());

        let req = b"POST /fraud-score HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello";
        let parsed = parse_handoff_request(req).unwrap().unwrap();
        assert_eq!(parsed.route, HandoffRoute::FraudScore);
        assert_eq!(&req[parsed.header_len..parsed.total_len], b"hello");
        assert!(!parsed.close_after_response);
    }

    #[test]
    fn handoff_parser_keeps_ready_lightweight() {
        let req = b"GET /ready HTTP/1.1\r\nHost: lb\r\n\r\n";
        let parsed = parse_handoff_request(req).unwrap().unwrap();
        assert_eq!(parsed.route, HandoffRoute::Ready);
        assert_eq!(parsed.header_len, parsed.total_len);
    }

    #[test]
    fn handoff_parser_rejects_chunked() {
        let req = b"POST /fraud-score HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n";
        let err = parse_handoff_request(req).unwrap_err();
        assert_eq!(err.response, HTTP_BAD_REQUEST);
    }

    #[test]
    fn handoff_score_responses_match_body_lengths() {
        for bucket in 0..=5 {
            let response = handoff_response_for_bucket(bucket);
            let header_end = find_header_end(response).unwrap() + 4;
            let content_len = std::str::from_utf8(&response[..header_end])
                .unwrap()
                .split("\r\n")
                .find_map(|line| {
                    line.strip_prefix("Content-Length:")
                        .map(|value| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            assert_eq!(content_len, response.len() - header_end);
        }
    }
}
