#!/bin/bash
# 局域网吞吐基准。用法：
#   bench.sh direct <URL前缀> <流数> <每流字节>     不经代理
#   bench.sh proxy  <URL前缀> <流数> <每流字节>     经 127.0.0.1:25500
#
# 输出：完整流数 / 墙钟 / 聚合 MB/s / 占基线百分比 / 客户端 CPU 峰值+均值
#
# CPU 采样与下载**并行**，且只 wait 下载的 pid——曾经把采样子进程也 wait
# 进去，墙钟被拉长到采样时长，聚合吞吐直接算错一个数量级。
#
# CPU 用**累计 CPU 时间的差值**算，不用 `ps -o %cpu`。后者在 macOS 上是
# "累计 CPU 时间 ÷ 进程存活时长"的平均值，不是瞬时占用：一个刚重启的进程
# 里混着大段空闲时间，读数会被稀释；一个跑了几小时的转发进程更是永远显示
# 0.0%。差值法量的是"这次传输真正烧掉多少 CPU 秒"，与进程活了多久无关。
set -u
MODE=$1; BASE=$2; N=$3; BYTES=$4
BASELINE=${BASELINE:-117600000}          # 直连实测线速，用于算百分比
APP=$(pgrep -f 'MacOS/wsieve-app' | head -1)

RES=$(mktemp); CPU=$(mktemp)
PROXY_ARG=""
[ "$MODE" = "proxy" ] && PROXY_ARG="-x http://127.0.0.1:25500"

# 把 `ps` 的 MM:SS.ss / HH:MM:SS.ss 累计 CPU 时间读成秒
cpu_secs() {
  ps -o cputime= -p "$1" 2>/dev/null | tr -d ' ' \
    | awk -F: '{ if (NF==3) print $1*3600+$2*60+$3; else if (NF==2) print $1*60+$2; else print $1+0 }'
}
# 承载页所在的 WebContent 进程（每个 tab 一个，取最近启动的那个）
WEB=$(ps -eo pid,lstart,command | grep 'WebKit.WebContent' | grep -v grep | sort -k2 -r | head -1 | awk '{print $1}')

C0=$(cpu_secs "${APP:-0}"); W0=$(cpu_secs "${WEB:-0}")

PIDS=()
START=$(date +%s.%N)
for i in $(seq 1 "$N"); do
  ( curl -o /dev/null -s --max-time 180 -w "%{exitcode} %{size_download}\n" \
      $PROXY_ARG "${BASE}/__down?bytes=${BYTES}" >> "$RES" ) &
  PIDS+=($!)
done
for p in "${PIDS[@]}"; do wait "$p"; done
END=$(date +%s.%N)
C1=$(cpu_secs "${APP:-0}"); W1=$(cpu_secs "${WEB:-0}")

EL=$(echo "$END - $START" | bc)
OK=$(awk -v b="$BYTES" '$1==0 && $2==b' "$RES" | wc -l | tr -d ' ')
GOT=$(awk '{s+=$2} END{print s+0}' "$RES")
MBPS=$(echo "scale=1; $GOT/$EL/1000000" | bc)
PCT=$(echo "scale=1; $GOT/$EL*100/$BASELINE" | bc)
APPC=$(echo "scale=1; ($C1-$C0)*100/$EL" | bc -l 2>/dev/null | sed 's/^\./0./')
WEBC=$(echo "scale=1; ($W1-$W0)*100/$EL" | bc -l 2>/dev/null | sed 's/^\./0./')
SUMC=$(echo "scale=1; ($C1-$C0+$W1-$W0)*100/$EL" | bc -l 2>/dev/null | sed 's/^\./0./')

printf "  %-6s %2d流: %s/%d  %6.2fs  %7s MB/s  基线%5s%%  CPU 应用%5s%% + 页面%5s%% = %5s%%\n" \
  "$MODE" "$N" "$OK" "$N" "$EL" "$MBPS" "$PCT" "${APPC:-0}" "${WEBC:-0}" "${SUMC:-0}"
rm -f "$RES" "$CPU"
