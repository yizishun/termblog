#!/bin/sh
# deploy-root.sh —— 生产部署(必须以 root 运行, 一步到位)
#
#   su -   (切到 root)
#   sh /home/yzs/termblog/scripts/deploy-root.sh
#
# 做的事: 检查/写入 racct(loader tunable, 首次需要重启机器) ->
# 构建 jail 模板(已存在则跳过, 需网络) ->
# 以 yzs 身份编译(避免 target/ 被 root 污染) -> 安装二进制/前端/配置 ->
# 拉起 jaild[root] 与 termblog-web/termblog-ssh[www](幂等)。

set -eu

REPO=/home/yzs/termblog

# ── 1. racct: rctl 限额的前提。loader tunable, 运行期只读, 必须重启生效 ──
if ! grep -q '^kern.racct.enable=1' /boot/loader.conf 2>/dev/null; then
    echo 'kern.racct.enable=1' >> /boot/loader.conf
    echo ">> 已写入 /boot/loader.conf: kern.racct.enable=1"
fi
if [ "$(sysctl -n kern.racct.enable 2>/dev/null)" != "1" ]; then
    echo ""
    echo "!! kern.racct.enable 尚未生效(只读 tunable, 需要重启)"
    echo "!! JailBackend 对 rctl 失败是 fail-closed: 不重启就没有会话可用"
    echo "!! 请重启机器(shutdown -r now), 然后重新运行本脚本"
    exit 1
fi
echo ">> kern.racct.enable 已生效"

# ── 2. jail 模板 ──
if zfs list -H -o name zroot/jails/template@release >/dev/null 2>&1; then
    echo ">> 模板已存在: zroot/jails/template@release, 跳过构建"
else
    echo ">> 构建 jail 模板(下载 base.txz + pkg 装 zsh, 约 2-5 分钟)"
    sh "$REPO/jailtpl/build-template.sh"
fi

# ── 3. 编译(以 yzs 跑: HOME/PATH 正确, 且不污染仓库属主) ──
# Makefile 是 GNU make 语法(define/endef + $(shell)), FreeBSD 默认 make 是
# bmake 不兼容, 必须显式用 gmake。
echo ">> 编译"
if ! command -v gmake >/dev/null 2>&1; then
    echo ">> 安装 gmake"
    pkg install -y gmake
fi
su -l yzs -c "cd $REPO && gmake build"

# ── 4. 安装 ──
echo ">> 安装二进制 / 前端 / rc 脚本"
install -d /usr/local/sbin /usr/local/share/termblog/frontend /usr/local/etc/rc.d
install -m 555 "$REPO/target/release/termblog-web" /usr/local/sbin/termblog-web
install -m 555 "$REPO/target/release/termblog-ssh" /usr/local/sbin/termblog-ssh
install -m 555 "$REPO/target/release/termblog-jaild" /usr/local/sbin/jaild
cp -R "$REPO/frontend/dist/." /usr/local/share/termblog/frontend/
install -m 644 "$REPO/etc/termblog.toml" /usr/local/etc/termblog.toml.sample
if [ -f /usr/local/etc/termblog.toml ] && ! cmp -s "$REPO/etc/termblog.toml" /usr/local/etc/termblog.toml; then
    # 仓库配置有更新(如 backend -> jail.socket 迁移): 备份本地版后刷新。
    # 本地如有自定义项, 部署后自行合并回新文件
    cp /usr/local/etc/termblog.toml /usr/local/etc/termblog.toml.old
    install -m 644 "$REPO/etc/termblog.toml" /usr/local/etc/termblog.toml
    echo ">> 配置有更新: 旧版已备份到 /usr/local/etc/termblog.toml.old, 现配置已刷新为仓库版本"
elif [ ! -f /usr/local/etc/termblog.toml ]; then
    cp /usr/local/etc/termblog.toml.sample /usr/local/etc/termblog.toml
fi
install -m 555 "$REPO/etc/rc.d/jaild" "$REPO/etc/rc.d/termblog" /usr/local/etc/rc.d/

# ── 5. 运行时目录(降权 www 需要写的部分) ──
mkdir -p /var/db/termblog /var/log
chown www /var/db/termblog
touch /var/log/jaild.log /var/log/termblog-web.log /var/log/termblog-ssh.log
chown www /var/log/termblog-web.log /var/log/termblog-ssh.log

# ── 6. 拉起/重启服务(先停旧进程再起新二进制, 部署即滚动重启) ──
start_daemon() { # $1=服务名 $2=用户(可空) $3=二进制
    pidf="/var/run/$1.pid"
    if [ -f "$pidf" ] && kill -0 "$(cat "$pidf")" 2>/dev/null; then
        kill "$(cat "$pidf")" 2>/dev/null || true
        sleep 0.5
    fi
    rm -f "$pidf"
    if [ -n "$2" ]; then
        daemon -u "$2" -p "$pidf" -o "/var/log/$1.log" "$3"
    else
        daemon -p "$pidf" -o "/var/log/$1.log" "$3"
    fi
    echo ">> $1 已启动/重启"
}
start_daemon jaild       ""    /usr/local/sbin/jaild
start_daemon termblog-web www /usr/local/sbin/termblog-web
start_daemon termblog-ssh www /usr/local/sbin/termblog-ssh

sleep 1
echo ""
echo "== 部署完成, 当前状态 =="
ls -l /var/run/termblog.sock
ps -axo user,pid,comm | grep -E "jaild|termblog-" | grep -v grep
echo ""
echo ">> 网页: http://$(hostname):8080   ssh: ssh -p 2222 blog@$(hostname)"
echo ">> 验收: sh $REPO/scripts/verify-jail.sh (root)"
