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
root_response=$(curl -fsS -G --data-urlencode 'target=/' --data-urlencode 'limit=100' \
    "$BASE_URL/api/comments")
printf '%s\n' "$root_response" | grep -q '"comments"' || { echo "FAIL: public API"; exit 1; }
if printf '%s\n' "$root_response" | grep -Eq '"(id|reply_to_id)"[[:space:]]*:'; then
    echo "FAIL: public API 泄露数据库全局 ID"; exit 1
fi
curl -fsS -G --data-urlencode 'target=/notes/' --data-urlencode 'limit=100' \
    "$BASE_URL/api/comments" | grep -q '"comments"' || { echo "FAIL: 通用 /notes/ target"; exit 1; }
grep -Fxq 'notes/comment	/notes/' /usr/local/share/termblog/comment-targets.tsv || {
    echo "FAIL: 显式空目录 attachment 未进入 target 清单"; exit 1;
}

echo "OK: 双 socket、commentctl、显式 attachment 清单与通用 target API"

JAIL_ROOT=${TERMBLOG_VERIFY_JAIL_ROOT:-}
if [ -z "$JAIL_ROOT" ]; then
    echo "SKIP: 未设置 TERMBLOG_VERIFY_JAIL_ROOT，未执行投稿/审核闭环"
    exit 0
fi
[ -p "$JAIL_ROOT/home/guest/comment" ] || { echo "FAIL: 全局 FIFO 不存在"; exit 1; }
[ -p "$JAIL_ROOT/home/guest/notes/comment" ] || { echo "FAIL: notes attachment FIFO 不存在"; exit 1; }
nonce="verify-comments-$(date +%s)-$$"
printf 'verify: %s\n' "$nonce" > "$JAIL_ROOT/home/guest/comment"
sleep 1
row=$(TERMBLOG_CONFIG="$CONFIG" "$COMMENTCTL" queue | grep "$nonce" | tail -1)
[ -n "$row" ] || { echo "FAIL: FIFO 投稿未进入 pending"; exit 1; }
id=$(printf '%s\n' "$row" | awk -F '\t' '{sub(/^#/, "", $1); print $1}')
TERMBLOG_CONFIG="$CONFIG" "$COMMENTCTL" approve "$id" >/dev/null
api=$(curl -fsS -G --data-urlencode 'target=/' --data-urlencode 'limit=100' \
    "$BASE_URL/api/comments")
printf '%s\n' "$api" | grep -Fq "$nonce" || { echo "FAIL: approve 后 API 不可见"; exit 1; }
comment_json=$(printf '%s\n' "$api" | sed 's/},{/}\
{/g' | grep -F "$nonce" | tail -1)
number=$(printf '%s\n' "$comment_json" | sed -n 's/.*"number":\([0-9][0-9]*\).*/\1/p')
[ -n "$number" ] || { echo "FAIL: API 缺少目录内局部编号"; exit 1; }

reply_nonce="${nonce}-reply"
printf 'verify-reply: #%s: %s first line\n%s second line\n' \
    "$number" "$reply_nonce" "$reply_nonce" > "$JAIL_ROOT/home/guest/comment"
sleep 1
queue_output=$(TERMBLOG_CONFIG="$CONFIG" "$COMMENTCTL" queue)
reply_count=$(printf '%s\n' "$queue_output" | grep -Fc "$reply_nonce" || true)
[ "$reply_count" -eq 1 ] || {
    echo "FAIL: 一次多行 FIFO 写入应只生成一条 pending，实际为 $reply_count"; exit 1;
}
reply_row=$(printf '%s\n' "$queue_output" | grep -F "$reply_nonce" | tail -1)
[ -n "$reply_row" ] || { echo "FAIL: 多行嵌套回复未进入 pending"; exit 1; }
printf '%s\n' "$reply_row" | grep -Fq "${reply_nonce} first line\\n${reply_nonce} second line" || {
    echo "FAIL: commentctl queue 未在单行中保留多行正文"; exit 1;
}
printf '%s\n' "$reply_row" | awk -F '\t' -v parent="$id" '
    $5 == "reply_to=#" parent { found=1 }
    END { exit found ? 0 : 1 }
' || { echo "FAIL: pending 回复未绑定父评论"; exit 1; }
reply_id=$(printf '%s\n' "$reply_row" | awk -F '\t' '{sub(/^#/, "", $1); print $1}')
TERMBLOG_CONFIG="$CONFIG" "$COMMENTCTL" approve "$reply_id" >/dev/null
reply_api=$(curl -fsS -G --data-urlencode 'target=/' --data-urlencode 'limit=100' \
    "$BASE_URL/api/comments")
reply_json=$(printf '%s\n' "$reply_api" | sed 's/},{/}\
{/g' | grep -F "$reply_nonce" | tail -1)
printf '%s\n' "$reply_json" | grep -Fq '"reply_to":{' &&
    printf '%s\n' "$reply_json" | grep -Fq "\"number\":$number" &&
    printf '%s\n' "$reply_json" | grep -Fq '"author":"verify"' &&
    printf '%s\n' "$reply_json" |
        grep -Fq "${reply_nonce} first line\\n${reply_nonce} second line" || {
    echo "FAIL: API 未保留多行正文或回复摘要不正确"; exit 1;
}

# 当前会话保留创建时快照；审核后的评论由新会话在启动 barrier 中取得。
snapshot="$JAIL_ROOT/var/run/termblog/comments.jsonl"
[ -r "$snapshot" ] || { echo "FAIL: 缺少初始评论快照"; exit 1; }
if grep -Eq '"(id|reply_to_id)"[[:space:]]*:' "$snapshot"; then
    echo "FAIL: guest 快照泄露数据库全局 ID"; exit 1
fi
echo "OK: multiline FIFO -> one queue row -> approve -> nested reply -> local-number API；终端评论需新建会话后查看"
