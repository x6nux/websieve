#!/bin/bash
# 小包压测：量请求速率和单请求延迟，而不是带宽。
#
#   bench-rps.sh direct|proxy <URL前缀> <并发数> <每并发请求数> <每请求字节>
#
# 和 bench.sh 的分工：大包考的是每**字节**成本（拷贝、加密、窗口），小包考的是
# 每**请求**成本（开流、帧头、往返、任务唤醒）。两者的瓶颈完全不在一个地方，
# 必须分开量——只测大包会让"每请求开销很重"这类问题完全隐身。
#
# 每个并发用一个 curl 进程、一次传多个 URL，让它在同一条连接上把请求打完。
# 这模拟的是真实客户端的连接复用；换成每请求一个 curl 进程，量到的会是进程
# 启动开销而不是代理开销。
#
# CPU 同样用累计 CPU 时间差值算，理由见 bench.sh 里的注释。
set -u
MODE=$1; BASE=$2; CONC=$3; PER=$4; BYTES=$5
APP=$(pgrep -f 'MacOS/wsieve-app' | head -1)

cpu_secs() {
  ps -o cputime= -p "$1" 2>/dev/null | tr -d ' ' \
    | awk -F: '{ if (NF==3) print $1*3600+$2*60+$3; else if (NF==2) print $1*60+$2; else print $1+0 }'
}
WEB=$(ps -eo pid,lstart,command | grep 'WebKit.WebContent' | grep -v grep | sort -k2 -r | head -1 | awk '{print $1}')

PROXY_ARG=""
[ "$MODE" = "proxy" ] && PROXY_ARG="-x http://127.0.0.1:25500"

# 一个并发要请求的 URL 列表（同一条连接上跑完）。
#
# 每个 URL 都要配自己的 `-o`：curl 在 `-o` 少于 URL 数时，会把多出来那些
# 请求的响应体直接吐到 stdout，混进 `-w` 的统计行里，量出来就成了"20 个只
# 成功 1 个"。
URLS=""
for i in $(seq 1 "$PER"); do URLS="$URLS -o /dev/null ${BASE}/__down?bytes=${BYTES}"; done

RES=$(mktemp)
C0=$(cpu_secs "${APP:-0}"); W0=$(cpu_secs "${WEB:-0}")
START=$(date +%s.%N)
PIDS=()
for c in $(seq 1 "$CONC"); do
  ( curl -s --max-time 120 $PROXY_ARG \
      -w "%{exitcode} %{size_download} %{time_total}\n" $URLS >> "$RES" ) &
  PIDS+=($!)
done
for p in "${PIDS[@]}"; do wait "$p"; done
END=$(date +%s.%N)
C1=$(cpu_secs "${APP:-0}"); W1=$(cpu_secs "${WEB:-0}")

EL=$(echo "$END - $START" | bc)
TOTAL=$((CONC * PER))
OK=$(awk -v b="$BYTES" '$1==0 && $2==b' "$RES" | wc -l | tr -d ' ')
RPS=$(echo "scale=0; $OK/$EL" | bc -l | cut -d. -f1)
# 延迟分位：curl 的 time_total 是单请求的完整往返
P50=$(awk '{print $3}' "$RES" | sort -n | awk '{a[NR]=$1} END{if(NR)printf "%.1f", a[int(NR*0.5)+0]*1000}')
P99=$(awk '{print $3}' "$RES" | sort -n | awk '{a[NR]=$1} END{if(NR)printf "%.1f", a[int(NR*0.99)]*1000}')
CPU=$(echo "scale=1; ($C1-$C0+$W1-$W0)*100/$EL" | bc -l 2>/dev/null | sed 's/^\./0./')

printf "  %-6s %2d并发×%4d个 %5sB: %s/%d  %6.2fs  %6s req/s  P50 %6sms  P99 %7sms  CPU %5s%%\n" \
  "$MODE" "$CONC" "$PER" "$BYTES" "$OK" "$TOTAL" "$EL" "${RPS:-0}" "${P50:--}" "${P99:--}" "${CPU:-0}"
rm -f "$RES"
