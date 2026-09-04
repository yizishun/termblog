#!/bin/sh
# verify-m5.sh —— M5 验收(非 root, 需生产实例在跑; root 项标 [root])
BASE=${1:-http://127.0.0.1:8080}
CONTENT=${CONTENT:-jailtpl/content}
SSH="ssh -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
     -o PreferredAuthentications=none -o LogLevel=ERROR blog@127.0.0.1"
pass=0; fail=0
check() { if [ "$1" -eq 0 ]; then echo "✅ $2"; pass=$((pass+1)); else echo "❌ $2"; fail=$((fail+1)); fi }

INDEX="$CONTENT/.rendered/.index"
if [ ! -s "$INDEX" ]; then
    echo "❌ 缺少文章索引: $INDEX (请先运行 content-build)"
    exit 2
fi

# 从构建产物发现验收对象。ARTICLE_KEY 可显式覆盖，但默认不依赖任何内容目录名。
ARTICLE_KEY=${ARTICLE_KEY:-$(awk -F '	' 'NF >= 2 { print $2; exit }' "$INDEX")}
case "$ARTICLE_KEY" in
    ""|/*|*/|*//*|*[!a-z0-9/-]*)
        echo "❌ 非法 ARTICLE_KEY: $ARTICLE_KEY"
        exit 2
        ;;
esac
ARTICLE_SOURCE="$ARTICLE_KEY.md"
ARTICLE_ROUTE="/$ARTICLE_KEY/"

# 图片验收同样从 sidecar manifest 发现一篇带图文章；站点无图片时只跳过图片项。
IMAGE_MANIFEST=$(find "$CONTENT/.rendered" -type f -name '*.images.json' -print 2>/dev/null \
    | sort | sed -n '1p')
if [ -n "$IMAGE_MANIFEST" ]; then
    IMAGE_KEY=${IMAGE_MANIFEST#"$CONTENT/.rendered/"}
    IMAGE_KEY=${IMAGE_KEY%.images.json}
    IMAGE_ROUTE="/$IMAGE_KEY/"
    IMAGE_ASSET=$(sed -n 's/^[[:space:]]*"asset": "\([^"]*\)",*$/\1/p' "$IMAGE_MANIFEST" \
        | sed -n '1p')
    if [ -z "$IMAGE_ASSET" ]; then
        echo "❌ 图片 manifest 没有 asset: $IMAGE_MANIFEST"
        exit 2
    fi
fi

echo "验收文章: $ARTICLE_SOURCE → $ARTICLE_ROUTE"
if [ -n "$IMAGE_MANIFEST" ]; then
    echo "图片文章: $IMAGE_KEY (资源 /$IMAGE_ASSET)"
else
    echo "图片文章: 无，跳过图片验收"
fi

echo "== 1. 镜像页 =="
curl -sf "$BASE$ARTICLE_ROUTE" | grep -q '<article>'
check $? "GET $ARTICLE_ROUTE 返回含 <article> 的静态页"
curl -sf "$BASE$ARTICLE_ROUTE" | grep -qF "name=\"termblog-source\" content=\"$ARTICLE_SOURCE\""
check $? "镜像页带明确的 termblog-source meta"
curl -sf "$BASE$ARTICLE_ROUTE" | grep -qF "name=\"termblog-route\" content=\"$ARTICLE_ROUTE\""
check $? "镜像页带明确的 termblog-route meta"
curl -sf -o /dev/null -w '%{http_code}' "$BASE${ARTICLE_ROUTE%/}" | grep -q 307
check $? "无尾斜杠 307 → 尾斜杠(canonical 形态)"
if [ -n "$IMAGE_MANIFEST" ]; then
    curl -sf "$BASE$IMAGE_ROUTE" | grep -qF "<img src=\"/$IMAGE_ASSET\""
    check $? "镜像页含重写后的 <img>"
    curl -sf -o /dev/null -w '%{http_code}' "$BASE/$IMAGE_ASSET" | grep -q 200
    check $? "图片资源 HTTP 200"
fi

echo "== 2. 发现链路 =="
if curl -sf "$BASE/" | grep -q 'termblog:blog-index'; then
    check 1 "首页不再注入文章列表(应为纯终端)"
else
    check 0 "首页不再注入文章列表(应为纯终端)"
fi
curl -sf "$BASE/blog/" | grep -qF "href=\"$ARTICLE_ROUTE\""
check $? "/blog/ 全站列表含所选文章链接"

echo "== 3. feed =="
if grep -q '^site_url' /usr/local/etc/termblog.toml 2>/dev/null; then
    FEED_TMP=$(mktemp -d "${TMPDIR:-/tmp}/termblog-m5.XXXXXX") || exit 2
    SITEMAP_XML="$FEED_TMP/sitemap.xml"
    ATOM_XML="$FEED_TMP/atom.xml"
    trap 'rm -f "$SITEMAP_XML" "$ATOM_XML"; rmdir "$FEED_TMP" 2>/dev/null || true' EXIT
    curl -sf "$BASE/sitemap.xml" -o "$SITEMAP_XML"
    check $? "sitemap.xml 可取"
    xmllint --noout "$SITEMAP_XML"
    check $? "sitemap.xml 是合法 XML"
    curl -sf "$BASE/atom.xml" -o "$ATOM_XML" && xmllint --noout "$ATOM_XML"
    check $? "atom.xml 可取且合法"
else
    echo "⚠️ 跳过 sitemap/atom 检查(未配置 web.site_url)"
fi
curl -sf "$BASE/robots.txt" | grep -q '^Allow: /'
check $? "robots.txt 允许全站"

echo "== 4. jail 侧 [需 ssh 通] =="
(sleep 2; printf 'blog\n'; sleep 1) | timeout 10 $SSH 2>&1 | tr -d '\r' | grep -qF "$ARTICLE_KEY"
check $? "ssh: blog 列表含所选 article key"
out=$( (sleep 2; printf 'blog -- "$HOME/%s.md"\n' "$ARTICLE_KEY"; sleep 3) | timeout 15 $SSH 2>&1 | cat -v )
printf '%s\n' "$out" | grep -qF "]7777;url=$ARTICLE_ROUTE^G"
check $? "ssh: blog 读文章发出 OSC(进入)"
printf '%s\n' "$out" | grep -qF '^[[1m'
check $? "ssh: blog 显示预渲染排版(ANSI 粗体)"
# 复位检查按真实阅读路径: 读完后敲 q 退出 less, blog 脚本随后把地址栏复位为 /
(sleep 2; printf 'blog -- "$HOME/%s.md"\n' "$ARTICLE_KEY"; sleep 2; printf 'q'; sleep 1) | timeout 15 $SSH 2>&1 | cat -v \
  | grep -qF ']7777;url=/^G'
check $? "ssh: 退出 less 后 OSC 复位 /"
# 图片占位框: 不过 cat -v(它会把框线字符的 UTF-8 字节转成 M- 记法导致匹配失败);
# OSC8 的 ]8;; 是可打印 ASCII, 原始字节流里直接可匹配
if [ -n "$IMAGE_MANIFEST" ]; then
    out=$( (sleep 2; printf 'blog %s\n' "$IMAGE_KEY"; sleep 3) | timeout 15 $SSH 2>&1 )
    printf '%s\n' "$out" | grep -qF '┌─ 图片'
    check $? "ssh: 带图文章显示图片占位框"
    printf '%s\n' "$out" | grep -qF ']8;;'
    check $? "ssh: 占位框 URL 带 OSC8 超链接"
fi

echo ""
echo "== 结果: $pass 通过, $fail 失败 =="
[ "$fail" -eq 0 ]
