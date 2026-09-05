#!/bin/sh
# Read-only production checks for termblog-statd and per-session /proc snapshots.
set -eu

fail() { echo "FAIL: $*"; exit 1; }

[ "$(id -u)" -eq 0 ] || fail "root required"
SOCKET=${TERMBLOG_STATS_SOCKET:-/var/run/termblog-statd.sock}
DATA=${TERMBLOG_STATS_DATA:-/var/db/termblog-statd}
TARGETS=${TERMBLOG_TARGETS_FILE:-/usr/local/share/termblog/comment-targets.tsv}
INDEX=${TERMBLOG_ARTICLE_INDEX:-/usr/local/share/termblog/article-index.json}

[ -S "$SOCKET" ] || fail "missing statd socket $SOCKET"
[ "$(stat -f "%Su:%Sg:%Lp" "$SOCKET")" = "root:www:660" ] || fail "$SOCKET must be root:www 0660"
[ -d "$DATA" ] && [ ! -L "$DATA" ] || fail "$DATA must be a real directory"
[ "$(stat -f "%u:%g:%Lp" "$DATA")" = "0:0:700" ] || fail "$DATA must be root:wheel 0700"
for file in secret stats.sqlite3; do
    path="$DATA/$file"
    [ -f "$path" ] && [ ! -L "$path" ] || fail "$path must be a regular file"
    [ "$(stat -f "%u:%g:%Lp" "$path")" = "0:0:600" ] || fail "$path must be root:wheel 0600"
done
[ "$(stat -f "%z" "$DATA/secret")" -eq 32 ] || fail "statd secret must be exactly 32 bytes"
[ -s "$DATA/stats.sqlite3" ] || fail "stats database is empty"
[ -f "$TARGETS" ] && [ ! -L "$TARGETS" ] || fail "missing scope manifest $TARGETS"
[ -f "$INDEX" ] && [ ! -L "$INDEX" ] || fail "missing article index $INDEX"
echo "OK: statd socket, root-only database, secret, and manifests"

JAIL_ROOT=${TERMBLOG_VERIFY_JAIL_ROOT:-}
if [ -z "$JAIL_ROOT" ]; then
    echo "SKIP: set TERMBLOG_VERIFY_JAIL_ROOT=/jails/s-<sid> to inspect a live session snapshot"
    exit 0
fi

while IFS="$(printf "\t")" read -r rel target; do
    case "$rel" in
        comment) scope_dir=""; expected="/" ;;
        */comment)
            scope_dir=${rel%/comment}
            case "$scope_dir" in ""|/*|*/|*//*|*[!a-z0-9/-]*) fail "invalid scope path $rel" ;; esac
            expected="/$scope_dir/"
            ;;
        *) fail "invalid scope path $rel" ;;
    esac
    [ "$target" = "$expected" ] || fail "scope target mismatch: $rel -> $target"
    proc_dir="$JAIL_ROOT/proc"
    [ -z "$scope_dir" ] || proc_dir="$proc_dir/$scope_dir"
    stat_file="$proc_dir/stat"
    [ -d "$proc_dir" ] && [ ! -L "$proc_dir" ] || fail "missing real directory $proc_dir"
    [ "$(stat -f "%u:%g:%Lp" "$proc_dir")" = "0:0:555" ] || fail "$proc_dir must be root:wheel 0555"
    [ -f "$stat_file" ] && [ ! -L "$stat_file" ] || fail "missing regular file $stat_file"
    [ "$(stat -f "%u:%g:%Lp" "$stat_file")" = "0:0:444" ] || fail "$stat_file must be root:wheel 0444"
    [ "$(sed -n "1p" "$stat_file")" = "version 1" ] || fail "$stat_file has the wrong version"
    grep -Fqx "target $target" "$stat_file" || fail "$stat_file has the wrong target"
    grep -Eq "^snapshot_at [0-9]{4}-[0-9]{2}-[0-9]{2}T.*Z$" "$stat_file" || fail "$stat_file has an invalid timestamp"
    grep -Eq "^comments_approved [0-9]+$" "$stat_file" || fail "$stat_file has an invalid comment count"
    status=$(grep "^stats_status " "$stat_file" | cut -d " " -f2)
    case "$status" in
        ok)
            for field in terminal_read_sessions_total static_requests_total unique_visitors_approx; do
                grep -Eq "^$field [0-9]+$" "$stat_file" || fail "$stat_file is missing $field"
            done
            if grep "^article " "$stat_file" | grep -Ev "^article [a-z0-9/-]+ terminal_read_sessions=[0-9]+ static_requests=[0-9]+$"; then
                fail "$stat_file has a malformed article row"
            fi
            article_keys=$(sed -n "s/^article \([^ ]*\) .*/\1/p" "$stat_file")
            sorted_keys=$(printf "%s\n" "$article_keys" | LC_ALL=C sort)
            [ "$article_keys" = "$sorted_keys" ] || fail "$stat_file article rows are not sorted"
            ;;
        unavailable)
            if grep -Eq "^(terminal_read_sessions_total|static_requests_total|unique_visitors_approx) " "$stat_file"; then
                fail "$stat_file fabricates counters while statistics are unavailable"
            fi
            if grep -q "^article " "$stat_file"; then
                fail "$stat_file fabricates article counters while statistics are unavailable"
            fi
            ;;
        *) fail "$stat_file has invalid stats_status" ;;
    esac
    before=$(cksum "$stat_file")
    sleep 1
    after=$(cksum "$stat_file")
    [ "$before" = "$after" ] || fail "$stat_file changed inside an existing session"
done < "$TARGETS"

echo "OK: all configured /proc snapshots have stable format, ownership, mode, and contents"
