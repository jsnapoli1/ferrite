#!/bin/bash
# CPU cost of the SFU's forwarding: a fresh server, a call of PEOPLE browsers (e2e/callfps.mjs)
# and, over a steady WINDOW seconds, the CPU time of the server's "ferrite-sfu" thread against
# what it forwarded (from /api/admin/stats). The SFU forwards whole media frames (str0m
# depacketizes and repacketizes them), so the counts are frames and audio packets written to
# receivers, and the bytes are media payload. The thread's CPU time covers everything the SFU
# does: receiving, decrypting and reassembling every layer from every sender, choosing
# layers, and encrypting and sending each copy.
#
#   BIN=target/release/ferrite PEOPLE=3 RES=720p FPS=30 [CAMERA=cam.y4m] [VIEWPORT=2000x1300] \
#     bash docs/bench/sfu-cost.sh
set -euo pipefail
cd "$(dirname "$0")/../.."
BIN=${BIN:-target/release/ferrite}; PEOPLE=${PEOPLE:-3}; WINDOW=${WINDOW:-15}
PORT=${PORT:-8890}; URL=http://127.0.0.1:$PORT
D=$(mktemp -d); trap 'kill $SRV 2>/dev/null; rm -rf "$D"' EXIT
FERRITE_SFU=1 FERRITE_DATA_DIR=$D/data FERRITE_LISTEN=127.0.0.1:$PORT FERRITE_PUBLIC_URL=$URL \
  FERRITE_SFU_UDP=127.0.0.1:1$PORT FERRITE_CALL_RESOLUTION=${RES:-720p} FERRITE_CALL_FPS=${FPS:-30} \
  "$BIN" serve > "$D/server.log" 2>&1 &
SRV=$!
for _ in $(seq 1 100); do curl -s -o /dev/null $URL/api/status && break; sleep 0.1; done
(cd e2e && FERRITE_URL=$URL PEOPLE=$PEOPLE SECONDS=$((WINDOW + 25)) WARMUP=2 VIEWPORT=${VIEWPORT:-2000x1300} \
   timeout $((WINDOW + 120)) node callfps.mjs > "$D/probe.json" 2>&1) &
PROBE=$!
J=$D/cookies
for _ in $(seq 1 100); do
  curl -s -c $J -H 'X-Ferrite-Request: 1' -H 'Content-Type: application/json' \
    -d '{"username":"p0","password":"p0-password"}' $URL/api/auth/login -o /dev/null -w '%{http_code}' | grep -q 200 && break
  sleep 0.3
done
sample() { # ticks of the SFU thread, packets, bytes
  local tid; tid=$(grep -l '^ferrite-sfu$' /proc/$SRV/task/*/comm | head -1 | xargs dirname)
  local ticks; ticks=$(awk '{print $14+$15}' "$tid/stat")
  local st; st=$(curl -s -b $J -H 'X-Ferrite-Request: 1' $URL/api/admin/stats)
  echo "$ticks $(echo "$st" | python3 -I -c 'import json,sys; s=json.load(sys.stdin)["sfu"]; print(s["packets_forwarded"], s["bytes_forwarded"], s["clients"])')"
}
# Wait until everyone is connected and forwarding, then let bandwidth estimates settle.
for _ in $(seq 1 120); do read -r _ _ _ c <<<"$(sample)"; [ "$c" -ge "$PEOPLE" ] && break; sleep 0.5; done
sleep 10
read -r t0 p0 b0 _ <<<"$(sample)"
sleep "$WINDOW"
read -r t1 p1 b1 _ <<<"$(sample)"
hz=$(getconf CLK_TCK)
wait $PROBE || true
python3 -I - "$t0" "$t1" "$p0" "$p1" "$b0" "$b1" "$hz" "$WINDOW" "$D/probe.json" <<'PYEOF'
import json, sys
t0, t1, p0, p1, b0, b1, hz, w = (float(x) for x in sys.argv[1:9])
cpu = (t1 - t0) / hz
pk, by = p1 - p0, b1 - b0
txt = open(sys.argv[9]).read()
worst = None
try:
    worst = json.loads(txt[txt.index('{'):])["worst_receiver_fps"]
except Exception:
    pass
print(json.dumps({
    "window_s": w, "sfu_thread_cpu_s": round(cpu, 2), "sfu_thread_core_share": round(cpu / w, 3),
    "media_writes_per_s": round(pk / w), "mbit_forwarded_per_s": round(by * 8 / w / 1e6, 1),
    "us_cpu_per_write": round(cpu / pk * 1e6, 2) if pk else None,
    "forwarded_mbit_per_s_per_core": round(by * 8 / cpu / 1e6) if cpu else None,
    "worst_receiver_fps": worst,
}, indent=1))
PYEOF
