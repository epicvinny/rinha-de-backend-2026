# syntax=docker/dockerfile:1.4
# Multi-stage build:
#   stage 1: compile Rust binaries
#   stage 2: run builder to produce index.bin from references.json.gz
#   stage 3: slim runtime with api + index.bin only

# Stage 1: Compile Rust
FROM --platform=linux/amd64 rust:1.89-slim-bookworm AS rust-builder
WORKDIR /build

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock* ./
COPY .cargo .cargo
COPY crates crates

ENV RUSTFLAGS="-C target-cpu=haswell -C target-feature=+avx2,+fma,+sse4.2"
RUN cargo build --release -p builder -p api -p lb

# Stage 2: Build index
FROM --platform=linux/amd64 debian:bookworm-slim AS index-builder
WORKDIR /data

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=rust-builder /build/target/release/builder /usr/local/bin/builder
# Copy the references data files
COPY resources/references.json.gz /data/references.json.gz

RUN builder --refs /data/references.json.gz --out /data/index.bin && \
    echo "Index size: $(du -sh /data/index.bin)"

# Stage 3: Runtime
FROM --platform=linux/amd64 debian:bookworm-slim AS runtime
WORKDIR /opt

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=rust-builder /build/target/release/api /opt/api
COPY --from=rust-builder /build/target/release/lb /opt/lb
COPY --from=index-builder /data/index.bin /opt/index.bin

ENV INDEX_PATH=/opt/index.bin
ENV PORT=8080

EXPOSE 8080

CMD ["/opt/api"]
