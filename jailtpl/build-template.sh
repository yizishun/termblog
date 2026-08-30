#!/bin/sh
# jailtpl/build-template.sh —— 构建 zroot/jails/template@release
#
# 生成一个只读的 jail 模板数据集: FreeBSD base + zsh + 常用工具 + guest 用户
# + 定制 zshrc + 博客内容, 最后打 snapshot 并设 readonly=on。
# jaild 的 JailBackend 用它做 ZFS clone 秒开每访客一个的会话 jail。
#
# 用法(需要 root, 需要网络):
#   sh jailtpl/build-template.sh [base.txz 的 URL]
# 默认拉 download.freebsd.org 上 CURRENT 快照的 base.txz。

set -eu

DATASET=zroot/jails/template
MOUNT=/jails/template
BASE_TXZ_URL="${1:-https://download.freebsd.org/snapshots/amd64/16.0-CURRENT/base.txz}"
GUEST=guest
SCRIPT_DIR=$(dirname "$0")

[ "$(id -u)" -eq 0 ] || { echo "需要 root (zfs/mount/pw)"; exit 1; }
command -v zfs >/dev/null || { echo "需要 ZFS"; exit 1; }

# 0. 父数据集 + 模板数据集(已存在则拒绝重跑, 防覆盖在跑会话的模板)
if zfs list -H -o name "$DATASET" >/dev/null 2>&1; then
    echo "模板数据集已存在: $DATASET (先 zfs destroy -r $DATASET 再重跑)"; exit 1
fi
zfs list -H -o name zroot/jails >/dev/null 2>&1 || zfs create -o mountpoint=none zroot/jails
zfs create -o mountpoint="$MOUNT" "$DATASET"

# 1. base.txz(FreeBSD base 全量; /boot 对 jail 无用, 解完删掉)
if [ ! -f /tmp/termblog-base.txz ]; then
    echo ">> 下载 base.txz: $BASE_TXZ_URL"
    fetch -o /tmp/termblog-base.txz "$BASE_TXZ_URL"
fi
echo ">> 解压 base.txz -> $MOUNT"
tar -xf /tmp/termblog-base.txz -C "$MOUNT"
rm -rf "$MOUNT/boot"

# 2. devfs(供 pkg chroot 安装使用, 规则集 0 仅构建期可用, 会话 jail 用规则集 4)
mount -t devfs devfs "$MOUNT/dev"

# 3. DNS(pkg 拉包用)
cp /etc/resolv.conf "$MOUNT/etc/resolv.conf"

# 4. pkg + 软件(zsh 是登录 shell; less/tree 是访客常用工具)
echo ">> 安装 zsh 与常用工具"
pkg -c "$MOUNT" bootstrap -y
pkg -c "$MOUNT" install -y zsh less tree

# 5. guest 用户(会话 jail 里降权运行; uid 1001 避开 base 自带用户)
pw -R "$MOUNT" useradd -n "$GUEST" -u 1001 -d "/home/$GUEST" -s /usr/local/bin/zsh -m

# 6. 定制 zshrc(欢迎语 / 提示符 / 受限 PATH / locale / MOTD)
cat > "$MOUNT/home/$GUEST/.zshrc" <<'EOF'
# termblog guest shell —— 每个访客一个真实 FreeBSD jail
export PATH=/usr/local/bin:/usr/bin:/bin
export LANG=C.UTF-8
umask 022
PS1='%F{green}blog@jail%f %~ %# '
setopt INTERACTIVE_COMMENTS
echo '博客: 敲 blog 看文章列表, 读一篇: blog hello (或 blog ~/blog/hello.md)'
EOF

# 7. 博客内容: 文章进 ~/blog(与 URL /blog/ 一一对应), 预渲染产物进 ~/.rendered
#    (hidden 工具目录, 不混进文章); README 是仓库侧写作规范, 不进 jail
if [ -d "$SCRIPT_DIR/content" ]; then
    mkdir -p "$MOUNT/home/$GUEST/blog" "$MOUNT/home/$GUEST/.rendered"
    cp -R "$SCRIPT_DIR/content/blog/." "$MOUNT/home/$GUEST/blog/"
    if [ -d "$SCRIPT_DIR/content/.rendered" ]; then
        cp -R "$SCRIPT_DIR/content/.rendered/." "$MOUNT/home/$GUEST/.rendered/"
    fi
fi
chown -R 1001:1001 "$MOUNT/home/$GUEST"

# 7.5 blog / webctl 命令(0555, 只读; .rendered 已随第 7 步拷进 ~/.rendered)
if [ -d "$SCRIPT_DIR/bin" ]; then
    install -m 555 "$SCRIPT_DIR/bin/blog" "$SCRIPT_DIR/bin/webctl" "$MOUNT/usr/local/bin/"
fi

# 8. 收尾: 卸 devfs, 清 DNS, 打 snapshot, 模板转只读
umount -f "$MOUNT/dev" 2>/dev/null || true
rm -f "$MOUNT/etc/resolv.conf"
zfs snapshot "$DATASET@release"
zfs set readonly=on "$DATASET"

echo "完成: $DATASET@release (只读模板, jaild 可 clone)"
