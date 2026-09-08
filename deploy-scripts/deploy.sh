#!/bin/sh
# deploy-scripts/deploy.sh —— 全量/内容部署(默认生产配置，必须以 root 运行)
#
#   su -
#   sh /home/yzs/termblog/deploy-scripts/deploy.sh               # 全量部署(用已构建的模板)
#   sh /home/yzs/termblog/deploy-scripts/deploy.sh --static-only # 只发静态镜像(零停机改文章)
#   TERMBLOG_CONFIG=etc/termblog-debug.toml sh ...                # 使用 debug 配置
#
# 全量做的事: 检查/写入 racct(loader tunable, 首次需要重启机器) ->
# 检查 jail 模板已存在(不存在则提示先跑 build-template.sh) ->
# 以 yzs 身份编译(避免 target/ 被 root 污染) -> 安装二进制/前端/配置 ->
# 发布静态镜像 -> 拉起 commentd/statd/jaild[root] 与 termblog-web/termblog-ssh[www](幂等)。
#
# --static-only: 只编译内容 + 发布静态镜像。纯文件替换, 零进程重启、
# 零会话中断; jail 侧(模板内文章)将在下次模板重建时跟进。
# 内容两侧都更新(含模板)走: make content(= 本脚本 --static-only +
# build-template.sh --replace, 全程零停机)。

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
REPO=$(dirname "$SCRIPT_DIR")
STATIC_DIR=/usr/local/share/termblog/frontend
BUILD_USER=yzs
CONFIG=${TERMBLOG_CONFIG:-etc/termblog.toml}
case "$CONFIG" in
    /*) ;;
    *) CONFIG="$REPO/$CONFIG" ;;
esac

[ "$(id -u)" -eq 0 ] || { echo "root required (service/zfs/install)"; exit 1; }

[ -f "$CONFIG" ] || { echo "configuration not found: $CONFIG"; exit 1; }
echo ">> Configuration: $CONFIG"

publish_static() {
    static_parent=$(dirname "$STATIC_DIR")
    static_stage="$static_parent/.frontend.new.$$"
    static_old="$static_parent/.frontend.old.$$"
    [ ! -L "$STATIC_DIR" ] || { echo "static directory cannot be a symlink: $STATIC_DIR"; exit 1; }
    [ ! -e "$static_stage" ] && [ ! -e "$static_old" ] || {
        echo "temporary static publish path already exists"; exit 1;
    }
    install -d "$static_parent" "$static_stage"
    cp -R "$REPO/frontend/dist/." "$static_stage/"
    chmod -R a+rX "$static_stage"
    if [ -e "$STATIC_DIR" ]; then
        mv "$STATIC_DIR" "$static_old"
    fi
    if mv "$static_stage" "$STATIC_DIR"; then
        [ ! -e "$static_old" ] || rm -rf "$static_old"
    else
        [ ! -e "$static_old" ] || mv "$static_old" "$STATIC_DIR"
        exit 1
    fi
}

# ── --static-only: 内容小改, 零停机(不碰进程/模板/会话) ──
if [ "${1:-}" = "--static-only" ]; then
    echo ">> 1/2 Compiling content (as $BUILD_USER, artifacts to frontend/dist and jailtpl/content/.rendered)"
    su -l "$BUILD_USER" -c "set -e; cd $REPO; cargo build --release -p content-build; \
        TERMBLOG_CONFIG=$CONFIG ./target/release/content-build --content jailtpl/content --dist frontend/dist"
    echo ">> 2/2 Publishing static mirror (pure file replacement, seamless, no restart)"
    install -d /usr/local/share/termblog
    install -m 444 "$REPO/jailtpl/content/.comment-targets.tsv" /usr/local/share/termblog/comment-targets.tsv
    install -m 444 "$REPO/jailtpl/content/.rendered/.index.json" /usr/local/share/termblog/article-index.json
    publish_static
    echo ">> Done (mirror only). Jail side will be updated on next template rebuild."
    exit 0
fi

# ── 1. racct: rctl 限额的前提。loader tunable, 运行期只读, 必须重启生效 ──
if ! grep -q '^kern.racct.enable=1' /boot/loader.conf 2>/dev/null; then
    echo 'kern.racct.enable=1' >> /boot/loader.conf
    echo ">> Written to /boot/loader.conf: kern.racct.enable=1"
fi
if [ "$(sysctl -n kern.racct.enable 2>/dev/null)" != "1" ]; then
    echo ""
    echo "!! kern.racct.enable is not yet in effect (read-only tunable, reboot required)"
    echo "!! JailBackend fails closed on rctl failure: no sessions available without reboot"
    echo "!! Please reboot the system (shutdown -r now), then re-run this script"
    exit 1
fi
echo ">> kern.racct.enable is in effect"

# ── 2. jail 模板(只检查, 不构建: 模板构建归 build-template.sh) ──
if ! zfs list -H -o name zroot/jails/template@release >/dev/null 2>&1; then
    echo ""
    echo "!! jail template zroot/jails/template@release not found"
    echo "!! Please run first: sh $REPO/deploy-scripts/build-template.sh"
    exit 1
fi
echo ">> jail template is ready: zroot/jails/template@release"
for manifest in comment-targets.tsv article-index.json; do
    if [ ! -f "/jails/template/usr/local/share/termblog/$manifest" ]; then
        echo "!! Current jail template does not contain $manifest"
        echo "!! Please run first: sh $REPO/deploy-scripts/build-template.sh --replace"
        exit 1
    fi
done
while IFS="$(printf '\t')" read -r rel target; do
    case "$rel" in
        comment) scope_dir=""; expected="/" ;;
        */comment)
            scope_dir=${rel%/comment}
            case "$scope_dir" in ""|/*|*/|*//*|*[!a-z0-9/-]*) echo "!! invalid scope path $rel"; exit 1 ;; esac
            expected="/$scope_dir/"
            ;;
        *) echo "!! invalid scope path $rel"; exit 1 ;;
    esac
    [ "$target" = "$expected" ] || { echo "!! scope target mismatch: $rel -> $target"; exit 1; }
    proc="/jails/template/proc"
    [ -z "$scope_dir" ] || proc="$proc/$scope_dir"
    if [ ! -d "$proc" ] || [ -L "$proc" ]; then
        echo "!! Current jail template is missing root-owned scope directory $proc"
        echo "!! Please run first: sh $REPO/deploy-scripts/build-template.sh --replace"
        exit 1
    fi
    proc_metadata=$(stat -f "%u:%g:%Lp" "$proc")
    if [ "$proc_metadata" != "0:0:555" ]; then
        echo "!! $proc must be root:wheel 0555 (got $proc_metadata)"
        echo "!! Please run first: sh $REPO/deploy-scripts/build-template.sh --replace"
        exit 1
    fi
done < /jails/template/usr/local/share/termblog/comment-targets.tsv

# ── 3. 编译(以 yzs 跑: HOME/PATH 正确, 且不污染仓库属主) ──
# 全 workspace(web/ssh/jaild/jailbin/content-build)+ 前端 + 内容产物;
# 不再依赖 gmake: cargo/npm 直接调用。
echo ">> Compiling (full workspace + frontend + content artifacts)"
su -l "$BUILD_USER" -c "set -e; cd $REPO; cargo build --release; \
    cd frontend; [ -d node_modules ] || npm install; npm run build; \
    cd $REPO; TERMBLOG_CONFIG=$CONFIG ./target/release/content-build --content jailtpl/content --dist frontend/dist"

# ── 4. 安装 ──
echo ">> Installing binaries / frontend / rc scripts"
install -d /usr/local/sbin /usr/local/share/termblog /usr/local/etc/rc.d
install -m 555 "$REPO/target/release/termblog-web" /usr/local/sbin/termblog-web
install -m 555 "$REPO/target/release/termblog-ssh" /usr/local/sbin/termblog-ssh
install -m 555 "$REPO/target/release/termblog-jaild" /usr/local/sbin/jaild
install -m 555 "$REPO/target/release/commentd" /usr/local/sbin/commentd
install -m 555 "$REPO/target/release/termblog-statd" /usr/local/sbin/termblog-statd
ln -sf commentd /usr/local/sbin/commentctl
install -m 444 "$REPO/jailtpl/content/.comment-targets.tsv" /usr/local/share/termblog/comment-targets.tsv
install -m 444 "$REPO/jailtpl/content/.rendered/.index.json" /usr/local/share/termblog/article-index.json
install -m 644 "$CONFIG" /usr/local/etc/termblog.toml.sample
if [ -f /usr/local/etc/termblog.toml ] && ! cmp -s "$CONFIG" /usr/local/etc/termblog.toml; then
    cp /usr/local/etc/termblog.toml /usr/local/etc/termblog.toml.old
    install -m 644 "$CONFIG" /usr/local/etc/termblog.toml
    echo ">> Configuration updated: old version backed up to /usr/local/etc/termblog.toml.old, active config refreshed to repo version"
elif [ ! -f /usr/local/etc/termblog.toml ]; then
    cp /usr/local/etc/termblog.toml.sample /usr/local/etc/termblog.toml
fi
install -m 555 "$REPO/etc/rc.d/commentd" "$REPO/etc/rc.d/termblog-statd" "$REPO/etc/rc.d/jaild" "$REPO/etc/rc.d/termblog" /usr/local/etc/rc.d/
install -d /usr/local/etc/newsyslog.conf.d
install -m 644 "$REPO/etc/newsyslog.conf.d/termblog.conf" /usr/local/etc/newsyslog.conf.d/
sysrc commentd_enable=YES termblog_statd_enable=YES jaild_enable=YES termblog_enable=YES >/dev/null
echo ">> Enabled startup on boot: commentd_enable=YES termblog_statd_enable=YES jaild_enable=YES termblog_enable=YES"

# ── 5. 发布完整静态树（同父目录 staging 后整体换名，可回滚） ──
publish_static


# ── 6. commentd 独立 root-only 数据目录；只初始化本次新建的空目录 ──
COMMENT_DATA=/var/db/termblog-commentd
comment_data_new=0
if [ ! -e "$COMMENT_DATA" ]; then
    install -d -m 700 -o root -g wheel "$COMMENT_DATA"
    comment_data_new=1
elif [ ! -d "$COMMENT_DATA" ] || [ -L "$COMMENT_DATA" ]; then
    echo "!! $COMMENT_DATA must be a real directory"; exit 1
fi
if [ "$comment_data_new" -eq 1 ]; then
    /usr/local/sbin/commentd --init
else
    for f in comments.jsonl salt initialized; do
        [ -f "$COMMENT_DATA/$f" ] || { echo "!! existing comments directory missing $f, refusing automatic fix"; exit 1; }
    done
fi

# ── 7. statd 独立 root-only 数据目录；只初始化本次新建的空目录 ──
STATS_DATA=/var/db/termblog-statd
stats_data_new=0
if [ ! -e "$STATS_DATA" ]; then
    install -d -m 700 -o root -g wheel "$STATS_DATA"
    stats_data_new=1
elif [ ! -d "$STATS_DATA" ] || [ -L "$STATS_DATA" ]; then
    echo "!! $STATS_DATA must be a real directory"; exit 1
fi
if [ "$stats_data_new" -eq 1 ]; then
    /usr/local/sbin/termblog-statd --init
else
    for f in stats.sqlite3 secret; do
        [ -f "$STATS_DATA/$f" ] || { echo "!! existing statistics directory missing $f, refusing automatic fix"; exit 1; }
    done
fi

# ── 8. 运行时目录与日志 ──
mkdir -p /var/db/termblog /var/log
chown www /var/db/termblog
# rustls-acme caches the account key and certificate here. The enclosing 0700
# directory protects the files even though DirCache follows the process umask.
install -d -m 700 -o www -g www /var/db/termblog/acme
touch /var/log/commentd.log /var/log/termblog-statd.log /var/log/jaild.log /var/log/termblog-web.log /var/log/termblog-ssh.log
chown root:wheel /var/log/commentd.log /var/log/termblog-statd.log /var/log/jaild.log
chmod 640 /var/log/commentd.log /var/log/termblog-statd.log /var/log/jaild.log
chown www:wheel /var/log/termblog-web.log /var/log/termblog-ssh.log
chmod 640 /var/log/termblog-web.log /var/log/termblog-ssh.log

# ── 9. 拉起/重启服务(先停旧进程再起新二进制, 部署即滚动重启) ──
start_daemon() { # $1=服务名 $2=用户(可空) $3=二进制
    pidf="/var/run/$1.pid"
    if [ -f "$pidf" ] && kill -0 "$(cat "$pidf")" 2>/dev/null; then
        kill "$(cat "$pidf")" 2>/dev/null || true
        sleep 0.5
    fi
    rm -f "$pidf"
    # -H: SIGHUP 时重开输出文件(newsyslog 轮转) -P: 监督进程 pid 供轮转发信号
    if [ -n "$2" ]; then
        daemon -H -u "$2" -p "$pidf" -P "/var/run/$1-super.pid" -o "/var/log/$1.log" "$3"
    else
        daemon -H -p "$pidf" -P "/var/run/$1-super.pid" -o "/var/log/$1.log" "$3"
    fi
    echo ">> $1 started/restarted"
}
start_daemon commentd         ""  /usr/local/sbin/commentd
start_daemon termblog-statd  ""  /usr/local/sbin/termblog-statd
start_daemon jaild            ""  /usr/local/sbin/jaild
start_daemon termblog-web     www /usr/local/sbin/termblog-web
start_daemon termblog-ssh     www /usr/local/sbin/termblog-ssh

sleep 1
echo ""
echo "== Deployment complete, current status =="
ls -l /var/run/commentd-public.sock /var/run/commentd-private.sock /var/run/termblog-statd.sock /var/run/termblog.sock
ps -axo user,pid,comm | grep -E "commentd|jaild|termblog-" | grep -v grep
echo ""
site_url=$(awk -F '"' '/^[[:space:]]*site_url[[:space:]]*=/{print $2; exit}' /usr/local/etc/termblog.toml)
echo ">> Web: ${site_url:-http://$(hostname)}   ssh: ssh blog@$(hostname)   (端口以 etc/termblog.toml 为准)"
echo ">> Verification: sh $REPO/tests/verify-m3.sh (root), sh $REPO/tests/verify-m5.sh, sh $REPO/tests/verify-comments.sh, sh $REPO/tests/verify-stats.sh"
