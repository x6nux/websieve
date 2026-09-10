#!/bin/bash
# 上行压测。用法：bench-up.sh direct|proxy <URL前缀> <并发> <每流字节>
#
# 单独一个脚本是因为上下行的瓶颈根本不在一处：下行要过
# 「服务端 → WebKit 流式响应 → emitter → IPC」，上行要过
# 「Rust → eval 注入 → fetch POST → 服务端」，两条路的批处理策略、
# 拷贝次数、背压点全都不一样。只测下行会让上行的问题完全隐身。
set -u
MODE=$1; BASE=$2; N=$3; BYTES=$4
BASELINE=${BASELINE:-117600000}
APP=$(pgrep -f 'MacOS/wsieve-app' | head -1)

cpu_secs() {
  ps -o cputime= -p "$1" 2>/dev/null | tr -d ' ' \
    | awk -F: '{ if (NF==3) print $1*3600+$2*60+$3; else if (NF==2) print $1*60+$2; else print $1+0 }'
}
WEB=$(ps -eo pid,lstart,command | grep 'WebKit.WebContent' | grep -v grep | sort -k2 -r | head -1 | awk '{print $1}')

PROXY_ARG=""
[ "$MODE" = "proxy" ] && PROXY_ARG="-x http://127.0.0.1:25500"

# 预生成上传体。用文件而不是管道：curl 对管道输入只能 chunked 编码，
# 那会把「上传速率」和「分块编码开销」混在一起量。
SRC=$(mktemp)
dd if=/dev/zero of="$SRC" bs=1m count=$((BYTES / 1048576)) 2>/dev/null

RES=$(mktemp)
C0=$(cpu_secs "${APP:-0}"); W0=$(cpu_secs "${WEB:-0}")
START=$(date +%s.%N)
PIDS=()
for i in $(seq 1 "$N"); do
  ( curl -s -o /dev/null --max-time 180 $PROXY_ARG \
      -X POST --data-binary "@$SRC" \
      -w "%{exitcode} %{size_upload}\n" "${BASE}/__up" >> "$RES" ) &
  PIDS+=($!)
done
for p in "${PIDS[@]}"; do wait "$p"; done
END=$(date +%s.%N)
C1=$(cpu_secs "${APP:-0}"); W1=$(cpu_secs "${WEB:-0}")

EL=$(echo "$END - $START" | bc)
ACTUAL=$(stat -f%z "$SRC" 2>/dev/null || stat -c%s "$SRC")
OK=$(awk -v b="$ACTUAL" '$1==0 && $2==b' "$RES" | wc -l | tr -d ' ')
SENT=$(awk '{s+=$2} END{print s+0}' "$RES")
MBPS=$(echo "scale=1; $SENT/$EL/1000000" | bc)
PCT=$(echo "scale=1; $SENT/$EL*100/$BASELINE" | bc)
CPU=$(echo "scale=1; ($C1-$C0+$W1-$W0)*100/$EL" | bc -l 2>/dev/null | sed 's/^\./0./')

printf "  %-6s %2d流上行: %s/%d  %6.2fs  %7s MB/s  基线%5s%%  CPU %5s%%\n" \
  "$MODE" "$N" "$OK" "$N" "$EL" "$MBPS" "$PCT" "${CPU:-0}"
rm -f "$RES" "$SRC"
