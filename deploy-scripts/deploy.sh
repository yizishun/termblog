#!/bin/sh
# deploy-scripts/deploy.sh —— 生产部署(必须以 root 运行, 一步到位)
#
#   su -
#   sh /home/yzs/termblog/deploy-scripts/deploy.sh               # 全量部署(用已构建的模板)
#   sh /home/yzs/termblog/deploy-scripts/deploy.sh --static-only # 只发静态镜像(零停机改文章)
#
# 全量做的事: 检查/写入 racct(loader tunable, 首次需要重启机器) ->
# 检查 jail 模板已存在(不存在则提示先跑 build-template.sh) ->
# 以 yzs 身份编译(避免 target/ 被 root 污染) -> 安装二进制/前端/配置 ->
# 发布静态镜像 -> 拉起 jaild[root] 与 termblog-web/termblog-ssh[www](幂等)。
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

[ "$(id -u)" -eq 0 ] || { echo "需要 root (service/zfs/install)"; exit 1; }

# ── --static-only: 内容小改, 零停机(不碰进程/模板/会话) ──
if [ "${1:-}" = "--static-only" ]; then
    echo ">> 1/2 编译内容(以 $BUILD_USER, 产物进 frontend/dist 与 jailtpl/content/.rendered)"
    su -l "$BUILD_USER" -c "set -e; cd $REPO; cargo build --release -p content-build; \
        ./target/release/content-build --content jailtpl/content --dist frontend/dist"
    echo ">> 2/2 发布静态镜像(纯文件替换, 无感, 不重启)"
    install -d /usr/local/share/termblog
    install -m 444 "$REPO/jailtpl/content/.comment-targets.tsv" /usr/local/share/termblog/comment-targets.tsv
    rm -rf "$STATIC_DIR/blog"
    cp -R "$REPO/frontend/dist/." "$STATIC_DIR/"
    # 保证 www 可读(曾出过 600 权限导致 /blog.css 404 的事故)
    chmod -R a+rX "$STATIC_DIR"
    echo ">> 完成(仅镜像)。jail 侧将在下次模板重建时跟进。"
    exit 0
fi

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

# ── 2. jail 模板(只检查, 不构建: 模板构建归 build-template.sh) ──
if ! zfs list -H -o name zroot/jails/template@release >/dev/null 2>&1; then
    echo ""
    echo "!! 未找到 jail 模板 zroot/jails/template@release"
    echo "!! 请先运行: sh $REPO/deploy-scripts/build-template.sh"
    exit 1
fi
echo ">> jail 模板已就绪: zroot/jails/template@release"
if [ ! -f /jails/template/usr/local/share/termblog/comment-targets.tsv ]; then
    echo "!! 当前 jail 模板尚未包含评论设备清单"
    echo "!! 请先运行: sh $REPO/deploy-scripts/build-template.sh --replace"
    exit 1
fi

# ── 3. 编译(以 yzs 跑: HOME/PATH 正确, 且不污染仓库属主) ──
# 全 workspace(web/ssh/jaild/jailbin/content-build)+ 前端 + 内容产物;
# 不再依赖 gmake: cargo/npm 直接调用。
echo ">> 编译(全 workspace + 前端 + 内容产物)"
su -l "$BUILD_USER" -c "set -e; cd $REPO; cargo build --release; \
    cd frontend; [ -d node_modules ] || npm install; npm run build; \
    cd $REPO; ./target/release/content-build --content jailtpl/content --dist frontend/dist"

# ── 4. 安装 ──
echo ">> 安装二进制 / 前端 / rc 脚本"
install -d /usr/local/sbin /usr/local/share/termblog/frontend /usr/local/etc/rc.d
install -m 555 "$REPO/target/release/termblog-web" /usr/local/sbin/termblog-web
install -m 555 "$REPO/target/release/termblog-ssh" /usr/local/sbin/termblog-ssh
install -m 555 "$REPO/target/release/termblog-jaild" /usr/local/sbin/jaild
install -m 555 "$REPO/target/release/commentd" /usr/local/sbin/commentd
ln -sf commentd /usr/local/sbin/commentctl
cp -R "$REPO/frontend/dist/." /usr/local/share/termblog/frontend/
install -m 444 "$REPO/jailtpl/content/.comment-targets.tsv" /usr/local/share/termblog/comment-targets.tsv
install -m 644 "$REPO/etc/termblog.toml" /usr/local/etc/termblog.toml.sample
if [ -f /usr/local/etc/termblog.toml ] && ! cmp -s "$REPO/etc/termblog.toml" /usr/local/etc/termblog.toml; then
    cp /usr/local/etc/termblog.toml /usr/local/etc/termblog.toml.old
    install -m 644 "$REPO/etc/termblog.toml" /usr/local/etc/termblog.toml
    echo ">> 配置有更新: 旧版已备份到 /usr/local/etc/termblog.toml.old, 现配置已刷新为仓库版本"
elif [ ! -f /usr/local/etc/termblog.toml ]; then
    cp /usr/local/etc/termblog.toml.sample /usr/local/etc/termblog.toml
fi
install -m 555 "$REPO/etc/rc.d/commentd" "$REPO/etc/rc.d/jaild" "$REPO/etc/rc.d/termblog" /usr/local/etc/rc.d/
install -d /usr/local/etc/newsyslog.conf.d
install -m 644 "$REPO/etc/newsyslog.conf.d/termblog.conf" /usr/local/etc/newsyslog.conf.d/
sysrc commentd_enable=YES jaild_enable=YES termblog_enable=YES >/dev/null
echo ">> 已启用开机自启: commentd_enable=YES jaild_enable=YES termblog_enable=YES"

# ── 5. 发布静态镜像(纯文件替换; blog 目录先清掉防删文留僵尸) ──
rm -rf "$STATIC_DIR/blog"
cp -R "$REPO/frontend/dist/." "$STATIC_DIR/"
chmod -R a+rX "$STATIC_DIR"


# ── 6. commentd 独立 root-only 数据目录；只初始化本次新建的空目录 ──
COMMENT_DATA=/var/db/termblog-commentd
comment_data_new=0
if [ ! -e "$COMMENT_DATA" ]; then
    install -d -m 700 -o root -g wheel "$COMMENT_DATA"
    comment_data_new=1
elif [ ! -d "$COMMENT_DATA" ] || [ -L "$COMMENT_DATA" ]; then
    echo "!! $COMMENT_DATA 必须是真实目录"; exit 1
fi
if [ "$comment_data_new" -eq 1 ]; then
    /usr/local/sbin/commentd --init
else
    for f in comments.jsonl salt initialized; do
        [ -f "$COMMENT_DATA/$f" ] || { echo "!! 已有评论目录缺少 $f，拒绝自动修复"; exit 1; }
    done
fi

# ── 7. 运行时目录(降权 www 需要写的部分) ──
mkdir -p /var/db/termblog /var/log
chown www /var/db/termblog
touch /var/log/commentd.log /var/log/jaild.log /var/log/termblog-web.log /var/log/termblog-ssh.log
chown www /var/log/termblog-web.log /var/log/termblog-ssh.log

# ── 8. 拉起/重启服务(先停旧进程再起新二进制, 部署即滚动重启) ──
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
    echo ">> $1 已启动/重启"
}
start_daemon commentd    ""    /usr/local/sbin/commentd
start_daemon jaild       ""    /usr/local/sbin/jaild
start_daemon termblog-web www /usr/local/sbin/termblog-web
start_daemon termblog-ssh www /usr/local/sbin/termblog-ssh

sleep 1
echo ""
echo "== 部署完成, 当前状态 =="
ls -l /var/run/commentd-public.sock /var/run/commentd-private.sock /var/run/termblog.sock
ps -axo user,pid,comm | grep -E "commentd|jaild|termblog-" | grep -v grep
echo ""
echo ">> 网页: http://$(hostname):8080   ssh: ssh -p 2222 blog@$(hostname)"
echo ">> 验收: sh $REPO/tests/verify-m3.sh (root)、sh $REPO/tests/verify-m5.sh、sh $REPO/tests/verify-comments.sh"
