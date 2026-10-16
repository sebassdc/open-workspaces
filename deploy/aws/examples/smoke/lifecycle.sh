#!/bin/bash
# Runs on the host (as root, through SSM): create, connect, snapshot, fork,
# live fork, hibernate/resume, stop and delete, timing each step. Prints one JSON report.
set -euo pipefail
samples=${OW_SAMPLES:-5}
# Samples delete what they create, so the worker's record caps do not bound them.
(( samples >= 1 && samples <= 30 )) || { echo "OW_SAMPLES must be 1-30"; exit 2; }
run=$(mktemp -d /tmp/ow-smoke.XXXXXX)
log="$run/timings"
ow() { /usr/local/bin/ow "$@"; }

timed() { # timed NAME COMMAND...: run, append "NAME ms"
  local name=$1 start end
  shift
  start=$(date +%s%N)
  "$@" > "$run/last" 2>&1 || { echo "step $name failed:"; cat "$run/last"; return 1; }
  end=$(date +%s%N)
  echo "$name $(( (end - start) / 1000 ))" >> "$log"
}

# A guest process whose survival proves RAM state carried over.
ticker='( sh -c '\''echo $$ > /tmp/ticker.pid; i=0; while :; do i=$((i+1)); echo $i > /tmp/ticker; sleep 1; done'\'' </dev/null >/dev/null 2>&1 & )'
alive() { # alive MACHINE: ticker process still running and still counting
  local before after
  before=$(ow exec "$1" -- 'kill -0 "$(cat /tmp/ticker.pid)" && cat /tmp/ticker')
  sleep 2
  after=$(ow exec "$1" -- 'kill -0 "$(cat /tmp/ticker.pid)" && cat /tmp/ticker')
  [[ "${after##*$'\n'}" -gt "${before##*$'\n'}" ]] || { echo "ticker stopped in $1: $before -> $after"; return 1; }
}

http_code() { curl -s -o /dev/null -m 5 -w '%{http_code}' "$1" || true; }

gone() { # gone MACHINE...: no record and no snapshot of these machines remains
  ow list | python3 -c 'import json,sys; ids=set(sys.argv[1:]); left=[w["id"] for w in json.load(sys.stdin) if w["id"] in ids]; sys.exit(f"still listed: {left}" if left else 0)' "$@"
  ow snapshots | python3 -c 'import json,sys; ids=set(sys.argv[1:]); left=[s["name"] for s in json.load(sys.stdin) if s["workspace"] in ids]; sys.exit(f"snapshots left: {left}" if left else 0)' "$@"
}

# Controller first: it must be running and see its local runner online.
controller=$(ow node list)
python3 -c 'import json,sys; nodes=json.loads(sys.argv[1]); sys.exit(0 if any(n["online"] for n in nodes) else "controller reports no runner online")' "$controller"
ow status > /dev/null
for i in $(seq "$samples"); do
  box="smoke-$i-$RANDOM"
  timed create_to_exec sh -c "/usr/local/bin/ow create $box && /usr/local/bin/ow exec $box -- true"
  timed connect_exec ow exec "$box" -- 'uname -r'
  # A refused delete must leave the machine and its HTTP publication alone.
  url=$(ow publish "$box")
  ! ow delete "$box" > /dev/null 2>&1 || { echo "delete of running $box was accepted"; exit 1; }
  sleep 1
  [[ "$(http_code "$url")" != 000 ]] || { echo "refused delete closed the publication of $box"; exit 1; }
  ow exec "$box" -- "$ticker" > /dev/null
  timed snapshot ow snapshot "$box" "$box-snap"
  timed fork_from_snapshot ow fork "$box" "$box-f" --snapshot "$box-snap"
  timed live_fork ow fork "$box" "$box-l"
  alive "$box-f"
  alive "$box-l"
  timed hibernate ow hibernate "$box-l"
  timed resume ow start "$box-l"
  alive "$box-l"
  alive "$box"
  timed stop ow stop "$box"
  timed stop ow stop "$box-l"
  # Delete the parent (and its snapshots) while its snapshot fork still runs.
  timed delete ow delete "$box"
  for _ in $(seq 10); do [[ "$(http_code "$url")" == 000 ]] && break; sleep 0.5; done
  [[ "$(http_code "$url")" == 000 ]] || { echo "publication of deleted $box still answers"; exit 1; }
  alive "$box-f"
  timed stop ow stop "$box-f"
  timed delete ow delete "$box-f"
  timed delete ow delete "$box-l"
  gone "$box" "$box-f" "$box-l"
  echo "sample $i passed" >&2
done

python3 - "$log" "$samples" <<'REPORT'
import json, math, os, platform, subprocess, sys
rows = [line.split() for line in open(sys.argv[1])]
metrics = {}
for name in dict.fromkeys(n for n, _ in rows):
    values = sorted(int(v) / 1000 for n, v in rows if n == name)
    pick = lambda q: round(values[max(0, math.ceil(len(values) * q) - 1)], 1)
    metrics[name] = {"samples": len(values), "p50_ms": pick(0.5), "p95_ms": pick(0.95)}
def sh(cmd):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True).stdout.strip()
print(json.dumps({
    "samples": int(sys.argv[2]),
    "percentile_method": "nearest rank",
    "kernel": platform.release(),
    "cpus": os.cpu_count(),
    "memory_kib": sh("awk '/MemTotal/{print $2}' /proc/meminfo"),
    "commit": sh("runuser -u ow -- git -C /srv/ow/src rev-parse HEAD"),
    "firecracker": sh("/srv/ow/src/data/runtime-spike/official/release-v1.17.0-x86_64/firecracker-v1.17.0-x86_64 --version | head -1"),
    "controller": {"service": sh("systemctl is-active open-workspaces"),
                   "runners": [{"id": n["id"], "online": n["online"]} for n in json.loads(sh("/usr/local/bin/ow node list"))]},
    "metrics": metrics,
}))
REPORT
