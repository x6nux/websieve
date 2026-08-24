#!/usr/bin/env bash
# 多会话（多 TCP）条带基准：lossy_relay × {0,2,10}% 丢包 × {1,2,4} 会话。
# 拓扑：e2e_reqwest --SOCKS--> (curl 下载 64MB) ；客户端开 N 个 XHTTP 会话，
# 每个会话一条独立 TCP 经 lossy_relay 到服务端 → 每 lane 独立拥塞窗口。
#
# 用法：scripts/multi-session-bench.sh [文件MB]（默认 64）
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
MB=${1:-64}
WORK=$(mktemp -d /tmp/wsieve-bench.XXXXXX)
SRV_PORT=28443
RELAY_PORT=28080
HTTP_PORT=28081
SOCKS_PORT=21080
PIDS=()
cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
  rm -rf "$WORK"
}
trap cleanup EXIT

echo "== build (release: debug 版本的 mux/加密开销会淹没拥塞窗口信号) =="
cargo build --release -p wsieve-server --example e2e_reqwest --example lossy_relay >/dev/null
cargo build --release -p wsieve-server >/dev/null
curl --version >/dev/null

echo "== keys =="
python3 - "$WORK" <<'EOF'
import sys
from cryptography.hazmat.primitives.asymmetric.x25519 import X25519PrivateKey
w = sys.argv[1]
def gen(name):
    priv = X25519PrivateKey.generate()
    open(f"{w}/{name}.key", "wb").write(priv.private_bytes_raw())
    return priv.public_key().public_bytes_raw()
srv_pub = gen("server")
cli_pub = gen("client")
open(f"{w}/whitelist.txt", "w").write(cli_pub.hex() + "\n")
open(f"{w}/server.pub", "w").write(srv_pub.hex())
EOF
SRV_PUB=$(cat "$WORK/server.pub")
CLI_PRIV=$(xxd -p -c 64 "$WORK/client.key")

echo "== 64MB target =="
dd if=/dev/urandom of="$WORK/file.bin" bs=1048576 count=$MB 2>/dev/null
(cd "$WORK" && python3 -m http.server "$HTTP_PORT" --bind 127.0.0.1 >/dev/null 2>&1) &
PIDS+=($!)

"$ROOT/target/release/wsieve-server" --listen "127.0.0.1:$SRV_PORT" \
  --key-file "$WORK/server.key" --whitelist-file "$WORK/whitelist.txt" \
  --deployment cdn-flexible > "$WORK/server.log" 2>&1 &
PIDS+=($!)
sleep 1

run_one() { # loss sessions
  local loss=$1 sessions=$2
  WSIEVE_LOSSY_UPSTREAM="127.0.0.1:$SRV_PORT" WSIEVE_LOSSY_LISTEN="127.0.0.1:$RELAY_PORT" \
    WSIEVE_LOSS_RTT_MS=80 WSIEVE_LOSS_PCT=$loss \
    "$ROOT/target/release/examples/lossy_relay" > "$WORK/relay.log" 2>&1 &
  local relay_pid=$!
  PIDS+=($relay_pid)
  sleep 0.3
  WSIEVE_E2E_SERVER="http://127.0.0.1:$RELAY_PORT" \
    WSIEVE_E2E_SERVER_PUB="$SRV_PUB" WSIEVE_E2E_CLIENT_PRIV="$CLI_PRIV" \
    WSIEVE_E2E_SOCKS="127.0.0.1:$SOCKS_PORT" WSIEVE_E2E_MUX=smux \
    WSIEVE_EXTRA_SESSIONS=$((sessions - 1)) \
    "$ROOT/target/release/examples/e2e_reqwest" > "$WORK/client.log" 2>&1 &
  local cli_pid=$!
  PIDS+=($cli_pid)
  # 等 SOCKS5 就绪
  for _ in $(seq 1 100); do
    nc -z 127.0.0.1 "$SOCKS_PORT" 2>/dev/null && break
    sleep 0.2
  done
  local t0=$(date +%s.%N)
  curl -s --socks5-hostname "127.0.0.1:$SOCKS_PORT" \
    -o "$WORK/got.bin" "http://127.0.0.1:$HTTP_PORT/file.bin"
  local t1=$(date +%s.%N)
  local ok=FAIL
  cmp -s "$WORK/file.bin" "$WORK/got.bin" && ok=OK
  local secs=$(echo "$t1 $t0" | awk '{printf "%.2f", $1-$2}')
  local mbps=$(echo "$t1 $t0 $MB" | awk '{printf "%.1f", $3/($1-$2)}')
  printf "loss=%s%% sessions=%s  %s  %ss  %s MB/s\n" "$loss" "$sessions" "$ok" "$secs" "$mbps"
  kill $cli_pid $relay_pid 2>/dev/null || true
  sleep 0.5
}

echo "== matrix (RTT=80ms, smux, ${MB}MB) =="
printf "%-8s %-10s %-6s %-8s %-10s\n" loss sessions result secs MB/s
for loss in 0 2 10; do
  for sessions in 1 2 4; do
    run_one $loss $sessions
  done
done
