#!/usr/bin/env bash
# GCP Haswell bench-lab driver — run from WSL (Ubuntu) bash.
# Purpose: spin up an Intel-Haswell N1 VM (4 vCPU = 2C/4T, kernel >=6.9 -> real
# EPIOCSPARAMS busy-poll, runs our amd64+AVX2 image natively) and benchmark
# docker-compose configs there with co-located k6 — RELATIVE p99 signal without
# spending official preview tests. Absolute p99 won't match the Mac Mini; confirm
# winners on the official preview.
#
# ONE-TIME MANUAL PREREQ (user, once): gcloud auth login; gcloud config set project <P>;
#   gcloud services enable compute.googleapis.com  (+ billing). After that, fully automatic.
#
# Usage:
#   bench/gcp.sh find-zone                 # list zones that still offer Intel Haswell
#   bench/gcp.sh provision                 # create SPOT VM + install docker/k6/jq + push assets
#   bench/gcp.sh bench <compose.yml>       # run one experiment, prints {"p99_ms":..,"http_req_failed":..}
#   bench/gcp.sh build [tag]               # rsync repo + docker build on the VM (code experiments)
#   bench/gcp.sh ssh                       # interactive shell on the VM
#   bench/gcp.sh start | stop | delete     # manage the VM (STOP when idle — SPOT is cheap, not free)
#
# Config via env (or edit defaults):
set -euo pipefail
PROJECT="${GCP_PROJECT:-$(gcloud config get-value project 2>/dev/null)}"
ZONE="${GCP_ZONE:-us-central1-a}"
VM="${GCP_VM:-rinha-haswell}"
MACHINE_TYPE="${GCP_MT:-n1-standard-4}"          # 4 vCPU = 2 cores x 2 HT = matches 2C/4T
REPO_LOCAL="${REPO_LOCAL:-/mnt/d/Github/rinha-de-backend-2026}"
DATA_LOCAL="${DATA_LOCAL:-$REPO_LOCAL/test/test-data.json}"   # gitignored; pushed directly (never committed)
REFS_LOCAL="${REFS_LOCAL:-$REPO_LOCAL/resources/references.json.gz}"  # only needed for `build`
REMOTE_DIR="~/rinha"

g() { gcloud compute "$@" --zone="$ZONE"; }
ssh_vm() { gcloud compute ssh "$VM" --zone="$ZONE" --command="$1"; }

case "${1:-}" in
  find-zone)
    for z in us-central1-a us-central1-b us-central1-c us-east1-b us-east1-c us-east1-d \
             us-west1-a us-west1-b europe-west1-b europe-west1-c europe-west1-d asia-east1-a; do
      p=$(gcloud compute zones describe "$z" --format="value(availableCpuPlatforms)" 2>/dev/null || true)
      if echo "$p" | grep -q "Intel Haswell"; then echo "$z: HASWELL ✓"; else echo "$z: (no haswell)"; fi
    done
    ;;

  provision)
    gcloud compute instances create "$VM" \
      --zone="$ZONE" --machine-type="$MACHINE_TYPE" \
      --min-cpu-platform="Intel Haswell" \
      --image-family=ubuntu-2404-lts-amd64 --image-project=ubuntu-os-cloud \
      --boot-disk-size=30GB --boot-disk-type=pd-balanced \
      --provisioning-model=SPOT --no-restart-on-failure
    echo "waiting for ssh..."
    until gcloud compute ssh "$VM" --zone="$ZONE" --command="true" >/dev/null 2>&1; do sleep 5; done
    ssh_vm '
      set -e
      sudo apt-get update -q
      curl -fsSL https://get.docker.com | sudo sh
      sudo usermod -aG docker "$USER" || true
      sudo apt-get install -y jq gnupg ca-certificates
      # k6 apt repo
      sudo gpg --no-default-keyring --keyring /usr/share/keyrings/k6-archive-keyring.gpg \
        --keyserver hkp://keyserver.ubuntu.com:80 --recv-keys C5AD17C747E3415A3642D57D77C6C491D6AC1D69 || true
      echo "deb [signed-by=/usr/share/keyrings/k6-archive-keyring.gpg] https://dl.k6.io/deb stable main" \
        | sudo tee /etc/apt/sources.list.d/k6.list >/dev/null
      sudo apt-get update -q && sudo apt-get install -y k6
      mkdir -p ~/rinha
      echo "=== kernel ($(uname -r)) — need >= 6.9 for EPIOCSPARAMS busy-poll ==="
      grep -m1 "model name" /proc/cpuinfo
    '
    gcloud compute scp "$DATA_LOCAL"                 "$VM":"$REMOTE_DIR/test-data.json"   --zone="$ZONE"
    gcloud compute scp "$REPO_LOCAL/bench/remote-bench.sh" "$VM":"$REMOTE_DIR/remote-bench.sh" --zone="$ZONE"
    gcloud compute scp "$REPO_LOCAL/bench/k6-bench.js"     "$VM":"$REMOTE_DIR/k6-bench.js"      --zone="$ZONE"
    [ -f "$REFS_LOCAL" ] && gcloud compute scp "$REFS_LOCAL" "$VM":"$REMOTE_DIR/references.json.gz" --zone="$ZONE" || true
    echo "provisioned. Re-login the VM shell once so the docker group applies (or scripts use sudo)."
    ;;

  bench)
    compose="${2:?usage: bench/gcp.sh bench <compose.yml>}"
    gcloud compute scp "$compose" "$VM":"$REMOTE_DIR/docker-compose.yml" --zone="$ZONE"
    ssh_vm "bash $REMOTE_DIR/remote-bench.sh $REMOTE_DIR/docker-compose.yml"
    ;;

  build)
    tag="${2:-rinha-local:bench}"
    # rsync via gcloud-config'd ssh (run `gcloud compute config-ssh` first if needed)
    gcloud compute config-ssh >/dev/null 2>&1 || true
    host="$VM.$ZONE.$PROJECT"
    rsync -az --delete --exclude target --exclude .git --exclude resources/references.json.gz \
      "$REPO_LOCAL/" "$host:$REMOTE_DIR/src/"
    ssh_vm "cd $REMOTE_DIR/src && cp -f $REMOTE_DIR/references.json.gz resources/references.json.gz 2>/dev/null || true; \
            sudo docker build --platform linux/amd64 -t $tag . "
    echo "built $tag on VM — point the compose image: at it and run bench."
    ;;

  ssh)    gcloud compute ssh "$VM" --zone="$ZONE" ;;
  start)  g instances start "$VM" ;;
  stop)   g instances stop "$VM" ;;
  delete) gcloud compute instances delete "$VM" --zone="$ZONE" --quiet ;;
  *) echo "usage: $0 {find-zone|provision|bench <compose.yml>|build [tag]|ssh|start|stop|delete}"; exit 1 ;;
esac
