#!/bin/sh
# 评论系统部署验收。设置 TERMBLOG_VERIFY_JAIL_ROOT=/jails/s-<sid> 时额外跑
# FIFO -> pending -> approve -> public API 的破坏性闭环；默认只做只读检查。
set -eu

[ "$(id -u)" -eq 0 ] || { echo "需要 root"; exit 1; }
CONFIG=${TERMBLOG_CONFIG:-/usr/local/etc/termblog.toml}
COMMENTCTL=${COMMENTCTL:-/usr/local/sbin/commentctl}
BASE_URL=${TERMBLOG_BASE_URL:-http://127.0.0.1:8080}

for socket in /var/run/commentd-public.sock /var/run/commentd-private.sock; do
    [ -S "$socket" ] || { echo "FAIL: 缺少 socket $socket"; exit 1; }
done
[ -x "$COMMENTCTL" ] || { echo "FAIL: 缺少 commentctl"; exit 1; }
[ -f /usr/local/share/termblog/comment-targets.tsv ] || { echo "FAIL: 缺少 target 清单"; exit 1; }
TERMBLOG_CONFIG="$CONFIG" "$COMMENTCTL" queue --limit 10 >/dev/null
curl -fsS -G --data-urlencode 'target=/' --data-urlencode 'limit=100' \
    "$BASE_URL/api/comments" | grep -q '"comments"' || { echo "FAIL: public API"; exit 1; }

echo "OK: 双 socket、commentctl、target 清单与 public API"

JAIL_ROOT=${TERMBLOG_VERIFY_JAIL_ROOT:-}
if [ -z "$JAIL_ROOT" ]; then
    echo "SKIP: 未设置 TERMBLOG_VERIFY_JAIL_ROOT，未执行投稿/审核闭环"
    exit 0
fi
[ -p "$JAIL_ROOT/home/guest/comment" ] || { echo "FAIL: 全局 FIFO 不存在"; exit 1; }
nonce="verify-comments-$(date +%s)-$$"
printf 'verify: %s\n' "$nonce" > "$JAIL_ROOT/home/guest/comment"
sleep 1
row=$(TERMBLOG_CONFIG="$CONFIG" "$COMMENTCTL" queue | grep "$nonce" | tail -1)
[ -n "$row" ] || { echo "FAIL: FIFO 投稿未进入 pending"; exit 1; }
id=$(printf '%s\n' "$row" | awk -F '\t' '{sub(/^#/, "", $1); print $1}')
TERMBLOG_CONFIG="$CONFIG" "$COMMENTCTL" approve "$id" >/dev/null
curl -fsS -G --data-urlencode 'target=/' --data-urlencode 'limit=100' \
    "$BASE_URL/api/comments" | grep -Fq "$nonce" || { echo "FAIL: approve 后 API 不可见"; exit 1; }

# 当前会话保留创建时快照；审核后的评论由新会话在启动 barrier 中取得。
snapshot="$JAIL_ROOT/var/run/termblog/comments.jsonl"
[ -r "$snapshot" ] || { echo "FAIL: 缺少初始评论快照"; exit 1; }
echo "OK: FIFO -> queue -> approve -> API (#$id)；终端评论需新建会话后查看"
