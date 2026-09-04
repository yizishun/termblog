# termblog —— 终端即博客

每篇文章一个稳定 URL: 爬虫看到静态 HTML 镜像, 真人打开 URL 则自动接入
每个访客一个的真实 FreeBSD jail 终端(zsh + less), 读预渲染排版的文章。
URL⇄终端双向同步(OSC 7777), SEO 产物(sitemap/atom/canonical)构建期生成。

## 架构

- **jaild**(root, 会话特权进程): 每访客从只读模板 `zroot/jails/template@release`
  ZFS clone 出一个会话 jail(rctl 限额 + 4M 磁盘配额), PTY 经 Unix socket 供接入层使用。
- **commentd**(root, 评论单写者): 用独立 root-only JSONL 数据库存储待审/通过/删除状态，
  通过 public/private 两个 Unix socket 分隔只读查询与投稿、审核；访客向 jail 内 FIFO
  写一行即可投稿。文章评论绑定直属目录，同目录文章共享一个 FIFO；无直属文章的目录不创建。
- **termblog-web / termblog-ssh**(降权 www): 浏览器(WS)/ SSH 两个接入网关,
  经 SEQPACKET Unix socket 连 jaild, 零协议转换。
- **content-build**: 把唯一内容源 `jailtpl/content/blog/*.md` 一次解析成两个投影 ——
  HTML 静态镜像(`frontend/dist/blog/<slug>/`, 爬虫不开 jail 读全文)与 ANSI 预渲染
  (`jailtpl/content/.rendered/`, 终端 `less -R` 可读), 并产出列表页/
  sitemap/atom/robots。图片走资源管线: 相对引用构建期重写为 `/blog/...`、
  超宽自动缩放、尺寸/字节预算 fail-fast; 镜像页出真图(`<img>` 带真实宽高 +
  og:image), 终端出格式稳定的占位框(OSC 8 可点链接)。带图文章另产
  sidecar `~/.rendered/<slug>.images.json`(占位框行号区间 + 几何)与
  处理后图片 `~/.rendered-assets/`(webp 统一转 png, 与 dist/blog 同字节)。
  写作约定见 `jailtpl/content/README.md`。
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
  servers/       # proto(线协议) core(会话运行时) web ssh jaild commentd
  tools/         # content-build(内容编译器) jailbin(jail 内命令)
deploy-scripts/  # build-template.sh(模板构建/零停机换面) deploy.sh(全量部署)
tests/           # verify-m3.sh verify-m5.sh e2e-reconnect.mjs(验收脚本)
jailtpl/content/ # 唯一内容源: help.md + blog/*.md(每篇可带同名资源目录, 如录像 .cast)
                 # + .rendered 产物(README 写作规范只留仓库)
etc/             # termblog.toml 样例 + rc.d + newsyslog
frontend/        # xterm.js 前端(vite)
```

## 部署(需要 root)

| 入口 | 做什么 |
| --- | --- |
| `make tpl` | 构建 jail 模板(首次); 已存在则拒绝 |
| `make deploy` | 全量生产部署: racct 检查 → 编译 → 安装 → 发布镜像 → 拉起服务(需模板已构建) |
| `make content` | 只改文章的部署: 静态发布 + 模板零停机换面, 全程不停服、不杀会话 |
| `deploy.sh --static-only` | 只发静态镜像(零停机); jail 侧下次模板重建跟进 |
| `build-template.sh --replace` | 零停机换模板(旧会话继续用旧模板, 全部退出后回收) |

部署目标内嵌 sudo, 直接 `make tpl` / `make deploy` / `make content` 即可
(会提示输入密码)。部署脚本不依赖 Makefile; Makefile 只是薄入口。

> **模板功能升级注意**: 新 jailbin、评论 FIFO/清单、`help.md` 和图片产物只存在于
> 重建后的模板数据集里。升级已有安装时先零停机换模板，再部署守护进程：
> `sudo sh deploy-scripts/build-template.sh --replace && sudo sh deploy-scripts/deploy.sh`。
> `deploy.sh` 会拒绝启动缺少评论清单的旧模板，避免新 jaild 交付不了会话。
> 后续只改文章仍走 `make content`(内部已含 `--replace`)。

配置: `/usr/local/etc/termblog.toml`(仓库 `etc/termblog.toml` 为样例)。
部署后务必设 `web.site_url`(不设则不产 sitemap/atom/canonical);
`web.site_title` 用于镜像页标题 / og:site_name / atom 标题。

## 验收

- `sh tests/verify-m3.sh`(root): 进程形态 / 真实 jail / 隔离 / rctl / 配额 / zfs 无泄漏
- `sh tests/verify-m5.sh`: 镜像页 / 发现链路 / feed / robots + ssh 侧 blog 行为
- `sh tests/verify-comments.sh`(root): 双 socket / FIFO 投稿 / 审核 / API / 初始会话快照
- `node tests/e2e-reconnect.mjs`: 断线重连协议

## 开发

- `make build`: 全部二进制(含 jailbin)+ 前端 + 内容产物(bmake, FreeBSD 默认 make)
- `make run` / `make run-ssh`: 前台调试
- `cargo test`: Rust 单测(含 jailbin 对 blog/webctl 的移植等价性测试、play 的
  asciicast 解析与时间轴测试)
