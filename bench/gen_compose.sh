#!/usr/bin/env bash
# Generate a Rinha bench compose from knobs (env vars). Defaults = banked 285ee52
# baseline but with cpuset REMOVED (faithful to official engine, which strips it;
# affinity via API_PIN_CPU only). One file per experiment -> reproducible A/B.
set -euo pipefail
IMG="${IMG:-visuzano/rinha-2026:epoll-clean-ec97581}"
CMD="${CMD:-/opt/api_c_reactor}"
LB_CPU="${LB_CPU:-0.02}"
API_CPU="${API_CPU:-0.49}"
BUSY_US="${BUSY_US:-50}"
BUSY_BUDGET="${BUSY_BUDGET:-8}"
PREFER="${PREFER:-1}"
PIN1="${PIN1:-0}"
PIN2="${PIN2:-1}"
LB_PIN="${LB_PIN:-}"            # empty = unset
USE_CPUSET="${USE_CPUSET:-0}"  # 1 = add cpuset (NOT faithful to official)
EXTRA_API="${EXTRA_API:-}"     # extra api env lines (e.g. SO_INCOMING_CPU flag), newline-sep
INCOMING="${INCOMING:-}"
[ -n "$INCOMING" ] && EXTRA_API="$EXTRA_API
      - API_INCOMING_CPU=$INCOMING"

cs_lb=""; cs1=""; cs2=""
if [ "$USE_CPUSET" = "1" ]; then cs_lb="    cpuset: \"2,3\""; cs1="    cpuset: \"0\""; cs2="    cpuset: \"1\""; fi
lbpin_line=""; [ -n "$LB_PIN" ] && lbpin_line="      - LB_PIN_CPU=$LB_PIN"

api_block() {  # $1=name $2=sockid $3=pin $4=cpuset_line
  cat <<API
  $1:
    image: $IMG
    command: ["$CMD"]
$4
    environment:
      - PORT=8080
      - API_FD_LISTEN=/sockets/$2-fd.sock
      - API_PARSER=fast
      - API_CLASSIFIER=tree_only
      - API_FD_EPOLL=1
      - API_BUSY_POLL_US=$BUSY_US
      - API_BUSY_POLL_BUDGET=$BUSY_BUDGET
      - API_PREFER_BUSY_POLL=$PREFER
      - API_PIN_CPU=$3
$EXTRA_API
    volumes:
      - backend-sockets:/sockets
    ulimits:
      nofile: { soft: 65535, hard: 65535 }
      memlock: { soft: -1, hard: -1 }
    healthcheck:
      test: ["CMD", "curl", "-f", "http://localhost:8080/ready"]
      interval: 5s
      timeout: 5s
      retries: 20
      start_period: 60s
    deploy:
      resources:
        limits:
          cpus: "$API_CPU"
          memory: "160MB"
    networks: [rinha]
API
}

cat <<HEAD
# generated: LB_CPU=$LB_CPU API_CPU=$API_CPU BUSY_US=$BUSY_US BUSY_BUDGET=$BUSY_BUDGET PREFER=$PREFER PIN1=$PIN1 PIN2=$PIN2 LB_PIN=${LB_PIN:-none} CPUSET=$USE_CPUSET
services:
  lb:
    image: $IMG
    command: ["/opt/fd_handoff_lb"]
$cs_lb
    ports:
      - "9999:9999"
    environment:
      - LB_LISTEN=0.0.0.0:9999
      - BACKENDS=unix:/sockets/api1-fd.sock,unix:/sockets/api2-fd.sock
      - LB_SELF_WARM=2000
$lbpin_line
    volumes:
      - backend-sockets:/sockets
    ulimits:
      nofile: { soft: 65535, hard: 65535 }
    depends_on:
      api1: { condition: service_healthy }
      api2: { condition: service_healthy }
    deploy:
      resources:
        limits:
          cpus: "$LB_CPU"
          memory: "30MB"
    networks: [rinha]
HEAD
api_block api1 api1 "$PIN1" "$cs1"
api_block api2 api2 "$PIN2" "$cs2"
cat <<TAIL
networks:
  rinha: { driver: bridge }
volumes:
  backend-sockets:
    driver: local
    driver_opts: { type: tmpfs, device: tmpfs, o: "size=4m,mode=0777" }
TAIL
