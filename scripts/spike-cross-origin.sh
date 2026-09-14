#!/usr/bin/env bash
# 跨域名会话建立 spike（设计文档 §14 / §15 待实测项 #3；阶段 2 计划 Part A Task 2）。
#
# 问题：单 WebView 承载多出站时，页面加载自域名 A，而向域名 B 发的
# fetch 是跨域名的。它能否完成完整握手并跑数据？
#
# 这条路成立的前提是 sid 走 query 而非 cookie（设计文档 §3.2）——
# 若 sid 在 cookie 里，WKWebView 的 ITP 会丢掉它，握手必然失败。
# 代码事实见 crates/wsieve-xhttp/src/client.rs:133 / :205 / :453
# 与 crates/wsieve-server/src/lib.rs:228 / :250（全走 query）。
#
# 与计划书的一处**有意偏差**：计划书要求 sudo 往 /etc/hosts 加
# wsieve-a.test / wsieve-b.test。本脚本改用公共泛解析回环域名
# localtest.me 与 lvh.me，理由：
#   1) 无需 sudo、不改系统状态，因此没有「跑完忘了清理」的残留风险；
#   2) 二者是不同的 eTLD+1，即真正的 cross-site。ITP 的第三方 cookie
#      拦截按 eTLD+1 划界，故这是比两个 .test 子域**更严格**的条件。
# 若这两个公共域名将来解析不到 127.0.0.1，脚本会直接报错退出（见下）。
#
# 用法：scripts/spike-cross-origin.sh
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD

PORT_A=${WSIEVE_SPIKE_PORT_A:-18081}
PORT_B=${WSIEVE_SPIKE_PORT_B:-18082}
HOST_A=localtest.me
HOST_B=lvh.me
WORK=$(mktemp -d /tmp/wsieve-spike.XXXXXX)
PIDS=()

cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
  rm -rf "$WORK"
}
trap cleanup EXIT

fail() { echo "SPIKE FAIL: $*" >&2; exit 1; }

echo "== [0/4] 前置检查 =="
[ "$(uname)" = "Darwin" ] || fail "本 spike 测的是 WKWebView，只能在 macOS 上跑"
command -v swiftc >/dev/null || fail "缺 swiftc（WebView 驱动需要）"

# 两个域名必须都经**系统解析器**解析到 127.0.0.1。用 dscacheutil 而非 dig：
# dig 直接问 DNS 服务器，绕过了 /etc/hosts 与系统缓存，与 WebKit 实际走的
# 路径不一致。
for h in "$HOST_A" "$HOST_B"; do
  ip=$(dscacheutil -q host -a name "$h" 2>/dev/null | awk '/^ip_address:/{print $2; exit}')
  [ "$ip" = "127.0.0.1" ] || fail "$h 解析到 '${ip:-空}'，期望 127.0.0.1（需要网络；或自行在 /etc/hosts 加两行）"
  echo "   $h -> 127.0.0.1 ✅"
done

echo "== [1/4] 构建 =="
cargo build -p wsieve-server --example cross_origin_spike 2>&1 | tail -3
swiftc -O "$ROOT/scripts/spike-wkdriver.swift" -o "$WORK/wkdriver" || fail "WebView 驱动编译失败"

echo "== [2/4] 起 spike（两个服务端 + echo + 桥）=="
WSIEVE_SPIKE_PORT_A=$PORT_A WSIEVE_SPIKE_PORT_B=$PORT_B \
  "$ROOT/target/debug/examples/cross_origin_spike" > "$WORK/spike.log" 2>&1 &
SPIKE_PID=$!
PIDS+=($SPIKE_PID)

# 等驱动页可取（服务端起来了）
for _ in $(seq 1 60); do
  curl -s -o /dev/null "http://$HOST_A:$PORT_A/__spike/driver" && break
  sleep 0.25
done
curl -s -o /dev/null "http://$HOST_A:$PORT_A/__spike/driver" || {
  cat "$WORK/spike.log"; fail "spike 服务端未起来"; }

echo "== [3/4] 起真实 WKWebView（只建一个 —— 这就是 shared 形态）=="
SPIKE_DRIVER_URL="http://$HOST_A:$PORT_A/__spike/driver" SPIKE_SECONDS=120 \
  "$WORK/wkdriver" > "$WORK/wkdriver.log" 2>&1 &
WK_PID=$!
PIDS+=($WK_PID)

echo "== [4/4] 等待判定 =="
# spike 进程跑完三步就自己退出；轮询它的退出码。
RC=""
for _ in $(seq 1 180); do
  if ! kill -0 $SPIKE_PID 2>/dev/null; then
    wait $SPIKE_PID && RC=0 || RC=$?
    break
  fi
  sleep 1
done

# 内存实测：WebView 相关进程（WKWebView 是多进程架构，网络与渲染各一个）
echo
echo "-- WebView 进程内存（RSS KB）--"
ps -A -o rss=,comm= 2>/dev/null | grep -Ei "wkdriver|WebKit|com\.apple\.WebKit" | sort -rn | head -10 \
  || echo "   （未采集到）"
WK_TOTAL=$(ps -A -o rss=,comm= 2>/dev/null | grep -Ei "wkdriver|com\.apple\.WebKit" | awk '{s+=$1} END{print s+0}')
echo "   合计约 ${WK_TOTAL} KB（单 WebView 承载 2 个出站）"

echo
echo "===== spike 输出 ====="
cat "$WORK/spike.log"
echo
echo "===== WebView 驱动日志（尾部）====="
tail -25 "$WORK/wkdriver.log" || true

if [ -z "$RC" ]; then
  fail "spike 未在时限内给出判定（见上方日志）"
fi
echo
echo "spike 退出码 = $RC"
case "$RC" in
  0) echo "判定：carrier: shared 成立 —— 单 WebView 同时承载了两个 cross-site 出站" ;;
  3) echo "判定：carrier: shared 不成立（跨域名握手失败）。按计划书改默认为 isolated，不要硬推" ;;
  *) echo "判定：spike 未通过（退出码 $RC），原因见日志" ;;
esac
exit "$RC"
