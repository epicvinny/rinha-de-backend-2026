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
use std::fs::File;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod perf;
mod search;
use search::Index;

#[derive(Clone)]
struct AppState {
    index: Arc<Index>,
    constants: Arc<shared::Constants>,
    ready: Arc<AtomicBool>,
    perf: Option<Arc<perf::PerfCollector>>,
    log_search_avg: bool,
    parser: ApiParser,
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

static REQ_COUNT: AtomicU64 = AtomicU64::new(0);
static TOTAL_US: AtomicU64 = AtomicU64::new(0);

const RAW_BAD_REQUEST: u8 = 255;
const RAW_MAX_BODY_BYTES: usize = 8192;

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
    let handler_start = Instant::now();

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
    handler_start: Instant,
    build_response: bool,
) -> Result<u8, StatusCode> {
    if let Some(perf) = state.perf.as_deref() {
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
            state
                .index
                .search_vector_with_trace(&qv16, query_key, &mut search_trace)
        } else {
            state
                .index
                .search_with_trace(parsed_payload.unwrap(), &mut search_trace)
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

    let (_approved, fraud_score_val) = if state.parser == ApiParser::Fast {
        if let Some((qv16, query_key)) =
            shared::parse_payload_to_i16_and_key(body_bytes, &state.constants)
        {
            state.index.search_vector(&qv16, query_key)
        } else {
            let payload: shared::types::Payload<'_> =
                serde_json::from_slice(body_bytes).map_err(|_e| StatusCode::BAD_REQUEST)?;
            state.index.search(&payload)
        }
    } else {
        let payload: shared::types::Payload<'_> =
            serde_json::from_slice(body_bytes).map_err(|_e| StatusCode::BAD_REQUEST)?;
        state.index.search(&payload)
    };

    if state.log_search_avg {
        let elapsed = handler_start.elapsed().as_micros() as u64;
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

        let handler_start = Instant::now();
        let bucket = match score_body(&state, body_slice, handler_start, false) {
            Ok(bucket) => bucket,
            Err(_) => RAW_BAD_REQUEST,
        };
        stream.write_all(&[bucket]).await?;
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

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    runtime.block_on(async move {
        eprintln!("Loading index from {}...", index_path);
        let file = File::open(&index_path).expect("failed to open index.bin");
        let mmap = unsafe { MmapOptions::new().map(&file).expect("failed to mmap index") };

        #[cfg(unix)]
        if std::env::var("MLOCK_INDEX").ok().as_deref() == Some("1") {
            unsafe {
                let ptr = mmap.as_ptr() as *const libc::c_void;
                let len = mmap.len();
                if libc::mlock(ptr, len) == 0 {
                    eprintln!("Successfully locked index memory of size {} bytes in RAM via mlock.", len);
                } else {
                    let err = std::io::Error::last_os_error();
                    eprintln!("Warning: Failed to lock index memory in RAM: {}. Performance under memory pressure may degrade.", err);
                }
            }
        }

        let index = Arc::new(Index::new(mmap));
        let constants = Arc::new(shared::Constants::load_embedded());
        let ready = Arc::new(AtomicBool::new(false));

        // Warmup in background
        let index_clone = Arc::clone(&index);
        let ready_clone = Arc::clone(&ready);
        tokio::task::spawn_blocking(move || {
            eprintln!("Warming up index...");
            index_clone.warmup();
            eprintln!("Index warm. Serving on port {}", port);
            ready_clone.store(true, Ordering::Relaxed);
        });

        let state = AppState {
            index,
            constants,
            ready,
            perf,
            log_search_avg,
            parser,
        };

        if let Some(raw_listen) = raw_listen {
            let raw_state = state.clone();
            tokio::spawn(async move {
                if let Err(err) = run_raw_server(raw_listen, raw_state).await {
                    eprintln!("raw server exited with error: {}", err);
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
