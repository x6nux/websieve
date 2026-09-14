#!/usr/bin/env bash
# Task 18 E2E 验收：websieve 全栈（webview 之外）。
#
# 阶段 A（自动化，本脚本的主体，必须通过）：
#   python http.server 目标 + 真实 wsieve-server 二进制（plain HTTP，--deployment cdn-flexible）
#   + 真实客户端（Noise/XhttpConn/mux/SOCKS5，ReqwestTransport 顶替 WebView fetch）
#   + curl 经 SOCKS5 隧道取回已知文件并逐字节比对。
#
# 阶段 B（可选，需要 GUI/window server）：真实 Tauri app。传 --with-app 开启；
#   无窗口会话时会失败——这就是「需要人在本地跑」的部分，脚本会如实报告。
#
# 用法：scripts/e2e.sh [--with-app]
set -euo pipefail
cd "$(dirname "$0")/.."

ROOT=$PWD
WORK=$(mktemp -d /tmp/wsieve-e2e.XXXXXX)
SRV_PORT=18443
HTTP_PORT=18080
SOCKS_PORT=11080
PIDS=()

cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
  rm -rf "$WORK"
}
trap cleanup EXIT

fail() { echo "E2E FAIL: $*" >&2; exit 1; }

echo "== [0/5] build =="
cargo build -p wsieve-server || fail "server build"
cargo build -p wsieve-server --example e2e_reqwest || fail "example build"
curl --version >/dev/null || fail "curl missing"

echo "== [1/5] ephemeral keys =="
# X25519 静态密钥对：服务端 32B 裸私钥文件 + 客户端 priv/pub；
# 公钥派生必须与 wsieve-proto::crypto::gen_keypair 一致（X25519）。
python3 - "$WORK" <<'EOF' || fail "keygen"
import os, sys
from cryptography.hazmat.primitives.asymmetric.x25519 import X25519PrivateKey
w = sys.argv[1]
def gen(name):
    priv = X25519PrivateKey.generate()
    open(f"{w}/{name}.key", "wb").write(priv.private_bytes_raw())
    return priv.public_key().public_bytes_raw()
srv_pub = gen("server")
cli_pub = gen("client")
open(f"{w}/whitelist.txt", "w").write(cli_pub.hex() + "\n")
open(f"{w}/server.pub", "w").write(srv_pub.hex() + "\n")
EOF
SRV_PUB=$(cat "$WORK/server.pub")
CLI_PRIV=$(xxd -p -c 64 "$WORK/client.key")

echo "== [2/5] local HTTP target =="
printf 'e2e-acceptance-body-v1' > "$WORK/test.txt"
(cd "$WORK" && python3 -m http.server "$HTTP_PORT" --bind 127.0.0.1 >/dev/null 2>&1) &
PIDS+=($!)

echo "== [3/5] wsieve-server (plain HTTP, cdn-flexible) =="
RUST_LOG=info "$ROOT/target/debug/wsieve-server" \
  --listen "127.0.0.1:$SRV_PORT" \
  --key-file "$WORK/server.key" \
  --whitelist-file "$WORK/whitelist.txt" \
  --deployment cdn-flexible > "$WORK/server.log" 2>&1 &
PIDS+=($!)
for i in $(seq 1 50); do
  curl -s -o /dev/null "http://127.0.0.1:$SRV_PORT/" && break
  sleep 0.2
done

echo "== [4/5] full-stack client + SOCKS5 (webview 之外的一切) =="
# e2e_reqwest 示例：真实 Noise 握手 + mux + SOCKS5 :11080 + duplex 桥接
# （与 src-tauri/src/proxy.rs 同一路径），唯一差别是 fetch 走 reqwest。
WSIEVE_E2E_SERVER="http://127.0.0.1:$SRV_PORT" \
WSIEVE_E2E_SERVER_PUB="$SRV_PUB" \
WSIEVE_E2E_CLIENT_PRIV="$CLI_PRIV" \
WSIEVE_E2E_SOCKS="127.0.0.1:$SOCKS_PORT" \
  "$ROOT/target/debug/examples/e2e_reqwest" > "$WORK/client.log" 2>&1 &
PIDS+=($!)
for i in $(seq 1 50); do
  nc -z 127.0.0.1 "$SOCKS_PORT" 2>/dev/null && break
  sleep 0.2
done
nc -z 127.0.0.1 "$SOCKS_PORT" 2>/dev/null || { tail -20 "$WORK/client.log"; fail "SOCKS5 not up"; }

echo "== [5/5] curl through the tunnel =="
BODY=$(curl -s --max-time 20 --socks5-hostname "127.0.0.1:$SOCKS_PORT" \
  "http://127.0.0.1:$HTTP_PORT/test.txt") || fail "curl through proxy"
[ "$BODY" = "e2e-acceptance-body-v1" ] || fail "body mismatch: '$BODY'"
echo "PASS: curl -> SOCKS5 -> mux -> Noise -> HTTP -> target, body verified"

# ---- 阶段 B：真实 Tauri app（需要 GUI）----
if [ "${1:-}" = "--with-app" ]; then
  echo "== [B] launching real Tauri app (needs window server) =="
  cargo build --manifest-path src-tauri/Cargo.toml || fail "app build"
  WSIEVE_SERVER_URL="http://127.0.0.1:$SRV_PORT/" \
  WSIEVE_SERVER_PUB="$SRV_PUB" \
  WSIEVE_CLIENT_PRIV="$CLI_PRIV" \
  WSIEVE_SOCKS="127.0.0.1:11081" \
    "$ROOT/src-tauri/target/debug/wsieve-app" > "$WORK/app.log" 2>&1 &
  APP_PID=$!
  # 首次请求偶发空响应（会话刚建立时 mux 首流窗口未就绪），重试带间隔
  B2=""
  for i in $(seq 1 60); do
    sleep 1
    B2=$(curl -s --max-time 6 --socks5-hostname 127.0.0.1:11081 \
      "http://127.0.0.1:$HTTP_PORT/test.txt" || true)
    [ "$B2" = "e2e-acceptance-body-v1" ] && break
  done
  # 阶段 4：控制窗口共存断言。
  # 命题是「控制窗口的存在不破坏传输」—— 两个窗口共享事件循环与
  # WKWebsiteDataStore，控制窗口的 JS 阻塞主线程就会拖死传输的 fetch。
  #
  # 上面的 B2 已经证明隧道通了；这里再取一次，确认控制窗口完成加载与
  # 首次 IPC 之后隧道**仍然**通（而不是只在控制窗口 JS 跑起来之前通）。
  sleep 3
  B3=$(curl -s --max-time 8 --socks5-hostname 127.0.0.1:11081 \
    "http://127.0.0.1:$HTTP_PORT/test.txt" || true)
  [ "$B3" = "e2e-acceptance-body-v1" ] || {
    tail -40 "$WORK/app.log"
    fail "控制窗口起来之后隧道断了 —— 双窗口互相干扰"
  }
  echo "PASS: 双窗口共存，控制窗口不影响传输"
  kill $APP_PID 2>/dev/null || true
  [ "$B2" = "e2e-acceptance-body-v1" ] || { tail -30 "$WORK/app.log"; fail "app-mode proxy failed"; }
  echo "PASS: real Tauri app tunnel verified"
else
  echo "NOTE: webview 阶段（真实 Tauri app）未运行——需要 GUI 会话。"
  echo "      本地运行: scripts/e2e.sh --with-app"
fi

echo "E2E DONE"
