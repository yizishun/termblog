#!/bin/sh
# deploy-scripts/build-template.sh —— 构建 zroot/jails/template@release
#
# 生成一个只读的 jail 模板数据集: FreeBSD base + zsh + 常用工具 + guest 用户
# + 定制 zshrc + 博客内容 + jailbin 命令(blog/webctl 为其符号链接),
# 最后打 snapshot 并设 readonly=on。jaild 的 JailBackend 用它做 ZFS clone
# 秒开每访客一个的会话 jail。
#
# 用法(需要 root, 需要网络):
#   sh deploy-scripts/build-template.sh [base.txz 的 URL]
#       首次构建; 模板已存在则拒绝(防覆盖在跑会话的模板)
#   sh deploy-scripts/build-template.sh --replace [base.txz 的 URL]
#       零停机换模板(内容更新用): 构建到旁路名 template.new 再换名上场,
#       全程不停服、不杀会话。旧会话继续用旧模板(内容旧), 新会话取新模板
#       (内容新); 旧模板被旧会话的 clone pin 住, 全部退出后回收。
# 默认拉 download.freebsd.org 上 CURRENT 快照的 base.txz。
#
# 构建输入(jailbin 二进制 + 内容产物 .rendered)由本脚本自建(以 yzs 编译,
# 不依赖 Makefile)。

set -eu

REPLACE=0
[ "${1:-}" = "--replace" ] && { REPLACE=1; shift; }

DATASET=zroot/jails/template
MOUNT=/jails/template
BASE_TXZ_URL="${1:-https://download.freebsd.org/snapshots/amd64/16.0-CURRENT/base.txz}"
GUEST=guest
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
REPO=$(dirname "$SCRIPT_DIR")
BUILD_USER=yzs

[ "$(id -u)" -eq 0 ] || { echo "需要 root (zfs/mount/pw)"; exit 1; }
command -v zfs >/dev/null || { echo "需要 ZFS"; exit 1; }

# 0. 自包含前置(以 yzs 编译, 不依赖 Makefile): 模板要装的 jailbin 与内容
#    产物(.rendered)必须存在; content-build 还要读 frontend/dist 的入口资产。
#    cargo/npm 增量构建, 输入已新时近乎秒回。
echo ">> 准备构建输入(以 $BUILD_USER: content-build + jailbin + 内容产物)"
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
        echo "模板数据集已存在: $DATASET (先 zfs destroy -r $DATASET 或加 --replace)"; exit 1
    fi
    BUILD_DS="$DATASET"
    BUILD_MOUNT="$MOUNT"
fi
zfs list -H -o name zroot/jails >/dev/null 2>&1 || zfs create -o mountpoint=none zroot/jails
zfs create -o mountpoint="$BUILD_MOUNT" "$BUILD_DS"

# 2. base.txz(FreeBSD base 全量; /boot 对 jail 无用, 解完删掉)
if [ ! -f /tmp/termblog-base.txz ]; then
    echo ">> 下载 base.txz: $BASE_TXZ_URL"
    fetch -o /tmp/termblog-base.txz "$BASE_TXZ_URL"
fi
echo ">> 解压 base.txz -> $BUILD_MOUNT"
tar -xf /tmp/termblog-base.txz -C "$BUILD_MOUNT"
rm -rf "$BUILD_MOUNT/boot"

# 3. devfs(供 pkg chroot 安装使用, 规则集 0 仅构建期可用, 会话 jail 用规则集 4)
mount -t devfs devfs "$BUILD_MOUNT/dev"

# 4. DNS(pkg 拉包用)
cp /etc/resolv.conf "$BUILD_MOUNT/etc/resolv.conf"

# 5. pkg + 软件(zsh 是登录 shell; less/tree 是访客常用工具; less 供 blog 分页)
echo ">> 安装 zsh 与常用工具"
pkg -c "$BUILD_MOUNT" bootstrap -y
pkg -c "$BUILD_MOUNT" install -y zsh less tree

# 6. guest 用户(会话 jail 里降权运行; uid 1001 避开 base 自带用户)
pw -R "$BUILD_MOUNT" useradd -n "$GUEST" -u 1001 -d "/home/$GUEST" -s /usr/local/bin/zsh -m

# 7. 定制 zshrc(欢迎语 / 提示符 / 受限 PATH / locale / MOTD)
cat > "$BUILD_MOUNT/home/$GUEST/.zshrc" <<'EOF'
# termblog guest shell —— 每个访客一个真实 FreeBSD jail
export PATH=/usr/local/bin:/usr/bin:/bin
export LANG=C.UTF-8
umask 022
PS1='%F{green}blog@jail%f %~ %# '
setopt INTERACTIVE_COMMENTS
# jaild 的评论回执不经 PTY，可能在 prompt 之后异步到达。SIGURG
# 默认为忽略；在 zsh 内只用它通知 ZLE 重画 prompt 和未提交的编辑行。
TRAPURG() {
    [[ -o zle ]] && zle -I
    return 0
}
echo '帮助: blog ~/help.md    博客: blog 看列表, blog <article-key> 读文章'
echo '录像: play 看列表, play <cast-key> 播放 (空格暂停, q 退出)'
EOF

# 8. content 是 guest HOME 的唯一蓝图。复制全部非隐藏路径，系统生成的
#    .rendered 与 .rendered-assets 再按白名单单独安装。
CONTENT="$REPO/jailtpl/content"
HOME_DIR="$BUILD_MOUNT/home/$GUEST"
[ -d "$CONTENT" ] && [ ! -L "$CONTENT" ] || { echo "content 根必须是真实目录: $CONTENT"; exit 1; }
bad_link=$(find "$CONTENT" -type l -print -quit)
[ -z "$bad_link" ] || { echo "content 不支持符号链接: $bad_link"; exit 1; }
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
[ -f "$TARGETS" ] || { echo "缺少评论 target 清单: $TARGETS"; exit 1; }
install -d -m 755 "$BUILD_MOUNT/usr/local/share/termblog"
install -m 444 "$TARGETS" "$BUILD_MOUNT/usr/local/share/termblog/comment-targets.tsv"
while IFS="$(printf '\t')" read -r rel target; do
    [ -n "$rel" ] && [ -n "$target" ] || { echo "非法空 target 行"; exit 1; }
    case "$rel" in
        /*|*//*|.|..|../*|*/../*|*/..) echo "非法评论设备路径: $rel"; exit 1 ;;
    esac
    case "$rel" in
        comment) expected="/" ;;
        */comment)
            dir=${rel%/comment}
            case "$dir" in ""|/*|*/|*//*|*[!a-z0-9/-]*) echo "非法评论目录: $dir"; exit 1 ;; esac
            expected="/$dir/"
            ;;
        *) echo "非法评论设备路径: $rel"; exit 1 ;;
    esac
    [ "$target" = "$expected" ] || { echo "评论 target 不匹配: $rel -> $target"; exit 1; }
    fifo="$BUILD_MOUNT/home/$GUEST/$rel"
    [ ! -e "$fifo" ] || { echo "评论设备路径已存在: $fifo"; exit 1; }
    parent=$(dirname "$fifo")
    install -d -m 755 -o 1001 -g 1001 "$parent"
    mkfifo -m 600 "$fifo"
    chown 1001:1001 "$fifo"
done < "$TARGETS"

# 会话快照目录由 root 管理；guest 只能读取 comments.jsonl。
install -d -m 755 "$BUILD_MOUNT/var/run/termblog"

# 9. jailbin 命令(0555, 只读): blog / play / webctl 是指向 jailbin 的符号链接(busybox 式)
echo ">> 安装 jailbin 命令(blog / play / webctl → jailbin)"
install -m 555 "$REPO/target/release/jailbin" "$BUILD_MOUNT/usr/local/bin/jailbin"
ln -s jailbin "$BUILD_MOUNT/usr/local/bin/blog"
ln -s jailbin "$BUILD_MOUNT/usr/local/bin/play"
ln -s jailbin "$BUILD_MOUNT/usr/local/bin/webctl"

# 10. 收尾: 卸 devfs, 清 DNS, 打 snapshot, 模板转只读
umount -f "$BUILD_MOUNT/dev" 2>/dev/null || true
rm -f "$BUILD_MOUNT/etc/resolv.conf"
zfs snapshot "$BUILD_DS@release"
zfs set readonly=on "$BUILD_DS"

if [ "$REPLACE" -eq 1 ]; then
    # 11. 零停机换面: 名字让位 -> 换名 -> mountpoint 归位 -> 异步清理旧模板。
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
    echo "完成: $DATASET@release 已零停机换新(旧会话继续用旧模板, 全部退出后回收 template.old*)"
else
    echo "完成: $DATASET@release (只读模板, jaild 可 clone)"
fi
