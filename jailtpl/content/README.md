# 博客内容目录

本目录是文章的唯一事实源, 只放两块内容:

- `blog/` —— 文章目录(每篇一个 md, 可嵌套子目录, **无 frontmatter**);
- `.rendered/` —— content-build 的生成物(终端预渲染 + 列表), 勿手改、勿提交。

本 README 是仓库侧写作规范, 不拷进 jail。

约定:

- 文件名(相对 `blog/`)即 slug, 字符集限 `[a-z0-9/-]`(违规构建失败):
  `blog/hello.md` → 网页镜像 `/blog/hello/`;
- 标题取文中第一个 `# 一级标题`(缺失则用文件名);日期取 git 最后提交时间;
- `blog/` 下的 md 由 content-build 同时编译为网页镜像(frontend/dist/blog/)
  与终端预渲染(`.rendered/`, 与文章路径一一对应)。

构建时 build-template.sh 把两块分别拷进 jail:

- `blog/` → 访客家目录 `~/blog/`(与 URL 前缀 `/blog/` 一一对应);
- `.rendered/` → `~/.rendered/`(隐藏工具目录)。

jail 里的读法:

- `blog`                  文章列表;
- `blog hello`            = `blog ~/blog/hello.md`, 读**预渲染排版**(76 列折行、加粗/表格已排好);
- `blog <其他文件.md>`     任意路径任意层级, 像 cat 一样读原始内容(不渲染、不同步地址栏);
- `cat` / `less <文件.md>` 读原始未渲染的 markdown。
