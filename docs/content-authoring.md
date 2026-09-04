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

允许 `png`、`jpg`、`jpeg`、`webp`、`gif`；资源路径字符集为 `[a-z0-9/._-]`。普通位图宽度超过 1080 px 会缩小，处理后单张上限 256 KiB，GIF 上限 512 KiB，单篇总量上限 1.5 MiB。WebP 的终端资产会统一转成 PNG。外部绝对 URL 原样保留，未被文章引用的本地图片不发布并给出告警。

## 评论 attachment

评论目录由 `jailtpl/content/.termblog.toml` 显式配置，不从文章路径或目录名猜测：

```toml
[comments]
# 空字符串代表 HOME 根。
directories = ["", "notes", "projects/demo"]
```

每个目录必须真实存在且不能是符号链接。配置后，该目录中的 `comment` 名称保留给 FIFO：

| 配置目录 | jail FIFO | 评论 target |
| --- | --- | --- |
| `""` | `~/comment` | `/` |
| `notes` | `~/notes/comment` | `/notes/` |
| `projects/demo` | `~/projects/demo/comment` | `/projects/demo/` |

只有文章直属目录被配置时，文章页和终端阅读器才展示评论。同目录文章共享一个 target；空目录或只含录像的目录也可以启用评论。

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
- `.comment-targets.tsv`：构建生成的 FIFO/target 清单。
- `.web-outputs.tsv`：构建器拥有的 Web 输出清单，用于精确清理删文和移动后的旧文件。

生成物不要手工修改或提交。

## 构建与阅读

从仓库根运行 `make build` 会依次构建前端、内容 HTML/ANSI 投影、列表、feed 和清单。构建器在提交新产物前完成路径和输出冲突检查；失败不会清理旧内容。

在 jail 中：

```sh
blog                  # 列出所有正式文章
blog notes/unix       # 通过 HOME-relative key 阅读正式文章
blog ~/help.md        # 也可给出明确文件路径
less ~/notes/unix.md  # 查看原始 Markdown
```

`blog` 只依据 `~/.rendered/.index.json` 判断正式文章。HOME 中运行期新建的 Markdown 仍可作为普通文件读取，但不会获得公开路由、预渲染内容或评论映射。
