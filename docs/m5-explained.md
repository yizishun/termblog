# M5 内容镜像与 HOME 路径模型

termblog 为同一篇文章生成两种投影：搜索引擎和无 JavaScript 访客读取静态 HTML，真人浏览器则接入真实 FreeBSD jail 终端并读取 ANSI 预渲染文本。两种投影来自同一 Markdown，因此公开 URL、正文和终端内容保持一致。

## 一个内容根，两种投影

`jailtpl/content/` 既是内容事实源，也是 guest HOME 的蓝图。`blog/` 没有框架语义；任何可见目录中的小写 `.md` 都是文章：

```text
jailtpl/content/
├── help.md
├── notes/unix.md
├── notes/unix/arch.png
└── demos/boot.cast
```

确定性映射如下：

```text
notes/unix.md
├── jail source    ~/notes/unix.md
├── article key    notes/unix
├── public route   /notes/unix/
├── web output     frontend/dist/notes/unix/index.html
└── ANSI output    ~/.rendered/notes/unix
```

根文章 `help.md` 映射到 `/help/`，不会占用终端首页 `/`。`/blog/` 固定保留为全站文章列表，即使 HOME 中完全没有 `blog/` 目录也存在。

路径和映射由共享的 `termblog-content-model` crate 定义。文章源、article key、公开 route、渲染路径和评论 attachment 是不同概念，各组件不再从一个裸 slug 猜测其他路径。

## 构建期：content-build

Vite 先生成带 hash 的前端入口，随后 `content-build` 扫描整个 content 根。扫描遵循这些边界：

- 任意路径组件以 `.` 开头即属于隐藏控制面，不作为普通内容扫描。
- `.md` 路径限制为小写字母、数字、`-` 和 `/`，扩展名必须精确为 `.md`。
- 符号链接一律拒绝，防止逃逸、目录环和宿主/jail 复制语义不一致。
- 图片相对 Markdown 所在目录解析，合法的 `../` 可以使用，但规范化后不能越过 content 根。
- `.cast` 和其他可见文件属于 HOME 蓝图，不会被误当成 Web 文章。

Markdown 只解析一次，事件流分别进入 HTML 和 ANSI renderer。构建器还生成：

- `/blog/` 全站列表、`robots.txt`，以及配置站点 URL 后的 sitemap/Atom；
- `.rendered/.index` 人类可读列表；
- `.rendered/.index.json` 运行时唯一机器索引；
- 图片 sidecar 和 `.rendered-assets/`；
- `.comment-targets.tsv` 与 `.web-outputs.tsv`。

日期优先使用 Git 最后提交时间，没有历史才回退文件 mtime；标题来自第一个一级标题，摘要来自首段。

### 输出所有权与提交

在写入前，构建器将 Vite/public 文件、系统路由、文章页面、图片、列表和 feed 全部登记进输出 claim 表。两个来源声称同一路径，或文件与目录结构冲突时，构建会列出双方并失败。例如 `assets.md` 会与 `/assets/` 系统前缀冲突，`blog.md` 会与 `/blog/` 列表冲突。

新 HTML、图片和隐藏产物先写入同文件系统的 staging 目录。`.web-outputs.tsv` 记录上一轮由内容构建器拥有的文件，因此删文、移动文章和删除图片时可以精确移除旧输出，而不会碰 Vite/public 文件。部署时，整个静态目录也通过同父目录 staging 换名发布，避免线上出现半套文件。

## 浏览器访问流程

访问 `/notes/unix/` 时，ServeDir 返回包含完整正文的 HTML。爬虫和无 JavaScript 访客到此即可阅读；真人浏览器会建立 WebSocket，开启全新 jail 会话，并让终端接管页面。

镜像 HTML 明确携带：

```html
<meta name="termblog-source" content="notes/unix.md">
<meta name="termblog-route" content="/notes/unix/">
```

前端校验两项后直接使用它们：公开路径来自 `termblog-route`，自动输入的命令是 `blog -- "$HOME/notes/unix.md"`。前端不自行拼 `/blog/` 或猜测 HOME 源路径。

接管时序为：

```text
GET article route
→ 静态正文与等待层
→ WebSocket Open{fresh=1}
→ 新 jail 的 zsh 首帧
→ 自动输入 blog -- "$HOME/<source_rel>"
→ blog 发出本页 OSC 7777
→ 等待层淡出，终端接管
```

失败时，五秒兜底会撤掉等待层并保留静态全文。镜像页每次使用 fresh 会话，首页刷新才允许 attach，因此不需要从旧 scrollback 推断 shell 或分页器状态。

## URL 与终端同步

`webctl url <route>` 输出私有 OSC 7777 控制序列。它作为普通字节穿过 PTY、jaild、WebSocket，最终由 xterm.js 解析；服务端数据面不理解文章协议。前端只接受受限的站内规范路径并调用 `history.replaceState`，不会触发导航。

打开正式文章时，`blog` 从 `.index.json` 取得明确 route 并发进入 OSC；退出阅读器后发 `/`。索引缺失、损坏或映射不一致时，文件仍可按原文读取，但会明确报告“文章映射不可用”，不会猜测 URL。

## jail 中的 blog 与 play

`blog` 参数查找顺序是原样路径、`$HOME/<参数>`、`$HOME/<参数>.md`：

```sh
blog                  # 显示 .rendered/.index
blog notes/unix       # 找到 ~/notes/unix.md
blog ~/help.md        # 明确文件路径
blog -- -draft.md     # -- 结束选项
```

只有机器索引中 source_rel 与 canonical HOME 文件完全匹配的 Markdown 才是正式文章，并使用预渲染、route 和评论映射。其他可读文件保持 cat 式原文行为。

`play` 对 `.cast` 使用同样的 HOME 根语义：原样路径、`$HOME/<参数>`、`$HOME/<参数>.cast`。无参数时递归列出 HOME 中的可见录像，不进入隐藏目录，也不跟随符号链接。

## 图片与 TUI 阅读器

本地图片的 URL 和处理后资产路径都是 content-relative。嵌套文章的 sidecar 与 rendered 文件相邻：

```text
~/.rendered/notes/unix
~/.rendered/notes/unix.images.json
~/.rendered-assets/notes/unix/arch.png
```

具备 `img-iterm2` capability 的 Web 会话使用 TUI 阅读器显示像素图；SSH 和旧前端自动回退到带 OSC 8 链接的稳定占位框。sidecar 从 rendered 文件 basename 推导，避免嵌套 key 被重复拼接。

## 共享目录 scope

评论与统计归属共同由 `.termblog.toml` 显式配置：

```toml
[scopes]
directories = ["", "notes", "projects/demo"]
```

它生成有限且可信的映射：

```text
comment                 /
notes/comment           /notes/
projects/demo/comment   /projects/demo/
```

jaild 从 root-owned 清单取得 `(FIFO, target)`，以 HOME fd 为锚逐层使用 `O_NOFOLLOW` 打开，并确认末端确实是 FIFO；所有 scope 的快照统一映射到 jail 根目录的一棵普通 `/proc` 树，不占用 HOME 中的 `proc` 路径。HTML 与终端只在文章直属目录存在 scope 时展示评论和记录文章统计；配置可以指向空目录，与文章发现互不依赖。

每次会话在 guest fork 前从 `termblog-statd` 取一次统一快照，并原子生成 `/proc/stat` 或 `/proc/<scope>/stat`。该普通文件为 `root:wheel 0444`，会话内保持不变；统计服务不可用只会令文件标记 `stats_status unavailable`，不会阻断会话。

## HOME 模板与部署

`build-template.sh` 将 content 中完整的非隐藏树按原路径复制到 guest HOME，再单独安装生成的 `.rendered/`、`.rendered-assets/`、评论 FIFO 和 jail-root `/proc` 目录树。`.termblog.toml` 等隐藏控制面不会复制，`.zshrc` 仍由模板脚本生成。

常用入口：

- `make build`：Rust、Vite 和内容构建。
- `make tpl`：首次构建 jail 模板。
- `make content`：发布完整静态树并零停机替换模板。
- `tests/verify-m5.sh`：生产镜像、meta、feed、OSC 与终端阅读验收。
- `tests/e2e-content-paths.mjs`：不含 `blog/` 目录的路径、旧输出清理和冲突回归。

更完整的写作、图片和评论规则见 [content-authoring.md](content-authoring.md)。
