# 内容写作与路径规范

`jailtpl/content/` 是内容的唯一事实源，也是访客 HOME 的蓝图。目录中的所有可见文件都会按原路径装入 HOME；其中任意非隐藏目录下、扩展名精确为小写 `.md` 的文件会被发布为文章。`blog/` 只是普通目录，可以使用，也可以完全不存在。

## 路径映射

文章路径必须由小写字母、数字和 `-` 组成，目录可以任意嵌套：

| 源文件 | jail 路径 | article key | Web route |
| --- | --- | --- | --- |
| `help.md` | `~/help.md` | `help` | `/help/` |
| `notes/unix.md` | `~/notes/unix.md` | `notes/unix` | `/notes/unix/` |
| `projects/demo.md` | `~/projects/demo.md` | `projects/demo` | `/projects/demo/` |

根 URL `/` 仍是终端首页；全站文章列表固定在 `/blog/`，与源目录名称无关。因此 `blog.md` 会与列表路由冲突并导致构建失败。

文章不使用 frontmatter。标题取第一个一级标题，缺失时使用文件名；日期取 Git 最后提交时间，无历史时回退到文件 mtime。路径中的大写、下划线、空格、Unicode、隐藏组件以及非小写 `.md` 都会导致构建失败。

## 图片

图片路径相对 Markdown 所在目录解析，规范化后不得逃逸 `jailtpl/content/`。例如：

```text
notes/unix.md 中写 ![架构图](unix/arch.png)
源文件：notes/unix/arch.png
Web URL：/notes/unix/arch.png
```

允许 `png`、`jpg`、`jpeg`、`webp`、`gif`；资源路径字符集为 `[a-z0-9/._-]`。内联预算为普通位图单张 256 KiB、GIF 单张 512 KiB、单篇文章总量 1.5 MiB。在预算内的普通位图宽度超过 1080 px 时会缩小，WebP 终端资产会统一转成 PNG。原图、处理后产物或单篇内联总量超出预算时，构建不会失败：Web 按原字节发布无尺寸限制的高清原图，HTML 和终端正文只显示指向它的可点击链接，不生成终端图像载荷。路径错误、缺少文件、无法识别的图片和输出冲突仍会使构建失败。外部绝对 URL 原样保留，未被文章引用的本地图片不发布并给出告警。

## 目录 scope、评论与统计快照

需要评论或统计的 HOME 目录由 `jailtpl/content/.termblog.toml` 显式配置，不从文章路径或目录名猜测：

```toml
[scopes]
# 空字符串代表 HOME 根。
directories = ["", "notes", "projects/demo"]
```

每个目录必须真实存在且不能是符号链接。启用后，HOME 中只有该目录的 `comment` 路径保留给 FIFO；统计快照统一放在 jail 根目录的一棵普通 `/proc` 树中，HOME 里的 `proc` 仍是普通内容；这里不会挂载 procfs、FUSE 或其他文件系统：

| 配置目录 | jail FIFO | 统计快照 | target |
| --- | --- | --- | --- |
| `""` | `~/comment` | `/proc/stat` | `/` |
| `notes` | `~/notes/comment` | `/proc/notes/stat` | `/notes/` |
| `projects/demo` | `~/projects/demo/comment` | `/proc/projects/demo/stat` | `/projects/demo/` |

只有文章直属目录被配置时，文章页和终端阅读器才展示评论并记录该文章的阅读统计。同目录文章共享一个 target；空目录或只含录像的目录也可以配置。旧的 `[comments]` 拼写仍可读取以便升级，但不能与 `[scopes]` 同时出现。

根 scope 的 `/proc/stat` 与其他 scope 的 `/proc/<scope>/stat` 都是在创建终端会话时生成的普通只读快照，owner/mode 为 `root:wheel 0444`；当前会话中内容不会变化，新会话才会读取最新统计。目录汇总包含终端阅读会话数、静态 HTML 请求数、基于加盐 IP 的近似访客数和 approved 评论数；文章行仅列当前文章，并分别给出 terminal/static 次数。统计服务不可用时仍会创建文件，写入 `stats_status unavailable` 和可用的评论数，不把缺失统计伪装成零。target 汇总会保留已删除或移动文章的历史，因此可能大于当前文章行之和；NAT 会让访客数偏低，动态 IP 会让它偏高。

## 终端录像和普通文件

`.cast` 可以放在任意可见目录。`play` 递归列出 HOME 下的录像，查找顺序是原样路径、`$HOME/<参数>`、`$HOME/<参数>.cast`：

```sh
play
play demos/boot
```

其他可见文件同样按原路径复制进 HOME，但不会生成文章或 Web 资源。任意位置出现符号链接都会让内容构建失败。

## 隐藏控制面与生成物

所有含点前缀组件的路径不参与文章/录像发现和普通 HOME 复制。以下顶层路径由系统使用：

- `.termblog.toml`：作者配置，不复制到 jail。
- `.rendered/`：ANSI 文章、图片 sidecar、可读索引与 `.index.json` 机器索引。
- `.rendered-assets/`：终端 TUI 使用的处理后图片。
- `.comment-targets.tsv`：构建生成的共享 scope 清单（保留 FIFO/target 两列格式）。
- `.web-outputs.tsv`：构建器拥有的 Web 输出清单，用于精确清理删文和移动后的旧文件。

生成物不要手工修改或提交。

## 构建与阅读

从仓库根运行 `make build` 会依次构建前端、内容 HTML/ANSI 投影、列表、feed 和清单。构建器在提交新产物前完成路径和输出冲突检查；失败不会清理旧内容。

在 jail 中：

```sh
blog                  # 列出所有正式文章
blog notes/unix       # 通过 HOME-relative key 阅读正式文章
blog ~/help.md        # 也可给出明确文件路径
cat /proc/stat       # 查看本会话启动时冻结的根 scope 统计
less ~/notes/unix.md  # 查看原始 Markdown
```

`blog` 只依据 `~/.rendered/.index.json` 判断正式文章。HOME 中运行期新建的 Markdown 仍可作为普通文件读取，但不会获得公开路由、预渲染内容或评论映射。
