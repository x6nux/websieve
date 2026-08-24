#!/usr/bin/env bash
# TCP 归属测试：验证「N 个 XHTTP 会话是否真的落在 N 条独立 TCP 上」。
#
# 多会话条带（aria2 效应）的全部收益都来自「每会话一条 TCP ⇒ 独立拥塞
# 窗口」。客户端是否真开新 TCP 取决于它的 HTTP 栈，客户端侧看不出来——
# 只有服务端前面的 socket 能看出。本脚本在服务端前插 tcp_probe_relay
# 记账，跑一次真实下载，然后给出 verdict。
#
#   reqwest 模式：每个会话一个独立 reqwest::Client → 预期 INDEPENDENT
#   app 模式：真实 Tauri app，全部会话共用一个 WebViewTransport（同一
#             WKWebView、同一 origin）→ NSURLSession 连接池行为待测；
#             若协商 h2 则预期 FULLY-MULTIPLEXED（条带收益归零）
#
# 用法：scripts/tcp-attribution.sh [reqwest|app] [会话数]
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
MODE=${1:-reqwest}
SESSIONS=${2:-4}
WORK=$(mktemp -d /tmp/wsieve-attr.XXXXXX)
SRV_PORT=29443
PROBE_PORT=29080
HTTP_PORT=29081
SOCKS_PORT=22080
PIDS=()
cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
  rm -rf "$WORK"
}
trap cleanup EXIT

# 端口守卫：残留进程会静默顶替本次实例，得出的归属映射是上一轮的。
for port in $SRV_PORT $PROBE_PORT $HTTP_PORT $SOCKS_PORT 11081; do
  if lsof -nP -iTCP:$port -sTCP:LISTEN >/dev/null 2>&1; then
    echo "FATAL: 端口 $port 被占用（残留进程？）——先清干净，否则数据是假的" >&2
    lsof -nP -iTCP:$port -sTCP:LISTEN >&2
    exit 1
  fi
done

echo "== build =="
cargo build --release -p wsieve-server --example e2e_reqwest --example tcp_probe_relay >/dev/null
cargo build --release -p wsieve-server >/dev/null

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

# 目标文件：要足够大，让条带真正升级出 lane（否则测的是握手期的 TCP 数）
dd if=/dev/urandom of="$WORK/file.bin" bs=1048576 count=16 2>/dev/null
(cd "$WORK" && python3 -m http.server "$HTTP_PORT" --bind 127.0.0.1 >/dev/null 2>&1) &
PIDS+=($!)

"$ROOT/target/release/wsieve-server" --listen "127.0.0.1:$SRV_PORT" \
  --key-file "$WORK/server.key" --whitelist-file "$WORK/whitelist.txt" \
  --deployment cdn-flexible > "$WORK/server.log" 2>&1 &
PIDS+=($!)

WSIEVE_PROBE_UPSTREAM="127.0.0.1:$SRV_PORT" WSIEVE_PROBE_LISTEN="127.0.0.1:$PROBE_PORT" \
  WSIEVE_PROBE_INTERVAL=3 \
  "$ROOT/target/release/examples/tcp_probe_relay" > "$WORK/probe.log" 2>&1 &
PROBE_PID=$!
PIDS+=($PROBE_PID)
sleep 1

# 条带微阈值：lane 必须真升级，否则只有 1 条 lane、测不出多 TCP 意图
export WSIEVE_STRIPE_LANES=4
export WSIEVE_STRIPE_UPGRADE_BYTES=65536
export WSIEVE_STRIPE_UPGRADE_RATE_BPS=1
export WSIEVE_STRIPE_UPGRADE_WINDOW_MS=1
export WSIEVE_EXTRA_SESSIONS=$((SESSIONS - 1))

case "$MODE" in
  reqwest)
    echo "== client: e2e_reqwest ($SESSIONS 会话) =="
    WSIEVE_E2E_SERVER="http://127.0.0.1:$PROBE_PORT" \
      WSIEVE_E2E_SERVER_PUB="$SRV_PUB" WSIEVE_E2E_CLIENT_PRIV="$CLI_PRIV" \
      WSIEVE_E2E_SOCKS="127.0.0.1:$SOCKS_PORT" WSIEVE_E2E_MUX=smux \
      "$ROOT/target/release/examples/e2e_reqwest" > "$WORK/client.log" 2>&1 &
    PIDS+=($!)
    USE_SOCKS=$SOCKS_PORT
    ;;
  app)
    echo "== client: 真实 Tauri app ($SESSIONS 会话，需要 GUI 会话) =="
    cargo build --manifest-path src-tauri/Cargo.toml --release >/dev/null || {
      echo "FATAL: app build 失败" >&2; exit 1; }
    WSIEVE_SERVER_URL="http://127.0.0.1:$PROBE_PORT/" \
      WSIEVE_SERVER_PUB="$SRV_PUB" WSIEVE_CLIENT_PRIV="$CLI_PRIV" \
      WSIEVE_SOCKS="127.0.0.1:11081" \
      "$ROOT/src-tauri/target/release/wsieve-app" > "$WORK/app.log" 2>&1 &
    PIDS+=($!)
    USE_SOCKS=11081
    ;;
  *) echo "未知模式 '$MODE'（reqwest|app）" >&2; exit 1 ;;
esac

echo "== 等 SOCKS5 就绪 =="
for _ in $(seq 1 120); do
  nc -z 127.0.0.1 "$USE_SOCKS" 2>/dev/null && break
  sleep 0.5
done
nc -z 127.0.0.1 "$USE_SOCKS" 2>/dev/null || {
  echo "FATAL: SOCKS5 未就绪" >&2
  tail -30 "$WORK/${MODE}.log" 2>/dev/null || tail -30 "$WORK/client.log"
  exit 1
}

echo "== 跑一次 16MB 下载 =="
OK=FAIL
if curl -s --max-time 120 --socks5-hostname "127.0.0.1:$USE_SOCKS" \
     -o "$WORK/got.bin" "http://127.0.0.1:$HTTP_PORT/file.bin"; then
  cmp -s "$WORK/file.bin" "$WORK/got.bin" && OK=OK
fi
echo "下载结果: $OK ($(wc -c < "$WORK/got.bin" 2>/dev/null || echo 0) 字节)"
sleep 4  # 等探针打出最后一次汇总

echo
echo "== TCP 归属明细 =="
grep -E "^TCP#" "$WORK/probe.log" || echo "(无)"
echo
echo "== 结论 =="
tail -1 <(grep -E "^SUMMARY" "$WORK/probe.log") || echo "(无汇总)"
if [ "$OK" != OK ]; then
  echo "警告: 下载未通过校验，归属数据可能不完整"
  # SOCKS 端口在会话建立前就已监听（proxy.rs 先起 serve 任务），nc -z 会给出
  # 假就绪；失败时必须看客户端日志才知道是握手挂了还是隧道挂了。
  echo "--- 客户端日志尾部 ---"
  tail -40 "$WORK/app.log" 2>/dev/null || tail -40 "$WORK/client.log" 2>/dev/null || echo "(无)"
  echo "--- 服务端日志尾部 ---"
  tail -20 "$WORK/server.log" 2>/dev/null || echo "(无)"
fi
