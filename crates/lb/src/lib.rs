use std::io;
use std::net::SocketAddr;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::Instant;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex as AsyncMutex, Semaphore, SemaphorePermit};

const MAX_HEADER_BYTES: usize = 4096;
const MAX_BODY_BYTES: usize = 8192;
const READ_CHUNK_BYTES: usize = 2048;

const BAD_REQUEST: &[u8] =
    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const NOT_FOUND: &[u8] =
    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const METHOD_NOT_ALLOWED: &[u8] =
    b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const PAYLOAD_TOO_LARGE: &[u8] =
    b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const BAD_GATEWAY: &[u8] =
    b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const READY_OK: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n";

const RAW_BAD_REQUEST: u8 = 255;
const RAW_FRAME_HEADER_BYTES: usize = 2;
const STACK_UPSTREAM_FRAME_BYTES: usize = 1024;

const SCORE_0_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 33\r\nConnection: keep-alive\r\n\r\n{\"approved\":true,\"fraud_score\":0}";
const SCORE_1_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 35\r\nConnection: keep-alive\r\n\r\n{\"approved\":true,\"fraud_score\":0.2}";
const SCORE_2_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 35\r\nConnection: keep-alive\r\n\r\n{\"approved\":true,\"fraud_score\":0.4}";
const SCORE_3_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 36\r\nConnection: keep-alive\r\n\r\n{\"approved\":false,\"fraud_score\":0.6}";
const SCORE_4_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 36\r\nConnection: keep-alive\r\n\r\n{\"approved\":false,\"fraud_score\":0.8}";
const SCORE_5_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 34\r\nConnection: keep-alive\r\n\r\n{\"approved\":false,\"fraud_score\":1}";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpstreamProtocol {
    Http,
    Raw,
}

#[derive(Clone)]
pub struct LbConfig {
    pub listen: SocketAddr,
    pub backends: [String; 2],
    pub upstream_protocol: UpstreamProtocol,
    pub upstream_pool_per_backend: usize,
    pub upstream_preconnect_per_backend: usize,
    pub metrics: Option<Metrics>,
}

pub async fn run(config: LbConfig) -> io::Result<()> {
    let listener = TcpListener::bind(config.listen).await?;
    let state = Arc::new(LbState {
        backends: [
            BackendPool::new(config.backends[0].clone(), config.upstream_pool_per_backend),
            BackendPool::new(config.backends[1].clone(), config.upstream_pool_per_backend),
        ],
        upstream_protocol: config.upstream_protocol,
        next_backend: AtomicU64::new(0),
        active_clients: AtomicU64::new(0),
        max_active_clients: AtomicU64::new(0),
        metrics: config.metrics,
    });
    warm_backend_pools(&state, config.upstream_preconnect_per_backend).await;

    loop {
        let (stream, _) = listener.accept().await?;
        let accept_at = Instant::now();
        tune_socket(&stream);
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(err) = handle_client(stream, state, accept_at).await {
                if err.kind() != io::ErrorKind::UnexpectedEof {
                    eprintln!("lb client error: {}", err);
                }
            }
        });
    }
}

struct LbState {
    backends: [BackendPool; 2],
    upstream_protocol: UpstreamProtocol,
    next_backend: AtomicU64,
    active_clients: AtomicU64,
    max_active_clients: AtomicU64,
    metrics: Option<Metrics>,
}

impl LbState {
    #[inline]
    fn choose_backend(&self) -> usize {
        (self.next_backend.fetch_add(1, Ordering::Relaxed) & 1) as usize
    }

    fn begin_client(&self) -> ClientGuard<'_> {
        let active = self.active_clients.fetch_add(1, Ordering::Relaxed) + 1;
        self.update_max_active_clients(active);
        ClientGuard { state: self }
    }

    fn max_active_clients_seen(&self) -> u64 {
        self.max_active_clients.load(Ordering::Relaxed)
    }

    fn update_max_active_clients(&self, value: u64) {
        let mut current = self.max_active_clients.load(Ordering::Relaxed);
        while value > current {
            match self.max_active_clients.compare_exchange_weak(
                current,
                value,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(next) => current = next,
            }
        }
    }
}

struct ClientGuard<'a> {
    state: &'a LbState,
}

impl Drop for ClientGuard<'_> {
    fn drop(&mut self) {
        self.state.active_clients.fetch_sub(1, Ordering::Relaxed);
    }
}

struct BackendPool {
    addr: String,
    idle: AsyncMutex<Vec<TcpStream>>,
    permits: Semaphore,
    max_idle: usize,
}

impl BackendPool {
    fn new(addr: String, max_connections: usize) -> Self {
        Self {
            addr,
            idle: AsyncMutex::new(Vec::with_capacity(max_connections)),
            permits: Semaphore::new(max_connections),
            max_idle: max_connections,
        }
    }

    #[inline]
    fn addr(&self) -> &str {
        &self.addr
    }

    async fn take(
        &self,
        trace: &mut RequestTrace,
        force_fresh: bool,
    ) -> io::Result<(TcpStream, SemaphorePermit<'_>)> {
        let permit_start = Instant::now();
        let permit = self.permits.acquire().await.map_err(|_| {
            io::Error::new(io::ErrorKind::ConnectionAborted, "upstream pool closed")
        })?;
        trace.pool_permit_wait_us += elapsed_us(permit_start);

        if !force_fresh {
            let stream = {
                let idle_lock_start = Instant::now();
                let mut idle = self.idle.lock().await;
                trace.idle_lock_us += elapsed_us(idle_lock_start);
                idle.pop()
            };
            if let Some(stream) = stream {
                trace.idle_hits += 1;
                return Ok((stream, permit));
            }
            trace.idle_misses += 1;
        } else {
            let idle_lock_start = Instant::now();
            let mut idle = self.idle.lock().await;
            trace.idle_lock_us += elapsed_us(idle_lock_start);
            idle.clear();
            trace.idle_misses += 1;
        }

        let start = Instant::now();
        let stream = TcpStream::connect(&self.addr).await?;
        tune_socket(&stream);
        trace.upstream_connect_us += elapsed_us(start);
        Ok((stream, permit))
    }

    async fn put(&self, stream: TcpStream) {
        let mut idle = self.idle.lock().await;
        if idle.len() < self.max_idle {
            idle.push(stream);
        }
    }

    async fn warm(&self, target_idle: usize) -> io::Result<usize> {
        let target_idle = target_idle.min(self.max_idle);
        let mut connected = 0;
        while self.idle.lock().await.len() < target_idle {
            let start = Instant::now();
            let stream = TcpStream::connect(&self.addr).await?;
            tune_socket(&stream);
            connected += 1;

            let mut idle = self.idle.lock().await;
            if idle.len() < target_idle {
                idle.push(stream);
            }

            // Keep this visible to the optimizer without emitting per-connection logs.
            std::hint::black_box(elapsed_us(start));
        }
        Ok(connected)
    }
}

async fn warm_backend_pools(state: &LbState, target_idle: usize) {
    if target_idle == 0 {
        return;
    }

    for (idx, pool) in state.backends.iter().enumerate() {
        match pool.warm(target_idle).await {
            Ok(connected) => {
                eprintln!(
                    "LB preconnected {} upstream sockets for backend{} ({})",
                    connected,
                    idx,
                    pool.addr()
                );
            }
            Err(err) => {
                eprintln!(
                    "LB upstream preconnect skipped for backend{} ({}): {}",
                    idx,
                    pool.addr(),
                    err
                );
            }
        }
    }
}

async fn handle_client(
    mut client: TcpStream,
    state: Arc<LbState>,
    accept_at: Instant,
) -> io::Result<()> {
    let _client_guard = state.begin_client();
    let mut first_request_accept_at = Some(accept_at);
    let mut read_buf = Vec::with_capacity(1024);
    let mut response_buf = Vec::with_capacity(512);

    loop {
        let total_start = Instant::now();
        let request_id = state
            .metrics
            .as_ref()
            .map(|metrics| metrics.next_request_id())
            .unwrap_or(0);
        let mut request_trace = RequestTrace {
            request_id,
            accept_to_read_us: first_request_accept_at
                .take()
                .map(elapsed_us)
                .unwrap_or_default(),
            active_clients_at_start: state.active_clients.load(Ordering::Relaxed),
            max_active_clients_seen: state.max_active_clients_seen(),
            ..RequestTrace::default()
        };
        let request = match read_request(&mut client, &mut read_buf, &mut request_trace).await {
            Ok(Some(request)) => request,
            Ok(None) => return Ok(()),
            Err(err) => {
                let response = err.response;
                client.write_all(response).await?;
                return Ok(());
            }
        };

        request_trace.route = request.route.as_str();
        request_trace.body_len = request.total_len.saturating_sub(request.header_len) as u64;

        let body_start = request.header_len;
        let body_end = request.total_len;

        if state.upstream_protocol == UpstreamProtocol::Raw && request.route == Route::Ready {
            let write_start = Instant::now();
            client.write_all(READY_OK).await?;
            request_trace.client_write_us = elapsed_us(write_start);
            request_trace.total_us =
                elapsed_us(total_start).saturating_sub(request_trace.client_wait_us);
            if let Some(metrics) = state.metrics.as_ref() {
                metrics.record(request_trace, "ok");
            }
            if request.total_len == read_buf.len() {
                read_buf.clear();
            } else {
                read_buf.drain(..request.total_len);
            }
            if request.close_after_response {
                return Ok(());
            }
            continue;
        }

        let backend = state.choose_backend();

        let status = match proxy_with_retries(
            &state,
            backend,
            &request,
            &read_buf[body_start..body_end],
            &mut response_buf,
            &mut request_trace,
        )
        .await
        {
            Ok((used_backend, static_response)) => {
                request_trace.backend_idx = used_backend as u8;
                let write_start = Instant::now();
                if let Some(response) = static_response {
                    client.write_all(response).await?;
                } else {
                    client.write_all(&response_buf).await?;
                }
                request_trace.client_write_us = elapsed_us(write_start);
                "ok"
            }
            Err(_) => {
                request_trace.backend_idx = backend as u8;
                request_trace.status_5xx = 1;
                let write_start = Instant::now();
                client.write_all(BAD_GATEWAY).await?;
                request_trace.client_write_us = elapsed_us(write_start);
                "bad_gateway"
            }
        };

        request_trace.total_us =
            elapsed_us(total_start).saturating_sub(request_trace.client_wait_us);
        if let Some(metrics) = state.metrics.as_ref() {
            metrics.record(request_trace, status);
        }

        if request.total_len == read_buf.len() {
            read_buf.clear();
        } else {
            read_buf.drain(..request.total_len);
        }
        response_buf.clear();

        if request.close_after_response || status != "ok" {
            return Ok(());
        }
    }
}

async fn proxy_with_retries(
    state: &LbState,
    selected: usize,
    request: &ParsedRequest,
    body: &[u8],
    response_buf: &mut Vec<u8>,
    trace: &mut RequestTrace,
) -> io::Result<(usize, Option<&'static [u8]>)> {
    let attempts = [selected, selected, selected ^ 1];

    for (attempt_idx, backend_idx) in attempts.into_iter().enumerate() {
        if attempt_idx > 0 {
            trace.retries += 1;
        }

        let pool = &state.backends[backend_idx];
        let (mut stream, permit) = match pool.take(trace, attempt_idx > 0).await {
            Ok(pair) => pair,
            Err(_) => {
                trace.reconnects += 1;
                continue;
            }
        };

        let result = forward_once(
            &mut stream,
            backend_idx,
            pool.addr(),
            request,
            body,
            response_buf,
            trace,
            state.upstream_protocol,
        )
        .await;

        match result {
            Ok(static_response) => {
                pool.put(stream).await;
                drop(permit);
                return Ok((backend_idx, static_response));
            }
            Err(_) => {
                trace.reconnects += 1;
                drop(stream);
                drop(permit);
                response_buf.clear();
            }
        }
    }

    Err(io::Error::new(
        io::ErrorKind::ConnectionAborted,
        "all upstream attempts failed",
    ))
}

async fn forward_once(
    upstream: &mut TcpStream,
    backend_idx: usize,
    host: &str,
    request: &ParsedRequest,
    body: &[u8],
    response_buf: &mut Vec<u8>,
    trace: &mut RequestTrace,
    protocol: UpstreamProtocol,
) -> io::Result<Option<&'static [u8]>> {
    match protocol {
        UpstreamProtocol::Http => {
            forward_once_http(
                upstream,
                backend_idx,
                host,
                request,
                body,
                response_buf,
                trace,
            )
            .await
        }
        UpstreamProtocol::Raw => {
            forward_once_raw(upstream, backend_idx, request, body, response_buf, trace).await
        }
    }
}

async fn forward_once_http(
    upstream: &mut TcpStream,
    backend_idx: usize,
    host: &str,
    request: &ParsedRequest,
    body: &[u8],
    response_buf: &mut Vec<u8>,
    trace: &mut RequestTrace,
) -> io::Result<Option<&'static [u8]>> {
    let mut request_buf = [0u8; STACK_UPSTREAM_FRAME_BYTES];
    if let Some(frame) = build_upstream_request(request.route, host, body, &mut request_buf)? {
        let write_start = Instant::now();
        upstream.write_all(frame).await?;
        trace.upstream_write_header_us += elapsed_us(write_start);
    } else {
        let mut header_buf = [0u8; 256];
        let header = build_upstream_header(request.route, host, body.len(), &mut header_buf)?;
        let header_write_start = Instant::now();
        upstream.write_all(header).await?;
        trace.upstream_write_header_us += elapsed_us(header_write_start);
        let body_write_start = Instant::now();
        upstream.write_all(body).await?;
        trace.upstream_write_body_us += elapsed_us(body_write_start);
    }
    trace.upstream_write_us = trace.upstream_write_header_us + trace.upstream_write_body_us;

    read_response(upstream, response_buf, trace).await?;
    if backend_idx == 0 {
        trace.backend0 = 1;
    } else {
        trace.backend1 = 1;
    }
    Ok(None)
}

async fn forward_once_raw(
    upstream: &mut TcpStream,
    backend_idx: usize,
    request: &ParsedRequest,
    body: &[u8],
    _response_buf: &mut Vec<u8>,
    trace: &mut RequestTrace,
) -> io::Result<Option<&'static [u8]>> {
    if request.route != Route::FraudScore || body.len() > u16::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "raw upstream only supports fraud-score bodies",
        ));
    }

    let len = body.len();
    let write_start = Instant::now();
    if RAW_FRAME_HEADER_BYTES + len <= STACK_UPSTREAM_FRAME_BYTES {
        let mut frame = [0u8; STACK_UPSTREAM_FRAME_BYTES];
        frame[..RAW_FRAME_HEADER_BYTES].copy_from_slice(&(len as u16).to_be_bytes());
        frame[RAW_FRAME_HEADER_BYTES..RAW_FRAME_HEADER_BYTES + len].copy_from_slice(body);
        upstream
            .write_all(&frame[..RAW_FRAME_HEADER_BYTES + len])
            .await?;
    } else {
        let mut frame = Vec::with_capacity(RAW_FRAME_HEADER_BYTES + len);
        frame.extend_from_slice(&(len as u16).to_be_bytes());
        frame.extend_from_slice(body);
        upstream.write_all(&frame).await?;
    }
    trace.upstream_write_body_us += elapsed_us(write_start);
    trace.upstream_write_us = trace.upstream_write_header_us + trace.upstream_write_body_us;

    let mut code = [0u8; 1];
    let response_start = Instant::now();
    upstream.read_exact(&mut code).await?;
    trace.upstream_wait_header_us += elapsed_us(response_start);

    let response = if code[0] == RAW_BAD_REQUEST {
        BAD_REQUEST
    } else if let Some(response) = response_for_bucket(code[0]) {
        response
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "raw upstream returned invalid score bucket",
        ));
    };

    if backend_idx == 0 {
        trace.backend0 = 1;
    } else {
        trace.backend1 = 1;
    }
    Ok(Some(response))
}

fn response_for_bucket(bucket: u8) -> Option<&'static [u8]> {
    match bucket {
        0 => Some(SCORE_0_RESPONSE),
        1 => Some(SCORE_1_RESPONSE),
        2 => Some(SCORE_2_RESPONSE),
        3 => Some(SCORE_3_RESPONSE),
        4 => Some(SCORE_4_RESPONSE),
        5 => Some(SCORE_5_RESPONSE),
        _ => None,
    }
}

fn build_upstream_header<'a>(
    route: Route,
    host: &str,
    body_len: usize,
    out: &'a mut [u8; 256],
) -> io::Result<&'a [u8]> {
    let mut len = 0usize;
    match route {
        Route::Ready => {
            push_bytes(out, &mut len, b"GET /ready HTTP/1.1\r\nHost: ")?;
            push_bytes(out, &mut len, host.as_bytes())?;
            push_bytes(
                out,
                &mut len,
                b"\r\nConnection: keep-alive\r\nContent-Length: 0\r\n\r\n",
            )?;
        }
        Route::FraudScore => {
            push_bytes(out, &mut len, b"POST /fraud-score HTTP/1.1\r\nHost: ")?;
            push_bytes(out, &mut len, host.as_bytes())?;
            push_bytes(
                out,
                &mut len,
                b"\r\nContent-Type: application/json\r\nContent-Length: ",
            )?;
            push_usize_decimal(out, &mut len, body_len)?;
            push_bytes(out, &mut len, b"\r\nConnection: keep-alive\r\n\r\n")?;
        }
    }
    Ok(&out[..len])
}

fn build_upstream_request<'a>(
    route: Route,
    host: &str,
    body: &[u8],
    out: &'a mut [u8; STACK_UPSTREAM_FRAME_BYTES],
) -> io::Result<Option<&'a [u8]>> {
    let mut len = 0usize;
    match route {
        Route::Ready => {
            push_bytes(out, &mut len, b"GET /ready HTTP/1.1\r\nHost: ")?;
            push_bytes(out, &mut len, host.as_bytes())?;
            push_bytes(
                out,
                &mut len,
                b"\r\nConnection: keep-alive\r\nContent-Length: 0\r\n\r\n",
            )?;
        }
        Route::FraudScore => {
            push_bytes(out, &mut len, b"POST /fraud-score HTTP/1.1\r\nHost: ")?;
            push_bytes(out, &mut len, host.as_bytes())?;
            push_bytes(
                out,
                &mut len,
                b"\r\nContent-Type: application/json\r\nContent-Length: ",
            )?;
            push_usize_decimal(out, &mut len, body.len())?;
            push_bytes(out, &mut len, b"\r\nConnection: keep-alive\r\n\r\n")?;
            if len + body.len() > out.len() {
                return Ok(None);
            }
            out[len..len + body.len()].copy_from_slice(body);
            len += body.len();
        }
    }
    Ok(Some(&out[..len]))
}

fn push_bytes(out: &mut [u8], len: &mut usize, bytes: &[u8]) -> io::Result<()> {
    let end = *len + bytes.len();
    if end > out.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "upstream request header too large",
        ));
    }
    out[*len..end].copy_from_slice(bytes);
    *len = end;
    Ok(())
}

fn push_usize_decimal(out: &mut [u8], len: &mut usize, mut value: usize) -> io::Result<()> {
    let mut digits = [0u8; 20];
    let mut n = 0usize;
    loop {
        digits[n] = b'0' + (value % 10) as u8;
        n += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }

    let end = *len + n;
    if end > out.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "upstream request header too large",
        ));
    }
    for idx in 0..n {
        out[*len + idx] = digits[n - 1 - idx];
    }
    *len = end;
    Ok(())
}

async fn read_response(
    upstream: &mut TcpStream,
    response_buf: &mut Vec<u8>,
    trace: &mut RequestTrace,
) -> io::Result<()> {
    let mut tmp = [0u8; READ_CHUNK_BYTES];
    let header_start = Instant::now();
    let header_end = loop {
        if let Some(pos) = find_header_end(response_buf) {
            break pos + 4;
        }

        if response_buf.len() > MAX_HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "upstream response header too large",
            ));
        }

        let n = upstream.read(&mut tmp).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "upstream closed before response header",
            ));
        }
        response_buf.extend_from_slice(&tmp[..n]);
    };
    trace.upstream_wait_header_us += elapsed_us(header_start);

    let content_length = parse_response_content_length(&response_buf[..header_end])?;
    let total_len = header_end + content_length;
    if response_buf.len() < total_len {
        let body_start = Instant::now();
        while response_buf.len() < total_len {
            let n = upstream.read(&mut tmp).await?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "upstream closed before response body",
                ));
            }
            response_buf.extend_from_slice(&tmp[..n]);
        }
        trace.upstream_read_body_us += elapsed_us(body_start);
    }

    if response_buf.len() > total_len {
        // The API should not pipeline responses, but keep the client response exact.
        response_buf.truncate(total_len);
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Ready,
    FraudScore,
}

impl Route {
    #[inline]
    fn as_str(self) -> &'static str {
        match self {
            Route::Ready => "ready",
            Route::FraudScore => "fraud_score",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ParsedRequest {
    route: Route,
    header_len: usize,
    total_len: usize,
    close_after_response: bool,
}

#[derive(Debug)]
struct ClientReadError {
    response: &'static [u8],
}

async fn read_request<R: AsyncRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    trace: &mut RequestTrace,
) -> Result<Option<ParsedRequest>, ClientReadError> {
    let mut tmp = [0u8; READ_CHUNK_BYTES];
    let mut saw_request_bytes = !buf.is_empty();

    loop {
        if let Some(header_end_without_delim) = find_header_end(buf) {
            let header_len = header_end_without_delim + 4;
            let parse_start = Instant::now();
            let head = parse_request_head(&buf[..header_len])?;
            trace.request_parse_us += elapsed_us(parse_start);
            let total_len = header_len + head.content_length;
            if total_len > MAX_HEADER_BYTES + MAX_BODY_BYTES {
                return Err(ClientReadError {
                    response: PAYLOAD_TOO_LARGE,
                });
            }
            if buf.len() >= total_len {
                return Ok(Some(ParsedRequest {
                    route: head.route,
                    header_len,
                    total_len,
                    close_after_response: head.close_after_response,
                }));
            }
        } else if buf.len() > MAX_HEADER_BYTES {
            return Err(ClientReadError {
                response: BAD_REQUEST,
            });
        }

        let read_start = Instant::now();
        let n = reader.read(&mut tmp).await.map_err(|_| ClientReadError {
            response: BAD_REQUEST,
        })?;
        let read_us = elapsed_us(read_start);
        if n == 0 {
            if buf.is_empty() {
                return Ok(None);
            }
            return Err(ClientReadError {
                response: BAD_REQUEST,
            });
        }
        if saw_request_bytes {
            trace.client_read_us += read_us;
        } else {
            trace.client_wait_us += read_us;
            saw_request_bytes = true;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

#[derive(Debug, Clone, Copy)]
struct RequestHead {
    route: Route,
    content_length: usize,
    close_after_response: bool,
}

fn parse_request_head(header: &[u8]) -> Result<RequestHead, ClientReadError> {
    if let Some(head) = parse_request_head_fast(header)? {
        return Ok(head);
    }
    parse_request_head_generic(header)
}

fn parse_request_head_fast(header: &[u8]) -> Result<Option<RequestHead>, ClientReadError> {
    const POST_FRAUD_11: &[u8] = b"POST /fraud-score HTTP/1.1\r\n";
    const POST_FRAUD_10: &[u8] = b"POST /fraud-score HTTP/1.0\r\n";
    const GET_READY_11: &[u8] = b"GET /ready HTTP/1.1\r\n";
    const GET_READY_10: &[u8] = b"GET /ready HTTP/1.0\r\n";

    let (route, http10) = if header.starts_with(POST_FRAUD_11) {
        (Route::FraudScore, false)
    } else if header.starts_with(POST_FRAUD_10) {
        (Route::FraudScore, true)
    } else if header.starts_with(GET_READY_11) {
        (Route::Ready, false)
    } else if header.starts_with(GET_READY_10) {
        (Route::Ready, true)
    } else {
        return Ok(None);
    };

    if find_subslice(header, b"\r\nTransfer-Encoding:")
        .or_else(|| find_subslice(header, b"\r\ntransfer-encoding:"))
        .is_some()
    {
        return Ok(None);
    }

    let content_length = if route == Route::FraudScore {
        let Some(value_start) = find_content_length_value(header) else {
            return Ok(None);
        };
        parse_usize_decimal_header(header, value_start)?
    } else {
        0
    };

    if route == Route::FraudScore && content_length == 0 {
        return Err(ClientReadError {
            response: BAD_REQUEST,
        });
    }
    if content_length > MAX_BODY_BYTES {
        return Err(ClientReadError {
            response: PAYLOAD_TOO_LARGE,
        });
    }

    Ok(Some(RequestHead {
        route,
        content_length,
        close_after_response: http10 || contains_connection_close_fast(header),
    }))
}

fn parse_request_head_generic(header: &[u8]) -> Result<RequestHead, ClientReadError> {
    let header_str = std::str::from_utf8(header).map_err(|_| ClientReadError {
        response: BAD_REQUEST,
    })?;
    let mut lines = header_str.split("\r\n");
    let request_line = lines.next().ok_or(ClientReadError {
        response: BAD_REQUEST,
    })?;

    let mut parts = request_line.split_ascii_whitespace();
    let method = parts.next().ok_or(ClientReadError {
        response: BAD_REQUEST,
    })?;
    let path = parts.next().ok_or(ClientReadError {
        response: BAD_REQUEST,
    })?;
    let version = parts.next().ok_or(ClientReadError {
        response: BAD_REQUEST,
    })?;
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(ClientReadError {
            response: BAD_REQUEST,
        });
    }

    let route = match (method, path) {
        ("GET", "/ready") => Route::Ready,
        ("POST", "/fraud-score") => Route::FraudScore,
        ("GET" | "POST", _) => {
            return Err(ClientReadError {
                response: NOT_FOUND,
            })
        }
        _ => {
            return Err(ClientReadError {
                response: METHOD_NOT_ALLOWED,
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
            return Err(ClientReadError {
                response: BAD_REQUEST,
            });
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = Some(value.parse::<usize>().map_err(|_| ClientReadError {
                response: BAD_REQUEST,
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
        return Err(ClientReadError {
            response: BAD_REQUEST,
        });
    }

    let content_length = content_length.unwrap_or(0);
    if route == Route::FraudScore && content_length == 0 {
        return Err(ClientReadError {
            response: BAD_REQUEST,
        });
    }
    if content_length > MAX_BODY_BYTES {
        return Err(ClientReadError {
            response: PAYLOAD_TOO_LARGE,
        });
    }

    Ok(RequestHead {
        route,
        content_length,
        close_after_response,
    })
}

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

fn parse_usize_decimal_header(header: &[u8], mut pos: usize) -> Result<usize, ClientReadError> {
    let mut value = 0usize;
    let mut found = false;
    while let Some(&byte) = header.get(pos) {
        match byte {
            b'0'..=b'9' => {
                found = true;
                value = value
                    .checked_mul(10)
                    .and_then(|v| v.checked_add((byte - b'0') as usize))
                    .ok_or(ClientReadError {
                        response: BAD_REQUEST,
                    })?;
            }
            b'\r' | b'\n' => break,
            b' ' | b'\t' if !found => {}
            _ => {
                return Err(ClientReadError {
                    response: BAD_REQUEST,
                })
            }
        }
        pos += 1;
    }
    if found {
        Ok(value)
    } else {
        Err(ClientReadError {
            response: BAD_REQUEST,
        })
    }
}

fn contains_connection_close_fast(header: &[u8]) -> bool {
    find_subslice(header, b"\r\nConnection: close")
        .or_else(|| find_subslice(header, b"\r\nconnection: close"))
        .is_some()
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_response_content_length(header: &[u8]) -> io::Result<usize> {
    let header_str = std::str::from_utf8(header)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 response header"))?;
    for line in header_str.split("\r\n").skip(1) {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                return value.trim().parse::<usize>().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "bad response content-length")
                });
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "response missing content-length",
    ))
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn tune_socket(stream: &TcpStream) {
    let _ = stream.set_nodelay(true);

    #[cfg(target_os = "linux")]
    {
        use std::mem::size_of_val;
        use std::os::fd::AsRawFd;

        let fd = stream.as_raw_fd();
        let bytes: libc::c_int = 8192;
        let opt_len = size_of_val(&bytes) as libc::socklen_t;
        let opt_ptr = &bytes as *const libc::c_int as *const libc::c_void;
        unsafe {
            let _ = libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVBUF, opt_ptr, opt_len);
            let _ = libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_SNDBUF, opt_ptr, opt_len);
        }
    }

    #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
    {
        let _ = stream.set_quickack(true);
    }
}

#[derive(Default, Clone, Copy)]
pub struct RequestTrace {
    pub request_id: u64,
    pub route: &'static str,
    pub body_len: u64,
    pub accept_to_read_us: u64,
    pub client_wait_us: u64,
    pub client_read_us: u64,
    pub request_parse_us: u64,
    pub pool_permit_wait_us: u64,
    pub idle_lock_us: u64,
    pub upstream_connect_us: u64,
    pub upstream_write_us: u64,
    pub upstream_write_header_us: u64,
    pub upstream_write_body_us: u64,
    pub upstream_wait_header_us: u64,
    pub upstream_read_body_us: u64,
    pub client_write_us: u64,
    pub total_us: u64,
    pub backend_idx: u8,
    pub backend0: u64,
    pub backend1: u64,
    pub idle_hits: u64,
    pub idle_misses: u64,
    pub active_clients_at_start: u64,
    pub max_active_clients_seen: u64,
    pub retries: u64,
    pub reconnects: u64,
    pub status_5xx: u64,
}

#[derive(Clone)]
pub struct Metrics {
    every: usize,
    slow_us: u64,
    sample: u64,
    request_seq: Arc<AtomicU64>,
    window: Arc<Mutex<Vec<RequestTrace>>>,
}

impl Metrics {
    pub fn new(every: usize, slow_us: u64, sample: u64) -> Self {
        Self {
            every,
            slow_us,
            sample,
            request_seq: Arc::new(AtomicU64::new(0)),
            window: Arc::new(Mutex::new(Vec::with_capacity(every))),
        }
    }

    fn next_request_id(&self) -> u64 {
        self.request_seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn record(&self, trace: RequestTrace, status: &str) {
        if trace.total_us >= self.slow_us {
            emit_request_trace("lb_slow_request", trace, status);
        } else if self.sample > 0 && trace.request_id % self.sample == 0 {
            emit_request_trace("lb_sample_request", trace, status);
        }

        let flush = {
            let mut window = self.window.lock().expect("lb metrics poisoned");
            window.push(trace);
            if window.len() >= self.every {
                Some(std::mem::take(&mut *window))
            } else {
                None
            }
        };

        if let Some(records) = flush {
            emit_summary(&records, status);
        }
    }
}

fn emit_summary(records: &[RequestTrace], last_status: &str) {
    let backend0: u64 = records.iter().map(|r| r.backend0).sum();
    let backend1: u64 = records.iter().map(|r| r.backend1).sum();
    let idle_hits: u64 = records.iter().map(|r| r.idle_hits).sum();
    let idle_misses: u64 = records.iter().map(|r| r.idle_misses).sum();
    let retries: u64 = records.iter().map(|r| r.retries).sum();
    let reconnects: u64 = records.iter().map(|r| r.reconnects).sum();
    let status_5xx: u64 = records.iter().map(|r| r.status_5xx).sum();
    eprintln!(
        "{{\"kind\":\"lb_summary\",\"requests\":{},\"last_status\":\"{}\",\"backend0\":{},\"backend1\":{},\"idle_hits\":{},\"idle_misses\":{},\"retries\":{},\"reconnects\":{},\"status_5xx\":{},\"client_concurrency\":{{\"at_start\":{},\"max_seen\":{}}},\"request_counters\":{{\"body_len\":{}}},\"timings_us\":{{\"accept_to_read\":{},\"client_wait\":{},\"client_read\":{},\"request_parse\":{},\"pool_permit_wait\":{},\"idle_lock\":{},\"upstream_connect\":{},\"upstream_write\":{},\"upstream_write_header\":{},\"upstream_write_body\":{},\"upstream_wait_header\":{},\"upstream_read_body\":{},\"client_write\":{},\"total\":{}}}}}",
        records.len(),
        last_status,
        backend0,
        backend1,
        idle_hits,
        idle_misses,
        retries,
        reconnects,
        status_5xx,
        stats_json(records.iter().map(|r| r.active_clients_at_start).collect()),
        stats_json(records.iter().map(|r| r.max_active_clients_seen).collect()),
        stats_json(records.iter().map(|r| r.body_len).collect()),
        stats_json(records.iter().map(|r| r.accept_to_read_us).collect()),
        stats_json(records.iter().map(|r| r.client_wait_us).collect()),
        stats_json(records.iter().map(|r| r.client_read_us).collect()),
        stats_json(records.iter().map(|r| r.request_parse_us).collect()),
        stats_json(records.iter().map(|r| r.pool_permit_wait_us).collect()),
        stats_json(records.iter().map(|r| r.idle_lock_us).collect()),
        stats_json(records.iter().map(|r| r.upstream_connect_us).collect()),
        stats_json(records.iter().map(|r| r.upstream_write_us).collect()),
        stats_json(records.iter().map(|r| r.upstream_write_header_us).collect()),
        stats_json(records.iter().map(|r| r.upstream_write_body_us).collect()),
        stats_json(records.iter().map(|r| r.upstream_wait_header_us).collect()),
        stats_json(records.iter().map(|r| r.upstream_read_body_us).collect()),
        stats_json(records.iter().map(|r| r.client_write_us).collect()),
        stats_json(records.iter().map(|r| r.total_us).collect()),
    );
}

fn emit_request_trace(kind: &str, trace: RequestTrace, status: &str) {
    eprintln!(
        "{{\"kind\":\"{}\",\"request_id\":{},\"status\":\"{}\",\"route\":\"{}\",\"backend_idx\":{},\"body_len\":{},\"idle_hits\":{},\"idle_misses\":{},\"retries\":{},\"reconnects\":{},\"status_5xx\":{},\"client_concurrency\":{{\"at_start\":{},\"max_seen\":{}}},\"timings_us\":{{\"accept_to_read\":{},\"client_wait\":{},\"client_read\":{},\"request_parse\":{},\"pool_permit_wait\":{},\"idle_lock\":{},\"upstream_connect\":{},\"upstream_write\":{},\"upstream_write_header\":{},\"upstream_write_body\":{},\"upstream_wait_header\":{},\"upstream_read_body\":{},\"client_write\":{},\"total\":{}}}}}",
        kind,
        trace.request_id,
        status,
        trace.route,
        trace.backend_idx,
        trace.body_len,
        trace.idle_hits,
        trace.idle_misses,
        trace.retries,
        trace.reconnects,
        trace.status_5xx,
        trace.active_clients_at_start,
        trace.max_active_clients_seen,
        trace.accept_to_read_us,
        trace.client_wait_us,
        trace.client_read_us,
        trace.request_parse_us,
        trace.pool_permit_wait_us,
        trace.idle_lock_us,
        trace.upstream_connect_us,
        trace.upstream_write_us,
        trace.upstream_write_header_us,
        trace.upstream_write_body_us,
        trace.upstream_wait_header_us,
        trace.upstream_read_body_us,
        trace.client_write_us,
        trace.total_us,
    );
}

fn stats_json(mut values: Vec<u64>) -> String {
    if values.is_empty() {
        return "{\"avg\":0,\"p50\":0,\"p90\":0,\"p99\":0,\"max\":0}".to_string();
    }
    values.sort_unstable();
    let len = values.len();
    let sum: u128 = values.iter().map(|&v| v as u128).sum();
    format!(
        "{{\"avg\":{:.3},\"p50\":{},\"p90\":{},\"p99\":{},\"max\":{}}}",
        sum as f64 / len as f64,
        percentile(&values, 50),
        percentile(&values, 90),
        percentile(&values, 99),
        values[len - 1]
    )
}

fn percentile(values: &[u64], pct: usize) -> u64 {
    let idx = ((values.len() - 1) * pct).div_ceil(100);
    values[idx]
}

#[inline]
fn elapsed_us(start: Instant) -> u64 {
    start.elapsed().as_micros() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn parses_complete_post_head_case_insensitive() {
        let req = b"POST /fraud-score HTTP/1.1\r\nhost: x\r\nCONTENT-LENGTH: 3\r\nconnection: close\r\n\r\nabc";
        let head = parse_request_head(&req[..req.len() - 3]).unwrap();
        assert_eq!(head.route, Route::FraudScore);
        assert_eq!(head.content_length, 3);
        assert!(head.close_after_response);
    }

    #[test]
    fn rejects_chunked_request() {
        let req = b"POST /fraud-score HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n";
        let err = parse_request_head(req).unwrap_err();
        assert_eq!(err.response, BAD_REQUEST);
    }

    #[test]
    fn body_limit_returns_413() {
        let req = format!(
            "POST /fraud-score HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        let err = parse_request_head(req.as_bytes()).unwrap_err();
        assert_eq!(err.response, PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn parses_zero_length_ready() {
        let req = b"GET /ready HTTP/1.1\r\nHost: lb\r\n\r\n";
        let head = parse_request_head(req).unwrap();
        assert_eq!(head.route, Route::Ready);
        assert_eq!(head.content_length, 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_request_handles_fragmented_header_and_body() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            client
                .write_all(b"POST /fraud-score HTTP/1.1\r\nContent-L")
                .await
                .unwrap();
            client
                .write_all(b"ength: 5\r\nConnection: keep-alive\r\n\r\nhe")
                .await
                .unwrap();
            client.write_all(b"llo").await.unwrap();
        });

        let mut buf = Vec::new();
        let mut trace = RequestTrace::default();
        let req = read_request(&mut server, &mut buf, &mut trace)
            .await
            .unwrap()
            .expect("request");
        assert_eq!(req.route, Route::FraudScore);
        assert_eq!(&buf[req.header_len..req.total_len], b"hello");
        assert!(!req.close_after_response);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_request_keeps_next_request_bytes_for_keep_alive() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            client
                .write_all(
                    b"GET /ready HTTP/1.1\r\nContent-Length: 0\r\n\r\nGET /ready HTTP/1.1\r\nContent-Length: 0\r\n\r\n",
                )
                .await
                .unwrap();
        });

        let mut buf = Vec::new();
        let mut trace = RequestTrace::default();
        let first = read_request(&mut server, &mut buf, &mut trace)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.route, Route::Ready);
        buf.drain(..first.total_len);
        let second = read_request(&mut server, &mut buf, &mut trace)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.route, Route::Ready);
    }

    #[test]
    fn response_content_length_allows_zero() {
        let header = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
        assert_eq!(parse_response_content_length(header).unwrap(), 0);
    }

    #[test]
    fn builds_upstream_post_header_without_heap_formatting() {
        let mut buf = [0u8; 256];
        let header = build_upstream_header(Route::FraudScore, "api1:8080", 123, &mut buf).unwrap();
        assert_eq!(
            header,
            b"POST /fraud-score HTTP/1.1\r\nHost: api1:8080\r\nContent-Type: application/json\r\nContent-Length: 123\r\nConnection: keep-alive\r\n\r\n"
        );
    }

    #[test]
    fn builds_upstream_ready_header() {
        let mut buf = [0u8; 256];
        let header = build_upstream_header(Route::Ready, "api2:8080", 0, &mut buf).unwrap();
        assert_eq!(
            header,
            b"GET /ready HTTP/1.1\r\nHost: api2:8080\r\nConnection: keep-alive\r\nContent-Length: 0\r\n\r\n"
        );
    }

    #[test]
    fn raw_score_responses_have_valid_content_length() {
        for bucket in 0..=5 {
            let response = response_for_bucket(bucket).unwrap();
            let header_end = find_header_end(response).unwrap() + 4;
            let content_length = parse_response_content_length(&response[..header_end]).unwrap();
            assert_eq!(content_length, response.len() - header_end);
        }
    }
}
