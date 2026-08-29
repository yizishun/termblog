#!/bin/sh
# verify-jail.sh —— M3 验收(以 root 运行, 全自动, 约 2 分钟)
#
#   sh /home/yzs/termblog/scripts/verify-jail.sh
#
# 覆盖 plan §9 M3 验收点:
#   1. 进程形态: jaild=root, termblog-web/ssh=www, socket 0660 root:www
#   2. ssh 会话跑在真实 jail 里(uid=guest, hostname=blog)
#   3. 多访客互不可见(会话 A 写的文件会话 B 看不到)
#   4. rctl 掐死 fork bomb(maxproc=32 deny)
#   5. 配额: 同一 IP 第 4 个并发会话被拒(3 个 jail 封顶)
#   6. 断线后 jail 立即回收, zfs 无泄漏
#   7. 磁盘配额: 每会话 zfs quota=4M, 写超被拒
#
# 注意: socket 断开后会话立即回收(jail 与每 IP 配额同步释放); 需要观察
# jail 的检查(jls/zfs/rctl)一律趁会话还活着时做(后台起会话、末条命令
# sleep 撑住存活窗口), 步骤组之间留 8s 覆盖回收耗时(shell 无视 HUP 时
# 5s SIGKILL 兜底 + 清理)。
# web 侧用浏览器开 http://<host>:8080 目测即可, 协议路径已由
# scripts/e2e-reconnect.mjs 覆盖。

set -u

SSH="ssh -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o PreferredAuthentications=none -o LogLevel=ERROR blog@127.0.0.1"
pass=0; fail=0
check() { if [ "$1" -eq 0 ]; then echo "✅ $2"; pass=$((pass+1)); else echo "❌ $2"; fail=$((fail+1)); fi }

# 跑一条 ssh 会话, 输出去掉 \r(无 pty 时 zsh 用 CRLF 回显), 供锚点 grep
run_ssh() { # $1=输出文件, 其余=要发的命令串
    out="$1"; shift
    (printf '%s\n' "$@"; sleep 2) | timeout 15 $SSH 2>&1 | tr -d '\r' > "$out"
}

jail_count() { jls name 2>/dev/null | grep -c '^s-[0-9a-f]\{8\}$' || true; }

echo "== 1. 进程形态与权限边界 =="
[ "$(ps -axo user,comm | awk '$2=="jaild"{print $1; exit}')" = "root" ]
check $? "jaild 以 root 运行"
[ "$(ps -axo user,comm | awk '$2=="termblog-web"{print $1; exit}')" = "www" ]
check $? "termblog-web 以 www 运行"
[ "$(ps -axo user,comm | awk '$2=="termblog-ssh"{print $1; exit}')" = "www" ]
check $? "termblog-ssh 以 www 运行"
PERM=$(stat -f '%Sp %Su:%Sg' /var/run/termblog.sock 2>/dev/null)
[ "$PERM" = "srw-rw---- root:www" ]
check $? "socket 权限 0660 root:www (实际: $PERM)"

echo "== 2. ssh 会话跑在真实 jail 里 =="
# 新语义: 会话断开即回收, jls/zfs 观察必须趁会话还活着做——后台起会话
# (末条命令 sleep 8 撑住存活窗口), 先查 jls/zfs, 再 wait 收尾验输出。
# 先 sleep 2 等会话建好再投喂命令: 服务刚重启后冷 clone 较慢, 命令早到
# 会撞上会话创建窗口(曾导致一次性竞态失败)。
(sleep 2; printf 'echo IN_JAIL_$((39+3))\n'; printf 'id -un\n'; printf 'hostname\n'; \
  printf 'pwd\n'; printf 'sleep 8\n'; sleep 9) \
    | timeout 20 $SSH 2>&1 | tr -d '\r' > /tmp/tb-verify1.txt &
SSHPID=$!
sleep 5
[ "$(jail_count)" -ge 1 ]
check $? "存在会话 jail (jls)"
zfs list -H -o name -r zroot/jails 2>/dev/null | grep -q 'zroot/jails/s-[0-9a-f]\{8\}'
check $? "存在会话数据集 (zfs list)"
wait "$SSHPID"
grep -q "IN_JAIL_42" /tmp/tb-verify1.txt
check $? "jail 内 shell 可用"
grep -q "^guest$" /tmp/tb-verify1.txt
check $? "jail 内以 guest 身份运行"
grep -q "^blog$" /tmp/tb-verify1.txt
check $? "jail hostname=blog"
grep -q "^/home/guest$" /tmp/tb-verify1.txt
check $? "起始目录为 guest 家目录 (/home/guest)"

echo "== (等 8s: 会话回收, 释放每 IP 配额) =="
sleep 8

echo "== 3. 多访客互不可见 =="
run_ssh /tmp/tb-verify2.txt 'echo SECRET_MARK_$RANDOM > /tmp/mine.txt' 'sleep 1'
run_ssh /tmp/tb-verify3.txt 'test ! -e /tmp/mine.txt && echo ISOLATED_$((6+6))' 'sleep 1'
grep -q "ISOLATED_12" /tmp/tb-verify3.txt
check $? "会话 B 看不到会话 A 写的文件"

echo "== 4. rctl 掐死 fork bomb =="
# 受控炸弹: 后台起 40 个 sleep 撞资源限额。实测 vmemoryuse=512M 在 maxproc
# 之前就会掐住 fork(zsh 每 fork 一份虚拟内存 ~90MB), 所以"fork failed"即
# rctl 拒绝的证据。sleep 只睡 5s, 自然死亡后资源释放; 7s 时再从 ssh 侧
# 投喂存活标记(等资源释放后再发命令, shell 无需在饱和期 fork)。
# 会话断开即回收: jail 名与 rctl 规则必须在会话存活窗口内抓取, 否则已销毁。
# 先 sleep 2 等会话建好再投喂命令(同第 2 组, 避免撞会话创建窗口)。
(sleep 2; printf 'i=0; while [ $i -lt 40 ]; do sleep 5 & i=$((i+1)); done\n'; sleep 7; \
 printf 'echo BOMB_SURVIVED_$((6+6))\n'; sleep 1) \
    | timeout 20 $SSH 2>&1 | tr -d '\r' > /tmp/tb-verify4.txt &
BOMBPID=$!
sleep 5
BJAIL=$(jls name 2>/dev/null | grep '^s-[0-9a-f]\{8\}$' | tail -1) # 最新 = 炸弹会话
RCTL_OUT=$(rctl jail:"$BJAIL" 2>&1 | head -8)
echo "$RCTL_OUT" | grep -q ':deny='
RCTL_OK=$?
check $RCTL_OK "rctl 限额规则已挂载 ($BJAIL)"
if [ "$RCTL_OK" -ne 0 ]; then
    echo "    rctl jail:$BJAIL 实际输出:"; echo "$RCTL_OUT" | sed 's/^/    /'
fi
wait "$BOMBPID"
grep -q "fork failed" /tmp/tb-verify4.txt
check $? "rctl 拒绝超限 fork (输出含 fork failed)"
grep -q "BOMB_SURVIVED_12" /tmp/tb-verify4.txt
check $? "炸弹进程自然死亡后 shell 仍存活"

echo "== (等 8s: 会话回收, 清空配额) =="
sleep 8

echo "== 5. 配额: 每 IP 3 个并发会话封顶 =="
BASE=$(jail_count)
for i in 1 2 3 4; do
    (printf 'sleep 8\n'; sleep 9) | timeout 12 $SSH > /dev/null 2>&1 &
done
sleep 4
N=$(jail_count)
[ "$N" -ge 1 ] && [ "$N" -le 3 ]
check $? "4 并发连接时 jail 数在 1..3 (基线 $BASE, 实际 $N)"
wait 2>/dev/null

echo "== 6. 断线后 jail 回收, zfs 无泄漏 (等 8s) =="
sleep 8
NJAIL=$(jail_count)
NZFS=$(zfs list -H -o name -r zroot/jails 2>/dev/null | grep -c 'zroot/jails/s-' || true)
[ "$NJAIL" -eq 0 ]
check $? "jail 全部回收 (残留: $NJAIL)"
[ "$NZFS" -eq 0 ]
check $? "zfs 数据集无泄漏 (残留: $NZFS)"

echo "== 7. 磁盘配额: 每会话写空间 4M 封顶 =="
# 会话存活窗口内: 抓最新会话数据集验 quota 属性; dd 写超 4M 触发超限。
# 数据必须不可压缩: zroot 开 lz4, 全零数据会压成近零字节、永远碰不到 quota
# (曾导致 dd 16M 零块成功写完), 所以用 /dev/urandom。先 sleep 2 等会话建好。
(sleep 2; printf 'dd if=/dev/urandom of=/tmp/big bs=1M count=8 2>&1\n'; \
  printf 'sleep 8\n'; sleep 9) \
    | timeout 20 $SSH 2>&1 | tr -d '\r' > /tmp/tb-verify7.txt &
QPID=$!
sleep 5
QJAIL=$(jls name 2>/dev/null | grep '^s-[0-9a-f]\{8\}$' | tail -1) # 最新 = 配额会话
QDS="zroot/jails/$QJAIL"
QUOTA=$(zfs get -H -o value quota "$QDS" 2>/dev/null)
[ "$QUOTA" = "4M" ]
check $? "会话数据集挂有 quota=4M ($QDS: $QUOTA)"
wait "$QPID"
grep -qi "quota exceeded" /tmp/tb-verify7.txt
check $? "写超 4M 被拒 (输出含 quota exceeded)"

echo ""
echo "== 结果: $pass 通过, $fail 失败 =="
[ "$fail" -eq 0 ]
