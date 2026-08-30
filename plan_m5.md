# M5 实现规格书:SEO 静态镜像 + URL⇄终端双向同步

> 本文档是**实现规格书**:目标读者是被指派实现 M5 的 AI(或人)。旧版 plan_m5.md 的
> 全部决策原样保留(见 §2),本文在其基础上补齐到"照着做就能落地"的粒度:精确的
> 文件清单、数据格式、算法规则、完整的小脚本源码、逐条验收命令。
>
> 前情:M1–M3 已完成(见 m3.md,16/16 验收通过);M4(打磨)与本计划无依赖,可先做。
>
> 问题:博客是纯终端(WS/SSH → jail 里的 zsh),文章没有任何静态 URL。爬虫访问
> `/` 只能看到一个空的 xterm 容器 —— 搜索引擎检索不到任何一篇文章。
>
> 目标:每篇文章有真实、稳定、爬虫可读的 URL;真人点开该 URL 的最终体验仍是
> 终端本身;终端里翻文章时地址栏跟着变。**不是两套内容,是一个源头的两个投影。**

---

## 0. 本文档怎么读

- §2 是**已定案的决策**,实现时不要再讨论、不要再发明替代方案;发现决策与现状
  冲突时,停下来在交付说明里说明,不要自行改设计。
- §4 是**已核实的事实基线**(2026-08-29 在本机 FreeBSD 16.0-CURRENT 实测),直接
  采信,不要重复验证;其中含一个与旧版计划表述不符的实测结果(§4.1 条目 1,
  `/blog/hello` 的 307 重定向),相关设计已据此调整。
- 每个交付物(§7 起)按"文件 → 职责 → 精确规格"组织。标注【完整源码】的小文件
  (webctl、blog、update-content.sh、blog.css、HTML 模板)**必须逐字采用**;
  其余(如 content-build 的 Rust 代码)给出行为规格,实现细节自定,但对外行为
  (CLI、输出路径、文件格式、错误信息)必须与本规格一致,因为验收命令依赖它们。
- 里程碑(§13)按 M5.1→M5.4 顺序执行,每步的验收命令全绿才进入下一步。标注
  `[root]` 的命令需要 root(在 jailed 会话不可用时向人类申请,不要自行绕过);
  标注 `[人工]` 的检查需要人类开浏览器确认。
- 实现语言/风格约定:Rust 代码注释、日志、错误信息与仓库现有风格一致(中文注释
  + 简洁错误信息);shell 脚本一律 POSIX sh(`#!/bin/sh`),不用 bash。

### 0.1 术语

| 术语 | 含义 |
|---|---|
| 真身 | `jailtpl/content/blog/*.md`,唯一的内容源文件,无 frontmatter |
| 投影 | 由 content-build 从真身编译出的产物(HTML 镜像页、ANSI 预渲染) |
| 镜像页 | `frontend/dist/blog/<slug>/index.html`,爬虫可读的静态全文页 |
| 落地页 | 访客从搜索引擎点进来的那个镜像页 URL |
| 接管(takeover) | 前端把镜像页的静态正文层淡出、露出底层已就绪终端的过程 |
| slug | 文章在 URL/命令行里的标识,见 §5.1 |
| OSC 7777 | 终端转义序列通道,终端→浏览器地址栏的 URL 同步协议,见 §6 |

---

## 1. 问题与目标(展开)

现状:三个服务(jaild / termblog-web / termblog-ssh)已上线,内容在
`jailtpl/content/`(目前只有 README.md,尚无文章)。爬虫抓 `/` 得到的是一个
靠 JS 起终端的空壳页:`<div id="term-screen">` 里没有任何文本内容,WebSocket
爬虫也不升级 —— 于是**全站对搜索引擎不可见**。

要达到的终态(每一条都是验收项):

1. 每篇文章有稳定 URL,GET 该 URL 返回**含全文**的 HTML(不需要执行 JS);
2. sitemap.xml / atom.xml / robots.txt 齐备,Google Search Console 可提交;
3. 真人打开文章 URL:静态正文秒开可读 → 终端在后台启动 → 自动执行 `blog <slug>`
   → 静态层淡出,访客落进真实终端,画面与 ssh 进来敲同一条命令**逐字节一致**;
4. 终端里 `blog <另一篇>` → 浏览器地址栏变为那篇的 URL(replaceState,不刷新);
   复制地址栏即为精确分享链接;ssh 客户端看不到任何杂质(未知 OSC 被吞);
5. 无 JS / 字体加载失败 / jail 配额满 / WS 挂了:文章照样完整可读(降级即静态博客);
6. 写作流程不变:往 `jailtpl/content/blog/` 扔 markdown,跑构建,结束。

---

## 2. 已定案的决策(定案,不再讨论)

1. **落地页形态**:静态页秒开 + 终端后台接管。爬虫止步静态页(不开 jail、不耗
   配额);真人落进终端,不看"网页版备胎"。两层内容同源同文,无 cloaking(§12)。
2. **URL 同步**:双向。
   - 入站:落地页 JS 照常连 WS 开 jail,新会话就绪后**替访客敲** `blog <slug>\r`
     (走正常键入通路,进 zsh 历史,诚实而非魔法);
   - 出站:终端侧 `webctl` 打 OSC 7777,前端解析后 `history.replaceState`。
   - 刷新页面:attach 回放把历史 OSC 一并重放,最后一条胜出,地址栏自动收敛到
     "你正在读的那篇"(特性而非 bug,无需去重)。
3. **阅读命令**:定制 `blog` 命令为推荐入口(发 OSC + less -R 排版);`~/blog/`
   下的源 md 保留,`less`/`cat` 照用(不同步 URL,接受)。
4. **编译器**:workspace 内新 crate `content-build`(Rust bin),不用 node 脚本;
   pulldown-cmark 一次解析,事件流分别喂 HTML/ANSI 两个 renderer,杜绝方言分歧。
5. **镜像页 = 纯静态文件**:tower-http `ServeDir` 直接服务,**axum 路由零新增**,
   web 二进制不知道"文章"这个概念(解耦边界与 M3 一致)。
6. **webctl/blog 都是 sh 脚本**,不建 crate(webctl 的全部职责是一句 printf)。
7. **数据面零改动**:crates/proto、crates/jaild、crates/ssh、crates/web 的数据
   通路一行不改;OSC 只是 Data 帧里的普通字节。唯一例外是 `crates/core/src/config.rs`
   加一个 `site_url` 字段(§11.1)。

---

## 3. 总体架构

```
jailtpl/content/blog/**/*.md      (唯一真身, 无 frontmatter)
        │
        ├─[content-build]──▶ HTML 镜像页   frontend/dist/blog/<slug>/index.html
        │                    文章列表页    frontend/dist/blog/index.html
        │                    首页注入      frontend/dist/index.html (幂等追加)
        │                    sitemap.xml / atom.xml / robots.txt  (进 dist)
        │                    (ServeDir 直接服务, 爬虫吃这个, 永不碰 jail)
        │
        └─[content-build]──▶ ANSI 预渲染   jailtpl/content/.rendered/<slug>
                             列表数据      jailtpl/content/.rendered/.index
                             (build-template.sh 拷进 jail 模板 ~/blog/.rendered/,
                              blog 命令用 less -R 读)
```

双向同步闭环:

```
入站:  Google → GET /blog/hello/ → 静态页秒开(全文可读, 爬虫到此为止)
          └─ 同页 JS 照常连 WS 开 jail → Opened(attached=false)
             → 前端发 Data 帧 "blog hello\r" (替访客敲命令)
             → blog 脚本: webctl 发 OSC → less 显示同一篇
             → 前端收到首条 OSC 7777 → 静态层淡出, 终端接管
             → 画面与 ssh 进来敲 blog hello 逐字节一致

出站:  终端里 blog world → webctl printf OSC 7777 (ESC]7777;url=/blog/world/BEL)
          → 裸字节穿 PTY → jaild 泵 → web 网关(纯透传) → xterm.js OSC handler
          → history.replaceState("/blog/world/")  (地址栏变, 不刷新)
          → 访客复制地址栏 = 精确分享链接; ssh 客户端不认识该 OSC, 自动忽略
```

为什么这不是"硬融合":

- 内容单一来源,镜像是编译产物,写文章流程不变(往 `jailtpl/content/blog/` 扔 md);
- 爬虫与真人各取所需:爬虫读 HTML 不开 jail(省配额),真人进终端;
- OSC 通道是原计划(plan_claude_aws_fable.md §6)预留的扩展点(`osc.ts` 注册点 +
  webctl 骨架),本计划把它落地;
- 数据面一行不改,ssh 侧零感知。

---

## 4. 实现前的事实基线(已实测核实,直接采信)

1. **ServeDir 目录解析行为(tower-http 0.6.11,2026-08-29 实测)**:
   - `GET /blog/hello/`(带尾斜杠,目录下有 index.html)→ **200**,直接返回
     `blog/hello/index.html` 内容;
   - `GET /blog/hello`(无尾斜杠)→ **307** 重定向,`Location: /blog/hello/`。
   - **推论(设计已据此定案)**:canonical URL 统一采用**带尾斜杠**形式
     `/blog/<slug>/`,本站自己发出的所有链接(sitemap、atom、OG、canonical、
     首页列表、/blog/ 列表页、webctl 发的 OSC)全部带尾斜杠;无尾斜杠形态只可能
     来自外部手敲,307 会被爬虫跟随,无害。web 二进制因此仍然零路由。
2. **xterm.js 5.5(`@xterm/xterm`,frontend/package.json 已锁)**:
   - `term.parser.registerOscHandler(7777, cb)` 存在且可用;cb 收到的 payload 是
     `ESC]7777;` 之后、终止符(BEL 或 `ESC\`)之前的字符串,即 `url=/blog/hello/`;
   - parser 状态机跨 `write()` 调用保持 —— OSC 序列被 WS 分帧拆开也能正确重组,
     前端无需拼帧(这在 attach 回放按 4096 字节切片时同样成立);
   - 通过 `term.parser` 注册的 handler 是配置而非状态,`term.reset()`(main.ts 在
     attach 时会调用)不会注销它。
3. **工具链(本机 FreeBSD 16.0-CURRENT,仓库 /home/yzs/termblog)**:`cargo 1.96`、
   `gmake`、`node 22`/`npm`、`xmllint` 均可用;crates.io 可达
   (pulldown-cmark 0.13.4 已验证可拉取)。
4. **生产实例在跑**:termblog-web/termblog-ssh 以 www、jaild 以 root 运行;生产
   web 服务 `/usr/local/share/termblog/frontend`(配置在 `/usr/local/etc/termblog.toml`)。
   开发期想让 web 读仓库 `frontend/dist`:从仓库根跑
   `TERMBLOG_CONFIG=/nonexistent.toml TERMBLOG_LISTEN=127.0.0.1:18099 ./target/release/termblog-web`
   (已验证)。注意该 dev 实例以 yzs 身份连不上 root:www 的 jaild socket,开会话会
   失败 —— 正好用来测"jail 不可用时的降级路径";完整流程测试须用生产实例(8080)。
5. **前端构建产物布局**:`npm run build` 后 `frontend/dist/` = `index.html` +
   `assets/index-<hash>.js`(+ css)+ `public/` 原样拷贝(`style.css`、`fonts/`、
   `favicon.svg`)。镜像页引用的入口 JS 是**带 hash 的文件名**,content-build 需
   在 dist 里 glob 出来(§8.3 步骤 5)。vite build 会清空 dist,因此构建顺序固定为
   **vite 先、content-build 后**(§11.2)。
6. **jail 模板现状**:`build-template.sh` 装 zsh/less/tree,guest 用户 uid 1001,
   `~/blog` = content/ 全量拷贝;模板数据集存在时拒绝重跑(内容更新须 destroy 后
   重建,runbook 见 §11.4)。jail 内 zshrc 目前**没有设置 locale**,中文在 less 里
   会按字节错位 —— 本计划在 zshrc 加 `export LANG=C.UTF-8`(§10.3)。
7. **仓库尚无文章**:`jailtpl/content/` 目前只有 README.md。M5.1 需先创建示例
   文章(§10.4,兼作渲染器的测试夹具)。
8. **git 日期可用**:`git log -1 --format=%cI -- <file>` 在本仓库正常;未提交文件
   返回空(退 mtime + 告警),这是预期路径而非错误。

---

## 5. 内容模型规范

沿用 ghpage 哲学:markdown 里没有任何元数据标记,目录树即结构。

### 5.1 slug 规则(红线,构建期强制)

- 文章 = `jailtpl/content/blog/` 下所有 `*.md`(递归);
- **slug = 相对 `content/blog/` 的路径去掉 `.md`**:
  `content/blog/hello.md` → slug `hello` → URL `/blog/hello/`;
  `content/blog/2026/notes.md` → slug `2026/notes` → URL `/blog/2026/notes/`;
- slug 字符集白名单:`^[a-z0-9/-]+$`(小写字母、数字、连字符、路径分隔;不允许
  大写、下划线、点、空格、中文);
- **违规即构建失败**,错误信息逐个列出违规文件与原因(宁可写作时改名,不做运行期
  转义)。同时校验:slug 非空、不含 `//`、不以 `/` 结尾(这些在白名单下天然成立,
  但防御性检查保留)。

### 5.2 元数据提取(约定优于配置)

| 元数据 | 来源 | fallback |
|---|---|---|
| slug / URL | §5.1 | — |
| 标题 | 文中**第一个** `# ` 一级标题(收集该标题内全部 inline 文本) | 文件名 stem(如 `my-post`) |
| 日期 | `git log -1 --format=%cI -- <file>`(commit date,ISO8601) | 文件 mtime + **构建告警** |
| 摘要 | 第一个**段落**(Paragraph 块)的纯文本,折叠空白,截 ~160 字符(超长以 `…` 结尾) | 标题 |

补充规则:

- 读文件时统一去掉 UTF-8 BOM、CRLF→LF;
- 标题里出现的 Tab 替换为空格(进 .index 的行是 TSV);
- 日期同时用于:镜像页 footer 展示(`YYYY-MM-DD`)、sitemap `<lastmod>`
  (`YYYY-MM-DD`)、atom `<updated>`(完整 RFC3339)、.index 排序;
- 排序:日期倒序,同日按 slug 字典序升序。

### 5.3 目录约定

```
jailtpl/content/
├── README.md          # 顶层散文件: 进 jail(~/blog/README.md), 不进镜像、不进 feed
├── about.md           # 同上(将来若要镜像再议, 本期不做)
└── blog/              # 只有这个子树进镜像 + feed + .rendered
    ├── hello.md
    └── 2026/notes.md
```

`content/blog/` 下的非 `.md` 文件(图片等):忽略并 WARN(模板仍会原样拷进 jail,
但镜像不引用)。`content/blog/` 不存在或为空:构建**不失败**,WARN"没有文章",
仍产出 robots.txt,跳过 sitemap/atom(§8.9)、跳过首页注入。

`jailtpl/content/README.md` 需更新为记录上述约定的说明(§10.4)。

---

## 6. OSC 7777 协议规范

### 6.1 线上格式

```
ESC ] 7777 ; url = <path> BEL        (0x1b 0x5d '7' '7' '7' '7' ';' 'u' 'r' 'l' '=' <path> 0x07)
```

- 终止符 BEL(`\a`);`ESC\`(ST)同样合法(xterm.js parser 两种都接受),发射方
  一律用 BEL;
- `<path>` 例:`/`、`/blog/hello/`。**本站发出的所有文章路径带尾斜杠**(§4.1);
- 该序列只从 jail 内进程的 stdout 进入 PTY,作为 Data 帧的普通字节穿透整条链路
  (jaild/web 零感知);ssh 客户端不认识 OSC 7777,按 VT 规范吞掉,无副作用。

### 6.2 前端侧约束(安全边界,红线)

jail 里的任何进程都可以 printf 任意字节 —— **OSC payload 是不可信输入**。前端
handler 必须:

1. 只认 `url=` 前缀,其余子命令一律吞掉(handler 返回 true)不动作;
2. path 必须匹配 `^\/[a-z0-9-]*(\/[a-z0-9-]+)*\/?$`(即 `/` 或 `/a/b/` 形的纯站内
   路径,禁止 `?`、`#`、`.`、`..`、非 ASCII),长度 ≤ 512;
3. **只 `history.replaceState(null, "", path)`,绝不 location 跳转、绝不写
   innerHTML、绝不 fetch**;replaceState 包 try/catch;
4. 天花板 = "地址栏显示一个站内路径",到此为止。

### 6.3 收敛语义

attach 回放(sessions.rs 把 scrollback 按序重放进 xterm)会把历史 OSC 逐条重放,
handler 依次触发,**最后一条胜出** → 地址栏自动收敛到会话当前真实状态。无需去重、
无需状态机;scrollback(128KiB)截断导致某条 OSC 被拦腰砍掉时,该条退化为纯文本
上屏,不产生错误跳转(旧有的 ANSI 截断问题,非本计划引入,不处理)。

---

## 7. 交付物总览(文件清单)

```
termblog/
├── crates/
│   └── content-build/            [新增] 内容编译器(workspace 成员, bin)
│       ├── Cargo.toml
│       └── src/
│           ├── main.rs           CLI + 流程编排
│           ├── meta.rs           slug 校验 / 标题 / 摘要 / 日期
│           ├── html.rs           镜像页 / 列表页 / 首页注入
│           ├── ansi.rs           ANSI 预渲染(76 列)
│           └── feed.rs           sitemap / atom / robots
├── frontend/
│   ├── index.html                [不改] 首页注入发生在构建产物 dist/index.html 上
│   ├── src/
│   │   ├── main.ts               [修改] 落地页接管状态机 + 自动敲命令(§9.2)
│   │   └── osc.ts                [新增] OSC 7777 handler(§9.1)
│   └── public/blog.css           [新增] 镜像层 + 列表样式(§9.3)
├── jailtpl/
│   ├── build-template.sh         [修改] §10.3(装脚本 + locale + MOTD)
│   ├── bin/webctl                [新增] 【完整源码】§10.1
│   ├── bin/blog                  [新增] 【完整源码】§10.2
│   └── content/
│       ├── README.md             [修改] 目录约定说明(§10.4)
│       └── blog/
│           ├── hello.md          [新增] 示例文章(§10.4)
│           └── why-a-terminal-blog.md  [新增] 示例文章(§10.4)
├── crates/core/src/config.rs     [修改] WebConfig 增加 site_url(§11.1)
├── etc/termblog.toml             [修改] site_url 样例(注释态,§11.1)
├── Makefile                      [修改] build-content 目标等(§11.2)
├── .gitignore                    [修改] 忽略 jailtpl/content/.rendered/(§11.3)
├── scripts/update-content.sh     [新增] 【完整源码】内容更新 runbook(§11.4)
└── scripts/verify-m5.sh          [新增] 【完整源码】M5 验收脚本(§13 M5.4)
```

**明确不改**:crates/proto、crates/jaild、crates/ssh、crates/web(路由零新增,
`ServeDir` 的目录→index.html 行为开箱即用)、frontend/index.html、
frontend/vite.config.ts、scripts/deploy-root.sh(内容更新流程独立成
update-content.sh,见 §11.4 的说明)。

与旧版计划的清单差异(有意为之,理由):
- `frontend/index.html` 从[修改]改为**不改**:首页的爬虫可见文章列表由 content-build
  幂等注入到**构建产物** `dist/index.html`,不动 vite 模板(注入在每次 vite build
  后重做,天然幂等);
- `scripts/deploy-root.sh` 从[修改]改为**不改**:其 `gmake build` → `cp -R
  frontend/dist/.` 链路已天然携带镜像产物;内容更新(含模板重建)独立成
  `scripts/update-content.sh`,不把模板销毁逻辑塞进通用部署脚本。

---

## 8. 规格 A:content-build crate

### 8.1 CLI 与配置

```
content-build [--content <dir>] [--dist <dir>] [--config <file>]
              [--site-url <url>] [--site-title <title>]
```

- 默认值:`--content jailtpl/content`、`--dist frontend/dist`(相对 cwd,与仓库
  "从根目录跑 make" 的惯例一致;Makefile 会显式传参)、`--site-title "~yzs"`;
- `--config` 缺省时读 `TERMBLOG_CONFIG`,再缺省读 `/usr/local/etc/termblog.toml`
  (复用 `termblog_core::Config::load` 的整套机制,文件不存在则全默认);
- `site_url` 优先级:`--site-url` 参数 > 配置文件 `web.site_url`(§11.1)> 无;
- `--site-url` 传入时须去掉尾部 `/`(规范化:程序内 strip 尾部 `/`)。

### 8.2 依赖(Cargo.toml)

```toml
[package]
name = "content-build"
version = "0.1.0"
edition = "2021"

[dependencies]
anyhow.workspace = true
chrono = { version = "0.4", default-features = false, features = ["std", "clock"] }
pulldown-cmark = "0.13"
termblog-core = { path = "../core" }
unicode-width = "0.2"
```

并在根 `Cargo.toml` 的 `members` 加 `"crates/content-build"`。
(pulldown-cmark 只用默认特性;若想裁掉 getopts 可 `default-features = false,
features = ["html"]`,以实际 0.13.x 的特性名为准,非硬性要求。)

### 8.3 主流程(main.rs)

按序执行,任何一步 Err 即以非零码退出并打印人话错误:

1. 解析 CLI + 配置(§8.1);
2. 扫描 `<content>/blog/` 递归收集 `*.md` → 校验 slug(§5.1,违规列出全部后统一
   失败);空则 WARN 并继续(§5.3);
3. 对每篇文章:读文件(BOM/CRLF 规范化)→ `Parser::new_ext` 解析(**选项只用
   `ENABLE_TABLES | ENABLE_STRIKETHROUGH`,两个 renderer 共用同一套选项**)→ 收集
   `Vec<Event>` 一次 → 提取元数据(§5.2)→ 从 git/mtime 取日期;
4. 排序(§5.2);
5. 在 `<dist>/assets/` glob `index-*.js`:恰 1 个 → 记为 entry_js;0 个 → 报错
   "找不到前端入口 JS,请先 gmake build-frontend";>1 个 → 报错列出文件;
6. **清理旧产物**(幂等,§8.10);
7. 逐篇产出:HTML 镜像页 → `<dist>/blog/<slug>/index.html`(嵌套 slug 建目录);
   ANSI 预渲染 → `<content>/.rendered/<slug>`;
8. 产出 `.rendered/.index`(§8.6);
9. 产出 `<dist>/blog/index.html` 列表页(§8.7);
10. 产出 sitemap/atom(有 site_url 时;否则 WARN 跳过)与 robots.txt(§8.9);
11. 首页注入(§8.8);
12. 打印摘要:文章数、各产物路径、全部 WARN(日期 fallback、非 md 文件、无
    site_url 等)。

### 8.4 HTML 镜像页(html.rs)

**标题去重规则**:若事件流的**第一个块级元素**是 H1(即元数据标题的来源),HTML
投影从事件流中**剥掉这个 H1 块**(模板自己渲染 `<h1>`,并附带日期 footer);ANSI
投影**保留** H1(终端里它是唯一的标题显示)。若文中无 H1(标题走文件名 fallback),
HTML 模板仍渲染 `<h1>{{标题}}</h1>`,body 原样。两投影正文其余内容逐事件一致。

**正文 HTML**:用 `pulldown_cmark::html::push_html` 生成(转义由其保证);H1 剥除
后喂给它。镜像页完整模板(占位符 `{{...}}`,`{{#if site_url}}...{{/if}}` 条件块;
实现用简单字符串替换即可,值先做 HTML 属性转义 `& < > "`):

```html
<!doctype html>
<html lang="zh-CN">

<head>
  <meta charset="UTF-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1.0" />
  <title>{{TITLE}} — {{SITE_TITLE}}</title>
  <link rel="icon" type="image/svg+xml" href="/favicon.svg" />
  <link rel="stylesheet" href="/style.css" />
  <link rel="stylesheet" href="/blog.css" />
  {{#if site_url}}<link rel="canonical" href="{{SITE_URL}}/blog/{{SLUG}}/" />{{/if}}
  <meta name="description" content="{{EXCERPT}}" />
  <meta name="termblog-slug" content="{{SLUG}}" />
  <meta property="og:title" content="{{TITLE}}" />
  {{#if site_url}}<meta property="og:url" content="{{SITE_URL}}/blog/{{SLUG}}/" />{{/if}}
  <meta property="og:type" content="article" />
  <meta property="og:description" content="{{EXCERPT}}" />
  <meta property="og:site_name" content="{{SITE_TITLE}}" />
  {{#if site_url}}<link rel="alternate" type="application/atom+xml" title="{{SITE_TITLE}}" href="/atom.xml" />{{/if}}
</head>

<body>
  <!-- CRT 滤镜 defs: 与 frontend/index.html 中的完全一致(style.css 对
       #term-screen 应用 url(#crt-smudge), 缺 defs 会导致滤镜引用失效) -->
  <svg class="defs-only" aria-hidden="true" focusable="false">
    <defs>
      <filter id="crt-smudge" color-interpolation-filters="sRGB"
              x="0" y="0" width="100%" height="100%">
        <feTurbulence type="fractalNoise" baseFrequency="0.01" numOctaves="1" seed="5" result="noise"></feTurbulence>
        <feDisplacementMap in="SourceGraphic" in2="noise" scale="4"
                           xChannelSelector="R" yChannelSelector="G"></feDisplacementMap>
      </filter>
    </defs>
  </svg>
  <div id="app">
    <!-- 静态正文层: 覆盖在终端之上(不透明), 接管时淡出(§9.2) -->
    <div id="static-view">
      <article>
        <h1>{{TITLE}}</h1>
        {{BODY_HTML}}
        <footer class="post-meta">
          {{DATE}}{{#if ssh_hint}} · 终端里也可以读: ssh -p 2222 blog@{{HOST}} 然后敲 blog {{SLUG}}{{/if}}
        </footer>
      </article>
    </div>
    <!-- 终端层: 与首页同构, 静态层不透明地盖在上面; 需保持正常布局
         (不能 display:none, 否则 FitAddon 量不到尺寸) -->
    <div id="term-host">
      <div id="term-screen"></div>
    </div>
  </div>
  <button id="enter-terminal" type="button" hidden>进入终端 ↵</button>
  <script type="module" src="/assets/{{ENTRY_JS}}"></script>
</body>

</html>
```

要点:

- `termblog-slug` 的值是**纯 slug**(`hello`,不含 `blog/` 前缀、不含斜杠包裹),
  main.ts 据此拼 `blog {{slug}}\r`;
- `ssh_hint` 仅当 site_url 存在时渲染,`{{HOST}}` 取 site_url 的 host 部分;
- `<html lang>` 用 `zh-CN`(站点内容为中文);
- **没有 `<noscript>` 特殊处理**:静态层本身就是 no-JS 视图(terminal 藏在下面,
  JS 不跑就永远不接管);
- `#enter-terminal` 初始 `hidden`,由 main.ts 在降级时揭示(§9.2)。

### 8.5 ANSI 预渲染(ansi.rs)

输出:UTF-8 文本 + SGR 颜色序列,`less -R` 直接读;**目标宽度 76 显示列**
(unicode-width 计宽,CJK=2)。行尾 `\n`,文件尾保证一个 `\n`。

块级规则(事件 → 输出;`「」`内为字面输出):

| markdown 元素 | 渲染 |
|---|---|
| H1 | 空行 → `\x1b[1m标题\x1b[22m`(按 76 列折行,样式跨行重发)→ 空行 |
| H2–H6 | 空行 → `\x1b[1m…\x1b[22m` → 空行(H3 起在行首加对应缩进不要求,统一顶格即可) |
| 段落 | 折行到 76 列;块间空一行 |
| Strong | `\x1b[1m…\x1b[22m` |
| Emphasis | `\x1b[3m…\x1b[23m` |
| 行内代码 | `\x1b[36m…\x1b[39m` |
| 链接 | 文本后跟 `\x1b[36m(url)\x1b[39m`;文本与 url 相同或文本为空时只输出 url |
| 图片 | 一行 `image: <alt> <url>`(dim) |
| 围栏/缩进代码块 | 每行缩进 4 空格,**不折行**(长行原样),块前后空行;围栏线本身不输出;内容不上色 |
| 块引用 | 每行前缀 `\x1b[2m> \x1b[22m`,内容照常折行 |
| 无序列表 | 前缀 `  • `,续行缩进 4 空格;嵌套每级 +2 |
| 有序列表 | 前缀 `  1. `(序号递增),续行缩进 4 空格 |
| 分隔线 | 空行 → `\x1b[2m` + 76 个 `-` + `\x1b[22m` → 空行(用 ASCII,不用制表符:U+2500 宽度歧义) |
| 表格 | 表头行: 单元格以 ` | ` 连接;下一行 dim 的 `---- | ----` 分隔;数据行同表头。对齐信息丢弃 |
| Strikethrough | `\x1b[2m…\x1b[22m` |
| 软换行 | 空格 |
| 硬换行 | 换行 |
| 内联/块级 HTML 事件 | 跳过(站点约定纯 markdown) |

折行算法(必须处理样式):

- 贪心填充:逐"原子"(一个字符或一个空格)累加显示宽度,超 76 即断行;
- 断行机会:空格之后;或相邻两个字符中任一是宽字符(width==2,即 CJK 可在任意
  字符间断);
- 单个不可断原子超 76 列:硬切;
- **样式跨行**:断行时若有活动 SGR,行尾发 `\x1b[0m`,新行行首重发活动样式;
  行内的样式开启/关闭一律成对(开启 `1/3/36/2`,关闭 `22/23/39/22`),保证 less
  分屏重绘时不串色。

### 8.6 产物:`.rendered/` 与 `.index`

- `<content>/.rendered/<slug>`:无扩展名(不是 markdown),UTF-8;嵌套 slug 建子目录;
- `<content>/.rendered/.index`:一行一篇文章,TSV:

```
<YYYY-MM-DD>\t<slug>\t<title>
```

  按日期倒序排(与镜像列表一致);title 内 Tab 已被替换为空格。例:

```
2026-08-29	why-a-terminal-blog	为什么把博客做成一个终端
2026-08-29	hello	你好, 世界
```

### 8.7 列表页 `<dist>/blog/index.html`

纯静态、**不引导终端**(不引 entry JS、无 term-host/static-view),作用:人类浏览
`/blog/` + 爬虫枢纽。结构:`<!doctype html>` + 与镜像页相同的 head 基础项
(charset/viewport/title=`文章 — {{SITE_TITLE}}`/favicon/style.css/blog.css)+
`<main>` 内 `<h1>文章</h1>` + `<ul>` 每篇 `<li><a href="/blog/<slug>/">title</a>
<span class="date">date</span></li>`(排序同 .index)+ 指向 `/` 的返回链接。
site_url 存在时同样输出 atom alternate link。

### 8.8 首页注入(dist/index.html,幂等)

- **注入 1**:`</head>` 前插入 `<link rel="stylesheet" href="/blog.css" />`
  (若 dist/index.html 已含 `blog.css` 引用则跳过);
- **注入 2**:`</body>` 前插入带标记注释的 footer 块(幂等:若标记已存在,先删旧块
  再插入新块——vite build 会重新生成 index.html,旧块一般不存在,此逻辑防的是
  content-build 重复运行):

```html
<!--BEGIN termblog:blog-index-->
<footer id="blog-index">
  <h2>文章</h2>
  <ul>
    <li><span class="date">2026-08-29</span><a href="/blog/hello/">你好, 世界</a></li>
  </ul>
</footer>
<!--END termblog:blog-index-->
```

- footer 在 `#app`(100vh)之下,真人要滚动才看到,但**对爬虫和无 JS 访客完整
  可见**——这是刻意的非 cloaking 设计:同一份 HTML,谁都能看到这个列表(§12);
- 无文章时:跳过注入 2(WARN),注入 1 也跳过(首页没有镜像层样式需求);
- `#blog-index` 样式在 blog.css(§9.3)。

### 8.9 sitemap.xml / atom.xml / robots.txt(feed.rs)

全部写到 `<dist>/` 根。site_url 缺失时:robots.txt 照写(无 Sitemap 行),
sitemap/atom 跳过并 WARN。URL 一律尾斜杠形式。

`sitemap.xml`(XML 声明 + urlset;loc 用 site_url 拼接;lastmod 只给文章页):

```xml
<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <url><loc>{{SITE_URL}}/</loc></url>
  <url><loc>{{SITE_URL}}/blog/</loc></url>
  <url><loc>{{SITE_URL}}/blog/hello/</loc><lastmod>2026-08-29</lastmod></url>
</urlset>
```

`atom.xml`:

```xml
<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>{{SITE_TITLE}}</title>
  <id>{{SITE_URL}}/</id>
  <updated>{{feed_updated}}</updated>
  <link href="{{SITE_URL}}/" />
  <link href="{{SITE_URL}}/atom.xml" rel="self" />
  <author><name>{{SITE_TITLE}}</name></author>
  <entry>
    <title>{{title}}</title>
    <id>{{SITE_URL}}/blog/{{slug}}/</id>
    <link href="{{SITE_URL}}/blog/{{slug}}/" />
    <updated>{{rfc3339_date}}</updated>
    <summary>{{excerpt}}</summary>
    <content type="html">{{body_html 转义}}</content>
  </entry>
</feed>
```

- `feed_updated` = 最新文章日期(无文章则构建时刻);
- XML 转义统一函数:`& < > " '` → 实体;atom 的 content 是**转义后的完整 HTML**
  (type="html" 语义);摘要里的控制字符剔除;
- 每个文件生成后自检:用同一套转义保证 xmllint --noout 通过(验收 §13)。

`robots.txt`:

```
User-agent: *
Allow: /
{{#if site_url}}Sitemap: {{SITE_URL}}/sitemap.xml{{/if}}
```

(/ws 无需屏蔽:爬虫不升级 WebSocket。)

### 8.10 清理与幂等(content-build 拥有的输出树)

每次运行,生成前先删除旧产物,保证删文后不留僵尸:

- `rm -rf <dist>/blog`(整棵重建,含列表页);
- `rm -f <dist>/sitemap.xml <dist>/atom.xml <dist>/robots.txt`;
- `rm -rf <content>/.rendered`(整棵重建);
- dist/index.html 的注入块按 §8.8 幂等替换;
- **不碰** dist 的其余内容(assets/、fonts/、style.css、index.html 其余部分)。

### 8.11 错误处理矩阵

| 情形 | 行为 |
|---|---|
| slug 违规(§5.1) | 构建失败,列出全部违规文件 |
| dist 缺 `assets/index-*.js` | 失败:"先跑 gmake build-frontend(vite build 先于 content-build)" |
| `assets/index-*.js` 多于一个 | 失败,列出文件 |
| 单篇 md 解析异常(理论上 pulldown 不失败) | 失败并指名文件 |
| 文件无 git 历史(未提交) | mtime fallback + WARN |
| content/blog/ 为空/缺失 | WARN,继续(robots 仍产出) |
| site_url 缺失 | WARN,sitemap/atom 跳过,镜像页无 canonical/OG:url/ssh_hint |
| 写文件失败(权限等) | 失败,指名路径 |

### 8.12 单元测试(`cargo test -p content-build`,全部必须通过)

1. slug 校验:合法/大写/下划线/中文/`..`/空,逐例断言;
2. 标题提取:有 H1 / 无 H1(fallback 文件名)/ H1 带行内代码与链接;
3. 摘要:首段纯文本、折叠空白、160 截断;
4. HTML:首块 H1 被剥离、push_html 转义(`<script>` 字面量安全)、模板占位符替换
   后无残留 `{{`;
5. ANSI:76 列折行(纯 ASCII)、CJK 宽度(宽字符按 2 计,断行位置正确)、样式跨行
   重发(断言行尾 `\x1b[0m` 与行首重发)、代码块不折行、列表缩进;
6. .index:排序(TSV 内容断言)、title 含 Tab 被替换;
7. feed:XML 转义、尾斜杠 URL、atom content 转义;
8. 首页注入:幂等(注入两遍结果等于一遍)、无文章时不注入。

---

## 9. 规格 B:前端(frontend/)

### 9.1 新文件 `frontend/src/osc.ts`【行为规格】

```ts
// OSC 7777: 终端 → 浏览器地址栏。payload 不可信(jail 内任意进程可 printf),
// 只做"同源纯路径"的 replaceState, 天花板 = 地址栏显示一个站内路径。
import type { Terminal } from "@xterm/xterm";

export function installOsc(term: Terminal, onUrl: (path: string) => void): void;
```

实现约束(与 §6.2 一一对应):

- `term.parser.registerOscHandler(7777, payload => { …; return true; })`;
- payload 不以 `url=` 开头 → 直接 `return true`(吞掉);
- 取 `v = payload.slice(4)`;校验 `v.length >= 1 && v.length <= 512` 且
  `/^\/[a-z0-9-]*(\/[a-z0-9-]+)*\/?$/.test(v)`,不过 → `return true`;
- `try { history.replaceState(null, "", v); } catch { /* 忽略 */ }`;
- `onUrl(v)`,返回 `true`。

### 9.2 `frontend/src/main.ts` 改动(落地页接管状态机)

现有 131 行结构不变(boot → fonts → open → refit → connect → onmessage),新增
以下逻辑。时序常量:**接管兜底 5000ms;attach 回放宽限 1500ms;淡出 450ms**。

新增模块级状态(顶部,frame 编解码之后):

```ts
import { installOsc } from "./osc";

// 落地页检测: 镜像页带 <meta name="termblog-slug" content="hello">; 普通页面无此 meta
const LANDING = document.querySelector<HTMLMetaElement>('meta[name="termblog-slug"]')?.content?.trim() ?? "";
const onMirror = LANDING !== "";
let takeoverDone = false;        // 静态层是否已交给终端(一次性)
let lastAttached = false;        // 最近一次 Opened 的 attached
let sawData = false;             // 本连接是否已收到 Data(attach 回放标志)
```

(1)**boot() 内**,`term.open(...)` 之后、`connect()` 之前:

```ts
installOsc(term, handleOscUrl);
if (onMirror) {
  // JS 注入的状态行(无 JS 访客永远看不到, 诚实): 静态层顶部一行小字
  const s = document.createElement("p");
  s.className = "mirror-status";
  s.textContent = "正文由静态镜像渲染, 正在接入真实终端…";
  document.querySelector("#static-view article")!.prepend(s);
}
```

(2)**connect() 的 `T_OPENED` 分支**(`term.reset()`、存 token、`sendResize()` 之后):

```ts
lastAttached = !!opened.attached;
if (!lastAttached && onMirror) {
  // 替访客敲一条命令: 走正常键入通路, 进 zsh 历史, 诚实而非魔法。
  // 字节在 PTY 输入队列排队, zsh 就绪后按序执行, 无竞态。
  send(frame(T_DATA, textEnc.encode(`blog ${LANDING}\r`)));
}
armFallback(); // 5s 兜底(§9.2 末)
```

(3)**`T_DATA` 分支**,`term.write(payload)` 之外:

```ts
if (!sawData) {
  sawData = true;
  // attach 场景: 回放里可能没有 OSC(会话停在提示符), 收到数据 1.5s 后仍无 OSC
  // 就直接接管 —— 老用户回来看的是会话本身, 不必等 blog 命令。
  if (onMirror && lastAttached && !takeoverDone) {
    setTimeout(takeOver, 1500);
  }
}
```

(4)**`T_CLOSED` 分支**与 **`socket.onclose`** 末尾各加:
`if (onMirror && !takeoverDone) revealEnterButton();`
(会话死了 / WS 断了:静态层保留可读,给出手动入口;若 OSC 稍后才到,takeOver 会
顺手把按钮重新藏掉,按钮出现不是终态)。

(5)**OSC 回调**:

```ts
function handleOscUrl(_path: string) {
  // replaceState 已在 osc.ts 完成; 这里只负责镜像层接管:
  // 收到本会话首条 OSC = blog 命令已跑起来、内容已在画 —— 此时淡出静态层才不闪。
  if (onMirror && !takeoverDone) takeOver();
}
```

(6)**接管与兜底**:

```ts
function takeOver() {
  if (takeoverDone) return;
  takeoverDone = true;
  const sv = document.getElementById("static-view");
  sv?.classList.add("fade-out");            // 400ms 淡出(blog.css)
  setTimeout(() => sv?.remove(), 450);      // 之后彻底移出 DOM
  document.getElementById("enter-terminal")?.setAttribute("hidden", "");
}
function revealEnterButton() {
  document.getElementById("enter-terminal")?.removeAttribute("hidden");
}
function armFallback() {
  setTimeout(() => { if (!takeoverDone) revealEnterButton(); }, 5000);
}
document.getElementById("enter-terminal")?.addEventListener("click", takeOver);
```

行为要点(验收依据):

- **静态层是覆盖层而不是 display:none 的兄弟层**:镜像页里 `#static-view` 以
  `position:absolute` 不透明盖在 `#term-host` 上(§9.3),终端全程保持正常布局,
  FitAddon 在字体就绪时量的尺寸有效——**绝不可用 display:none 藏 term-host 再切换**,
  那会让 fit 量到 0 列;
- 新会话(落地):静态层淡出的**唯一自动信号是首条 OSC 7777**(blog 命令已在画),
  而不是"首条 Data"(避免把 zsh 启动 MOTD 的半成品亮给用户);5s 无 OSC(jail 满/
  命令失败/连不上)→ 静态层保留 + 右上角"进入终端"按钮;
- attach(刷新):scrollback 回放里的 OSC 触发接管(老路径);回放里没有 OSC 时由
  (3) 的 1.5s 宽限兜底;地址栏由 §6.3 的收敛语义自动恢复;
- 普通页面(`onMirror == false`):以上全部为 no-op,行为与现在逐字节一致;
- `enter-terminal` 的 click 绑定对普通页面也执行(元素不存在,`?.` 短路,无害)。

### 9.3 新文件 `frontend/public/blog.css`【完整源码】

```css
/* blog.css —— M5 静态镜像层 + 文章列表样式。加载于镜像页与注入后的首页。 */

/* #app 变为定位上下文, 静态层以覆盖层姿态盖在终端之上(不能 display:none 终端,
   否则 FitAddon 量不到尺寸)。inset 与 #app 的 padding 对齐。 */
#app { position: relative; }

#static-view {
  position: absolute;
  inset: 12px;
  z-index: 20;
  background: var(--bg);      /* 不透明: 终端在其下就绪但不被看见 */
  overflow-y: auto;
  padding: 24px 18px;
  transition: opacity 400ms ease-out;
}
#static-view.fade-out { opacity: 0; pointer-events: none; }

#static-view article {
  max-width: 80ch;
  margin: 0 auto;
  font-size: 15px;
  line-height: 1.7;
}
#static-view article h1 { font-size: 1.6em; margin: 0 0 .6em; }
#static-view article h2 { font-size: 1.3em; margin: 1.4em 0 .5em; }
#static-view article h3 { font-size: 1.1em; margin: 1.2em 0 .4em; }
#static-view article p { margin: .6em 0; }
#static-view article pre {
  background: #f0f2f4;
  border: 1px solid var(--border);
  border-radius: 6px;
  padding: 10px 12px;
  overflow-x: auto;
}
#static-view article code { font-family: inherit; }
#static-view article pre code { padding: 0; background: none; border: 0; }
#static-view article :not(pre) > code {
  background: #f0f2f4;
  border-radius: 4px;
  padding: 1px 5px;
}
#static-view article blockquote {
  border-left: 3px solid var(--border);
  margin: .8em 0;
  padding: 2px 0 2px 14px;
  color: #6a737d;
}
#static-view article a { color: #0969da; }
#static-view article img { max-width: 100%; }
.post-meta { color: #6a737d; font-size: 13px; margin-top: 2.5em; }

.mirror-status { color: #6a737d; font-size: 13px; margin: 0 0 1.5em; }

/* 降级手动入口: 由 main.ts 揭示 */
#enter-terminal {
  position: fixed;
  top: 16px;
  right: 16px;
  z-index: 30;
  font: inherit;
  font-size: 13px;
  padding: 6px 14px;
  background: var(--fg);
  color: var(--bg);
  border: none;
  border-radius: 6px;
  cursor: pointer;
}
#enter-terminal:hover { opacity: .85; }

/* 首页底部文章列表(content-build 注入, 对爬虫与无 JS 访客可见) */
#blog-index {
  max-width: 80ch;
  margin: 0 auto 24px;
  padding: 8px 12px;
  border-top: 1px solid var(--border);
  font-size: 13px;
  color: #6a737d;
}
#blog-index h2 { font-size: 1em; margin: 12px 0 6px; color: var(--fg); }
#blog-index ul { list-style: none; margin: 0; padding: 0; }
#blog-index li { margin: 4px 0; }
#blog-index a { color: #0969da; text-decoration: none; }
#blog-index .date { color: #6a737d; margin-right: 10px; }

@media (max-width: 640px) {
  #static-view { inset: 4px; padding: 14px 8px; }
}
```

---

## 10. 规格 C:jail 侧(jailtpl/)

### 10.1 新文件 `jailtpl/bin/webctl`【完整源码】

```sh
#!/bin/sh
# webctl —— 终端↔网页桥(termblog M5)。
# 唯一职责: 把 OSC 7777 序列打到 stdout, 由前端的 osc.ts 消费(replaceState)。
# ssh 客户端不认识该 OSC, 按 VT 规范吞掉, 无副作用。
# 用法: webctl url /path       (path 必须以 / 开头)
# 子命令位留给后续(theme 等, 本期不做)。

case "$1" in
    url)
        case "$2" in
            /*) printf '\033]7777;url=%s\007' "$2" ;;
            *) echo "usage: webctl url /path" >&2; exit 2 ;;
        esac
        ;;
    *)
        echo "usage: webctl url /path" >&2
        exit 2
        ;;
esac
```

### 10.2 新文件 `jailtpl/bin/blog`【完整源码】

```sh
#!/bin/sh
# blog —— 文章阅读入口(termblog M5)。列表 / 阅读, 与网页镜像同源同文。
#   blog            列出文章(读 ~/blog/.rendered/.index)
#   blog <slug>     阅读一篇: 发 OSC 同步地址栏 → less -R → 退出后复位地址栏
# 模板构建时由 build-template.sh 安装到 /usr/local/bin(0555)。

set -eu
R="$HOME/blog/.rendered"

case "${1:-}" in
    "")
        if [ -f "$R/.index" ]; then
            cat "$R/.index"
        else
            echo "暂无文章(模板里没有 .rendered/.index, 重建模板试试)"
        fi
        ;;
    *)
        slug=$1
        case "$slug" in
            *[!a-z0-9/-]*) echo "blog: 非法 slug: $slug" >&2; exit 2 ;;
        esac
        if [ ! -f "$R/$slug" ]; then
            echo "blog: 没有这篇文章: $slug (敲 blog 看列表)" >&2
            exit 1
        fi
        # 读文章: 先同步地址栏(带尾斜杠 = canonical 形态), less 退出后复位。
        # webctl 不存在时(旧模板)优雅降级为纯阅读, 不报错。
        if command -v webctl >/dev/null 2>&1; then
            webctl url "/blog/$slug/"
        fi
        less -R "$R/$slug" || true
        if command -v webctl >/dev/null 2>&1; then
            webctl url /
        fi
        ;;
esac
```

(slug 白名单 `[a-z0-9/-]` 与 §5.1 一致;`..` 因不含点字符天然不可构造,拼接
`$R/$slug` 无逃逸。)

### 10.3 `jailtpl/build-template.sh` 修改

三处,其余不动:

**(a) 第 6 步的 .zshrc heredoc 整体替换为**(新增两行:`export LANG=C.UTF-8`
保证中文在 less/zsh 里按字符处理;末尾 MOTD 提示 blog 命令 —— 每个会话 jail 都是
全新实例,每次登录提示一次恰好):

```sh
cat > "$MOUNT/home/$GUEST/.zshrc" <<'EOF'
# termblog guest shell —— 每个访客一个真实 FreeBSD jail
export PATH=/usr/local/bin:/usr/bin:/bin
export LANG=C.UTF-8
umask 022
PS1='%F{green}blog@jail%f %~ %# '
setopt INTERACTIVE_COMMENTS
echo '博客: 敲 blog 看文章列表, blog <slug> 读一篇(如 blog hello)'
EOF
```

**(b) 第 7 步内容拷贝之后新增 7.5 步**(脚本目录下 bin/ 两个脚本装进模板,guest 的
受限 PATH 第一位就是 /usr/local/bin):

```sh
# 7.5 blog / webctl 命令(0555, 只读; .rendered/ 已随第 7 步 content 拷贝进 ~/blog)
if [ -d "$SCRIPT_DIR/bin" ]; then
    install -m 555 "$SCRIPT_DIR/bin/blog" "$SCRIPT_DIR/bin/webctl" "$MOUNT/usr/local/bin/"
fi
```

**(c)** 无(readonly/快照收尾逻辑不变;`~/blog/.rendered/` 的属主跟随既有
`chown -R 1001:1001`)。

### 10.4 示例文章与 README(内容目录)

`jailtpl/content/blog/hello.md`(同时是 ANSI/HTML 渲染器的联合测试夹具,覆盖:
H1/H2、段落、CJK 折行、行内代码、粗斜体、链接、列表、引用、围栏代码块、分隔线):

````markdown
# 你好, 世界

这是 termblog 的第一篇文章。你现在读到的, 是同一份 markdown 的两种投影之一:
搜索引擎看到的是这个静态镜像, 终端用户看到的是 `blog hello` 的排版输出。

## 两种入口, 一个源头

- 网页:打开 `/blog/hello/`,几秒后由真实终端接管
- SSH:`ssh -p 2222 blog@<host>`,然后敲 `blog hello`

> 文章本体只有一个:`jailtpl/content/blog/hello.md`,其余全是编译产物。

## 排版自检

```sh
$ blog hello
```

行内代码 `less -R`、**粗体**、*斜体* 与 [链接](https://example.com) 会被两种
渲染器各自正确呈现;中文段落按 76 列折行,连字符与标点悬挂不做特殊处理。

---

这里是分隔线之后的一段,验证块间距。
````

`jailtpl/content/blog/why-a-terminal-blog.md`(第二篇,给列表排序/收敛验收用;
两三段即可,内容随意但须含一个 H1 与至少一段中文长段落)。

`jailtpl/content/README.md` 替换为:

```markdown
# 博客内容目录

build-template.sh 会把这里的全部文件拷进 jail 模板的 ~/blog/。约定:

- 文章放 `blog/` 子树:每篇一个 md,**无 frontmatter**;
- 文件名即 slug,字符集限 `[a-z0-9/-]`(违规构建失败):`blog/hello.md` →
  网页 `/blog/hello/`、终端 `blog hello`;
- 标题取文中第一个 `# 一级标题`(缺失则用文件名);日期取 git 最后提交时间;
- 顶层散文件(本 README、about.md)只进 jail,不进镜像与 feed;
- `blog/` 下的 md 由 content-build 同时编译为网页镜像(frontend/dist/blog/)与
  终端预渲染(.rendered/,均为生成物,勿手改、勿提交)。
```

---

## 11. 规格 D:配置与构建集成

### 11.1 配置增量

`crates/core/src/config.rs` 的 `WebConfig` 增加字段(serde default,向后兼容,
三个服务的二进制行为不变):

```rust
/// 站点对外绝对地址(https://host[:port], 尾部不带 /)。
/// canonical / sitemap / atom / OG 的前缀, 仅 content-build 消费。
pub site_url: Option<String>,
```

`WebConfig::default()` 里为 `None`。`etc/termblog.toml` 的 `[web]` 段追加(**保持
注释态**——deploy-root.sh 会把样例刷成生产配置,占位域名直接生效会造成 canonical
指向 example.com):

```toml
# 站点对外绝对地址(不带尾斜杠)。canonical/sitemap/atom/OG 用;不设则镜像页
# 照常生成, 只是不产出 sitemap/atom 与 canonical(部署后务必设置!)
# site_url = "https://your-real-domain.example.com"
```

### 11.2 Makefile 修改

```make
BIN_CONTENT := target/release/content-build
```

目标改动(其余不动;`.PHONY` 加 `build-content`):

```make
# 内容编译器: md → HTML 镜像(进 dist) + ANSI 预渲染(进 jailtpl/content/.rendered)
# 注意顺序: 必须在 vite build 之后跑(产物写入 dist 且需读 assets/index-*.js)
build-content:
	cargo build --release -p content-build
	$(BIN_CONTENT) --content jailtpl/content --dist frontend/dist

# ── 构建: 一次产出 web + ssh + jaild 三个二进制 + 前端 + 内容镜像 ──
build: build-frontend
	cargo build --release
	$(BIN_CONTENT) --content jailtpl/content --dist frontend/dist
```

`clean` 目标追加一行:`rm -rf jailtpl/content/.rendered`。

(vite build 清空 dist → 镜像被冲掉的风险由此顺序消解:任何 `gmake build` 都以
content-build 收尾;单独重跑 `gmake build-frontend` 后记得 `gmake build-content`。)

### 11.3 .gitignore

追加一行(生成物不入库):

```
jailtpl/content/.rendered/
```

### 11.4 新文件 `scripts/update-content.sh`【完整源码】(内容更新 runbook)

背景:内容更新涉及两侧——静态镜像(纯文件,即时生效)与 jail 模板(readonly ZFS
快照,必须销毁重建;重建会杀掉全部在线会话,且模板快照在有会话 clone 残留时删不掉,
必须先清)。该流程做成脚本而非手改 deploy-root.sh:通用部署脚本不应背负"销毁模板"
这种破坏性逻辑。

```sh
#!/bin/sh
# update-content.sh —— 内容更新两步走(root 运行)
#
#   sh scripts/update-content.sh               # 镜像 + jail 两侧(重建模板, 杀会话)
#   sh scripts/update-content.sh --static-only # 只发布静态镜像(jail 侧下次重建跟进)
#
# 前提: 文章 md 已改好并提交(git 日期进 sitemap/atom)。

set -eu
REPO=/home/yzs/termblog
STATIC_DIR=/usr/local/share/termblog/frontend

[ "$(id -u)" -eq 0 ] || { echo "需要 root (service/zfs/install)"; exit 1; }

echo ">> 1/3 编译内容(以 yzs, 产物进 frontend/dist 与 jailtpl/content/.rendered)"
su -l yzs -c "cd $REPO && gmake build-content"

echo ">> 2/3 发布静态镜像(纯文件替换, 无感, 不重启)"
rm -rf "$STATIC_DIR/blog"
cp -R "$REPO/frontend/dist/." "$STATIC_DIR/"

if [ "${1:-}" = "--static-only" ]; then
    echo ">> 完成(仅镜像)。jail 侧将在下次模板重建时跟进。"
    exit 0
fi

echo ">> 3/3 重建 jail 模板(杀掉全部在线会话, 走 deploy-root.sh 全流程)"
service termblog stop 2>/dev/null || true
service jaild stop 2>/dev/null || true
sleep 5
# 会话 jail 持有模板快照的 clone, 不先清掉则 zfs destroy template 失败
for j in $(jls name 2>/dev/null | grep '^s-'); do
    jail -r "$j" 2>/dev/null || true
done
for ds in $(zfs list -H -o name -r zroot/jails 2>/dev/null | grep 'zroot/jails/s-'); do
    zfs destroy -f "$ds" 2>/dev/null || true
done
zfs destroy -r zroot/jails/template 2>/dev/null || true
sh "$REPO/scripts/deploy-root.sh"
```

说明:镜像侧与 jail 侧允许短暂不一致(镜像先新、jail 后新),可接受;`--static-only`
就是利用这一点做"发布即可读"的快路径。deploy-root.sh 本身**零修改**:其
`gmake build` 链已含 content-build,`cp -R frontend/dist/.` 已携带镜像产物,模板
不存在时会自动重建。

---

## 12. 安全与 SEO 红线(实现自检清单,逐条对照)

**SEO(踩线即作弊,不可越)**

- [ ] 无 UA 嗅探、无 cloaking:所有 UA 拿到同一份 HTML;静态层对真人同样可见
      (几秒后被终端接管),Googlebot 与无 JS 访客停留在完整全文;
- [ ] 每个镜像页:canonical(设 site_url 时)指向 `site_url + /blog/<slug>/`,
      OG:title/description/type=article/url/site_name 齐备,`<title>` 含文章标题;
- [ ] 全站内部链接(首页 footer、/blog/ 列表、atom)统一**尾斜杠**形态,与
      canonical 一致(§4.1 的 307 实测结论);
- [ ] 无 JS / 配额满 / WS 失败:文章完整可读(静态层不被移除);
- [ ] 首页静态链接列表保证爬虫无 sitemap 也能沿链接发现全部文章;
- [ ] sitemap/atom 通过 `xmllint --noout`。

**安全**

- [ ] osc.ts:payload 仅 `url=` + 同源纯路径白名单(§6.2),只 replaceState;
- [ ] blog 脚本 slug 白名单 + 存在性检查,无路径逃逸;
- [ ] webctl 参数必须 `/` 开头,其余 usage 报错;
- [ ] HTML 输出全转义(push_html + 手写模板值的属性转义);
- [ ] 数据面(proto/jaild/ssh/web)零改动 —— diff 审查确认。

---

## 13. 里程碑与验收(M5.1 → M5.4,顺序执行)

> 验收命令均从仓库根执行;`$DEV` 指 §4 条目 4 的开发实例
> (`TERMBLOG_CONFIG=/nonexistent.toml TERMBLOG_LISTEN=127.0.0.1:18099 ./target/release/termblog-web &`)。
> `[root]` 需 root;`[人工]` 需人类开浏览器。

### M5.1 编译器(content-build + 示例内容 + config 字段)

交付:crate、示例文章(§10.4)、config site_url(§11.1)、Makefile(§11.2)、
.gitignore(§11.3)、blog.css 占位可后补(前端不本步重点)。

验收(全绿才进 M5.2):

```sh
cargo test -p content-build                 # §8.12 全过
gmake build                                 # 全链路构建成功(无 site_url: 无 sitemap, 有 WARN, 预期)
ls frontend/dist/blog/hello/index.html frontend/dist/blog/index.html
ls jailtpl/content/.rendered/hello jailtpl/content/.rendered/.index

# 带 site_url 再跑一遍编译器, 验 feed(本地验收用参数覆盖; 未提交的示例文章走
# mtime fallback, 出现 WARN 是预期):
target/release/content-build --content jailtpl/content --dist frontend/dist \
    --site-url https://blog.example.com
ls frontend/dist/sitemap.xml frontend/dist/atom.xml frontend/dist/robots.txt
xmllint --noout frontend/dist/sitemap.xml frontend/dist/atom.xml && echo XML_OK
grep -q '/blog/hello/' frontend/dist/sitemap.xml
grep -q '<entry>' frontend/dist/atom.xml
grep -q 'Sitemap: https://blog.example.com/sitemap.xml' frontend/dist/robots.txt
grep -q 'termblog-slug' frontend/dist/blog/hello/index.html
grep -q '<article>' frontend/dist/blog/hello/index.html
grep -c 'termblog:blog-index' frontend/dist/index.html   # = 2(BEGIN+END 各一次)

# 静态服务实测(ServeDir 尾斜杠语义):
$DEV
curl -s http://127.0.0.1:18099/blog/hello/ | grep -q 'termblog-slug'    # 200
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:18099/blog/hello   # 307 → /blog/hello/
kill %1

# ANSI 侧人工核对面板:
less -R jailtpl/content/.rendered/hello    # 标题加粗/折行/代码块缩进/颜色正常
# 幂等: 连跑两遍 content-build, diff 产物无变化; 删一篇 md 重跑, dist/blog/<slug>/ 消失
```

### M5.2 jail 侧(webctl + blog + 模板重建)`[root]`

交付:webctl、blog(§10.1/§10.2)、build-template.sh 修改(§10.3)、
`scripts/update-content.sh`(§11.4 —— 本步就需要它来重建模板:模板数据集已存在,
直接重跑 build-template.sh 会被拒绝,必须走"清会话 clone → destroy 模板 →
deploy-root.sh 重建"的流程,脚本已封装)。

验收(模板重建后;ssh 参数沿用 verify-jail.sh 的样式,下文以 `$SSH` 指代):

```sh
SSH="ssh -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
     -o PreferredAuthentications=none -o LogLevel=ERROR blog@127.0.0.1"

# webctl/blog 已就位(输出两行路径即通过):
(sleep 2; printf 'command -v blog\ncommand -v webctl\n'; sleep 1) | timeout 10 $SSH

# blog 列表(日期倒序、含标题):
(sleep 2; printf 'blog\n'; sleep 1) | timeout 10 $SSH 2>&1 | tr -d '\r' | grep hello

# OSC 逐字节可见: cat -v 把 ESC 显示为 ^[、BEL 显示为 ^G。
# 进入与复位两条都要有(注意 grep -F 字面量, BEL 在行尾是 ^G 不是行结束):
(sleep 2; printf 'blog hello\n'; sleep 3) | timeout 15 $SSH 2>&1 | cat -v \
  | grep -cF ']7777;url='                          # ≥ 2
(sleep 2; printf 'blog hello\n'; sleep 3) | timeout 15 $SSH 2>&1 | cat -v \
  | grep -qF ']7777;url=/blog/hello/^G'            # 进入: /blog/hello/
(sleep 2; printf 'blog hello\n'; sleep 3) | timeout 15 $SSH 2>&1 | cat -v \
  | grep -qF ']7777;url=/^G'                       # 复位: /
# less 在无 pty 的管道里可能打印 "WARNING: terminal is not fully functional", 无害

# 纯 ssh 会话无杂质(未知 OSC 被客户端吞掉, 肉眼无异样):
(sleep 2; printf 'echo CLEAN_$((6*7))\n'; sleep 1) | timeout 10 $SSH 2>&1 | tr -d '\r' | grep CLEAN_42
# 中文正常(jail 内 LANG=C.UTF-8, less 按 UTF-8 处理):
(sleep 2; printf 'blog hello\n'; sleep 3) | timeout 15 $SSH 2>&1 | grep -q '你好'
```

`[人工]`:手机窄终端 ssh 读一篇,确认 76 列内容软换行可接受。

### M5.3 前端(osc.ts + 接管状态机)

验收:

```sh
cd frontend && npx tsc --noEmit && cd ..     # 类型过
gmake build && $DEV
# 镜像页 DOM 结构就位(静态层 + 手动入口按钮在源码里; 5s 揭示与淡出是 JS 行为,
# 属 [人工] 项):
curl -s http://127.0.0.1:18099/blog/hello/ | grep -q 'id="static-view"'
curl -s http://127.0.0.1:18099/blog/hello/ | grep -q 'id="enter-terminal"'
curl -s http://127.0.0.1:18099/blog/hello/ | grep -q 'assets/index-'   # 注入了带 hash 的入口 JS
kill %1
```

`[人工]`(对生产实例 :8080;root 先发布新前端:`su -l yzs -c "cd /home/yzs/termblog && gmake build"`,
再 `sh scripts/update-content.sh --static-only` 同步 dist):

1. 打开 `http://<host>:8080/blog/hello/`:静态正文秒开 → 数秒内淡出,终端显示
   `blog@jail ~ % blog hello` 与 less 内容 —— 与 ssh 敲同一命令逐字节一致;
2. 终端里 `blog why-a-terminal-blog`:地址栏变为 `/blog/why-a-terminal-blog/`,
   页面不刷新;`q` 退出后地址栏回到 `/`;
3. 读文中刷新页面:接回原会话,scrollback 恢复画面,地址栏收敛回 `/blog/.../`;
4. 首页(无 slug):行为与现在完全一致,无静态层、无自动敲命令;滚到底部可见
   文章列表(带链接);
5. 停掉 jaild(或占满配额)再开落地页:静态层保留可读,右上角"进入终端"出现。

### M5.4 收尾(部署链 + 验收脚本)

交付:`scripts/verify-m5.sh`【完整源码如下】+ update-content.sh(§11.4)。

```sh
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
(sleep 2; printf 'blog hello\n'; sleep 3) | timeout 15 $SSH 2>&1 | cat -v \
  | grep -qF ']7777;url=/blog/hello/^G'
check $? "ssh: blog hello 发出 OSC(进入)"
(sleep 2; printf 'blog hello\n'; sleep 3) | timeout 15 $SSH 2>&1 | cat -v \
  | grep -qF ']7777;url=/^G'
check $? "ssh: 退出 less 后 OSC 复位 /"

echo ""
echo "== 结果: $pass 通过, $fail 失败 =="
[ "$fail" -eq 0 ]
```

终验收(全链路):

```sh
gmake build && gmake install        # install 的 cp -R 已携带镜像产物 [root]
sh scripts/verify-m5.sh             # 全绿
sh scripts/verify-jail.sh [root]    # 16 项不回归
```

`[人工]`:Google Search Console 提交 sitemap(需真实域名 + site_url 配置);URL
检查工具抓 `/blog/hello/` 应返回"已编入索引"可读全文。

---

## 14. 风险与预案

| 风险 | 预案 |
|---|---|
| jail 内访客伪造 OSC 7777(printf 任意 payload) | osc.ts 只认 `url=` + 同源纯路径白名单;只 replaceState 不导航;天花板="地址栏显示站内路径"(§6.2) |
| slug 特殊字符/中文文件名 | 构建期白名单 `^[a-z0-9/-]+$`,违规即失败,不做运行期转义(§5.1) |
| vite build 清空 dist 冲掉镜像 | 构建顺序固定 vite → content-build;Makefile 显式排序 + content-build 的入口 JS glob 自检(缺则报错)(§8.3/§11.2) |
| entry JS 文件名带 hash,镜像页无法静态引用 | content-build glob `assets/index-*.js` 注入(恰 1 个,否则失败)(§8.3) |
| 静态层盖住终端导致 fit 量不到尺寸 | 静态层是不透明覆盖层(position:absolute),终端保持正常布局,禁止 display:none(§9.2/§9.3) |
| 自动敲命令与 MOTD/zsh 启动竞态 | 命令字节在 PTY 输入队列排队,zsh 就绪后按序执行;接管时机以首条 OSC 为准而非首字节(§9.2) |
| 接管超时(jail 满/命令失败) | 5s 兜底:静态层保留 + "进入终端"手动入口,降级即静态博客(§9.2) |
| attach 回放里没有 OSC(会话停在提示符) | 收到首帧 Data 后 1.5s 宽限即接管;URL 以最后一次 replaceState 为准(§9.2) |
| scrollback 128KiB 截断把 OSC 拦腰截断 | 退化为纯文本上屏,不产生错误跳转;既有 ANSI 截断问题,不处理(§6.3) |
| git 日期在 shallow clone/脏树失效 | fallback mtime + 构建告警(§5.2) |
| 镜像与 jail 模板短暂不一致 | 接受;update-content.sh 的两段式流程(镜像即时、jail 重建)即为此设计(§11.4) |
| 模板重建时有会话 clone 残留导致 destroy 失败 | update-content.sh 先 `jail -r` + 清 s-* 数据集再 destroy(§11.4) |
| 生产配置误带占位 site_url(example.com 进 canonical) | 样例保持注释态;部署后人工设置(§11.1) |
| ANSI 76 列 vs 更窄终端 | less -R 软换行兜底;76 覆盖绝大多数;按需实时渲染本期不做 |

---

## 15. 明确不做(本期)

webctl 除 `url` 外的子命令(theme 等)、文章内互链在终端里的可点击化、实时(按
终端宽度)markdown 渲染、评论、Anubis 前置、多语言 slug、顶层散文件(about.md 等)
的镜像、`/blog/` 列表页引导终端、sitemap 分页、JSON feed。

---

## 16. 交付偏差补记(实现期修订, 与冻结决策冲突处以此为准)

1. **blog 读取源与 jail 目录布局**(用户修订, 替代 §10.2 / §10.3):
   - jail 布局打平: 文章目录即 `~/blog/`(与 URL 前缀 `/blog/` 一一对应,
     不再有 `~/blog/blog/`), 预渲染等工具产物移到隐藏目录 `~/.rendered/`;
     `content/README.md` 只留仓库, 不进 jail;
   - `blog` 保持 cat 式接口(任意路径 md 可读), 但 `~/blog/` 下的文章读
     预渲染排版(`~/.rendered/<slug>`, 无产物回退原始 md 并提示);
     `blog hello` 等价 `blog ~/blog/hello.md`; `cat`/`less` 读原始 md;
   - 镜像页 footer 的 ssh 提示随之改为 `blog <slug>`, 自动命令改为
     `blog ~/blog/<slug>.md`(§9.2);
   - verify-m5.sh 增加"预渲染排版含 ANSI 粗体"检查。
2. push_html 不转义行内 HTML: html.rs 在事件进 push_html 前统一
   escape_raw_html, atom 内容因此双重转义(§8 决策偏差)。
3. pulldown-cmark 0.13 紧凑列表项无 Paragraph 包裹、TableHead 无 TableRow
   包裹: ansi.rs 补隐式段落与表头解析(§8 决策偏差)。
4. 博客目录为空时跳过 entry js/css 的"恰 1 个"强制(§8.3 决策偏差)。
5. **镜像页一律全新会话(Open.fresh), attach 只留给首页**(用户修订, 替代
   §9.2 的 attach 宽限接管): 实践发现"attach 回镜像页"要求浏览器判断旧
   shell 状态(在文章里 / 别的分页器 / 提示符)——曾依次尝试 OSC 心跳 +
   ^C/q⏎/^U 恢复序列与 REPLAY_DONE 边界标记, 最终用户拍板换思路: 镜像页
   Open 带 fresh=true, 网关跳过 attach、回收该 token 的**闲置**旧会话
   (connections==0 才回收, 有别的标签页在用则保留)后开新会话, 落地永远是
   干净 shell → 等首帧 → 自动敲 blog; 状态判断与恢复按键整块删除, 每次
   访问行为完全一致。quota 收益: 旧 jail 立即回收, 不等 60s 宽限(每 IP
   ≤3, 快速切换文章不会超配额)。attach 仅用于首页刷新(终端浏览保留历史)。
   镜像页接管前只对本页路径做 replaceState, 接管后放开; osc.ts 瘦身为纯
   解析 + 白名单, replaceState 政策上收 main.ts。
