use api::perf;
use api::scoring;
use api::search::Index;
use memmap2::MmapOptions;
use std::fs::File;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const MAX_HEADER_BYTES: usize = 4096;
const MAX_BODY_BYTES: usize = 8192;
const READ_CHUNK_BYTES: usize = 2048;

#[derive(Clone)]
struct EdgeState {
    index: Arc<Index>,
    ready: Arc<AtomicBool>,
    perf: Option<Arc<perf::PerfCollector>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Ready,
    FraudScore,
}

#[derive(Debug, Clone, Copy)]
struct ParsedRequest {
    route: Route,
    header_len: usize,
    total_len: usize,
    close_after_response: bool,
}

#[derive(Debug, Clone, Copy)]
struct RequestHead {
    route: Route,
    content_length: usize,
    close_after_response: bool,
}

#[derive(Debug)]
struct ClientReadError {
    response: &'static [u8],
}

fn main() {
    #[cfg(target_feature = "avx2")]
    eprintln!("EDGE compiled with AVX2 enabled.");
    #[cfg(not(target_feature = "avx2"))]
    eprintln!("WARNING: EDGE compiled WITHOUT AVX2 enabled!");

    let listen = std::env::var("EDGE_LISTEN")
        .unwrap_or_else(|_| "0.0.0.0:9999".to_string())
        .parse::<SocketAddr>()
        .expect("invalid EDGE_LISTEN");
    let worker_threads = std::env::var("EDGE_WORKERS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, 4);
    let index_path = std::env::var("INDEX_PATH").unwrap_or_else(|_| "/opt/index.bin".to_string());
    let perf = perf::PerfConfig::from_env().map(|config| {
        eprintln!(
            "EDGE PERF_TRACE enabled: every={} slow_us={} sample={}",
            config.every, config.slow_us, config.sample
        );
        Arc::new(perf::PerfCollector::new(config))
    });

    let runtime = if worker_threads == 1 {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build current-thread runtime")
    } else {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(worker_threads)
            .enable_all()
            .build()
            .expect("failed to build multi-thread runtime")
    };

    runtime.block_on(async move {
        eprintln!("EDGE worker_threads={}", worker_threads);
        eprintln!("EDGE loading index from {}...", index_path);
        let file = File::open(&index_path).expect("failed to open index.bin");
        let mmap = unsafe { MmapOptions::new().map(&file).expect("failed to mmap index") };
        let index = Arc::new(Index::new(mmap));
        let ready = Arc::new(AtomicBool::new(false));

        let index_clone = Arc::clone(&index);
        let ready_clone = Arc::clone(&ready);
        tokio::task::spawn_blocking(move || {
            eprintln!("EDGE warming up index...");
            index_clone.warmup();
            eprintln!("EDGE index warm.");
            ready_clone.store(true, Ordering::Relaxed);
        });

        let state = Arc::new(EdgeState { index, ready, perf });
        run_edge(listen, state).await.expect("edge exited");
    });
}

async fn run_edge(listen: SocketAddr, state: Arc<EdgeState>) -> io::Result<()> {
    let listener = TcpListener::bind(listen).await?;
    eprintln!("EDGE listening on {}", listen);

    loop {
        let (stream, _) = listener.accept().await?;
        tune_socket(&stream);
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(err) = handle_client(stream, state).await {
                if err.kind() != io::ErrorKind::UnexpectedEof {
                    eprintln!("edge client error: {}", err);
                }
            }
        });
    }
}

async fn handle_client(mut client: TcpStream, state: Arc<EdgeState>) -> io::Result<()> {
    let mut read_buf = Vec::with_capacity(1024);

    loop {
        let request = match read_request(&mut client, &mut read_buf).await {
            Ok(Some(request)) => request,
            Ok(None) => return Ok(()),
            Err(err) => {
                client.write_all(err.response).await?;
                return Ok(());
            }
        };

        let (response, force_close) = match request.route {
            Route::Ready => {
                if state.ready.load(Ordering::Relaxed) {
                    (scoring::READY_OK_RESPONSE, false)
                } else {
                    (scoring::SERVICE_UNAVAILABLE_RESPONSE, true)
                }
            }
            Route::FraudScore => {
                if !state.ready.load(Ordering::Relaxed) {
                    (scoring::SERVICE_UNAVAILABLE_RESPONSE, true)
                } else {
                    let body = &read_buf[request.header_len..request.total_len];
                    match score_response(&state, body) {
                        Some(response) => (response, false),
                        None => (scoring::BAD_REQUEST_RESPONSE, true),
                    }
                }
            }
        };

        client.write_all(response).await?;

        if request.total_len == read_buf.len() {
            read_buf.clear();
        } else {
            read_buf.drain(..request.total_len);
        }

        if request.close_after_response || force_close {
            return Ok(());
        }
    }
}

fn score_response(state: &EdgeState, body: &[u8]) -> Option<&'static [u8]> {
    let handler_start = Instant::now();

    if let Some(perf) = state.perf.as_deref() {
        let request = perf.begin_request();

        let parse_start = Instant::now();
        let payload: shared::types::Payload<'_> = serde_json::from_slice(body).ok()?;
        let json_parse_us = perf::elapsed_us(parse_start);

        let mut search_trace = perf::SearchTrace::default();
        let search_start = Instant::now();
        let (_approved, fraud_score) = state.index.search_with_trace(&payload, &mut search_trace);
        let search_total_us = perf::elapsed_us(search_start);

        let response_start = Instant::now();
        let bucket = scoring::bucket_for_fraud_score(fraud_score);
        let response = scoring::http_response_for_bucket(bucket)?;
        let response_build_us = perf::elapsed_us(response_start);

        perf.record_request(perf::RequestPerf {
            request_id: request.request_id(),
            in_flight_at_start: request.in_flight_at_start(),
            max_in_flight_seen: perf.max_in_flight_seen(),
            body_len: body.len(),
            score_bucket: bucket,
            handler_total_us: perf::elapsed_us(handler_start),
            json_parse_us,
            search_total_us,
            response_build_us,
            search: search_trace,
        });

        return Some(response);
    }

    let payload: shared::types::Payload<'_> = serde_json::from_slice(body).ok()?;
    let (_approved, fraud_score) = state.index.search(&payload);
    scoring::http_response_for_bucket(scoring::bucket_for_fraud_score(fraud_score))
}

async fn read_request<R: AsyncRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> Result<Option<ParsedRequest>, ClientReadError> {
    let mut tmp = [0u8; READ_CHUNK_BYTES];

    loop {
        if let Some(header_end_without_delim) = find_header_end(buf) {
            let header_len = header_end_without_delim + 4;
            let head = parse_request_head(&buf[..header_len])?;
            let total_len = header_len + head.content_length;
            if total_len > MAX_HEADER_BYTES + MAX_BODY_BYTES {
                return Err(ClientReadError {
                    response: scoring::PAYLOAD_TOO_LARGE_RESPONSE,
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
                response: scoring::BAD_REQUEST_RESPONSE,
            });
        }

        let n = reader.read(&mut tmp).await.map_err(|_| ClientReadError {
            response: scoring::BAD_REQUEST_RESPONSE,
        })?;
        if n == 0 {
            if buf.is_empty() {
                return Ok(None);
            }
            return Err(ClientReadError {
                response: scoring::BAD_REQUEST_RESPONSE,
            });
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

fn parse_request_head(header: &[u8]) -> Result<RequestHead, ClientReadError> {
    let header_str = std::str::from_utf8(header).map_err(|_| ClientReadError {
        response: scoring::BAD_REQUEST_RESPONSE,
    })?;
    let mut lines = header_str.split("\r\n");
    let request_line = lines.next().ok_or(ClientReadError {
        response: scoring::BAD_REQUEST_RESPONSE,
    })?;

    let mut parts = request_line.split_ascii_whitespace();
    let method = parts.next().ok_or(ClientReadError {
        response: scoring::BAD_REQUEST_RESPONSE,
    })?;
    let path = parts.next().ok_or(ClientReadError {
        response: scoring::BAD_REQUEST_RESPONSE,
    })?;
    let version = parts.next().ok_or(ClientReadError {
        response: scoring::BAD_REQUEST_RESPONSE,
    })?;
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(ClientReadError {
            response: scoring::BAD_REQUEST_RESPONSE,
        });
    }

    let route = match (method, path) {
        ("GET", "/ready") => Route::Ready,
        ("POST", "/fraud-score") => Route::FraudScore,
        ("GET" | "POST", _) => {
            return Err(ClientReadError {
                response: scoring::NOT_FOUND_RESPONSE,
            })
        }
        _ => {
            return Err(ClientReadError {
                response: scoring::METHOD_NOT_ALLOWED_RESPONSE,
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
                response: scoring::BAD_REQUEST_RESPONSE,
            });
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = Some(value.parse::<usize>().map_err(|_| ClientReadError {
                response: scoring::BAD_REQUEST_RESPONSE,
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
            response: scoring::BAD_REQUEST_RESPONSE,
        });
    }

    let content_length = content_length.unwrap_or(0);
    if route == Route::FraudScore && content_length == 0 {
        return Err(ClientReadError {
            response: scoring::BAD_REQUEST_RESPONSE,
        });
    }
    if content_length > MAX_BODY_BYTES {
        return Err(ClientReadError {
            response: scoring::PAYLOAD_TOO_LARGE_RESPONSE,
        });
    }

    Ok(RequestHead {
        route,
        content_length,
        close_after_response,
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn parses_post_head_case_insensitive() {
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
        assert_eq!(err.response, scoring::BAD_REQUEST_RESPONSE);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reads_pipelined_keepalive_requests() {
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
        let first = read_request(&mut server, &mut buf).await.unwrap().unwrap();
        assert_eq!(first.route, Route::Ready);
        buf.drain(..first.total_len);
        let second = read_request(&mut server, &mut buf).await.unwrap().unwrap();
        assert_eq!(second.route, Route::Ready);
    }
}
