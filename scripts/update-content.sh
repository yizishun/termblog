#!/bin/sh
# update-content.sh —— 内容更新两步走(root 运行)
#
#   sh scripts/update-content.sh               # 镜像 + jail 两侧(重建模板, 杀会话)
#   sh scripts/update-content.sh --static-only # 只发布静态镜像(jail 侧下次重建跟进)
#
# 前提: 文章 md 已改好并提交(git 日期进 sitemap/atom)。

set -eu
REPO=/home/yzs/termblog
STATIC_DIR=/usr/local/share/termblog/frontend

[ "$(id -u)" -eq 0 ] || { echo "需要 root (service/zfs/install)"; exit 1; }

echo ">> 1/3 编译内容(以 yzs, 产物进 frontend/dist 与 jailtpl/content/.rendered)"
su -l yzs -c "cd $REPO && gmake build-content"

echo ">> 2/3 发布静态镜像(纯文件替换, 无感, 不重启)"
rm -rf "$STATIC_DIR/blog"
cp -R "$REPO/frontend/dist/." "$STATIC_DIR/"
# 保证 www 可读(曾出过 600 权限导致 /blog.css 404 的事故)
chmod -R a+rX "$STATIC_DIR"

if [ "${1:-}" = "--static-only" ]; then
    echo ">> 完成(仅镜像)。jail 侧将在下次模板重建时跟进。"
    exit 0
fi

echo ">> 3/3 重建 jail 模板(杀掉全部在线会话, 走 deploy-root.sh 全流程)"
service termblog stop 2>/dev/null || true
service jaild stop 2>/dev/null || true
sleep 5
# 会话 jail 持有模板快照的 clone, 不先清掉则 zfs destroy template 失败
for j in $(jls name 2>/dev/null | grep '^s-'); do
    jail -r "$j" 2>/dev/null || true
done
for ds in $(zfs list -H -o name -r zroot/jails 2>/dev/null | grep 'zroot/jails/s-'); do
    zfs destroy -f "$ds" 2>/dev/null || true
done
zfs destroy -r zroot/jails/template 2>/dev/null || true
sh "$REPO/scripts/deploy-root.sh"
