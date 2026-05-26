use api::scoring;
use std::io;
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    let listen = std::env::var("PLACEHOLDER_LISTEN")
        .unwrap_or_else(|_| "0.0.0.0:8080".to_string())
        .parse::<SocketAddr>()
        .expect("invalid PLACEHOLDER_LISTEN");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build current-thread runtime");

    runtime
        .block_on(run_placeholder(listen))
        .expect("placeholder exited");
}

async fn run_placeholder(listen: SocketAddr) -> io::Result<()> {
    let listener = TcpListener::bind(listen).await?;
    eprintln!("placeholder listening on {}", listen);

    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(async move {
            let _ = handle_client(stream).await;
        });
    }
}

async fn handle_client(mut stream: TcpStream) -> io::Result<()> {
    let mut buf = [0u8; 512];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        if buf[..n].starts_with(b"GET /ready ") {
            stream.write_all(scoring::READY_OK_RESPONSE).await?;
        } else {
            stream.write_all(scoring::NOT_FOUND_RESPONSE).await?;
            return Ok(());
        }
    }
}
