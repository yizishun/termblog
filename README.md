# termblog —— 终端即博客

每篇文章一个稳定 URL: 爬虫看到静态 HTML 镜像, 真人打开 URL 则自动接入
每个访客一个的真实 FreeBSD jail 终端(zsh + less), 读预渲染排版的文章。
URL⇄终端双向同步(OSC 7777)，浏览器标题随 shell 当前路径/文章标题同步(OSC 2)；
SEO 产物(sitemap/atom/canonical)构建期生成。

## 架构

- **jaild**(root, 会话特权进程): 每访客从只读模板 `zroot/jails/template@release`
  ZFS clone 出一个会话 jail(rctl 限额 + 4M 磁盘配额), PTY 经 Unix socket 供接入层使用。
- **commentd**(root, 评论单写者): 用独立 root-only JSONL 数据库存储待审/通过/删除状态，
  通过 public/private 两个 Unix socket 分隔只读查询与投稿、审核；访客每次打开并写入
  jail 内 FIFO 即投稿一条评论，正文可包含换行。评论 attachment 由内容配置显式列出；
  同目录文章共享一个 FIFO，
  空目录也可独立启用评论。`alice: #1: 内容`（或 guest 的 `#1: 内容`）可回复同目录的
  已公开评论；数据库全局 ID 不进入公开 API 或 guest 快照。
- **termblog-statd**(root, 统计单写者): 用 root-only SQLite 持久化 target/article 计数，
  通过 Unix socket 接收有界批次。终端侧以 FreeBSD kqueue `NOTE_READ` 对每会话、
  每文章最多计一次，Web 侧在 canonical HTML `GET 200/304` 后异步入队；访客数是
  两种来源的加盐 IP HMAC 并集，原始 IP 与哈希均不公开。jaild 在 guest fork 前
  根 scope 生成 `/proc/stat`，其他 scope 生成 `/proc/<scope>/stat`；文件均为 `root:wheel 0444` 的会话快照。
- **termblog-web / termblog-ssh**(降权 www): 浏览器(WS)/ SSH 两个接入网关,
  经 SEQPACKET Unix socket 连 jaild, 零协议转换。
- **content-build**: 把 `jailtpl/content/` 可见目录中的全部 `.md` 一次解析成两个投影 ——
  HTML 静态镜像(输出路径直接对应文章 route，爬虫不开 jail 也能读全文)与 ANSI 预渲染
  (`jailtpl/content/.rendered/`, 终端 `less -R` 可读), 并产出列表页/
  sitemap/atom/robots。图片走资源管线: 相对 Markdown 目录解析并改写为站点绝对路径、
  超宽自动缩放、尺寸/字节预算 fail-fast; 镜像页出真图(`<img>` 带真实宽高 +
  og:image), 终端出格式稳定的占位框(OSC 8 可点链接)。带图文章另产
  sidecar `~/.rendered/<article-key>.images.json`(占位框行号区间 + 几何)与
  处理后图片 `~/.rendered-assets/`(webp 统一转 png, 与 Web 产物同字节)。
  路径映射、评论配置和写作约定见 `docs/content-authoring.md`。
- **jailbin**: 装进 jail 模板的访客命令多合一二进制(busybox 式), `blog`(cat 式
  文章阅读器: 读预渲染排版 + 同步地址栏)、`play`(asciicast 终端录像播放器,
  自包含实现: 定时回放 + 暂停/逐帧/倍速)与 `webctl`(发 OSC 7777)是其符号链接。
  带图文章在有能力的会话(前端传 caps `img-iterm2` → jail 里 `TERMBLOG_IMG=iterm2`)
  改走自写 TUI 阅读器: 图片以 iTerm2 Inline Images Protocol 像素内嵌
  (ratatui + crossterm, 全链路有界队列背压、字节零丢失), 无能力会话
  (SSH/旧前端)自动回落 v1 占位框。

## 目录

```
crates/
  config/        # termblog-config: TOML 配置(servers 与 content-build 共用)
  servers/       # proto(线协议) core(会话运行时) web ssh jaild commentd statd
  content-model/ # content/HOME 路径、公开 route、评论/统计 scope 的共享模型
  tools/         # content-build(内容编译器) jailbin(jail 内命令)
deploy-scripts/  # build-template.sh(模板构建/零停机换面) deploy.sh(全量部署)
tests/           # verify 脚本与无 blog 目录的 content path 端到端验收
jailtpl/content/ # guest HOME 蓝图；任意可见 *.md 是文章，blog/ 无特殊语义
                 # 隐藏配置和 .rendered 等生成物不按普通内容复制
etc/             # production/debug TOML 配置 + rc.d + newsyslog
frontend/        # xterm.js 前端(vite)
```

## 部署(需要 root)

| 入口 | 做什么 |
| --- | --- |
| `make tpl` | 首次准备 `template-base@prepared` 并构建 jail 模板; 仅首次下载 FreeBSD base/pkg |
| `make tpl-refresh` | 显式联网刷新 FreeBSD base/pkg 基础层，并零停机换模板 |
| `make deploy` | 全量生产部署: racct 检查 → 编译 → 安装 → 发布镜像 → 拉起服务(需模板已构建) |
| `make content` | 只改文章的部署: 静态发布 + 模板零停机换面, 全程不停服、不杀会话 |
| `make deploy-debug` | 使用 `etc/termblog-debug.toml` 全量部署(HTTP 8080 / SSH 2222 / 关闭 TLS) |
| `make content-debug` | 使用 debug 配置发布静态镜像并零停机换模板 |
| `deploy.sh --static-only` | 只发静态镜像(零停机); jail 侧下次模板重建跟进 |
| `build-template.sh --replace` | 零停机换模板(旧会话继续用旧模板, 全部退出后回收) |

`build-template.sh` 使用两层 ZFS 模板。`zroot/jails/template-base@prepared`
只含 FreeBSD base 和 jail 的通用包；首次构建时下载并将 `base.txz`
持久缓存到 `/var/cache/termblog`。后续 `--replace`/`make content` 直接从
该快照本地 clone，只更新 guest 配置、`jailbin` 和博客内容，不再访问
FreeBSD/pkg 网络。需要安全更新或升级 jail 用户态时才运行
`make tpl-refresh`。Cargo/npm 仍按各自的本地依赖缓存做增量构建。

部署目标内嵌 sudo, 直接 `make tpl` / `make tpl-refresh` / `make deploy` /
`make content` 即可；debug 环境使用 `make deploy-debug` / `make content-debug`
即可
(会提示输入密码)。部署脚本不依赖 Makefile; Makefile 只是薄入口。

> **模板功能升级注意**: 新 jailbin、评论 FIFO/scope 清单、jail-root `/proc` 目录树、文章机器索引、`help.md` 和图片产物只存在于
> 重建后的模板数据集里。升级已有安装时先零停机换模板，再部署守护进程：
> `sudo sh deploy-scripts/build-template.sh --replace && sudo sh deploy-scripts/deploy.sh`。
> `deploy.sh` 会拒绝启动缺少 scope 清单、文章索引或 `/proc` 目录的旧模板，避免新 jaild 交付不了会话。
> 后续只改文章仍走 `make content`(内部已含 `--replace`)。

配置: `/usr/local/etc/termblog.toml`。仓库中 `etc/termblog.toml` 是默认生产
配置，`etc/termblog-debug.toml` 是 debug 配置；不带 `-debug` 的部署目标
总是使用生产配置。`deploy-debug` 会将 debug 配置安装为运行配置，
`content-debug` 只按 debug URL 重建并发布内容，不改动已安装的运行配置。

部署后务必设 `web.site_url`(不设则不产 sitemap/atom/canonical);
`web.site_title` 用于镜像页标题 / og:site_name / atom 标题。`[stats]` 配置统计 socket、root-only 数据目录和非关键请求超时；首次全量部署会显式执行 `termblog-statd --init`，已有目录缺文件或 schema 损坏时拒绝自动修复。

## 验收

- `sh tests/verify-m3.sh`(root): 进程形态 / 真实 jail / 隔离 / rctl / 配额 / zfs 无泄漏
- `sh tests/verify-m5.sh`: 镜像页 / 发现链路 / feed / robots + ssh 侧 blog 行为
- `sh tests/verify-comments.sh`(root): 双 socket / FIFO 投稿 / 审核 / 嵌套回复 / 局部编号 API /
  初始会话快照
- `TERMBLOG_PW=1 node tests/e2e-comments-playwright.mjs`: mock API 下的回复线性顺序与注入回归
- `node tests/e2e-reconnect.mjs`: 断线重连协议
- `node tests/e2e-content-paths.mjs`: 通用 content/HOME 路径、scope 到 `comment` 与 jail-root `/proc` 的映射、清理与冲突回归
- `sh tests/verify-stats.sh`(root；可传 `TERMBLOG_VERIFY_JAIL_ROOT`): statd socket/数据权限与当前会话所有 `/proc/.../stat` 的格式、权限和内容冻结
- `cargo test -p termblog-statd -p termblog-jaild -p termblog-web -p content-build`: SQLite 事务/持久化、访客并集、FreeBSD `NOTE_READ`、快照格式与 Web canonical 请求分类

## 开发

- `make build`: 全部二进制(含 jailbin)+ 前端 + 内容产物(bmake, FreeBSD 默认 make)
- `make run` / `make run-ssh`: 前台调试
- `cargo test`: Rust 单测(含 jailbin 对 blog/webctl 的移植等价性测试、play 的
  asciicast 解析与时间轴测试)
