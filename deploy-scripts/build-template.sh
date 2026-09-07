#!/bin/sh
# deploy-scripts/build-template.sh —— 构建 zroot/jails/template@release
#
# 分两层构建:
#   template-base@prepared: FreeBSD base + pkg + zsh/less/tree，只在首次或
#                           --refresh-base 时联网构建。
#   template@release:      从 prepared 本地 clone，再加 guest、zshrc、博客内容
#                           和 jailbin。常规 --replace 不再下载 base/pkg。
# 最终模板设 readonly=on；jaild 用它做 ZFS clone，秒开每访客一个会话 jail。
#
# 用法(需要 root; 只有首次/--refresh-base 需要网络):
#   sh deploy-scripts/build-template.sh [base.txz 的 URL]
#       首次准备 base 并构建模板; 模板已存在则拒绝
#   sh deploy-scripts/build-template.sh --replace [base.txz 的 URL]
#       从本地 prepared base 零停机换模板(内容更新用)。
#   sh deploy-scripts/build-template.sh --refresh-base [base.txz 的 URL]
#       显式联网重建 prepared base，并零停机替换当前模板。
# 模板替换构建到旁路名 template.new 再换名上场,
#       全程不停服、不杀会话。旧会话继续用旧模板(内容旧), 新会话取新模板
#       (内容新); 旧模板被旧会话的 clone pin 住, 全部退出后回收。
# 默认使用与宿主同版本的 RELEASE base.txz，持久缓存在 /var/cache/termblog。
#
# 构建输入(jailbin 二进制 + 内容产物 .rendered)由本脚本自建(以 yzs 编译,
# 不依赖 Makefile)。

set -eu

REPLACE=0
REFRESH_BASE=0
BASE_TXZ_ARG=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --replace) REPLACE=1 ;;
        --refresh-base) REFRESH_BASE=1 ;;
        --*) echo "unknown option: $1"; exit 64 ;;
        *)
            [ -z "$BASE_TXZ_ARG" ] || { echo "only one base.txz URL may be specified"; exit 64; }
            BASE_TXZ_ARG=$1
            ;;
    esac
    shift
done

DATASET=zroot/jails/template
MOUNT=/jails/template
BASE_DATASET=zroot/jails/template-base
BASE_SNAPSHOT="$BASE_DATASET@prepared"
BASE_MOUNT=/jails/template-base
HOST_RELEASE=$(freebsd-version -u 2>/dev/null || uname -r)
HOST_RELEASE=${HOST_RELEASE%%-p*}
PLATFORM=$(uname -m)
MACHINE=$(uname -p)
BASE_TXZ_URL="${BASE_TXZ_ARG:-https://download.freebsd.org/releases/$PLATFORM/$MACHINE/$HOST_RELEASE/base.txz}"
BASE_CACHE_DIR=/var/cache/termblog
BASE_TXZ_CACHE="$BASE_CACHE_DIR/base.txz"
BASE_URL_CACHE="$BASE_CACHE_DIR/base.txz.url"
GUEST=guest
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
REPO=$(dirname "$SCRIPT_DIR")
BUILD_USER=yzs

[ "$(id -u)" -eq 0 ] || { echo "root required (zfs/mount/pw)"; exit 1; }
command -v zfs >/dev/null || { echo "ZFS required"; exit 1; }

CLEANUP_BASE_DS=
CLEANUP_BASE_MOUNT=
CACHE_TMP="$BASE_TXZ_CACHE.new.$$"
CACHE_URL_TMP="$BASE_URL_CACHE.new.$$"
cleanup_on_exit() {
    status=$?
    trap - EXIT HUP INT TERM
    rm -f "$CACHE_TMP" "$CACHE_URL_TMP"
    if [ -n "$CLEANUP_BASE_DS" ]; then
        [ -z "$CLEANUP_BASE_MOUNT" ] || umount -f "$CLEANUP_BASE_MOUNT/dev" 2>/dev/null || true
        zfs destroy -r "$CLEANUP_BASE_DS" 2>/dev/null || true
    fi
    exit "$status"
}
trap cleanup_on_exit EXIT HUP INT TERM

ensure_base_archive() {
    install -d -m 755 "$BASE_CACHE_DIR"
    cached_url=
    [ ! -f "$BASE_URL_CACHE" ] || cached_url=$(cat "$BASE_URL_CACHE")
    if [ ! -f "$BASE_TXZ_CACHE" ] || [ "$cached_url" != "$BASE_TXZ_URL" ]; then
        echo ">> Downloading base.txz once: $BASE_TXZ_URL"
        fetch -o "$CACHE_TMP" "$BASE_TXZ_URL"
        chmod 644 "$CACHE_TMP"
        printf '%s\n' "$BASE_TXZ_URL" > "$CACHE_URL_TMP"
        mv "$CACHE_TMP" "$BASE_TXZ_CACHE"
        mv "$CACHE_URL_TMP" "$BASE_URL_CACHE"
    else
        echo ">> Reusing cached base.txz: $BASE_TXZ_CACHE"
    fi
}

prepare_base() {
    base_ds=$1
    base_mount=$2
    CLEANUP_BASE_DS=$base_ds
    CLEANUP_BASE_MOUNT=$base_mount

    zfs create -o mountpoint="$base_mount" -o org.termblog:base-url="$BASE_TXZ_URL" "$base_ds"
    ensure_base_archive
    echo ">> Extracting prepared base -> $base_mount"
    tar -xf "$BASE_TXZ_CACHE" -C "$base_mount"
    rm -rf "$base_mount/boot"

    mkdir -p "$base_mount/dev"
    mount -t devfs devfs "$base_mount/dev"
    cp /etc/resolv.conf "$base_mount/etc/resolv.conf"
    echo ">> Bootstrapping pkg and installing zsh/less/tree (prepared base only)"
    pkg -c "$base_mount" bootstrap -y
    pkg -c "$base_mount" install -y zsh less tree
    umount -f "$base_mount/dev"
    rm -f "$base_mount/etc/resolv.conf"

    zfs snapshot "$base_ds@prepared"
    zfs set readonly=on "$base_ds"
    zfs set mountpoint=none "$base_ds"
    CLEANUP_BASE_DS=
    CLEANUP_BASE_MOUNT=
}

zfs list -H -o name zroot/jails >/dev/null 2>&1 || zfs create -o mountpoint=none zroot/jails

# 先拒绝误用，避免在明知不会替换现有模板时才去准备 base。
if [ "$REPLACE" -eq 0 ] && [ "$REFRESH_BASE" -eq 0 ] \
    && zfs list -H -o name "$DATASET" >/dev/null 2>&1; then
    echo "template dataset already exists: $DATASET (add --replace to rebuild it)"
    exit 1
fi

# prepared base 用旁路数据集构建：失败不会破坏现有 base/template。
if [ "$REFRESH_BASE" -eq 1 ] || ! zfs list -H -o name "$BASE_SNAPSHOT" >/dev/null 2>&1; then
    if [ "$REFRESH_BASE" -eq 0 ] && zfs list -H -o name "$BASE_DATASET" >/dev/null 2>&1; then
        echo "prepared base dataset exists but $BASE_SNAPSHOT is missing; refusing to overwrite it"
        exit 1
    fi
    zfs destroy -r "$BASE_DATASET.new" 2>/dev/null || true
    prepare_base "$BASE_DATASET.new" "$BASE_MOUNT.new"

    previous_base=
    if zfs list -H -o name "$BASE_DATASET" >/dev/null 2>&1; then
        previous_base="$BASE_DATASET.old-$(date +%s)-$$"
        zfs rename "$BASE_DATASET" "$previous_base"
    fi
    if ! zfs rename "$BASE_DATASET.new" "$BASE_DATASET"; then
        [ -z "$previous_base" ] || zfs rename "$previous_base" "$BASE_DATASET" 2>/dev/null || true
        exit 1
    fi
    echo ">> Prepared base ready: $BASE_SNAPSHOT"
    # 更新 base 时同步换掉旧 template；旧会话仍由 template.old* 承载。
    if [ "$REFRESH_BASE" -eq 1 ] && zfs list -H -o name "$DATASET" >/dev/null 2>&1; then
        REPLACE=1
    fi
else
    prepared_url=$(zfs get -H -o value org.termblog:base-url "$BASE_DATASET" 2>/dev/null || true)
    if [ -n "$prepared_url" ] && [ "$prepared_url" != "-" ] && [ "$prepared_url" != "$BASE_TXZ_URL" ]; then
        echo "prepared base uses a different URL: $prepared_url"
        echo "re-run with --refresh-base to change it to: $BASE_TXZ_URL"
        exit 1
    fi
    echo ">> Reusing prepared base: $BASE_SNAPSHOT (no base/pkg download)"
fi

# 0. 自包含前置(以 yzs 编译, 不依赖 Makefile): 模板要装的 jailbin 与内容
#    产物(.rendered)必须存在; content-build 还要读 frontend/dist 的入口资产。
#    cargo/npm 增量构建, 输入已新时近乎秒回。
echo ">> Preparing build inputs (as $BUILD_USER: content-build + jailbin + content artifacts)"
su -l "$BUILD_USER" -c "set -e; cd $REPO; \
    cargo build --release -p content-build -p termblog-jailbin; \
    ( cd frontend; [ -d node_modules ] || npm install; npm run build ); \
    ./target/release/content-build --content jailtpl/content --dist frontend/dist"

# 1. 确定构建目标数据集: --replace 走旁路名(旧模板与在线会话全程不动)
if [ "$REPLACE" -eq 1 ]; then
    zfs destroy -r "$DATASET.new" 2>/dev/null || true   # 清上次构建残留
    BUILD_DS="$DATASET.new"
    BUILD_MOUNT="$MOUNT.new"
else
    if zfs list -H -o name "$DATASET" >/dev/null 2>&1; then
        echo "template dataset already exists: $DATASET (zfs destroy -r $DATASET first or add --replace)"; exit 1
    fi
    BUILD_DS="$DATASET"
    BUILD_MOUNT="$MOUNT"
fi
echo ">> Cloning local prepared base -> $BUILD_DS"
zfs clone -o readonly=off -o mountpoint="$BUILD_MOUNT" "$BASE_SNAPSHOT" "$BUILD_DS"
zfs mount "$BUILD_DS" 2>/dev/null || true
[ -d "$BUILD_MOUNT" ] || { echo "prepared template clone did not mount at $BUILD_MOUNT"; exit 1; }

# 2. guest 用户(会话 jail 里降权运行; uid 1001 避开 base 自带用户)
pw -R "$BUILD_MOUNT" useradd -n "$GUEST" -u 1001 -d "/home/$GUEST" -s /usr/local/bin/zsh -m

# 3. 定制 zshrc(欢迎语 / 提示符 / 受限 PATH / locale / MOTD)
cat > "$BUILD_MOUNT/home/$GUEST/.zshrc" <<'EOF'
# termblog guest shell —— 每个访客一个真实 FreeBSD jail
export PATH=/usr/local/bin:/usr/bin:/bin
export LANG=C.UTF-8
umask 022
PS1='%F{green}blog@jail%f %~ %# '
setopt INTERACTIVE_COMMENTS
# 每次提示符出现前以标准 OSC 2 报告当前目录。Web xterm 用它更新浏览器
# 标签标题；cd/pushd/popd 都无需包装，blog 退出后也会自然恢复路径标题。
autoload -Uz add-zsh-hook
_termblog_title_precmd() {
    print -Pn '\e]2;%~\a'
}
add-zsh-hook precmd _termblog_title_precmd
# jaild 的评论回执不经 PTY，可能在 prompt 之后异步到达。SIGURG
# 默认为忽略；在 zsh 内只用它通知 ZLE 重画 prompt 和未提交的编辑行。
TRAPURG() {
    [[ -o zle ]] && zle -I
    return 0
}
echo 'Help: blog ~/help.md    Blog: blog for list, blog <article-key> to read'
echo 'Casts: play for list, play <cast-key> to watch (space to pause, q to quit)'
EOF

# 4. content 是 guest HOME 的唯一蓝图。复制全部非隐藏路径，系统生成的
#    .rendered 与 .rendered-assets 再按白名单单独安装。
CONTENT="$REPO/jailtpl/content"
HOME_DIR="$BUILD_MOUNT/home/$GUEST"
[ -d "$CONTENT" ] && [ ! -L "$CONTENT" ] || { echo "content root must be a real directory: $CONTENT"; exit 1; }
bad_link=$(find "$CONTENT" -type l -print -quit)
[ -z "$bad_link" ] || { echo "content does not support symlinks: $bad_link"; exit 1; }
mkdir -p "$HOME_DIR/.rendered" "$HOME_DIR/.rendered-assets"
(
    cd "$CONTENT"
    # 从 content 根开始，并在任意点前缀组件处剪枝；空内容树也能正常完成。
    find . -mindepth 1 -name '.*' -prune -o -print | pax -rw -pe -d "$HOME_DIR"
)
if [ -d "$CONTENT/.rendered" ]; then
    cp -R "$CONTENT/.rendered/." "$HOME_DIR/.rendered/"
fi
if [ -d "$CONTENT/.rendered-assets" ]; then
    cp -R "$CONTENT/.rendered-assets/." "$HOME_DIR/.rendered-assets/"
fi
chown -R 1001:1001 "$BUILD_MOUNT/home/$GUEST"

# 评论设备：只按 content-build 从 .termblog.toml 生成的可信清单创建。
TARGETS="$REPO/jailtpl/content/.comment-targets.tsv"
[ -f "$TARGETS" ] || { echo "missing comment targets manifest: $TARGETS"; exit 1; }
ARTICLE_INDEX="$REPO/jailtpl/content/.rendered/.index.json"
[ -f "$ARTICLE_INDEX" ] || { echo "missing article index: $ARTICLE_INDEX"; exit 1; }
install -d -m 755 "$BUILD_MOUNT/usr/local/share/termblog"
install -m 444 "$TARGETS" "$BUILD_MOUNT/usr/local/share/termblog/comment-targets.tsv"
install -m 444 "$ARTICLE_INDEX" "$BUILD_MOUNT/usr/local/share/termblog/article-index.json"
while IFS="$(printf '\t')" read -r rel target; do
    [ -n "$rel" ] && [ -n "$target" ] || { echo "invalid empty target line"; exit 1; }
    case "$rel" in
        /*|*//*|.|..|../*|*/../*|*/..) echo "invalid comment device path: $rel"; exit 1 ;;
    esac
    case "$rel" in
        comment) expected="/"; scope_dir="" ;;
        */comment)
            dir=${rel%/comment}
            case "$dir" in ""|/*|*/|*//*|*[!a-z0-9/-]*) echo "invalid comment directory: $dir"; exit 1 ;; esac
            expected="/$dir/"
            scope_dir=$dir
            ;;
        *) echo "invalid comment device path: $rel"; exit 1 ;;
    esac
    [ "$target" = "$expected" ] || { echo "comment target mismatch: $rel -> $target"; exit 1; }
    fifo="$BUILD_MOUNT/home/$GUEST/$rel"
    [ ! -e "$fifo" ] || { echo "comment device path already exists: $fifo"; exit 1; }
    parent=$(dirname "$fifo")
    install -d -m 755 -o 1001 -g 1001 "$parent"
    mkfifo -m 600 "$fifo"
    chown 1001:1001 "$fifo"

    # One ordinary jail-root /proc tree mirrors configured HOME scopes.
    # jaild publishes /proc/stat or /proc/<scope>/stat before the guest fork.
    proc="$BUILD_MOUNT/proc"
    [ -z "$scope_dir" ] || proc="$proc/$scope_dir"
    if [ -e "$proc" ]; then
        [ -d "$proc" ] && [ ! -L "$proc" ] || { echo "scope proc path is not a real directory: $proc"; exit 1; }
    fi
    install -d -m 555 -o root -g wheel "$proc"
    [ ! -e "$proc/stat" ] || { echo "scope stat path already exists: $proc/stat"; exit 1; }
done < "$TARGETS"

# 评论快照运行目录由 root 管理；guest 只能读取 comments.jsonl。
install -d -m 755 "$BUILD_MOUNT/var/run/termblog"

# 5. jailbin 命令(0555, 只读): blog / play / webctl 是指向 jailbin 的符号链接(busybox 式)
echo ">> Installing jailbin commands (blog / play / webctl → jailbin)"
install -m 555 "$REPO/target/release/jailbin" "$BUILD_MOUNT/usr/local/bin/jailbin"
ln -s jailbin "$BUILD_MOUNT/usr/local/bin/blog"
ln -s jailbin "$BUILD_MOUNT/usr/local/bin/play"
ln -s jailbin "$BUILD_MOUNT/usr/local/bin/webctl"

# 6. 收尾: 打 snapshot, 模板转只读
zfs snapshot "$BUILD_DS@release"
zfs set readonly=on "$BUILD_DS"

if [ "$REPLACE" -eq 1 ]; then
    # 7. 零停机换面: 名字让位 -> 换名 -> mountpoint 归位 -> 异步清理旧模板。
    #     两条 rename 之间有微秒级窗口(template 名字瞬时不存在), 恰逢其会的
    #     新会话 clone 会失败 -> jaild fail-closed, 访客重试即可。
    zfs unmount "$DATASET" 2>/dev/null || true        # 旧模板(构建后保持挂载)
    zfs unmount "$DATASET.new" 2>/dev/null || true
    if zfs list -H -o name "$DATASET.old" >/dev/null 2>&1; then
        # 上次换下来的旧模板: 无会话 pin 则销毁; 仍被旧会话 pin 则改名让位
        zfs destroy -r "$DATASET.old" 2>/dev/null || zfs rename "$DATASET.old" "$DATASET.old-$(date +%s)"
    fi
    zfs rename "$DATASET" "$DATASET.old"
    zfs rename "$DATASET.new" "$DATASET"
    zfs set mountpoint="$MOUNT" "$DATASET"
    zfs set mountpoint=none "$DATASET.old"
    zfs mount "$DATASET" 2>/dev/null || true
    # 历史旧模板: 逐个尝试回收(被旧会话 pin 的跳过, 下次更新再试;
    # 会话硬寿命 7200s 兜底, 不会永远 pin 住)
    for old in $(zfs list -H -o name -r zroot/jails 2>/dev/null | grep -E '^zroot/jails/template\.old(-[0-9]+)?$' || true); do
        zfs destroy -r "$old" 2>/dev/null || true
    done
    # base 刷新时退役的旧 base 会被 template.old* pin 住；待相关会话
    # 退出且旧 template 回收后，在此处一并尝试回收。
    for old_base in $(zfs list -H -o name -r zroot/jails 2>/dev/null | grep -E '^zroot/jails/template-base\.old-[0-9]+-[0-9]+$' || true); do
        zfs destroy -r "$old_base" 2>/dev/null || true
    done
    echo "Done: $DATASET@release replaced with zero downtime (old sessions continue using old template; template.old* reclaimed after all exit)"
else
    echo "Done: $DATASET@release (read-only template, jaild can clone)"
fi
