#!/usr/bin/env bash
# VM-side launcher. Usage:
#   run.sh val                      -> detached single baseline bench -> val.log / val-results.tsv
#   run.sh start <exp.tsv> [rep]    -> detached full sweep -> sweep.log / sweep-results.tsv
#   run.sh status                   -> tail logs + results + running?
#   run.sh stop                     -> kill any running sweep-runner/remote-bench/k6
set -uo pipefail
cd "$HOME/rinha"
export K6_CPUS="${K6_CPUS:-2,3,6,7}"   # keep k6 off stack cores 0,1
TAB=$(printf "\t")
case "${1:-}" in
  val)
    printf "baseline${TAB}\n" > val.tsv
    rm -f val.log val-results.tsv
    OUT="$HOME/rinha/val-results.tsv" REPEATS=1 nohup bash sweep-runner.sh val.tsv > val.log 2>&1 &
    echo "VAL launched pid=$!"
    ;;
  start)
    exp="${2:-experiments.tsv}"; rep="${3:-3}"
    rm -f sweep.log
    REPEATS="$rep" nohup bash sweep-runner.sh "$exp" > sweep.log 2>&1 &
    echo "SWEEP launched pid=$! exp=$exp REPEATS=$rep"
    ;;
  status)
    echo "=== running? ==="; pgrep -af "sweep-runner.sh|remote-bench.sh|k6 run" || echo "(nothing running)"
    echo "=== sweep.log tail ==="; tail -12 sweep.log 2>/dev/null || echo none
    echo "=== val.log tail ==="; tail -12 val.log 2>/dev/null || echo none
    echo "=== sweep-results.tsv ==="; column -t -s"$TAB" sweep-results.tsv 2>/dev/null || echo none
    echo "=== val-results.tsv ==="; column -t -s"$TAB" val-results.tsv 2>/dev/null || echo none
    ;;
  stop)
    pkill -f "sweep-runner.sh" 2>/dev/null; pkill -f "remote-bench.sh" 2>/dev/null; pkill -f "k6 run" 2>/dev/null
    sudo docker ps -aq | xargs -r sudo docker rm -f >/dev/null 2>&1 || true
    echo "stopped + cleaned"
    ;;
  *) echo "usage: run.sh {val|start <exp> [rep]|status|stop}";;
esac
