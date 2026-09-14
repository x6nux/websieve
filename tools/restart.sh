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
# 口令**没有默认值**：写死一个进版本库，等于把一台可 root 登录的机器的凭据
# 提交进去了。它还会悄悄生效——忘了设环境变量时脚本照跑，连的是别人手里
# 那台同口令的机器。宁可当场失败。
: "${WSIEVE_BENCH_PW:?请先设 WSIEVE_BENCH_PW（压测机 root 口令），不提供默认值}"
# `accept-new` 而不是 `no`：首次连接照样自动接受（压测机常重装，要的就是这个
# 便利），但主机密钥**变了**时会拒绝。`no` 是连变化也一起吞掉，那时 sshpass
# 会把 root 口令直接递给任何冒充这个地址的东西。
SSH="sshpass -p $WSIEVE_BENCH_PW ssh -o StrictHostKeyChecking=accept-new -o LogLevel=ERROR root@$HOST"
# `/tmp/relaunch.sh` 不在版本库里（它是本机那份带用户私钥的临时配置的启动器，
# 见 tools/ 的说明）。缺了它下面会报一句 "No such file"，看起来像压测失败而
# 不是环境没准备好——这两件事的排查方向完全不同。
[ -x /tmp/relaunch.sh ] || { echo "缺 /tmp/relaunch.sh（客户端启动器，不入库）——先准备它再跑" >&2; exit 1; }

SRV_ARGS="--listen 0.0.0.0:8443 --key-file /etc/wsieve/server.key --whitelist-file /etc/wsieve/whitelist.txt --deployment cdn-flexible"

SRV_ENV=${1:-}
CLI_ENV=${2:-}

$SSH "pkill -x wsieve-server; sleep 1; setsid nohup env $SRV_ENV /usr/local/bin/wsieve-server $SRV_ARGS < /dev/null > /root/wsieve.log 2>&1 & sleep 3; ss -lntp | grep -q :8443 || { echo '服务端启动失败'; tail -3 /root/wsieve.log; exit 1; }" 2>&1 | grep -v setlocale || true

/tmp/relaunch.sh $CLI_ENV > /tmp/relaunch.out 2>&1 || { echo "客户端启动失败"; tail -3 /tmp/relaunch.out; exit 1; }
echo "两端已重启  服务端[${SRV_ENV:-默认}]  客户端[${CLI_ENV:-默认}]"
