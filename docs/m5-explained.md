# M5 讲解:SEO 静态镜像 + URL⇄终端双向同步

> 本文面向想理解 M5 的读者, 自上而下:先讲动机和核心思想, 再跟着一次真实访问
> 走完全程, 最后拆解每个组件与关键机制。读到任何术语都不用担心, 第一次出现
> 时都有解释。规格细节以 `plan_m5.md` 为准, 本文只讲"为什么"与"怎么配合"。

---

## 1. 动机:一个 URL, 两种访客

termblog 的正文活在**真实终端**里:访客 ssh 进来(或网页打开一个终端),敲
`blog` 读文章。这个形态有两个问题:

1. **搜索引擎看不懂终端**。爬虫抓的是网页, 博客正文在 jail 的 shell 里,
   对它来说就是不存在 —— 文章永远进不了索引, 也不会有链接预览卡片;
2. **人类点开链接时的体验很怪**。你分享 `https://站点/blog/hello/` 给别人,
   对方浏览器打开后必须等终端连上、命令敲完, 中间要么白屏要么闪一下文章
   静态页再切换 —— 很廉价。

M5 的目标一句话:**同一个 URL, 爬虫看到完整静态文章, 真人看到真实终端**。

---

## 2. 核心思想:一个源头, 两种投影

所有文章只有一个事实源 —— 仓库里的 markdown 文件:

```
jailtpl/content/blog/hello.md          ← 唯一手写源(无 frontmatter, 文件名即 slug)
        │
        │  content-build(构建期跑一次, 把 md 翻译成两种形态)
        │
        ├──▶ HTML 投影:  frontend/dist/blog/hello/index.html   爬虫/无 JS 访客读的静态全文页
        └──▶ ANSI 投影:  jailtpl/content/.rendered/hello        终端里 blog 命令读的排版文本
```

为什么要"翻译"而不是直接复制 md?因为浏览器和终端是两种完全不同的"屏幕":
浏览器要 `<h1>` 标签才能排版和语义化, 终端要 ANSI 转义序列才能显示颜色与粗体,
而且终端的折行必须按显示宽度计算(一个中文占 2 列)。**这两个翻译都放在构建期
(部署时)做一次**, 而不是每个访客来了现做 —— 内容是静态的, 算一次就够了;
jail 里也因此不需要装任何 markdown 渲染器。

---

## 3. 全景:一次访问的两条命运

访问 `http://站点/blog/hello/` 时, 同一个 URL 分出两条路:

```
                     GET /blog/hello/
                          │
          ┌───────────────┴────────────────┐
          │ 爬虫 / 无 JS 访客               │ 真人(浏览器会跑 JS)
          ▼                                ▼
   ServeDir 直接返回静态 HTML        镜像页加载, 三层结构:
  ┌─────────────────────┐           ┌─────────────────────────┐
  │ #static-view 全文    │           │ ① #static-view  全文     │ ← 兜底, 被盖住
  │ (语义完整, 可索引)    │           │ ② #mirror-cover 等待层   │ ← 首屏只看到它
  └─────────────────────┘           │ ③ #term-host   终端     │ ← 藏在等待层下面
                                    └─────────────────────────┘
                                       │ WS 连接 → Open{fresh=1}
                                       ▼
                                    全新 jail 会话(zsh 启动)
                                       │ 自动敲 blog ~/blog/hello.md
                                       ▼
                                    OSC 到达 → 等待层淡出 → 终端接管
```

爬虫和真人的**内容永远一致**:真人在终端里读到的排版文本, 和爬虫抓到的 HTML,
来自同一份 md、同一次编译 —— 这就是"同源同文"。

---

## 4. 构建期:content-build 做了什么

`content-build` 是仓库里的一个 Rust 工具(`crates/tools/content-build`), 由
`gmake build` 在 vite 之后调用。它用 pulldown-cmark 把每篇 md **只解析一次**,
得到一串事件, 再喂给两个渲染器:

- **HTML 渲染器**产出:镜像页(每篇一个 `/blog/<slug>/index.html`)、列表页
  `/blog/index.html`、`sitemap.xml`/`atom.xml`(配置了
  `web.site_url` 才生成, 否则跳过)、`robots.txt`;
- **ANSI 渲染器**产出:`.rendered/<slug>`(76 显示列折行、中文按 2 列计宽、
  标题加粗/表格对齐都已算好)与 `.rendered/.index`(文章列表, TSV:日期/标题)。

它同时负责派生 md 里没有的元数据:**发布日期取自 git 历史**(`git log %cI`,
没有才退回文件 mtime)、标题取第一个 `# 一级标题`、摘要 160 字符、排序(日期
倒序)。还有一道硬性校验:slug 白名单 `[a-z0-9/-]`, 文件名违规直接构建失败。

**构建顺序敏感**:vite 打包出的入口文件带内容 hash(`assets/index-*.js/css`,
每次构建都变), 镜像页必须引用它们才能获得终端样式与逻辑;所以 content-build
必须在 vite 之后跑, 去 `dist/assets/` 里 glob 到"恰好 1 个"入口 JS/CSS 注入
每个镜像页, 找不到就失败 —— 缺了这个, 镜像页里的终端 DOM 会"裸奔"
(曾经真实发生过的 bug:无样式的 textarea 输入框 + 乱码)。

---

## 5. 运行期:一条字节流的旅程, 与 OSC

### 5.1 数据面

真人访问时, 数据面是一条纯字节管道:

```
浏览器 xterm.js ⇄ WebSocket ⇄ termblog-web ⇄ Unix socket ⇄ jaild ⇄ PTY ⇄ zsh(blog/less)
```

web 与 jaild 都**不理解字节内容**, 只做透传;一条会话的输出按需缓冲(web 侧
128KiB scrollback, 供首页 attach 回放)。这个"数据面零改动"是 M5 的约束:所有
新功能都不能要求 web/jaild 理解"文章"。

### 5.2 OSC 7777:终端对浏览器的悄悄话

OSC(Operating System Command)是终端世界里的一类控制序列:形如
`ESC ] 编号 ; 内容 BEL`, 它**不显示在屏幕上**, 而是给终端模拟器的一条带外
消息(最常见的用法是设置窗口标题)。M5 注册了一个私有编号 7777, 约定内容为
`url=<路径>`:

```
webctl url /blog/hello/   实际输出: ESC ] 7777 ; url=/blog/hello/ BEL
```

这串字节从 jail 里的 shell 出发, 穿过 PTY → jaild → web → WebSocket, 每一步
都被当作普通字节透传, 直到 xterm.js 的 OSC 解析器认出 7777, 把它交给前端
代码而不是画上屏。前端校验白名单(`^/[a-z0-9/-]+$`, ≤512 字节)后, 只做
`history.replaceState` —— **改地址栏但不导航**。这就是"URL⇄终端双向同步"
的终端→URL 方向:你在终端里读哪篇, 浏览器地址栏就跟到哪。

为什么用 OSC 而不是新协议?① 数据面零新增, web/jaild 完全不知道有这回事;
② 不污染画面;③ 安全性有天然天花板 —— jail 里任何进程都能 printf 任意 OSC,
但客户端白名单 + 只 replaceState 意味着最坏结果只是地址栏显示一个站内路径。

**另一个隐藏用途:心跳**。`blog` 脚本进分页器时发 `url=/blog/<slug>/`,
退出分页器时发 `url=/`, 这对成对的 OSC 客观记录了 shell 的当前状态 —— 这个
特性在后面的迭代史(§10)里起了关键作用。

---

## 6. 浏览器侧:镜像页接管状态机

### 6.1 判定与三层结构

前端靠 `<meta name="termblog-slug">` 区分页面:有这个 meta 的就是**镜像页**
(文章页), 没有的就是普通页面(首页, 纯终端)。

镜像页的 HTML 有三层:静态正文(`#static-view`, 爬虫的完整视图)、不透明等待层
(`#mirror-cover`, "正在接入真实终端…")、终端(`#term-host`)。等待层从首屏就
盖住静态正文, 所以真人**永远不会看到静态文章闪烁**;等终端就绪, 等待层淡出,
终端接管(静态正文 `display:none`)。

### 6.2 全新会话的接管时序(最终方案)

镜像页连上 WS 后发 `Open { fresh: true }`, 网关**跳过 attach、直接开新会话**:

```
Open{fresh=1} → web 回收旧 token 的闲置会话 → jaild 起全新 jail(zsh 启动)
→ OPENED → 等首帧数据(zsh 已画出 MOTD/提示符) → +200ms 自动敲
  blog ~/blog/<slug>.md⏎ → blog 发 OSC → +200ms(等 less 画出文章第一帧)
→ 等待层淡出, 文章呈现在眼前
```

两个 200ms 都有讲究:命令必须等 zsh 就绪再敲(早敲的字节会被 tty 驱动回显在
提示符不存在时的屏幕顶部, 造成孤儿乱码);接管必须等 OSC 之后 200ms(blog 先发
OSC、less 随后才画第一帧, 淡出时透出的是画好的文章而不是空屏)。整条时序
失败时, 5 秒兜底撤掉等待层、露出静态全文 + "进入终端"按钮。

### 6.3 为什么镜像页每次都是全新会话

这是本次迭代的最终决策。早期方案是"attach 回旧会话, 想办法判断旧 shell 的
状态(在文章里 / 别的分页器 / 提示符), 再决定敲什么键"——但状态判断在浏览器
侧做极其别扭:回放里的 OSC 是**历史字节**(第一次访问的进入 OSC 永远留在
scrollback 里), 各种按键恢复序列(`^C`、`q⏎`、`^U`)要么被运行中的 less 当
分页器命令吃掉造成视图跳变, 要么在提示符下制造 `command not found` 垃圾。

最终思路是**消除状态判断本身**:镜像页每次都开全新会话, 落地永远是干净 shell,
行为每次完全一致;attach 只留给首页刷新(终端浏览时保留历史)。附带收益:开新
会话前网关回收旧 token 的**闲置**会话(没有其它标签页在用才回收), 旧 jail
立即释放, 不等 60 秒宽限 —— 每 IP ≤3 的配额下, 一分钟内连开四篇文章也不会
被拒。

### 6.4 地址栏方向(URL→终端)

"URL⇄双向同步"的另一半其实在入口处就完成了:浏览器打开哪个 URL, 前端就自动
敲哪篇文章的 `blog` 命令;终端里手动读别的文章时, OSC 再把地址栏同步过去。
接管前镜像页只接受本页路径的 OSC(防御:其它路径不得劫持本页地址栏), 接管后
放开。

---

## 7. 终端侧:blog 命令

`blog` 是装进 jail 模板的命令(`/usr/local/bin/blog`, 0555), 由 Rust 多合一
二进制 `jailbin` 提供 —— `blog` / `webctl` 都是指向它的符号链接(busybox 式,
访客无感)。接口**像 cat**:任意路径的 md 都能读(相对 cwd / 相对 `~/blog` /
绝对路径, 任意层级), `blog hello` 等价 `blog ~/blog/hello.md`, 裸 `blog`
列出文章列表。

内容上更聪明:

- 文件在 `~/blog/`(文章目录)下 → 读**预渲染排版**(`~/.rendered/<slug>`,
  less -RXc 分页;没有产物才退回原始 md 并提示);
- 文件在其它任意位置 → 像 cat 一样读原始内容, 不同步地址栏;
- 读文章时发进入 OSC(地址栏同步到 `/blog/<slug>/`), 退出分页器发 `/`
  复位 —— 这就是 §5.2 说的那对心跳。

`cat`/`less` 直接读 md 时看到的永远是原始未渲染的 markdown。

---

## 8. 目录布局:仓库与 jail 的对应

```
仓库(唯一事实源 + 生成物)                jail 模板(访客家目录, 构建时拷入)
jailtpl/content/
  ├── blog/hello.md        ────────▶    ~/blog/hello.md       文章目录 = URL 前缀 /blog/
  │   (路径即 slug, 可嵌套)             (hello.md ↔ /blog/hello/)
  ├── .rendered/hello      ────────▶    ~/.rendered/hello     隐藏工具目录(预渲染产物)
  └── README.md(写作规范, 只留仓库, 不进 jail)
```

文章目录名 `~/blog` 直接对应 URL 前缀 `/blog/`, 文件相对路径即 slug —— 这个
一一对应是 OSC 同步与镜像页 URL 的基础, 中间没有任何映射表。镜像页 URL 统一
带尾斜杠(`/blog/hello/`, canonical 形态;不带斜杠由 ServeDir 307 重定向过去)。

---

## 9. 部署与验收

部署逻辑在 `deploy-scripts/`(Makefile 只做薄入口, 需 root):

- **构建**:`make build`(vite → cargo → content-build, 开发期);
- **模板**:`sh deploy-scripts/build-template.sh`(首次构建);`--replace` =
  零停机换模板: 构建到旁路名 `template.new` 再换名上场, 不停服、不杀会话,
  旧会话继续用旧模板、新会话取新模板(旧模板被 pin 至旧会话全部退出后回收);
- **全量部署**:`sh deploy-scripts/deploy.sh`(需模板已构建): racct 检查 →
  编译 → 安装二进制/前端/配置 → 发布静态镜像 → 拉起服务;
- **只改文章**:`make content` = `deploy.sh --static-only`(编译内容 + 纯文件
  替换发布镜像, 零停机)+ `build-template.sh --replace`(模板零停机换面);
- **验收**:`tests/verify-m5.sh` —— 镜像页/列表页/feed/robots、无尾斜杠
  307、ssh 侧 `blog` 列表与进入/退出 OSC、预渲染 ANSI 粗体等检查项;
  `tests/verify-m3.sh`(root)与 `tests/e2e-reconnect.mjs` 覆盖 M3 与重连协议。

---

## 10. 附录:镜像页接管的三个方案(迭代史, 教学价值)

| 方案 | 思路 | 失败/放弃原因 |
|---|---|---|
| ① attach + 无条件敲 `^C` + `blog` | 复用旧会话, 先退分页器再重敲 | `^C` 不退 less;键被当分页器命令吃掉, `/`+词+回车=搜索跳转, 重复访问文章上移 |
| ② attach + OSC 心跳判定 + 分态按键(`q⏎`/`^U`) | 用回放末条 OSC 判断 shell 状态, 按状态敲键 | 回放何时结束靠 1.5s 计时猜测;提示符下 `q` 产生 `command not found`;状态×时序的组合边角无穷 |
| ③ **镜像页一律全新会话(fresh)** | 消除状态判断:每次落地干净 shell, attach 只留首页 | ✅ 采用:每次行为一致、零按键垃圾、旧 jail 立即回收 |

方案③的教训:**当判断一个复杂状态很困难时, 先问"这个状态必须存在吗"** ——
镜像页的旧 shell 状态本来就不需要保留, 杀掉重开是更便宜、更可靠的解法。
详细偏差记录见 `plan_m5.md` §16 交付偏差补记。
