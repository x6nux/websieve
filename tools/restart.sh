#!/bin/bash
# 用指定环境变量重启压测两端（不重编，秒级）。
#
#   restart.sh "<服务端env>" "<客户端env>"
#   restart.sh "WSIEVE_WSMUX_HIWATER=4194304" "WSIEVE_WSMUX_WINDOW=8388608"
#
# 两端各自的参数是**不对称**的，别混：下行方向上，服务端的出站水位决定它一次
# 能囤多少待发数据，而客户端声明的接收窗口决定服务端的发送信用。测下行吞吐时
# 这两个要分开调、分开看。
set -eu
HOST=${WSIEVE_BENCH_HOST:-10.0.5.2}
PW=${WSIEVE_BENCH_PW:-123456}
SSH="sshpass -p $PW ssh -o StrictHostKeyChecking=no -o LogLevel=ERROR root@$HOST"
SRV_ARGS="--listen 0.0.0.0:8443 --key-file /etc/wsieve/server.key --whitelist-file /etc/wsieve/whitelist.txt --deployment cdn-flexible"

SRV_ENV=${1:-}
CLI_ENV=${2:-}

$SSH "pkill -x wsieve-server; sleep 1; setsid nohup env $SRV_ENV /usr/local/bin/wsieve-server $SRV_ARGS < /dev/null > /root/wsieve.log 2>&1 & sleep 3; ss -lntp | grep -q :8443 || { echo '服务端启动失败'; tail -3 /root/wsieve.log; exit 1; }" 2>&1 | grep -v setlocale || true

/tmp/relaunch.sh $CLI_ENV > /tmp/relaunch.out 2>&1 || { echo "客户端启动失败"; tail -3 /tmp/relaunch.out; exit 1; }
echo "两端已重启  服务端[${SRV_ENV:-默认}]  客户端[${CLI_ENV:-默认}]"
