#!/usr/bin/env bash
# Spike 编排：普通 http 本地 origin 下，Tauri 的 raw body 快路径可不可用？
#
# 退出码：0 = 可用（方案成立）；3 = 不可用（方案作废）；4 = spike 本身无效
# （对照组也通过了，说明探针没复现出 Tauri 的限制，结论不可采信）。
#
# 见 scripts/spike-ipc-origin.swift 顶部的完整背景。
set -uo pipefail
cd "$(dirname "$0")/.."

PORT="${PROBE_PORT:-18099}"
TMP="$(mktemp -d)"
BIN="$TMP/probe"
trap 'kill "${SRV_PID:-0}" 2>/dev/null; rm -rf "$TMP"' EXIT

echo "== 编译探针 =="
swiftc -O scripts/spike-ipc-origin.swift -o "$BIN" || { echo "swiftc 失败"; exit 2; }

# 实验组的页面。内容无关紧要——被测的是 origin，不是页面。
mkdir -p "$TMP/www"
printf '<!doctype html><meta charset=utf-8><title>probe</title><body>probe</body>' \
  > "$TMP/www/index.html"
python3 -m http.server "$PORT" --bind 127.0.0.1 --directory "$TMP/www" >/dev/null 2>&1 &
SRV_PID=$!
sleep 1
curl -sf "http://127.0.0.1:$PORT/" >/dev/null || { echo "本地 server 没起来"; exit 2; }

run() { # run <名称> <URL>
  echo
  echo "== $1: $2 =="
  PROBE_URL="$2" PROBE_SECONDS=20 "$BIN" 2>&1 | tee "$TMP/$1.log" >/dev/null
  grep -hE '^PROBE_' "$TMP/$1.log" || echo "PROBE_RESULT <无输出>"
}
ipc()   { grep -hm1 '^PROBE_RESULT' "$TMP/$1.log" 2>/dev/null || echo "<无输出>"; }
mixed() { grep -hm1 '^PROBE_MIXED'  "$TMP/$1.log" 2>/dev/null || echo "<无输出>"; }

# 对照组在前：先确认这个 spike 真的能测出失败，再看实验组的结果。
run control "${CONTROL_URL:-https://example.com/}"
run subject "http://127.0.0.1:$PORT/"

echo
echo "=================== 判定 ==================="
echo "对照组 远程 https origin"
echo "  ipc raw : $(ipc control)"
echo "实验组 本地 http  origin"
echo "  ipc raw : $(ipc subject)"
echo "  →https  : $(mixed subject)"
echo "============================================"

case "$(ipc control)" in
  *ok*)
    echo "结论：spike 无效 —— 对照组本应失败却通过了，探针没能复现 Tauri 的限制。"
    exit 4 ;;
esac

# 两问都要通过。只有 raw 可用但数据面发不出 https，方案同样不成立
# —— 数据面必须留在 https 才有真实 TLS 指纹，那是本项目的立身之本。
case "$(ipc subject):$(mixed subject)" in
  *ok*:*ok*)
    echo "结论：http origin 既能走 raw 快路径、又能 fetch https 端点 → 方案成立。"
    exit 0 ;;
  *ok*:*)
    echo "结论：raw 可用，但 http 页面 fetch 不了 https 端点 → 方案作废"
    echo "      （数据面若被迫降到 http，TLS 指纹就没了，那是整个项目的前提）。"
    exit 3 ;;
  *)
    echo "结论：http origin 同样走不了 raw → 方案作废，base64 保留。"
    exit 3 ;;
esac
