# plan_image_v2 —— 博客图片支持(第二期:终端内嵌像素图)

> 目标读者:实现者(可能是另一个 AI)。本文是自包含规格。
> **前置:`plan_image_v1.md` 已完成并部署**。v1 的占位框就是本期的降级形态,
> 本期任何一环缺失/失败,体验都必须完整回落到 v1。
> 仓库根:`/home/yzs/termblog`。先读 `README.md` 与 v1 plan。

## 1. 目标 / 非目标

**目标**:web 访客(浏览器 xterm.js)在终端里 `blog <文章>` 时,图片以像素
形式内嵌显示在文章流中(iTerm2 Inline Images Protocol);无能力的会话
(SSH、未装 addon 的前端)自动回落 v1 占位框。

**非目标(本期不做,列此防 scope creep)**:
- SSH 侧终端能力探测(DA1/sixel 主动查询)——v2 的 SSH 访客固定走 v1 降级;
- sixel / kitty 协议(协议层选型见 §3,留了扩展位);
- TUI 阅读器内的 `/` 搜索;无图文章切到 TUI(它们继续走 less);
- TUI 内链接点击(OSC 8 在 TUI 里降级为样式文本,链接完整保留在镜像页);
- 前端 lightbox。

## 2. 架构决策记录(为什么这些方向被排除)

- **不用 less 显示图片**:`less -R` 只透传 SGR(新版额外透传 OSC 8),图像
  协议序列会被吃掉;less 的行模型不知道图片占 N 行,滚动必然错位。所以
  带图文章在有能力的会话里改走**自写 TUI 阅读器**(§5.6)。
- **scrollback 回放不是障碍**(已逐条核实):
  - 镜像页刷新:`Open.fresh=1`(`frontend/src/main.ts` ~L107),永远开新会话,
    不回放;
  - 首页终端里读文章再刷新:`blog` 已用 OSC 7777 把地址栏同步成
    `/blog/<slug>/`,刷新加载的是**镜像页**,仍是 fresh 新会话;
  - 首页 attach 只发生在地址栏为 `/` 时(文章已退出);且 web 层 scrollback
    上限 128 KiB(`crates/servers/web/src/sessions.rs` L32 `SCROLLBACK_BYTES`),
    图像字节基本留不进回放;attach 后网关会推 Resize → SIGWINCH,前台 TUI
    在回放内容之上整体重绘,残片被盖掉(与现在 less 的恢复模式相同)。
- **协议与重活靠库,阅读器本体自写**:与 play.rs 参考 asciinema 的模式一致。
  桌面端 markdown 查看器(mdfried/mdterm/mdink 等)不搬——它们面向本地可信
  终端,且会顶掉我们 76 列 CJK 预排版的排版主权。引入库收敛为:
  `ratatui`(TUI 框架)+ `crossterm`(终端控制)+ `image`(解码)。
  **ratatui-image 评估后排除**:其 iterm2 后端每次 render 把 DynamicImage
  重编码成 PNG(JPEG 源成倍膨胀,预算失控,§5.6.4)、默认 chafa-dyn 特性要
  链接 jail 里没有的 libchafa、Picker API 已经漂移。IIP 编码自写
  (序列简单,<100 行),直接透传 v1 处理后字节。
- **PTY 链路字节零丢失**:终端字节流是有状态协议(ANSI/IIP 无帧界),任意
  丢一块都会破坏后续序列且不能靠"下次重绘"自愈。所以三层 broadcast
  (Lagged 即丢帧)全部改为有界 mpsc + 背压;web 侧慢客户端背压或断开,
  绝不静默丢帧(§5.8)。

## 3. 协议选型:iTerm2 Inline Images Protocol(IIP)

- xterm.js 的 `@xterm/addon-image` 支持 IIP 与 sixel;IIP 实现最简单
  (base64 内嵌,无调色板/光栅顺序问题),addon-image 支持 png/jpeg/gif
  载荷。
- **IIP 直接携带 v1 处理后的字节,不做二次编码**:线上字节 = 处理后字节
  × 4/3,于是 v1 的构建期预算(单篇 ≤1.5 MiB、单张位图 ≤256 KiB、gif
  ≤512 KiB)直接约束线上流量(单篇 ≈2 MiB、单图 ≤683 KiB)。
  原计划依赖 ratatui-image 编码,但其 iterm2 后端会把 DynamicImage 重编码
  成 PNG,97 KiB 的 JPEG 重编码后可膨胀一个数量级,"1.5 MiB × 1.37"推导
  不成立——这正是 §2 排除该库的原因之一,IIP 编码自写(§5.6.4)。
- 构建期保证处理后产物只含 png/jpeg/gif(webp 产物转 png,§5.1),把
  addon-image 的载荷兼容面收窄到有把握的三种格式。
- sixel 留作扩展位(caps 字符串机制天然支持多值,见 §5.2)。

## 4. 总览:改动面

| 组件 | 文件 | 改动 |
| --- | --- | --- |
| content-build | `src/ansi.rs` + `src/main.rs` + `src/img.rs` | 渲染时记录占位框行号区间与几何,吐 sidecar manifest;处理后图片同时写 `.rendered-assets/`;webp 产物转 png |
| proto | `crates/servers/proto/src/lib.rs` | `Open` 加 `caps: Vec<String>`(向后兼容) |
| core | `crates/servers/core/src/client.rs` | `SessionClient::open` 加 caps 参数;输出 broadcast → 有界 mpsc |
| web 网关 | `crates/servers/web/src/main.rs` / `sessions.rs` | Open.caps 透传;输出扇出改每连接私有队列,慢连接断开 |
| ssh 网关 | `crates/servers/ssh/src/lib.rs` | 传空 caps(显式无图) |
| jaild | `crates/servers/jaild/src/{main,session,jail}.rs` | caps 白名单过滤 → 注入 `TERMBLOG_IMG=iterm2`;输出 broadcast → 有界 mpsc |
| jailbin | `crates/tools/jailbin/src/blog.rs` + 新 `src/reader.rs` + 新 `src/iip.rs` | 带图文章走 TUI 阅读器(条件进入、预检、ANSI→Span、sliced、自写 IIP) |
| 前端 | `frontend/package.json` / `src/main.ts` | `@xterm/addon-image` 0.9.0;Open 加 caps;`iipSizeLimit` |
| 模板脚本 | `deploy-scripts/build-template.sh` | +3 行:复制 `.rendered-assets/` 进 jail |
| PTY 链路 | jaild `session.rs` / core `client.rs` / web `sessions.rs` | 三层 broadcast → 有界 mpsc 背压(§5.8) |
| 测试 | 各 crate 单测 + `tests/verify-m5.sh` + 新 e2e(Node + Playwright) | §7 |

## 5. 详细规格

### 5.1 sidecar manifest 与处理后图片(content-build)

渲染 ANSI 时记录每个图片占位框在最终文本中的行号区间与几何,输出到
`jailtpl/content/.rendered/<slug>.images.json`(与 `.rendered/<slug>` 同层)。
**同时把处理后图片字节写进 `jailtpl/content/.rendered-assets/<rel>`**——
与 `frontend/dist/blog/<rel>` 同字节、同路径(main.rs 的 `asset_bytes`
循环写两份;目录与 dist/blog 一样每轮先清后写,僵尸资源天然清理)。

```json
{
  "version": 1,
  "images": [
    {
      "block_start": 34,
      "block_end": 38,
      "path": "hello/arch.png",
      "asset": "hello/arch.png",
      "w": 1080, "h": 607,
      "indent_cols": 0, "display_cols": 76,
      "alt": "架构图"
    }
  ]
}
```

- `block_start`:占位框首行(`┌─ 图片 …`)在 `.rendered/<slug>` 中的行号,
  **0-based,含**;`block_end`:占位框末行之后一行(**不含**)。区间必须精确
  覆盖整个占位框(含折行的 alt/URL 续行)——**用最终渲染行号锚定,这是
  相对 review 意见保留的设计**。实现:`render_ansi` 改为返回
  `(String, Vec<ImageAnchor>)`,渲染 Block::Image 时记录 `lines.len()`
  起止与 ind 几何。
- `path`:相对 `~/blog/` 的资源路径(v1 规范化结果,占位框降级与链接的
  URL 语义)。
- `asset`:相对 `.rendered-assets/` 的**处理后**文件路径——reader 的实际
  读取源。**`w`/`h` 必须从处理后字节解码得到**,保证与 reader 读到的像素
  一致(原计划 w/h 是处理后尺寸、reader 却读 `~/blog/` 原图,二者不同源,
  是错误)。
- `indent_cols`:占位框首行的缩进列数(Quote/List 内 > 0,顶层 = 0);
  `display_cols`:占位框内容宽度(= ansi.rs 中 `ind.width`)。TUI 按这两个值
  放置图像(§5.6.3),与 v1 视觉结构一致——解决引用块/列表内图片"带缩进且
  内容宽收缩"的布局问题。
- 文章无图 → 不写 manifest 文件(而不是写空数组),reader 据此快速判断。
- 行内图(§v1-5.5c)**不进 manifest**,TUI 里保持 `[图: alt]` 文本。
- **构建期格式保证**:img.rs 的 webp 产物(含"重编码变大回退原字节"分支)
  统一转 png,预算按转换后字节核算(webp 只出现在 lossless 场景,转 png
  字节级等价);处理后产物因此只含 png/jpeg/gif。

`build-template.sh` 增 3 行(§6):mkdir `~/.rendered-assets` + 整目录 cp。
**原计划的"部署零改动"不成立——这是模板脚本的最小改动,如实写明。**

### 5.2 proto:Open.caps(proto/src/lib.rs)

```rust
pub struct Open {
    …现有字段…
    /// 客户端能力通告(白名单字符串;空 = 无增强能力)。已知值:
    ///   "img-iterm2" —— 终端链路支持 iTerm2 Inline Images Protocol
    #[serde(default)]
    pub caps: Vec<String>,
}
```

- `#[serde(default)]` 保证旧客户端/旧网关 JSON 互通(协议向后兼容)。
- proto 层不校验内容(它只是管道);白名单过滤在 jaild(§5.5)。

### 5.3 core:SessionClient::open(core/src/client.rs)

签名加 `caps: Vec<String>`,填进 `proto::Open`(L48 的构造点)。其余不变。
(输出通道类型改动见 §5.8。)

### 5.4 网关透传

- **web**(`web/src/main.rs`):`handle()` 解析出 `open.caps` 后传给
  `SessionStore::create`(sessions.rs,签名加 caps),再传 `client.open`。
  注意 `attach` 路径不重新开会话,caps 只在 create 时生效——可接受
  (attach 回来的会话按创建时能力运行;镜像页永远 fresh,主路径无此问题)。
- **ssh**(`ssh/src/lib.rs` L162 附近):`client.open(self.peer, cols, rows, None, vec![])`
  ——显式空 caps。SSH 能力探测是明确的非目标。

### 5.5 jaild:caps → 环境变量

- `handle_conn`(jaild/src/main.rs)解析 `open.caps`,经**白名单过滤**
  (只认 `img-iterm2`,未知值 warn 日志后丢弃)后传入
  `SessionManager::create` → `JailBackend::spawn`。
- `jail.rs::spawn_inner`:现有 envp 是定长数组(L231-234),
  改为 fork 前构建 `Vec<CString>` + `Vec<*const c_char>`(fork 前分配是安全的,
  现有代码同此模式;子进程只用指针)。caps 含 `img-iterm2` 时设置
  `TERMBLOG_IMG=iterm2`;**否则完全不设置该变量**(绝不设空值——空环境变量
  仍"存在",会误导 §5.6 的严格相等判断)。
- **安全论证**(写进代码注释):caps 来自客户端、不可信,但它只影响该访客
  自己会话的一个提示性环境变量,最坏后果是自己的终端收到图像字节;无横向
  风险。白名单过滤是为了防止环境变量值被注入奇怪字符串。

### 5.6 jailbin:TUI 阅读器(新文件 `jailbin/src/reader.rs` + `src/iip.rs`)

`blog.rs` 中,满足**全部**条件时走 TUI,否则现状 less 路径一字不动:

```
slug 非空(文章在 ~/blog/ 下)
且 stdin 与 stdout 都是终端
且 env TERMBLOG_IMG == "iterm2"   (严格相等;未设置/空串/其他值一律走 less)
且 终端 cols ≥ 76                 (窄窗口语义见 §5.6.3)
且 ~/.rendered/<slug> 存在且非空
且 ~/.rendered/<slug>.images.json 存在、解析成功、version == 1、images 非空
且 §5.6.1 全部预检通过
```

#### 5.6.1 预检(任一失败 → less 路径;进入终端后的任何错误由 RAII 兜底)

不只"manifest 解析失败"才回退,以下每一项都必须在进入 alternate screen
之前检查:

- 区间合法:每个区间 `0 ≤ block_start < block_end ≤ rendered 总行数`;相邻
  区间递增且不重叠(`block_end ≤ 下一 block_start`);
- 尺寸合法:`w ≥ 1` 且 `h ≥ 1`;`indent_cols ≤ display_cols ≤ 76`;
- asset 安全:`asset` 字符集限 `[a-z0-9/._-]`(与 img.rs 的
  `validate_asset_path` 同规则),逐段拒绝 `..`/空段,规范化后不得逃逸
  `.rendered-assets/` 根;文件存在且非空。
- **RAII**:构造 `TermGuard`(进入 raw mode / alternate screen 之前);
  Drop 依次 disable raw mode → leave alternate screen → show cursor。
  正常错误返回与 panic(默认 unwind profile)都经 Drop 恢复;
  若 workspace profile 是 `panic="abort"`,reader 需覆盖为 unwind 或注册
  panic hook 执行同样三步。

#### 5.6.2 文本模型:ANSI → ratatui Text

- ratatui 的文本模型是 `Text → Line → Span + Style`,**不会自动解释原始
  ANSI 字符串**,需要解析层。
- content-build 的输出只含固定 SGR 子集:`1/22`(粗体)、`3/23`(斜体)、
  `2/22`(dim)、`36/39`(cyan)、`0`(复位),以及 OSC 8
  (`ESC]8;;<url>ST` … `ESC]8;;ST`)——调色板封闭,见 ansi.rs 的
  `render_line`。**自写最小解析器**(jailbin 内,<150 行):每行切成
  Span + Style;**OSC 8 序列剥除**(链接语义丢弃,保留其 SGR 样式:链接文本
  本就是 cyan)——TUI 无鼠标交互,链接点击是非目标,镜像页仍有完整链接。
- 不引 `ansi-to-tui` crate:它与锁定 ratatui 版本的兼容矩阵不透明,而本
  需求只是固定子集;单测用 content-build 真实输出逐行断言"可见文本 +
  span 样式"round-trip。
- 视口模型:1 个模型行 = 1 个预渲染行(文本行不换行);图像块占据 R 行
  (§5.6.3)。

#### 5.6.3 布局与视口(cell 尺寸、缩进、sliced、窄窗口)

- **cell 像素尺寸**:进入 TUI 时发 `CSI 16 t`,300ms 超时;响应是
  **`CSI 6 ; h ; w t`(h = 像素行高在前,w = 像素字宽在后)**(xterm
  ctlseqs 与 xterm.js typings 均如此)。解析成功则用,否则默认
  行高 ≈20px、字宽 ≈9px(Maple Mono 15px 的近似)。读响应期间 stdin 需
  raw mode(nix termios)。原计划的 `CSI 4;…t` 是 `CSI 14 t` 的响应,写错,
  照原计划实现会永远走默认 cell size。
- **图像几何**:起点 x = `indent_cols` 列;可用宽
  `W = min(display_cols, cols − indent_cols).max(1)` 列;显示宽 px =
  `W × cell_w`;图像块总行数 `R = ceil(显示宽px × h / w / cell_h)`。
- **sliced(部分可见)**:图片顶部滚出视口、或图片高于视口时,"显示哪一段"
  必须有定义。视口模型为每个图像块保存 `row_offset`(图片内第一个可见行,
  0-based);当块高度 R 超过视口内剩余可见行时只渲染视口覆盖的那段:
  解码 asset → 先缩放到终端显示像素尺寸 → 按 `row_offset × cell_h` 在显示
  像素空间裁剪(不能先映射回源像素:2×2 等小图会因量化重新膨胀并覆盖后文)
  → 按源格式重编码(png 源→png;gif 源
  →首帧 png;jpeg 源→jpeg q80)→ 发 IIP(§5.6.4)。滚动/resize 时更新
  `row_offset`,切片窗口变化才重发。**失败降级**:解码/裁剪/重编码失败或
  切片 payload > 1 MiB → 该块退回显示 v1 占位框文本(那段原始 ANSI 行),
  其余块不受影响。
- **窄窗口**:进入条件已要求 cols ≥ 76。TUI 中 resize 到 <76 列 →
  RAII 恢复 → 移交 `less -RXc +<N>`(`N = 当前视口首模型行 + 1`,1-based
  行号)继续阅读,阅读位置不丢——模型行号与 rendered 行号一一对应,移交
  语义明确。cols ≥ 76 时保持 76 列预排版不变(文本宽度固定,图像按 W 缩放
  ——与现状 less 行为一致),resize 重算图像几何并按需重发。
- 图像块替换整个占位框区间(含 alt/URL 续行);`alt` 在图下方以一行 dim
  文本显示(超出 W 截断,不折行)。

#### 5.6.4 IIP 编码与预算(自写 `iip.rs`,不引 ratatui-image)

- **完整可见图**:OSC 1337 序列
  `ESC]1337;File=name=<base64(asset)>;size=<处理后字节数>;inline=1;width=<Wpx>px;height=<Hpx>px;preserveAspectRatio=0:<base64(处理后字节)>BEL`。
  IIP 的 `name` 字段本身必须是 base64 编码的 UTF-8 文件名;
  `@xterm/addon-image` 0.9.0 要求非零 `size` 并以它初始化/限制解码器,
  缺失时会静默丢弃整张图片。
  直接携带 §5.1 的处理后字节,**不解码、不重编码**;addon-image 支持
  png/jpeg/gif 载荷并按 width/height 属性缩放。
- **预算**:线上字节 = 处理后字节 × 4/3。v1 构建期预算因此**直接成立**:
  单张位图 ≤256 KiB → payload ≤342 KiB;gif ≤512 KiB → ≤683 KiB;单篇
  ≤1.5 MiB → ≈2 MiB。这是不引 ratatui-image 的核心原因——其 iterm2 后端
  每次 render 把 DynamicImage 重编码成 PNG(JPEG 源成倍膨胀),原计划的
  "1.5 MiB × 1.37"推导不成立;其默认 chafa-dyn 特性还引入 jail 未安装的
  libchafa;其 Picker API 也已在计划撰写后漂移(现在是 from_fontsize +
  set_protocol_type)。三条理由,干脆自写编码器。
- **缓存**:完整图 payload 按 (asset, 显示宽) 缓存——同一尺寸只编码一次;
  整帧切片按 (asset, 显示宽, y0, y1) 做 LRU ≤ 3;解码后与缩放到显示尺寸的
  DynamicImage 各自 LRU ≤ 3。单行增量切片很小,另按
  (asset, 显示宽, row_offset) 做 LRU ≤ 256,来回滚动不重复编码。
- **平滑滚动/无闪烁换帧**:`j/k/↑/↓` 的一行滚动不再全清和重发整张可见图:
  用 `CSI 1 S` / `CSI 1 T` 移动现有终端行;addon-image 的图片属性附着在
  xterm cell 上,会随 BufferLine 一起移动。随后只补新露出的边缘行;若该行
  是图片,只裁剪/编码约一个 cell 高的条带并发 IIP,目标高度用 `height=1`
  cell(末行不足一格时保留实际 px),避免浮点 cell 高度取整成两行。初始帧、
  翻页、`g/G` 与 resize 才走“全清 + 文本 + IIP”的确定性整帧路径。两条
  路径都用 DEC mode 2026 synchronized output 包裹,不显示中间状态。
- **运行时硬上限**:任何单条 IIP payload > 1 MiB → 该块降级为占位框
  (§5.6.3)。与前端 `iipSizeLimit: 1 MiB`(§5.7)呼应,两侧一致。

#### 5.6.5 键位与 resize

- **键位**(对齐 less 直觉):`j/k/↑/↓` 一行;`Space/PgDn`、`b/PgUp` 一屏;
  `g/G` 首尾;`q` 退出。搜索不做(非目标)。
- **resize**:监听 SIGWINCH → `terminal.resize()` → 按 §5.6.3 重算几何;
  宽度不变时复用缓存 payload;<76 列移交 less(§5.6.3)。

#### 5.6.6 退出路径

- q / Esc / Ctrl-C → RAII 恢复 → **主屏重画最后视口**:清屏后打印退出前
  可视行对应的原始 ANSI 行(含占位框文本与 OSC 8)。这与 `less -X` 的
  "退出后留下退出时视口"一致。原计划写的是"cat 全文",但 cat 后屏幕
  最终只留下文章尾部、并非退出时视口——语义不符,废弃,如实按 less -X
  语义实现。
- 窄窗口 resize 移交:恢复后 `less -RXc +<N>`(§5.6.3)。
- 返回 blog.rs 主流程(OSC 复位照旧)。
- **测试钩子**:`blog --dump-image-frame <slug> <rows> <cols> <row>` 渲染
  单帧到 stdout 后退出(不进入交互),供 §7.3 的 Node e2e 断言 IIP 字节。

#### 5.6.7 依赖(精确锁版本,提交 Cargo.lock)

```toml
ratatui = { version = "=0.30.2", default-features = false, features = ["crossterm_0_29"] }
crossterm = "=0.29.0"
image = { version = "=0.25.10", default-features = false, features = ["png", "jpeg", "gif"] }
base64 = "0.22"
```

- **不引 ratatui-image**(§5.6.4 三条理由);chafa 依赖问题随之消失。
- 版本表是开工第 1 步"完整帧 spike"(§8)的锁定基线;若 spike 暴露不兼容,
  改版本必须同步更新本表 + Cargo.lock + 本节。锁定后不再漂移。
- ⚠️ 编译目标是 FreeBSD jail 模板,部署机上验证
  `cargo build --release -p termblog-jailbin` 通过;二进制会变大(MB 级),
  可接受(模板只读,不计访客 4M 配额)。

### 5.7 前端(main.ts)

- 仓库现状是 `@xterm/xterm ^6.0.0`(frontend/package.json L12,**不是计划
  原文的 5.5**);收紧为精确 `"6.0.0"`,配套 addon:
  `npm i @xterm/addon-image@0.9.0`(xterm.js 6.0.0 对应的官方版本)。

```ts
import { ImageAddon } from "@xterm/addon-image";
term.loadAddon(new ImageAddon({ sixelSupport: false, iipSizeLimit: 1 * 1024 * 1024 }));
```

- 选项名是 **`iipSizeLimit`**(不是 `sizeLimit`),单位字节;1 MiB 与
  §5.6.4 运行时上限呼应(> 单图 683 KiB 上限,留余量;同时小于 parser 保护)。
- Open 帧加能力:`jsonFrame(T_OPEN, { cols, rows, attach_token, fresh, caps: ["img-iterm2"] })`。
- xterm.js 6.0.0 默认 canvas renderer,IIP 可用;**若未来启用 WebglAddon,
  必须重新验证图像渲染**(列为部署 checklist 项)。
- CRT 滤镜(`#term-screen` 的 url(#crt-smudge))会同样作用于图像像素——
  视觉上一致,是有意为之;若实测图像糊得不能接受,再议(不属于本期)。

### 5.8 PTY 链路背压:字节零丢失

图像字节路径:jaild PTY 读缓冲 8192B → jaild 会话输出 → core 客户端输出 →
web 会话扇出 → WS。现状三层 `broadcast` 的容量单位是**消息条数**,不是
字节;PTY read 也不保证每块都是 8 KiB;任何一处 `Lagged` 都直接丢字节。
终端流是有状态协议,丢一块就破坏后续 ANSI/IIP 且不可自愈——**"扩容广播 +
下次重绘自愈"不是可靠修复,废弃**。改为全链路有界 mpsc + 背压:

- **jaild**(`session.rs` pump L105 起):`broadcast::Sender<Bytes>` →
  `tokio::sync::mpsc::Sender<Bytes>`,容量 128(条,≈1 MiB)。
  `out.send(chunk).await`:队列满即**停读 PTY** → 内核 PTY 缓冲填满 →
  子进程写阻塞 = 端到端背压,字节零丢失。SessionManager 的观察端相应改为
  单接收者(core 连接即单消费者,天然匹配 mpsc)。
- **core**(`client.rs` pump L61):`broadcast(256)` → 有界 mpsc(128);
  pump 读 socket 帧 → `send().await` 背压;`SessionHandle.output` 类型改
  `mpsc::Receiver`。jaild→core→web 是单消费者链路,mpsc 语义正合适。
- **web**(`sessions.rs`):每会话一个 forward task 是 core output 的唯一
  消费者,保留现有"订阅与回放同临界区"的设计:消费 → scrollback 环
  (128 KiB 上限不变)→ `try_send` 到各连接的私有 mpsc(容量 128;attach
  回放 32 条 4 KiB 块 ≤ 容量,回放不丢)。WS 写循环 `recv → send().await`
  天然背压;**某连接队列满(try_send 失败)→ 判定慢消费者 → 关闭该 WS
  连接**(close frame 注明 slow consumer),**绝不静默丢帧**。
  会话只有单个访客时背压直达浏览器;多个 attach 观察者时慢的断开、不拖累
  他人。
- **内存注释**(写进代码):每会话队列 1 MiB × `session.max_total` 是理论上限,
  实际远小于(队列只在突发时填满;背压后突发即摊平)。容量同时保证**单条
  完整 IIP 序列(≤683 KiB + 头)不被队列容量截断**——这比原计划 1024 条
  broadcast 更可靠,因为背压不再依赖"整块恰好 ≤8 KiB"的假设。
- 协议转换红线不破:网关依旧不解码、不重组终端字节流,只负责不丢字节。

### 5.9 OSC 7777 时序不变

`blog` 仍先发 OSC(进文章)、退出后发 OSC(复位 `/`);TUI 只是替换了
中间的分页器环节。镜像页接管信号(首条 OSC)→ 200ms 后淡出的时序
(main.ts)不变——TUI 首帧绘制略慢于 less(首次图像解码+编码),实测若露出
半画状态,把 `setTimeout(takeOver, 200)` 调大到 400ms 一档,写注释说明
原因。

## 6. 部署

- `build-template.sh`:**+3 行**——mkdir `~/.rendered-assets` + 整目录
  `cp -R`(与 §5.1 配套)。其余逻辑不变。
- **上线必须重建模板且带 `--replace`**:新 jailbin、`.rendered-assets/`、
  manifest 只存在于重建后的模板数据集里;`make deploy` **不会替换已有
  jail 模板**,新 reader 不会自动上线。部署 checklist 明确包含:
  ```
  make build
  sudo sh deploy-scripts/deploy.sh
  sudo sh deploy-scripts/build-template.sh --replace
  ```
  后续只改文章仍走 `make content`(内部已含 `--replace`,且会把新 jailbin
  一起装进新模板)。
- `deploy.sh` / rc/配置:零改动。

## 7. 测试与验收

### 7.1 cargo 单测

- content-build:带图文章的 manifest 行号区间精确(渲染结果按行 split,
  `block_start` 行以 `┌` 开头、`block_end - 1` 行以 `└` 开头);无图文章
  不产 manifest;多图区间不重叠且递增;**asset 字节与 dist/blog 对应文件
  一致,且 w/h 与 asset 解码尺寸一致**;Quote/List 内图片的
  indent_cols/display_cols 正确;`.rendered-assets` 每轮先清后写、僵尸清理;
  webp 产物转 png。
- proto:旧 JSON(无 caps 字段)能反序列化;caps 序列化往返。
- jaild:caps 白名单过滤函数(混入未知值被丢弃);envp 构建含
  `TERMBLOG_IMG=iterm2`,无 caps 时**不设置**该变量。
- jailbin:
  - 进入条件:`TERMBLOG_IMG` 严格相等判定(未设置/空串/`foo` 都不进 TUI);
  - 预检各失败分支(坏 JSON/缺字段/version 不匹配/区间越界或重叠/尺寸为
    零/asset 路径逃逸或缺失)→ 全部回落 less,不 panic;
  - ANSI→Span 解析器:用 content-build 真实输出断言 span 样式与可见文本,
    OSC 8 剥除且样式保留;
  - 图像几何:sliced 的 `row_offset`/裁剪像素映射/行高计算(给定
    w/h/cell/视口);
  - IIP 编码器:golden bytes(与手拼序列逐字节比对),payload =
    base64(处理后字节);单行增量方向/边缘模型行映射、`height=1` cell、
    末尾不足一行的像素高度与单行 payload 缓存。
- 背压链路(jaild/core/web):有界队列满时 pump 停读而非丢字节(pipe 直连
  测试断言收发字节流逐字节相等);web 侧队列满 → 连接被关闭而非丢帧;
  回放 + 实时输出不重不漏(保留现有 attach 设计断言)。

### 7.2 集成验收(部署机上,人工 + 脚本)

1. **回落路径**:`ssh blog@host` → `blog image-test` → 必须是 v1 占位框
   (ssh 无 caps);`TERMBLOG_IMG= blog image-test`(空值)与
   `TERMBLOG_IMG=foo blog image-test` 同样回落(严格相等)。
2. **web 路径**:浏览器开 `/blog/image-test/`,接管后终端内图片像素可见、
   位置正确(占位框文本不再显示;引用块/列表内图片按 indent/display 缩进);
   j/k 单行滚动图像跟着走且不闪烁、不逐帧顿挫;Space 滚屏正确;高图滚动到
   "半张图"时显示正确切片;
   q 退出后主屏留下**退出时视口**(含占位框文本,同 less -X)。
3. **resize**:显示图片时拖窗口(≥76)→ 图像不重影、不错位;缩到 <76 →
   移交 less 且从当前视口首行继续。
4. **刷新恢复**:镜像页刷新 → 新会话重开,图片重新加载,无 base64 乱码。
5. `sh tests/verify-m5.sh` 全绿(v1 的占位框断言对 ssh 路径仍成立)。
6. 内存抽查:并发 3 个 web 会话读大图文章,jaild/web 进程 RSS 无异常爬升。
7. **背压抽查**:对单个会话网络限速(如 ipfw/shape)读大图文章 → 输出无
   截断、无乱码;拔慢/假死连接的 WS → 连接被关闭,日志可查 slow consumer。

### 7.3 自动化 e2e

- **纯 Node 测试**(`tests/e2e-image.mjs`,无浏览器,CI 默认跑):
  addon-image 依赖浏览器的 Terminal/renderer/Canvas,`@xterm/headless`
  没有像素渲染,0.9.0 typings 也没有 `onImage` 回调——所以 Node 测试只
  负责 manifest、布局与 IIP 字节格式:
  跑 content-build(测试 fixture)→ 断言 manifest 行号锚定 / asset 与
  `.rendered-assets` 字节一致 / indent·display 几何;
  调 `blog --dump-image-frame` 录制一帧 → 提取 OSC 1337 序列 → 断言头部
  字段合法、base64 解码 == 对应处理后字节、width/height 属性与 §5.6.3
  几何计算一致、切片帧的裁剪窗口正确。
- **Playwright 像素测试**(`tests/e2e-image-playwright.mjs`,最小化:
  playwright + chromium headless,env `TERMBLOG_PW=1` 门控,人工/CI 按需):
  开 `/blog/image-test/` → 等镜像页接管 → 对 `#term-screen` canvas 截图
  → 采样断言存在非背景像素(验证真实像素渲染);可选断言图片区域尺寸。
  这是唯一真正验证像素渲染的路径。

## 8. 风险与回退

- **回退开关天然存在**:前端不传 caps(摘掉 ImageAddon)→ jail 无
  TERMBLOG_IMG → `blog` 走 less 老路径。v1 体验完整保留,回退是配置级操作。
- **相对 review 意见保留的设计**:caps 的 `#[serde(default)]` 向后兼容
  (§5.2)、SSH 显式传空能力(§5.4)、sidecar 用最终渲染行号锚定(§5.1)。
- **开工顺序**(按依赖关系重排,替代原"先手写一条 IIP 再动工"):

  1. **锁版本 + "完整 Ratatui 帧" spike**(不只是手写一条 IIP):按 §5.6.7/
     §5.7 锁 ratatui 0.30.2 / crossterm 0.29 / image 0.25.10 / xterm
     6.0.0 / addon-image 0.9.0;jail 外起最小 ratatui 应用输出**完整帧**
     (SGR 文本 + 真实 IIP 序列)到文件,喂给 xterm 6.0.0 + addon-image
     0.9.0(Playwright)验证:像素渲染、width/height 占位、JPEG/GIF 载荷、
     `iipSizeLimit`、CSI 16t 响应解析。通过后提交 Cargo.lock /
     package-lock,规格与实现锁定同一版本。
  2. **处理后图片进 jail + 真实预算**:§5.1 的 `.rendered-assets` +
     build-template.sh 改动;按"处理字节 × 4/3"复核 §5.6.4 预算与 §5.8
     队列容量(单条 IIP ≤683 KiB < 1 MiB)。
  3. **三套布局语义落地**:ANSI→Span(§5.6.2)、sliced 偏移(§5.6.3)、
     窄窗口移交(§5.6.3),各自配单测。
  4. **PTY 链路背压改造**(§5.8,不丢字节)。
  5. 正式实现 reader(预检 + RAII + TUI)+ 前端收尾 + §7 测试。

- **剩余不确定点**:
  - addon-image 对 JPEG/GIF 直传载荷与 width/height 属性的解析——第 1 步
    spike 验证,失败则构建期统一转 png(预算重算);
  - 切片重编码(JPEG crop→JPEG)的画质/体积:q80 起步,spike 校准;
  - 移交 less 的 `+N` 行号与 TUI 模型行号的一致性(单测覆盖)。

## 9. 未来可选项(不在本期,仅记录)

- SSH 侧能力探测:DA1 响应含 `;4;` → sixel;`TERM_PROGRAM` 经 AcceptEnv 透传
  → iTerm2。探测成功后注入对应 caps。
- sixel 协议输出(caps 值 `img-sixel`;届时 IIP 编码器换成对应协议后端)。
- TUI 接管无图文章(统一阅读体验)与 `/` 搜索。
- TUI 内 OSC 8 链接点击(鼠标交互 + 打开 URL)。
- 前端自定义 linkHandler:占位框链接 → 站内 lightbox(与像素图并存的看图
  路径,SSH 用户也能用 URL 手动打开)。
