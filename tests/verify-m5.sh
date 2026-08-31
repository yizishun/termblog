#!/bin/sh
# verify-m5.sh —— M5 验收(非 root, 需生产实例在跑; root 项标 [root])
BASE=${1:-http://127.0.0.1:8080}
SSH="ssh -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
     -o PreferredAuthentications=none -o LogLevel=ERROR blog@127.0.0.1"
pass=0; fail=0
check() { if [ "$1" -eq 0 ]; then echo "✅ $2"; pass=$((pass+1)); else echo "❌ $2"; fail=$((fail+1)); fi }

echo "== 1. 镜像页 =="
curl -sf "$BASE/blog/hello/" | grep -q '<article>'
check $? "GET /blog/hello/ 返回含 <article> 的静态页"
curl -sf "$BASE/blog/hello/" | grep -q 'name="termblog-slug" content="hello"'
check $? "镜像页带 termblog-slug meta"
curl -sf -o /dev/null -w '%{http_code}' "$BASE/blog/hello" | grep -q 307
check $? "无尾斜杠 307 → 尾斜杠(canonical 形态)"

echo "== 2. 发现链路 =="
curl -sf "$BASE/" | grep -q 'termblog:blog-index'
check $? "首页含注入的文章列表"
curl -sf "$BASE/blog/" | grep -q '/blog/hello/'
check $? "/blog/ 列表页含文章链接"

echo "== 3. feed =="
if grep -q '^site_url' /usr/local/etc/termblog.toml 2>/dev/null; then
    curl -sf "$BASE/sitemap.xml" -o /tmp/m5-sitemap.xml
    check $? "sitemap.xml 可取"
    xmllint --noout /tmp/m5-sitemap.xml
    check $? "sitemap.xml 是合法 XML"
    curl -sf "$BASE/atom.xml" -o /tmp/m5-atom.xml && xmllint --noout /tmp/m5-atom.xml
    check $? "atom.xml 可取且合法"
else
    echo "⚠️ 跳过 sitemap/atom 检查(未配置 web.site_url)"
fi
curl -sf "$BASE/robots.txt" | grep -q '^Allow: /'
check $? "robots.txt 允许全站"

echo "== 4. jail 侧 [需 ssh 通] =="
(sleep 2; printf 'blog\n'; sleep 1) | timeout 10 $SSH 2>&1 | tr -d '\r' | grep -q hello
check $? "ssh: blog 列表含 hello"
out=$( (sleep 2; printf 'blog ~/blog/hello.md\n'; sleep 3) | timeout 15 $SSH 2>&1 | cat -v )
printf '%s\n' "$out" | grep -qF ']7777;url=/blog/hello/^G'
check $? "ssh: blog 读文章发出 OSC(进入)"
printf '%s\n' "$out" | grep -qF '^[[1m'
check $? "ssh: blog 显示预渲染排版(ANSI 粗体)"
# 复位检查按真实阅读路径: 读完后敲 q 退出 less, blog 脚本随后把地址栏复位为 /
(sleep 2; printf 'blog ~/blog/hello.md\n'; sleep 2; printf 'q'; sleep 1) | timeout 15 $SSH 2>&1 | cat -v \
  | grep -qF ']7777;url=/^G'
check $? "ssh: 退出 less 后 OSC 复位 /"

echo ""
echo "== 结果: $pass 通过, $fail 失败 =="
[ "$fail" -eq 0 ]
