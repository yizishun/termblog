# plan_path_refactor —— 以 content 为 HOME 根的路径解耦重构

> 性质：路径模型与内容发布链路重构。目标是彻底移除 `blog/` 作为隐含系统根的语义，
> 使 `jailtpl/content/` 成为 jail 中 guest HOME 的唯一蓝图；文章、图片、录像和评论
> attachment 都以 content 根相对路径描述。本文只给出设计定案与执行顺序，不包含实现。

## 1. 背景与问题

当前系统把 `jailtpl/content/blog/` 同时当成：

1. 文章发现根；
2. slug 计算根；
3. jail 内文章根 `~/blog/`；
4. Web URL 根 `/blog/`；
5. HTML 与图片静态产物根 `frontend/dist/blog/`；
6. ANSI 预渲染与图片 manifest 的相对路径根；
7. 评论 FIFO 与评论 target 的推导根；
8. `blog`、`play` 两个 jail 命令的默认查找根。

因此，源码只要移出 `content/blog/`，不同组件便会分别出现漏扫描、找不到预渲染、
URL 错误、评论错绑、静态文件残留或 jail 中根本没有该文件等问题。根因不是某一个
硬编码字符串，而是一个 `slug` 同时承担了源文件路径、路由、产物键和评论归属四种职责。

本次重构以如下原则替代现状：

```text
jailtpl/content/<visible path>  <──一一映射──>  /home/guest/<visible path>
```

`blog/` 可以继续作为作者自选的普通目录，但不再具有任何框架语义；即使它完全不存在，
文章构建、终端阅读、录像播放、评论和 Web 镜像也必须正常工作。

## 2. 范围

### 2.1 做

1. 以 `jailtpl/content/` 为统一内容根和 guest HOME 蓝图。
2. 所有非隐藏路径下的 `.md` 都视为文章，目录层级任意。
3. 文章默认 Web URL 由 content 根相对路径去掉 `.md` 后直接生成。
4. 图片相对 Markdown 所在目录解析，但不得逃逸 content 根。
5. `.cast` 可以放在任意非隐藏目录；`play` 不再依赖 `~/blog`。
6. 评论目录由隐藏配置显式列出，与文章是否存在、目录里有几篇文章完全无关。
7. 对启用评论的目录注入特殊 FIFO `comment`；该名称在这些目录内明确保留。
8. 统一路径模型，拆开源路径、文章键、公开路由、渲染路径和评论 attachment。
9. 对 Web 系统路由、前端资产、文章页面、图片产物和 feed 产物做完整冲突检测；
   冲突时构建失败并同时打印两个占用来源。
10. 改造静态产物清理与部署，保证任意顶层目录删文后不残留旧页面。
11. 修正现有嵌套 slug 图片 manifest 定位问题。
12. 更新单测、端到端测试、部署验收和文档。

### 2.2 不做

- 不改变评论审核、限流、持久化和会话快照语义。
- 不增加 Web 投稿表单；投稿仍通过 jail 中的特殊 `comment` FIFO。
- 不允许内容路径逃逸 HOME 或静态输出根。
- 不把运行期访客新建的 Markdown 自动变成公开文章；正式文章集合仍由构建产物确定。
- 不自动迁移因作者主动移动目录而改变 target 的历史评论。
- 不改变根页面 `/` 的终端首页职责。
- 不为任意 Unicode、空格或 shell 元字符设计 URL 编码；本轮继续使用安全路径字符集。

## 3. 决策记录

| # | 决策 |
| --- | --- |
| D1 | `jailtpl/content/` 是 guest HOME 的唯一内容根；部署不再单列 `blog/` 和 `help.md`。 |
| D2 | 任一非隐藏 `.md` 都是文章；不使用目录名、frontmatter 或显式文章清单判断文章身份。 |
| D3 | `blog/` 只是普通路径组件；系统逻辑不得出现“位于 blog 下才是文章”的判断。 |
| D4 | 文章默认 route 为 `/<content-relative path without .md>/`，始终带首尾 `/`。 |
| D5 | 根目录 Markdown 不映射到 `/`；例如 `help.md → /help/`、`index.md → /index/`。根 `/` 仍是终端首页。 |
| D6 | 路径布局任意不等于字符任意。文章键继续限制为 `[a-z0-9/-]+`；源文件扩展名必须精确为小写 `.md`。 |
| D7 | 图片资源路径继续限制为 `[a-z0-9/._-]+`，相对文章目录解析，规范化后必须留在 content 根内。 |
| D8 | 所有含点前缀的路径组件均属于隐藏域，不参与文章/录像发现和普通 HOME 内容复制；已知系统隐藏路径按白名单单独处理。 |
| D9 | 评论 attachment 由隐藏配置显式列目录；不再从文章父目录推导。 |
| D10 | `comment` 是启用评论目录中的专用 FIFO 和保留名称；未启用评论的目录中可作为普通文件名使用。 |
| D11 | 评论 target 等于目录对应的规范 URL：HOME 根是 `/`，`notes` 是 `/notes/`。 |
| D12 | `blog` 命令只把构建索引中登记的 HOME 内 Markdown 当正式文章；其他可读文件仍按原始文件查看。 |
| D13 | HTML meta 直接携带源路径和 route；前端不得再从 slug 拼 `~/blog` 或 `/blog`。 |
| D14 | `/blog/` 暂时保留为全站文章列表 URL，以保持现有入口兼容；它不要求 HOME 中存在 `blog/`。`content/blog.md` 因 route 冲突而构建失败。 |
| D15 | Web URL 或输出文件发生任何冲突均 fail-fast，不采用覆盖顺序决定赢家。 |
| D16 | 符号链接不属于本轮支持的内容布局；扫描遇到 symlink 直接报错，防止逃逸、环和复制语义不一致。 |
| D17 | 旧内容 `content/blog/hello.md` 重构后自然仍映射为 `~/blog/hello.md` 与 `/blog/hello/`，无需为现有文章改 URL。 |
| D18 | `.rendered/.index` 保留为人类可读列表；新增有版本号的 `.rendered/.index.json` 作为 jailbin 唯一机器事实源。 |

## 4. 路径模型与术语

### 4.1 基础类型

实现中禁止继续仅靠裸 `String slug` 在各组件间传递全部信息。建议新增一个无 I/O 的共享
crate（暂名 `crates/content-model`，包名 `termblog-content-model`），集中定义验证和映射：

```rust
struct ArticlePath {
    // 含 .md，相对 content/HOME，例如 notes/unix.md
    source_rel: PathBuf,
    // 不含 .md，使用 / 分隔，例如 notes/unix
    key: String,
    // 规范公开路由，例如 /notes/unix/
    route: String,
    // 文章直属目录；根目录文章为 ""
    directory_rel: String,
}

struct CommentAttachment {
    // 相对 HOME 的目录；根目录为 ""
    directory_rel: String,
    // FIFO 相对 HOME 路径，例如 notes/comment
    fifo_rel: String,
    // 评论存储/API target，例如 /notes/
    target: String,
}
```

共享 crate 至少提供：

- content 相对路径的逐组件校验；
- `.md` 路径到 `ArticlePath` 的唯一转换；
- 目录路径到 `CommentAttachment` 的唯一转换；
- route/target 的规范化与验证；
- 路径是否含隐藏组件的判断；
- 路径转 URL 时统一使用 `/`，不依赖宿主平台分隔符。

`content-build`、`jailbin`、`jaild`、`commentd` 依赖该 crate，删除目前分散在四处的
slug/target 验证副本。前端不重新实现映射，只消费构建器写入 HTML 的明确值。

### 4.2 确定性映射

| content 源 | jail 路径 | article key | Web route | ANSI 产物 |
| --- | --- | --- | --- | --- |
| `help.md` | `~/help.md` | `help` | `/help/` | `~/.rendered/help` |
| `notes/unix.md` | `~/notes/unix.md` | `notes/unix` | `/notes/unix/` | `~/.rendered/notes/unix` |
| `blog/hello.md` | `~/blog/hello.md` | `blog/hello` | `/blog/hello/` | `~/.rendered/blog/hello` |
| `a/b/c.md` | `~/a/b/c.md` | `a/b/c` | `/a/b/c/` | `~/.rendered/a/b/c` |

图片示例：

```text
文章：       content/notes/unix.md
Markdown：   ![图](unix/jail.png)
源图片：     content/notes/unix/jail.png
Web URL：    /notes/unix/jail.png
Web 文件：   frontend/dist/notes/unix/jail.png
TUI 资产：   content/.rendered-assets/notes/unix/jail.png
```

`../` 可以在 content 根内部正常工作：

```text
content/notes/bsd/jail.md + ../shared/logo.png
→ content/notes/shared/logo.png
→ /notes/shared/logo.png
```

任何规范化后越过 content 根的引用构建失败。

### 4.3 隐藏路径

以下顶层路径是已知系统控制面：

| 路径 | 所有者 | 是否安装进 jail | 用途 |
| --- | --- | --- | --- |
| `.termblog.toml` | 作者 | 否 | 评论目录等内容构建配置 |
| `.rendered/` | 构建器 | 是 | ANSI、图片 sidecar、文章索引 |
| `.rendered-assets/` | 构建器 | 是 | TUI 使用的处理后图片 |
| `.comment-targets.tsv` | 构建器 | 否；另装到 root-owned 系统路径 | jaild 的可信 FIFO/target 清单 |
| `.web-outputs.tsv` | 构建器 | 否 | 上轮由 content-build 写入 dist 的文件清单 |

`.zshrc` 继续由模板脚本生成，不由 content 覆盖。扫描和普通复制遇到任意含 `.` 前缀的
组件都跳过；顶层未知隐藏项给出警告，避免作者误以为它会成为 HOME 内容。已知生成目录
不得作为文章扫描输入，否则第二次构建会把产物重新扫描进去。

## 5. 评论目录配置与语义

### 5.1 配置格式

新增 `jailtpl/content/.termblog.toml`：

```toml
[comments]
# 空字符串代表 HOME 根；其余均为相对 content/HOME 的目录。
directories = ["", "blog", "notes", "projects/demo"]
```

解析规则：

1. 每项必须是规范相对目录，不能以 `/` 开头或结尾；根目录只允许写成 `""`。
2. 禁止空组件、`.`、`..`、隐藏组件、反斜杠和当前 URL 白名单之外的字符。
3. 配置项必须对应 content 中真实存在的非 symlink 目录；根目录天然存在。
4. 重复项构建失败，并打印重复值与配置位置。
5. 对每个目录检查 `<dir>/comment`：若源树中已经存在文件、目录或 symlink，则构建失败，
   明确说明该路径已由评论 FIFO 保留。
6. 是否启用评论完全不取决于目录内有没有 Markdown；空目录、纯录像目录也可以启用。

生成清单示例：

```text
comment                 /
blog/comment            /blog/
notes/comment           /notes/
projects/demo/comment   /projects/demo/
```

### 5.2 展示规则

- 文章直属目录在显式 attachment 清单中：HTML 和终端阅读器展示该目录评论。
- 文章直属目录未启用评论：HTML 不生成评论 section，终端也不显示“评论暂不可用”。
- 根目录文章（例如 `help.md`）使用根 attachment `/`，不再对 `help.md` 写特殊分支。
- 同目录多篇文章仍共享一个 target，这是目录 attachment 的自然结果，而非文章推导规则。
- 配置中允许没有文章的目录；本轮不新增专门的评论浏览页面或命令，只保证 FIFO、存储和
  API target 独立成立。以后可在不改数据模型的前提下增加目录评论浏览入口。

### 5.3 协议与安全边界

`valid_target` 泛化为：

- `/`；或
- `/<segment>(/<segment>)*/`；
- segment 字符集与目录 URL 规则一致；
- 禁止 `//`、`.`、`..`、缺尾斜杠及超长 target。

jaild 仍从 root-owned `.comment-targets.tsv` 获取有限集合，并继续：

- 以 guest HOME fd 为锚逐级 `openat`；
- 每层 `O_NOFOLLOW`；
- 最终 `fstat` 确认 FIFO；
- shell 启动前固定 `(fd, target)`；
- 运行期移动/删除 FIFO 不改变 target；
- 不接受 guest 临时创建的新 comment 路径。

`target_for_fifo` 改为通用目录映射：`comment → /`，`a/b/comment → /a/b/`，不再识别
或要求 `blog/` 前缀。构建脚本的 shell 侧校验必须采用同一规则，不能保留第二套 blog
专用 case 分支。

已有 `/blog/` 评论数据继续合法且无需迁移。作者以后把内容从 `blog/` 移到 `notes/` 时，
target 按目录语义变为 `/notes/`；旧 `/blog/` 数据保留但不会自动搬迁，需显式管理迁移。

## 6. Web 路由与冲突检测

### 6.1 输出所有权表

`content-build` 在写任何文件前建立完整的 claim 表：

```text
规范 URL / 输出相对路径 → 产物类型 + 源文件
```

产物类型至少包括：

- Vite/public 已有文件；
- 系统 HTTP 路由；
- 文章 `index.html`；
- 文章处理后图片；
- `/blog/` 全局列表；
- `sitemap.xml`、`atom.xml`、`robots.txt`；
- content-build 新一轮将要提交的全部产物。

上一轮 manifest 中登记的旧内容文件在扫描现有 dist owner 时排除：它们是本轮允许替换或
删除的旧版本，不应与新版本冲突；manifest 之外的现有 dist 文件才视为 Vite/public owner。

注册第二个 owner 时立即记录冲突；扫描完成后一次性打印全部冲突并失败。例如：

```text
Web 路径冲突: /assets/
  系统占用: Vite 静态资源前缀 frontend/dist/assets/
  内容占用: jailtpl/content/assets.md → /assets/

Web 输出冲突: notes/logo.png
  图片一: jailtpl/content/notes/logo.webp 转换为 PNG
  图片二: jailtpl/content/notes/logo.png
```

### 6.2 保留项

至少预注册：

- `/`（终端首页）；
- `/ws`；
- `/api/`，尤其 `/api/comments`；
- `/assets/`；
- `/fonts/`；
- `/style.css`、`/blog.css`、`/favicon.svg`；
- `/sitemap.xml`、`/atom.xml`、`/robots.txt`；
- `/blog/` 的全站文章列表文件本身。

目录和子路由的共存按实际 HTTP/文件语义判断。`/blog/` 列表与 `/blog/hello/` 可以共存，
但 `content/blog.md → /blog/` 会与列表精确冲突。`content/assets.md → /assets/` 则与整个
前端资产前缀冲突。

构建器还要扫描 Vite 完成后的 `frontend/dist`，把真实存在且不属于上一轮内容产物的文件
注册为前端 owner，避免保留清单遗漏未来新增资源。

### 6.3 删除旧产物

当前只删除 `dist/blog` 的策略不再成立。改为由构建器维护隐藏的 Web 产物 manifest，建议：

```text
jailtpl/content/.web-outputs.tsv
```

每行记录 content-build 拥有的 `frontend/dist` 相对文件路径和来源。新一轮提交时：

1. 完成扫描、解析、图片处理和全部冲突校验，期间不动旧产物。
2. 把新 HTML、图片、feed 和两个隐藏产物目录写入同文件系统的临时目录。
3. 读取旧 `.web-outputs.tsv`，只删除旧清单登记的文件，拒绝其中的绝对路径、`.`、`..`。
4. 自底向上删除因此变空的目录，但不删除含未知文件的目录。
5. 安装新产物，逐文件使用临时文件 + rename。
6. 最后原子替换 `.web-outputs.tsv`、`.rendered/`、`.rendered-assets/`、
   `.comment-targets.tsv`。

构建在提交前失败时，线上可用的旧内容不变。迁移到新版本的第一次构建需显式识别并清理
旧版 `frontend/dist/blog` 内容产物，同时不能删除 Vite 的 `blog.css`。

部署到 `/usr/local/share/termblog/frontend` 时也不能继续只 `rm -rf "$STATIC_DIR/blog"`。
部署脚本应复制到同父目录 staging，校验完成后整体换目录，或采用等价的 `--delete`
同步方案；目标是删文、移动文章、删除图片后线上不存在僵尸路径，且失败不会留下半套站点。

## 7. content-build 重构

### 7.1 扫描

将 `scan_blog` 替换为 `scan_content`：

1. 根是 CLI `--content` 指定目录，不再 `join("blog")`。
2. 递归使用 `symlink_metadata`；遇 symlink 收集错误，不跟随。
3. 任一组件以 `.` 开头则进入隐藏路径处理，不参与普通扫描。
4. `.md` 一律转为 `ArticlePath`，违规路径全部收集后统一失败。
5. 图片扩展名进入资源表；`.cast` 为已知 jail 资源，不参与 Web 图片管线。
6. 其他普通文件原样属于 HOME 蓝图，但 content-build 不为它生成 Web 产物；可给信息级提示，
   不应再用“忽略非 markdown 文件”暗示部署也会忽略它。
7. 扫描顺序稳定排序，保证索引、错误输出和产物 manifest 可复现。

当前 `jailtpl/content/README.md` 必须移到 `docs/content-authoring.md`（名称可在实施时定案）。
否则它既会按“所有 Markdown 都是文章”变成 `/README/`，又会违反小写路径规则。

### 7.2 Article 数据结构

`Article.slug` 替换为 `Article.path: ArticlePath`。调用方禁止自己 `format!("/blog/{slug}")`。
图片字段保存规范 URL 和 content-relative 资源路径，错误信息使用 `source_rel`，不再把路径
概念统称为 slug。

排序仍为完整提交时间倒序，同时间按 article key 升序。

### 7.3 图片

`normalize_local_path` 接收文章 `source_rel` 或其直属目录，而不是 blog-relative slug：

- 基准是 Markdown 文件所在目录；
- 结果是 content-relative 资源路径；
- 逃逸边界是 content 根；
- Web URL 是 `/<normalized resource path>`；
- HTML、ANSI、OG、Atom 共用处理后的明确 URL；
- `.rendered-assets` 保持与 content-relative 路径同构。

webp 转 PNG 后必须把目标路径重新加入 claim 表。若 `x.webp` 与 `x.png` 最终都声称
同一目标，不再使用 HashMap 的先到先得，而是构建失败。

### 7.4 ANSI 与 sidecar

ANSI 产物和图片 manifest 使用完整 article key：

```text
notes/unix.md
→ .rendered/notes/unix
→ .rendered/notes/unix.images.json
```

reader 不能使用 `with_file_name(format!("{full_key}.images.json"))`。它应从 rendered 文件
自身 basename 推导相邻 manifest，或直接由索引传入明确 manifest 路径，从而修复当前
嵌套 key 会重复目录组件的问题。

### 7.5 索引

保留 `.rendered/.index` 作为裸 `blog` 可直接展示的人类文本，内容改为列出完整
HOME-relative key；新增 `.rendered/.index.json` 作为 jailbin 唯一机器事实源：

```json
{
  "version": 1,
  "articles": [
    {
      "date10": "2026-09-04",
      "source_rel": "notes/unix.md",
      "key": "notes/unix",
      "route": "/notes/unix/",
      "title": "Jail notes"
    }
  ]
}
```

- `source_rel` 含 `.md`；
- `key` 和 `route` 是构建器生成的规范值；
- JSON 使用 `deny_unknown_fields`、显式 `version` 和完整字段校验；
- `.index` 只负责展示，jailbin 不得从其中反推路径；
- `.index.json` 缺失或损坏时，裸 `blog` 仍可展示 `.index`，但打开正式文章时明确报告
  “文章映射不可用”，不能静默使用猜测出来的 route；
- 标题中的控制字符、Tab 和换行在生成两个索引前按现有元数据规则规范化。

### 7.6 HTML、列表、feed

- mirror canonical、OG URL、文章 meta、列表链接统一读取 `ArticlePath.route`。
- meta 至少写入 `termblog-source`（如 `notes/unix.md`）和 `termblog-route`
  （如 `/notes/unix/`）；移除前端对 `termblog-slug` 含义的猜测。
- footer 命令提示使用可直接执行的 HOME 相对路径，例如 `blog notes/unix`。
- 评论 section 只在文章直属目录存在显式 attachment 时生成，并直接写入 target 与 FIFO hint。
- `/blog/` 全局列表收集整个 content 树的所有文章，不代表 `blog/` 源目录。
- sitemap 与 Atom 遍历所有文章并使用明确 route；不再插入第二层 `/blog/`。
- 本地图、OG image、Atom 正文使用图片的明确公开 URL。

## 8. jailbin 重构

### 8.1 `blog`

启动时读取机器索引，建立：

```text
source_rel → key / route / rendered / comment attachment
```

参数解析顺序改为：

1. 用户原样路径（相对 cwd 或绝对路径）；
2. `$HOME/<arg>`；
3. `$HOME/<arg>.md`。

不再尝试 `$HOME/blog/<arg>`。列表输出展示 HOME-relative key，因此现有文章使用：

```text
blog blog/hello
blog ~/blog/hello.md
```

而任意布局可使用：

```text
blog notes/unix
blog ~/projects/demo/readme.md
```

解析出实际文件后：

- 与索引中的规范 `source_rel` 精确匹配才算正式文章；
- 正式文章读取 `.rendered/<key>`，发索引给定的 route OSC，并按索引/评论清单展示评论；
- HOME 内未登记或 HOME 外文件仍可原样读，但不读预渲染、不发文章 route、不展示评论；
- 删除 `is_home_help` 特例；`help.md` 与其他根目录文章走同一链路；
- 增加 `--` 结束选项，前端自动命令使用 `blog -- "$HOME/<source_rel>"`；路径白名单仍是
  第一层防护，引用方式是第二层 shell 注入防护。

是否保留裸 `blog hello` 对 `blog/hello.md` 的旧快捷解析：本计划选择不保留隐式 blog
fallback，以保证命令语义与目录模型一致；文档和验收同步改成 `blog blog/hello`。

### 8.2 reader

- `try_run` 接收明确的 rendered、manifest、assets 路径，不再从完整 slug 拼路径。
- `--dump-image-frame` 使用 article key，并通过同一索引/路径助手定位文件。
- `.rendered-assets` 仍是唯一 TUI 图片字节源。
- manifest 中图片路径改为 content-relative，而不是“相对 `~/blog`”。

### 8.3 评论快照校验

删除 jailbin 内 `/blog/` 专用 target validator，复用共享路径模型的通用 target 校验。
文章是否展示评论应查已安装的可信 attachment 清单或索引生成字段，不允许仅凭父目录自行
假设该目录启用了评论。

### 8.4 `play`

- 无参数时从 `$HOME` 递归列出所有非隐藏 `.cast`。
- 不跟随 symlink，不进入任何点前缀目录，尤其 `.rendered-assets`。
- 参数解析为原样、`$HOME/<arg>`、`$HOME/<arg>.cast`，不再补 `$HOME/blog`。
- 列表输出 HOME-relative、去 `.cast` 的可复制参数。
- 当前录像迁移后调用形式为 `play blog/demo`、`play blog/hello/demo`；绝对路径继续支持。

## 9. 前端重构

### 9.1 镜像页接管

`frontend/src/main.ts` 改为读取构建器给出的两个明确 meta：

```text
termblog-source = notes/unix.md
termblog-route  = /notes/unix/
```

- `WANT` 直接等于 `termblog-route`；
- 自动命令使用 `termblog-source`，不再拼 `~/blog/${slug}.md`；
- takeover 仍以收到完全相同的 route OSC 为准；
- fresh session、等待层、超时降级和地址栏同步时序保持不变；
- meta 缺一项、route 不合法或 source 不合法时不自动执行命令，直接走静态降级并记录错误。

`osc.ts` 的 URL 白名单本身已支持一般小写路径，保留并补充 `/notes/unix/` 等测试；不得重新
加入 `/blog/` 前缀要求。

### 9.2 评论提示

HTML 评论 section 直接携带：

```html
data-comments-target="/notes/"
data-comments-fifo="~/notes/comment"
```

`comments.ts` 使用这两个值显示空状态，不再从 `/blog/.../` 切片反推 jail 路径。
根目录 attachment 使用 `~/comment`。前端仍只通过 `/api/comments` 读取，不增加投稿接口。

## 10. 模板构建与部署

### 10.1 HOME 内容复制

`build-template.sh` 删除 `content/blog/.` 和 `content/help.md` 两个特例，改为：

1. 确认 content 根是真实目录。
2. 遍历并复制全部非隐藏顶层项，保留其内部非隐藏树结构。
3. 不跟随 symlink；由于 content-build 已拒绝 symlink，脚本再做防御性检查。
4. 明确不复制 `.termblog.toml`、`.comment-targets.tsv`、`.web-outputs.tsv`。
5. 单独复制生成物 `.rendered/`、`.rendered-assets/` 到 HOME 同名隐藏目录。
6. `.zshrc` 仍由模板生成，content 无权覆盖。
7. 普通内容复制完后再根据可信清单创建 `comment` FIFO；若路径已存在则 fail-closed。
8. 最后统一设置 guest owner 和预期权限。

README/欢迎语中的示例改成完整的 HOME-relative 路径，不再承诺 `blog hello` 或
`play hello/demo`。

### 10.2 清单安装

`.comment-targets.tsv` 继续安装为 `/usr/local/share/termblog/comment-targets.tsv`，权限和
root-owned 校验不变。模板构建脚本只校验通用 `directory/comment ↔ /directory/` 关系，
不再含 `blog/comment` case。

### 10.3 静态发布

`deploy.sh --static-only` 与全量部署统一采用完整静态树同步/换目录，删除只清理
`$STATIC_DIR/blog` 的逻辑。发布步骤须满足：

- 新树完整后才可见；
- 删除旧文章和旧图片；
- 保留权限 `a+rX`；
- staging/old 路径目标明确，失败可回滚；
- 不删除 static 根的父目录或其他服务数据。

Makefile 的 `clean` 同步清理 `.rendered-assets`、`.comment-targets.tsv`、`.web-outputs.tsv`
等生成物，但不能删除 `.termblog.toml` 作者配置。

## 11. 分文件实施清单

### 11.1 新增

| 文件 | 改动 |
| --- | --- |
| `crates/content-model/Cargo.toml` | 共享路径模型 crate。 |
| `crates/content-model/src/lib.rs` | ArticlePath、CommentAttachment、route/target/path 校验与单测。 |
| `jailtpl/content/.termblog.toml` | 显式评论目录配置。 |
| `docs/content-authoring.md` | 从 content 根移出的写作与布局说明。 |

### 11.2 内容构建器

| 文件 | 改动 |
| --- | --- |
| `crates/tools/content-build/src/main.rs` | content 根扫描、ArticlePath、评论配置、claim 表、事务式产物提交与新索引。 |
| `crates/tools/content-build/src/meta.rs` | 删除本地 slug 验证副本，索引字段与标题规范化。 |
| `crates/tools/content-build/src/img.rs` | content-relative 图片解析、URL、逃逸与产物冲突。 |
| `crates/tools/content-build/src/ansi.rs` | 使用明确图片 URL/key，更新 manifest 语义。 |
| `crates/tools/content-build/src/html.rs` | route/source meta、通用链接、可选评论 section/fifo。 |
| `crates/tools/content-build/src/feed.rs` | sitemap/Atom 使用 ArticlePath.route。 |
| `crates/tools/content-build/Cargo.toml` | 依赖共享路径模型及配置解析所需依赖。 |

### 11.3 jail 运行时与评论服务

| 文件 | 改动 |
| --- | --- |
| `crates/tools/jailbin/src/blog/mod.rs` | HOME 根文章识别、机器索引、明确 route/render 路径、去 help/blog 特例。 |
| `crates/tools/jailbin/src/blog/reader.rs` | 明确 manifest 路径，修复嵌套 key。 |
| `crates/tools/jailbin/src/blog/comments.rs` | 通用 target 校验，按显式 attachment 展示。 |
| `crates/tools/jailbin/src/play.rs` | HOME 根 `.cast` 查找和列表。 |
| `crates/tools/jailbin/Cargo.toml` | 依赖共享路径模型。 |
| `crates/servers/commentd/src/protocol.rs` | 通用目录 target 验证。 |
| `crates/servers/commentd/Cargo.toml` | 依赖共享路径模型。 |
| `crates/servers/jaild/src/jail.rs` | 通用 FIFO/target 清单校验。 |
| `crates/servers/jaild/Cargo.toml` | 依赖共享路径模型。 |
| `Cargo.toml` | 将共享路径模型加入 workspace。 |

### 11.4 前端、部署与文档

| 文件 | 改动 |
| --- | --- |
| `frontend/src/main.ts` | source/route meta，通用自动命令与接管。 |
| `frontend/src/comments.ts` | 使用显式 target/fifo，不再解析 `/blog/`。 |
| `deploy-scripts/build-template.sh` | content→HOME 全量非隐藏映射、通用 FIFO。 |
| `deploy-scripts/deploy.sh` | 完整静态树替换和任意路径僵尸清理。 |
| `Makefile` | clean 生成物集合、注释。 |
| `README.md` | 架构和命令示例。 |
| `docs/m5-explained.md` | 双投影、URL、终端接管说明。 |
| `jailtpl/content/help.md` | 新的完整路径命令与评论示例。 |
| `docs/architecture.drawio` | 内容根和产物路径图。 |
| `.gitignore` | 忽略 `.web-outputs.tsv`，继续忽略生成渲染和评论清单；不忽略 `.termblog.toml`。 |

### 11.5 测试

| 文件 | 改动 |
| --- | --- |
| `tests/e2e-content-paths.mjs` | 新增无 `blog/` fixture、route/冲突/清理/隐藏路径主测试。 |
| `tests/e2e-image.mjs` | 图片路径与 manifest 改成 content-relative，覆盖嵌套 article key。 |
| `tests/e2e-image-playwright.mjs` | 用通用文章 route 验证浏览器图片与接管。 |
| `tests/verify-m5.sh` | 更新页面、meta、终端命令和 OSC 验收。 |
| `tests/verify-comments.sh` | 增加非 blog target 与显式 attachment/FIFO 验收。 |

各 Rust 模块内现有依赖 `blog` 结构的单测同步改成至少一组 `notes/` 和一组普通 `blog/`
兼容示例；不能只机械替换字符串而失去“blog 不存在”这一回归条件。

历史 `plan_*.md` 和 `*.diff` 是过程资料，不参与运行；除非要求维护历史叙述，不机械重写。

## 12. 实施阶段

### 阶段 0：基线与夹具

1. 记录当前 `cargo test`、前端 build、content-build 和现有 e2e 结果。
2. 新建测试 fixture，故意不含 `blog/`：

```text
content/
├── help.md
├── notes/unix.md
├── notes/unix/pixel.png
├── demos/boot.cast
├── empty-comments-dir/
└── .termblog.toml
```

3. 保留现有 `blog/hello.md` fixture 验证向后 URL 兼容。
4. 先写失败测试表达新规则，再动生产实现。

### 阶段 1：共享路径模型

1. 新增 crate 与 workspace member。
2. 完成路径、route、target、隐藏组件和 attachment 的纯函数单测。
3. 在不改行为的前提下让现有组件逐步引用共享 validator。
4. 确认 Linux 开发环境与 FreeBSD 目标都使用相同 `/` 规范结果。

### 阶段 2：content-build 内部模型与扫描

1. `Article.slug` 替换为 `ArticlePath`。
2. 根扫描改成 content，跳过隐藏域并拒绝 symlink。
3. 全部文章、图片和录像分类改用 content-relative 路径。
4. 引入 `.termblog.toml` 和显式评论 attachment。
5. 此阶段先保留旧写出位置的适配层，确保中间提交可编译和测试。

### 阶段 3：所有投影与冲突事务

1. HTML、ANSI、图片、列表、feed 全部改用明确 route/key。
2. 实现 claim 表和一次性错误汇总。
3. 实现 `.web-outputs.tsv`、staging 和旧产物精确清理。
4. 修复 nested key manifest。
5. 用无 `blog/` fixture 完成 content-build 集成验证。

### 阶段 4：jailbin 与前端

1. 升级文章机器索引。
2. `blog` 改为 HOME 根和索引驱动。
3. reader、评论展示、`play` 跟进。
4. HTML meta 和 `main.ts` 同步切换，不能先单独发布一端。
5. 更新终端/静态切换 e2e。

### 阶段 5：评论通用化

1. commentd target validator 泛化。
2. jaild 与模板脚本改为通用 attachment 清单。
3. 前端 empty hint 使用显式 FIFO。
4. 验证“有文章无 attachment”“无文章有 attachment”“根 attachment”三种情况。

### 阶段 6：HOME 复制与部署

1. 模板改为完整非隐藏树复制。
2. 移走 content/README.md。
3. 部署改为完整静态树替换。
4. 在实际 FreeBSD/ZFS 环境执行模板 replace 和新会话验收。

### 阶段 7：文档与清理

1. 删除生产代码、错误信息和活跃文档中的结构性 `content/blog`、`~/blog` 拼接。
2. 保留作为普通示例路径的 `blog/hello`，但文字必须明确它不特殊。
3. 更新写作、评论配置、命令和故障排查文档。
4. 跑完整测试矩阵和最终搜索审计。

## 13. 测试矩阵

### 13.1 路径模型单测

- `help.md → key help → /help/`。
- `notes/a.md → notes/a → /notes/a/`。
- `blog/a.md` 与任意普通目录同规则。
- 拒绝空路径、绝对路径、`.`、`..`、`//`、尾 `/`、反斜杠。
- 拒绝大写、下划线、空格、Unicode 文章 route（本轮字符集决策）。
- 任一隐藏组件使路径进入隐藏域而非文章。
- Windows 风格输入不能绕过 `/` 组件校验。

### 13.2 内容扫描与文章

- 完全没有 `content/blog/` 仍能构建全部文章。
- 根和多层目录所有 `.md` 都进入列表、HTML、ANSI、sitemap、Atom。
- `.rendered`、`.rendered-assets` 和嵌套隐藏目录不会被二次扫描。
- `README.md` 等非法 article path 一次性报告清楚。
- symlink 文件、symlink 目录、环和指向 content 外的链接全部失败。
- 非 Markdown 普通文件最终存在于 jail HOME，但不生成文章页面。

### 13.3 图片

- 根文章、嵌套文章、同名资源目录、`./`、合法 `../`。
- 逃逸 content 根失败。
- URL 与 content-relative 资源路径一致，不额外加 `/blog`。
- webp→png 与已有 png 目标冲突失败。
- `.rendered-assets`、dist 和 manifest 字节/尺寸一致。
- `notes/a/b.md` 的 `b.images.json` 能被 reader 正确找到。

### 13.4 评论

- 配置根目录生成 `comment ↔ /`。
- 配置无文章目录仍生成 FIFO 清单。
- 有文章但未配置目录，不生成 FIFO，也不展示评论区。
- 同目录多篇文章共享显式 attachment。
- 配置目录不存在、重复、隐藏、含 `..` 时失败。
- `<enabled-dir>/comment` 已被普通文件/目录/symlink 占用时失败并打印双方来源。
- commentd/jaild/jailbin/Web 接受 `/notes/`、`/projects/demo/`，拒绝非规范 target。
- 旧 `/blog/` 评论查询与投稿仍正常。

### 13.5 Web 冲突

- `assets.md`、`ws.md`、`api/comments.md` 等保留路径失败。
- `blog.md` 与 `/blog/` 全站列表冲突失败。
- 文章与 Vite 新增 public 文件冲突失败。
- 两个处理后图片目标相同失败。
- 错误同时显示系统 owner 和内容源 owner。
- 冲突失败后旧 dist、旧 `.rendered` 和旧评论清单保持不变。

### 13.6 删除与部署

- 删除任意顶层 `notes/a.md` 后 `dist/notes/a/index.html` 被清除。
- 移动文章后旧 route 消失、新 route 出现。
- 删除图片后 dist 与 `.rendered-assets` 都无僵尸。
- 未登记的 Vite/public 文件不被 content 清理。
- static-only 发布后生产静态树与 `frontend/dist` 文件集合一致。
- 模板中所有非隐藏 content 文件与 HOME 相对路径一致。

### 13.7 用户链路

- `/notes/unix/` 静态 HTML 正确。
- 页面建立 fresh session 后自动执行对应 HOME 源文件，收到 `/notes/unix/` OSC 后接管。
- `blog` 列出完整 HOME-relative文章路径。
- `blog notes/unix` 使用预渲染并同步 URL。
- `blog ~/help.md` 不走特殊分支但仍显示根 attachment 评论。
- `play demos/boot` 和嵌套录像可播放。
- SSH、Web 终端、无 JS 静态降级、带图 TUI 都通过。

## 14. 验收命令

开发环境：

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cd frontend && npm run build
cd .. && ./target/release/content-build --content jailtpl/content --dist frontend/dist
node tests/e2e-image.mjs
```

现有测试需更新并继续通过：

```sh
sh tests/verify-m5.sh
sh tests/verify-comments.sh
node tests/e2e-image-playwright.mjs
```

新增一个无 `blog/` 目录的端到端测试，建议 `tests/e2e-content-paths.mjs`，负责断言扫描、
route、冲突、旧产物清理和隐藏路径；FreeBSD 部署侧新增或扩展验收，断言 template HOME
树、通用 FIFO 和实际 OSC。

最终静态审计：

```sh
rg -n 'content/blog|join\("blog"\)|~/blog|/blog/\{slug\}|strip_prefix\("blog/"\)' \
  crates frontend deploy-scripts Makefile README.md docs jailtpl/content tests
```

允许剩余的 `/blog/` 仅限：

- 全站文章列表这一明确系统 route；
- `blog/hello` 作为普通目录兼容性测试或示例；
- 历史评论 fixture；
- 历史 plan/diff 资料。

## 15. 完成标准

以下条件全部满足才算重构完成：

1. 删除或重命名 `jailtpl/content/blog/` 后，构建器、jail 模板和运行时不因缺少该目录失败。
2. content 中任意合法非隐藏目录下的 Markdown 都产生正确 HOME 文件、ANSI、HTML、feed 和 URL。
3. `blog/` 在生产逻辑中不再作为源根、HOME 根、图片根、slug 根或评论根。
4. 评论 FIFO 只由 `.termblog.toml` 显式目录产生，与文章集合无关。
5. 启用评论目录中的 `comment` 冲突会在模板构建前被 content-build 明确拒绝。
6. 前端不再拼 `~/blog` 或 `/blog/${slug}`，而是使用 HTML 给出的 source/route。
7. 所有 Web 冲突在写产物前汇总失败，错误包含双方来源。
8. 删除、移动任意顶层内容后，本地 dist 和生产 static 均无旧文件。
9. 嵌套文章的图片 manifest 和 TUI reader 正常。
10. 全部 Rust、前端、内容、图片、评论、SSH/Web 与 FreeBSD 模板验收通过。
11. 活跃文档明确说明：content 是 HOME，所有 Markdown 是文章，comment 是显式目录 attachment，
    `blog` 目录没有特殊语义。

## 16. 风险与回滚

### 16.1 主要风险

- **索引格式同时切换**：旧 jailbin 读不了新索引，新前端也不能配旧 HTML；模板、静态页和
  jailbin 应作为一个内容版本一起构建和发布。
- **静态目录交错**：内容页面从 `dist/blog` 扩散到任意顶层后，误删 Vite 文件的风险上升；
  必须先落产物 manifest 和 claim 表，再取消旧的整目录清理。
- **评论 target 扩大**：commentd validator 泛化后可接受更多规范 target，但实际投稿集合仍由
  root-owned 清单限定；不能误把“语法合法”等同于“已配置 attachment”。
- **全 Markdown 发布**：content 中不能继续存放仓库侧 Markdown 说明；实施前必须搬走 README。
- **命令兼容性**：`blog hello`、`play hello/demo` 不再隐式补 `blog/`；帮助文档和外部说明需同步。

### 16.2 回滚策略

1. 在开始路径写出切换前保留一个能完整通过旧测试的提交点。
2. 新旧索引不做运行时混读；回滚时同时回滚 jail 模板快照和静态树。
3. 评论数据库格式不变，target 字符串旧值仍合法，因此代码回滚不需回滚评论数据。
4. 部署 staging 保留上一版静态树直到新树完成验收，失败时恢复目录名。
5. ZFS template replace 继续利用现有 `template.old*` 机制；旧会话自然留在旧模板，新会话失败时
   可把旧 snapshot 换回。
