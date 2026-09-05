# plan_comment —— 评论系统第一版：设备写入即评论

> 性质：M5 后的第一个大功能。访客在 jail 里向预建 FIFO 写一行文本即提交评论，
> 经站长审核后显示在终端与网页。本文是经过安全与实现复核后的设计定案和实施计划。
> 第一版优先保证目标归属、权限隔离、持久化和展示闭环正确，再控制实现规模。

## 1. 范围

**做**：

1. `commentd` 新服务：单写者、独立 root-owned 存储、审核状态机、分页查询、
   public/private 两个 Unix socket。
2. 评论设备：模板只为含直属文章的目录预建一个共享 FIFO，另建首页说明的全局 FIFO。
3. jaild 评论泵：guest shell fork 前安全打开全部 FIFO，将每个 fd 固定绑定 target，
   限流、提交 commentd，并通过现有输出队列异步回显 ack。
4. 会话评论快照：jaild 在会话交付前同步一次并写入 jail 内 root-owned 目录；
   会话期间保持固定，重新连接才取得新快照。
5. jailbin `blog`：每篇文章按其直属目录渲染共享评论；裸 `blog` 只列文章。
6. Web：`GET /api/comments` 只读转译；独立 comments bundle；文章镜像页末尾按直属目录展示。
7. `commentctl`：`queue` / `approve` / `reject` 三个命令。
8. 配置节 `[comments]`、rc.d、部署脚本与端到端验证。

**不做**（第一版，部分留位）：

- 嵌套回复（平铺；想回复就写 `回复 #12: ...`）。
- 审核之外的自动策略（防滥用只靠默认 pending、限流和去重）。
- HTML/Web 投稿入口、Anubis 联动；Web 始终只读。
- 把评论内容烘焙进静态 HTML；`content-build` 只生成评论区结构与 target，
  approved 内容仍从 API 实时读取，无需重建文章。
- 推送通知、评论计数进入 `.index`/Atom、设备运行期自愈。
- 独立的访客 `comments` 命令，以及终端框下方的额外评论区域。

## 2. 决策记录

| # | 决策 |
| --- | --- |
| D1 | 特殊文件是 **FIFO (`mkfifo`)**，不是字符设备。 |
| D2 | 全局投稿设备是 `~/comment`，不是 `/dev/comment`。 |
| D3 | 文章设备按**直属目录**创建；同目录 Markdown 共享，目录没有直属文章就不创建。 |
| D4 | jaild 在 shell 启动前将每个 FIFO 固定为 `(fd, target)`；运行期不遍历、不重定位，移动 FIFO 不改变归属。 |
| D5 | 评论 target 对应文章直属目录：顶层文章 `/blog/`、嵌套目录 `/blog/<dir>/`、首页说明 `/`；文章列表不展示评论。 |
| D6 | 正文只走 FIFO；ack 只进入现有 `out` 输出队列，绝不写 PTY master。 |
| D7 | commentd 是单写者；所有变更使用 copy-on-write 持久化，落盘成功后才替换内存状态。 |
| D8 | 审核状态为 `pending / approved / deleted`；默认 pending，reject 是保留 ID 的软删除。 |
| D9 | 行内前缀 `名字: 内容` 只由 commentd 解析；IP 只保存带盐哈希。 |
| D10 | 协议复用 `termblog-proto` 帧编解码和 `core::Link`；两个 socket 各有独立 kind 命名空间。 |
| D11 | 每条连接只处理一个请求：收一帧、回一帧、断开；分页由多次短连接完成。 |
| D12 | public socket 只允许按 target 查询 approved；submit、同步和审核只走 private socket。 |
| D13 | 会话快照位于 jail 内 `/var/run/termblog/comments.jsonl`，其父目录和文件均由 root 管理。 |
| D14 | Web 文章评论由 `content-build` 放在文章末尾；终端接管后通过“文章/终端”视图切换访问，不在终端下方另铺评论。 |

## 3. Target 与 FIFO 布局

文章 URL 仍按完整 slug 生成，但评论 target 只取 Markdown 的直属目录。合法目录字符沿用
slug 的 `[a-z0-9/-]`；同目录多篇文章共享评论，评论不递归继承给子目录。

| 内容 | 投稿路径 | target |
| --- | --- | --- |
| `~/blog/hello.md` | `~/blog/comment` | `/blog/` |
| `~/blog/other.md` | `~/blog/comment` | `/blog/` |
| `~/blog/a/b.md` | `~/blog/a/comment` | `/blog/a/` |
| `~/blog/a/c.md` | `~/blog/a/comment` | `/blog/a/` |
| `~/help.md` | `~/comment` | `/` |

`content-build` 从每个 `Article.slug` 取父目录、去重后产出可信 target 清单，模板中以
root-owned、只读文件保存，例如 `/usr/local/share/termblog/comment-targets.tsv`：

```text
comment                  /
blog/comment             /blog/
blog/a/comment           /blog/a/
```

`build-template.sh` 只按清单创建 FIFO。目录只有图片、录像等资源而没有直属 Markdown 时
不创建；同目录有多篇文章也只创建一个。清单、静态镜像和终端都使用同一父目录映射，
避免归属漂移。清单解析拒绝绝对路径、空组件、`.`/`..` 和不合法 target。

删除“移动后按当前位置重新归属”：guest 移动已打开 FIFO 时，该 inode 仍绑定启动时的
target；删除或替换路径后，jaild 也不会以 root 重新打开它。

## 4. 数据模型、输入规范与存储

数据目录为独立的 `/var/db/termblog-commentd`（可配置），部署权限
`0700 root:wheel`。`comments.jsonl`、`salt` 与 `initialized` 均为
`0600 root:wheel`，不放进当前由 www 拥有的 `/var/db/termblog`。
commentd 启动时用 `lstat` 校验数据目录不是 symlink、owner 是 root，且 group/other
没有写权限；不满足即拒绝启动，不能只依赖部署脚本曾经设置正确。
数据目录另含 `initialized` 标记。commentd 正常启动从不隐式创建空库：首次安装只能显式
运行 `commentd --init`，且仅允许对新建空目录操作；它创建 salt、空主文件和最后写入的
initialized 标记，每一步均 fsync 文件与目录。之后任一文件缺失都视为损坏并拒绝启动，
避免主文件被删后误判成首次安装、从 ID 1 重新开始。

```json
{"id":42,"target":"/blog/hello/","author":"alice","text":"好文","ip_hash":"sha256…","created_at":"2026-09-01T12:34:56Z","status":"pending"}
```

- `id`：commentd 分配、单调递增；三种状态都占住 ID，永不重号。
- `target`：`/`、`/blog/` 或合法的 `/blog/<slug>/`，最长 200 UTF-8 字节。
- `author`：commentd 从 `line` 的第一个 `名字: 内容` 前缀解析；无合法前缀为
  `guest`；清洗后最长 32 UTF-8 字节。
- `text`：清洗后的单行纯文本，最长 512 UTF-8 字节。
- `ip_hash`：`sha256(salt + ip)`；永不保存明文 IP。
- `created_at`：commentd 的 UTC 时钟，不接受调用方时间。
- `status`：`pending`、`approved`、`deleted`；reject 为软删除。

输入规则依次为：

1. FIFO 以 `\n` 分行，换行不属于 `line`。整行超过 512 字节即拒绝并丢弃到下一换行，
   不截断后提交。
2. 字节必须是合法 UTF-8；jaild 在构造 JSON 前严格解码并 ack 非法输入，不使用
   lossy conversion。commentd 对 private socket 调用再次检查长度。
3. commentd 删除 C0 `U+0000..U+001F`、DEL `U+007F`、C1
   `U+0080..U+009F`，再解析名字前缀。
4. 清洗后正文为空、author/text 超限或 target 非法都拒绝；按 UTF-8 字节完整判断，
   绝不在多字节字符中间截断。

### 4.1 Copy-on-write 提交

`Store` 维护不可直接对外修改的状态与 opaque `revision`。revision 是完整、规范化
JSONL 字节的 SHA-256；相同状态值相同，任一内容或审核状态变化都会改变，daemon 重启
也不会撞回旧版本。submit、approve、reject 共用以下提交序列：

1. 从当前状态构造候选新状态，在候选中分配 ID、修改索引并序列化规范 JSONL。
2. 由序列化字节计算候选 revision；在同一 root-owned 目录以
   `O_CREAT | O_EXCL | O_NOFOLLOW` 创建唯一临时文件，权限 `0600`，写入这些字节。
3. `write_all` 后 `fsync` 临时文件。
4. `rename` 覆盖 `comments.jsonl`。
5. 打开并 `fsync` 数据目录。
6. 全部成功后才替换内存状态并返回成功/ID。

rename 前失败时删除临时文件，旧内存与旧主文件不变。rename 已发生但目录 fsync 失败时，
磁盘持久性和内存状态无法再安全对应：commentd fail-stop，所有请求返回故障并退出，
由人工/服务管理器检查后重启；不能继续暴露旧内存。ack 只在完整提交成功后生成。

启动时完整校验 JSON、字段、ID 唯一且严格递增、状态、target 和时间。任何损坏都保留
原文件原位、报告具体行号并退出非零；不绑定 socket、不改名原文件、不空库继续。
已有数据但 salt 缺失或非法时也拒绝启动，避免静默换盐。

## 5. commentd、权限边界与协议

```text
/var/run/commentd-public.sock   root:www   0660
  └─ 只接受 approved query，target 必填

/var/run/commentd-private.sock  root:wheel 0600
  └─ 接受 submit、approved 全量同步、pending queue、approve、reject
```

commentd 同时监听两个 `SOCK_SEQPACKET` socket。分离 listener 是权限边界，不依赖请求
里的身份字段，也不直接给当前 Link 加 `getpeereid`：FreeBSD
[`getpeereid(3)`](https://man.freebsd.org/cgi/man.cgi?query=getpeereid) 规定的是
`SOCK_STREAM` 接口。即使 www 被攻破，它也只能读指定 target 的 approved 评论。

### 5.1 Public socket

| kind | 请求 | 成功响应 |
| --- | --- | --- |
| 1 approved query | `{target, after_id?, limit?, revision?}` | `{ok, revision, total, omitted_earlier, comments, next_after_id?, has_more}` |

- `target` 必填；不能查全部 target，也不接受 status 参数。
- `limit` 默认 100，范围 `1..=100`。
- 无 `after_id`：取该 target 最新 `limit` 条，再按 ID/时间正序返回；
  `omitted_earlier` 是未返回的更早评论数。
- `after_id=0` 且无 revision 时，从当前 revision 的最早记录开始正向分页；
  `after_id>0` 时必须带同一轮首响应的 revision，只返回 `id > after_id`。
- revision 变化时返回 `stale_revision`，调用方丢弃整轮并重试。after_id 只用于固定
  revision 内的分页，不作为跨审核变更的实时游标；刷新最新列表应重新发无 after_id 请求。

### 5.2 Private socket

| kind | 请求 | 成功响应 |
| --- | --- | --- |
| 1 submit | `{target, line, ip}` | `{ok, id?, notice}` |
| 2 sync approved | `{after_id, limit, revision?}` | `{ok, revision, comments, next_after_id?, has_more}` |
| 3 pending queue | `{after_id, limit, revision?}` | `{ok, revision, comments, next_after_id?, has_more}` |
| 4 approve | `{ids:[…]}` | `{ok, changed}` |
| 5 reject | `{ids:[…]}` | `{ok, changed}` |

submit 不接受预解析的 `author`/`text`；commentd 是前缀解析与规范化的单一事实源。
处理链为：校验 → 清洗/解析 → 空值和长度拒绝 → 同 `ip_hash` 每小时 10 条 →
同 `ip_hash + target + text` 300 秒判重 → COW 写入 pending → 返回 notice。

两种私有查询的 `limit` 也为 `1..=100`。第一页用 `after_id=0` 且不传 revision，
后续页固定携带首响应 revision。翻页期间发生任何存储变更，下一页返回
`stale_revision`，调用方丢弃本轮并重试。这样数千条评论不会撞上现有 1 MiB 帧上限，
也不会拼出跨版本快照。

notice 由 commentd 生成，例如 `评论已投入待审队列`；不得向访客暴露数据库全局 ID。
它是持久化完成后的异步通知，不是 `echo` 的同步返回值。

## 6. jaild：安全打开、评论泵与快照

### 6.1 会话启动顺序

评论初始化是会话交付前的 barrier：

1. 创建/克隆 jail 文件系统，尚不 fork guest shell。
2. 从 root-owned 清单取得有限相对路径；以 jail 根为锚逐级 `openat`/校验父目录，
   不跟随符号链接。
3. 每个 FIFO 以 `O_RDWR | O_NONBLOCK | O_NOFOLLOW | O_CLOEXEC` 打开，再
   `fstat` 确认 `S_IFIFO`，保存固定 `(fd, target)`。缺失或类型错误使会话创建失败；
   运行期绝不重开。
4. 经 private socket 分页取得同一 revision 的全部 approved 评论，写入 jail 内
   root-owned 快照。
5. 首次快照完整落盘后才 fork shell、启动 pump 并发送 `Opened`。

`O_RDWR` 是主方案：jaild 自持 writer，避免最后一个 guest writer 断开后 kqueue
持续报告 FIFO `EV_EOF`。该行为见 FreeBSD
[`kqueue(2)`](https://man.freebsd.org/cgi/man.cgi?apropos=0&format=html&query=kqueue&sektion=0)。
即使意外读到 `0`，实现也须 clear/re-arm readiness，不能空转。

shell 启动后，guest 可以删除、移动或替换路径，但 jaild 只读启动前的 fd：不再以宿主
root 遍历 guest 树，不访问 guest 后放置的 symlink，不按 inode 当前位置重算 target。
`O_CLOEXEC` 保证 fd 不泄漏给 shell。

### 6.2 分行、提交与 ack

每个 fd 的读 task 按固定 target 累积字节。遇 `\n` 成行；超限后进入
discard-until-newline 且只发一次错误 ack。合法 UTF-8 行经有界 mpsc 送到每会话
comment worker；jaild 从可信会话元数据补入 IP，再向 private socket submit。

```text
FIFO reader → comment worker → ack mpsc → session pump → out.send(notice_bytes) → Web/SSH
```

pump 的 `tokio::select!` 新增 ack 分支，并继续独占 `out`。任何 notice 都不得调用
`write_all(master, ...)`：写 PTY master 等价于模拟键盘输入，可能进入编辑缓冲甚至执行。

`echo ... > comment` 成功只代表内核收到了字节，不代表 commentd 已持久化。成功/失败
ack 在提交完成后异步到达，可能晚于下一条 prompt。pump 把 ack 放入输出队列后向 shell
发送 `SIGURG`；模板 `.zshrc` 的 trap 用 `zle -I` 使 zsh 重画 prompt 及尚未提交的编辑行。
`SIGURG` 的默认动作是忽略，因此 shell 已退出或被 `exec` 替换时通知也不会误杀进程；trap
无论 ZLE 是否活跃都返回成功。这个通知只触发 shell 自己重画，仍不向 PTY master 写入字节。

会话总限额为 8 条。PTY EOF 或接入层关闭后，停止接收新投稿，对各 FIFO 做最后一次
非阻塞 drain，关闭 ingress，再给已读入队列的投稿最多 1 秒完成 submit/ack，之后才
取消 task 和清理 jail。这样 `echo '...'>...; exit` 不会随 session task 立即丢失，
也不会无限等待 commentd。
接入层已经断开时仍要完成这段持久化 drain，只忽略无法投递的 ack；不能把 ack 发送失败
反向解释为 submit 失败或取消已经开始的 commentd 请求。

### 6.3 root-owned 会话快照

删除 `~/.comments` 方案。快照为：

```text
/var/run/termblog/                 0755 root:wheel
/var/run/termblog/comments.jsonl  0644 root:wheel
```

这是 jail 内路径，由宿主 jaild 经 jail 根写入。每行包含 ID：

```json
{"id":42,"target":"/blog/hello/","author":"alice","date10":"2026-09-01","text":"好文"}
```

首次同步阻塞会话交付，并通过 private socket 做 revision 固定的全量分页；revision 变化时
丢弃临时结果并重试。快照在 root-owned 目录内写临时文件、fsync、rename，guest 无法
替换临时文件或诱导 root 跟随 symlink。首次同步失败时不交付 shell。会话启动后快照固定，
审核通过的新评论要在重新连接、创建新会话后才能看到。

## 7. jailbin：终端展示

`jailbin blog` 只读 `/var/run/termblog/comments.jsonl`：

- `blog hello` 的文章位于 `~/blog/`，查询该目录 target `/blog/`。
- `blog a/b` 的文章位于 `~/blog/a/`，查询 `/blog/a/`；同目录文章看到同一组评论。
- 裸 `blog` 只显示文章列表，不附加评论。
- blog 树之外的任意 Markdown 文件不附加评论，只有 `~/help.md` 查询全局 `/` 留言。

```text
── 评论 (152) ───────────────────────────
… 还有 52 条更早评论

#53  alice · 2026-09-01
    好文，学习了

#54  guest · 2026-09-01
    沙发
```

- 取最新 100 条，再在内部按时间/ID 正序展示。
- `N` 是 approved 总数；省略的是更早评论，因此提示放在列表顶部。
- `#序号` 在当前目录 target 内连续显示；数据库全局 ID 只用于审核和分页。序号/日期 dim、author 加粗、正文缩进 4 格，条目间空行。
- 终端只显示日期；第一版交给终端自动折行。
- jailbin 在输出边界再次过滤 C0、DEL、C1。JSON/字段校验失败时整段显示“评论暂不可用”，
  不跳过坏行后展示不完整结果。

空态必须显示完整路径，因为 `blog hello` 不改变父 shell 的 cwd：

```text
(暂无评论 —— echo 'alice: 好文' > ~/blog/comment 写第一条)
(暂无评论 —— echo 'alice: 好文' > ~/blog/a/comment 写第一条)
```

首页全局留言板的空态使用 `echo 'alice: 你好' > ~/comment`。

## 8. Web、前端与 content-build

### 8.1 只读 API

```text
GET /api/comments?target=/blog/hello/&limit=100
GET /api/comments?target=/blog/hello/&after_id=42&limit=100&revision=<opaque>
```

路由位于 `ServeDir` fallback 前，只连接 public socket。target 必填，after_id/limit
按 public 协议校验；返回 `{revision,total,omitted_earlier,comments,has_more}`。
commentd 不可用返回 503 JSON。没有 POST，也不代理 private kind。

### 8.2 独立 comments bundle

新增独立 `comments.ts` 入口，只依赖 `data-comments-target`，不假定 `#term-screen`
存在，也不导入/启动终端。它负责 fetch、loading/empty/error 和 DOM 渲染；author/text
一律用 `textContent`，时间用带完整 RFC3339 `datetime` 的 `<time>`，显示到分钟。
`content-build` 只让文章镜像页显式加载 comments bundle；文章列表与首页不加载。

`main.ts` 仍只加载于终端页。文章镜像页把现有单向 `enter-terminal` 扩展为双向
“阅读文章 / 进入终端”切换：文章态显示完整 `#static-view`，终端态隐藏它。
切换控件位于 `#static-view` 和 terminal host 之外，在两个视图中始终可达。
comments bundle 不依赖切换即可加载；终端视图外不追加常驻评论 DOM。

### 8.3 展示面

**文章镜像页 `/blog/<slug>/`**：`content-build` 在每篇 `<article>` 最后生成评论区，
`data-comments-target` 取文章直属目录。静态/降级浏览时评论位于文章最末尾；终端接管后
通过“阅读文章”切回完整文章再看。文章列表 `/blog/` 与首页 `/` 都不生成 Web 评论区。

```html
<div id="static-view">
  <article>
    <!-- title、正文、post meta -->
    <section class="comments" data-comments-target="/blog/">
      <h2>评论</h2>
      <p class="comments-status">正在加载…</p>
      <ol class="comment-list"></ol>
    </section>
  </article>
</div>
```

网页同样显示最新 100 条、内部正序，并在顶部提示“还有 N 条更早评论”。空态给出对应
完整终端命令；错误态为“评论暂不可用”。pending/deleted 不进入任何展示面。

## 9. commentctl

`commentctl` 与 `commentd` 位于同一 crate，作为多调用二进制符号链接，只连接
private socket：

```text
commentctl queue [--after-id N] [--limit N]
commentctl approve <id…>   # 或 --all
commentctl reject <id…>
```

`queue` 默认自动分页列出全部 pending（id、target、author、时间、正文首行）；分页期间
revision 变化则从头重试。所有命令只以 root/sudo 运行。不做面板、编辑器或审核规则。

M1 使用 Rust 集成测试客户端或 `commentctl` 验证协议；`nc -U` 不能构造带 5 字节
二进制帧头的 `SOCK_SEQPACKET` 请求，不作为验收手段。

## 10. 配置与部署

```toml
[comments]
public_socket      = "/var/run/commentd-public.sock"
private_socket     = "/var/run/commentd-private.sock"
data_dir           = "/var/db/termblog-commentd"
session_drain_ms   = 1000
```

- `etc/rc.d/commentd` 风格与现有 rc 脚本一致；commentd 以 root 运行并创建两个不同权限
  的 listener。
- `deploy.sh` 安装 commentd、commentctl、rc.d。仅当数据目录是本次新建且为空时运行
  `commentd --init`；已有目录缺少任何初始化文件时立即中止，不自动补 salt 或空主文件。
- 当前 `/var/db/termblog` 的 www 所有权不变，但评论数据不放在其中。
- `build-template.sh` 安装 target 清单、按清单建 FIFO，并预建 jail 内
  `/var/run/termblog` 为 `0755 root:wheel`。FIFO 建为 `0600 guest:guest`；快照目录
  不可由 guest 修改。
- FIFO 不进仓库内容树。若 FreeBSD/ZFS spike 证明模板 FIFO 不能安全克隆，则按同一
  可信清单在 shell 启动前创建并立即打开；仍不在运行期遍历。
- `Makefile`、日志轮转和 `tests/verify-comments.sh` 相应补齐。

## 11. 失败域与安全语义

| 场景 | 行为 |
| --- | --- |
| commentd 在新会话初始化时不可用 | 首次同步失败，不交付 shell；接入层显示暂时不可用。 |
| commentd 在已有会话期间宕机 | 投稿得到异步失败 ack；固定初始快照仍可读；API 返回 503。 |
| public socket 收到 submit/审核 kind | 协议层拒绝；www 无法触达 private listener。 |
| guest 删除/替换 FIFO | 已打开 fd 安全保留；jaild 不重开、不遍历。 |
| guest 移动已打开 FIFO 后写入 | 仍提交到启动时固定 target。 |
| guest 自建无读端 FIFO 并写入 | 只阻塞自己的 shell；会话回收时终止。 |
| 主文件、salt 或 initialized 损坏/缺失 | commentd 保留现场并拒绝启动；不空库、不复用 ID。 |
| 持久化在 rename 前失败 | 不替换内存状态、不返回成功。 |
| rename 后目录 fsync 失败 | commentd fail-stop，不继续提供不一致状态。 |
| 初始快照同步失败 | 不交付会话，不用空快照伪装成功。 |
| 防滥用 | pending + 8 条/会话 + 10 条/小时/ip_hash + 300 秒判重 + 512 字节/行。 |
| 终端/网页注入 | commentd 清洗 C0/DEL/C1；jailbin 再过滤；前端用 `textContent`。 |

## 12. 里程碑与验收

按“FIFO/路径安全 spike → commentd → 终端完整纵切 → Web → 部署”实施。

### M0：FIFO / 路径安全 spike

- 在目标 FreeBSD + ZFS 验证模板 FIFO 克隆后会话互不串台。
- 验证 `O_RDWR | O_NONBLOCK` 下 writer 不阻塞、kqueue 不因最后 writer 关闭空转；
  `read(0)`/`EAGAIN` 有回归测试。
- 验证 fd 在 shell fork 前打开并 `fstat(S_IFIFO)`；启动后 rm/mv/symlink 都不会触发
  root 重开/遍历，target 始终固定。
- 模板 FIFO 不可用时，改为按可信清单在 pre-shell 阶段创建并打开，不退回运行期遍历。

### M1：commentd + commentctl

- 双 socket kind 白名单与权限测试通过；www 只能查询 approved。
- 单测覆盖前缀、C0/DEL/C1、非法 UTF-8、长度、限流与判重。
- fault injection 覆盖写、文件 fsync、rename、目录 fsync；内存只在完整提交后变化。
- 损坏 JSONL、重复 ID、丢失主文件/salt/initialized 标记都拒绝启动，原文件不变且
  ID 不复用。
- 用测试客户端/commentctl 完成 submit → queue → approve → paged query；数千条数据下
  分页、revision 重试和帧大小正确。

### M2：终端完整纵切

- 新会话安全开 FIFO → 初始快照 → fork shell → `Opened`；镜像页自动 `blog` 时首屏
  可读到已有评论。
- `echo 'alice: 好文' > ~/blog/comment` → ack 经 out 回显 → queue → approve；
  新建会话后，同目录文章末尾可见连续序号。
- 覆盖顶层目录、嵌套目录和 `/` 三类 target，确认同目录共享、不同目录不串区。
- 证明 ack 不进 PTY 输入缓冲；覆盖异步 ack/prompt 交错、超长、非法 UTF-8、限流、
  rm/mv/symlink 和 `echo ...; exit` drain。

### M3：Web 展示闭环

- 文章评论由 content-build 生成在文章末尾；终端接管后可切回完整文章查看，页面不在
  terminal host 或终端下方追加评论区域。
- `/blog/` 文章列表和 `/` 首页不生成评论区。
- 文章镜像页覆盖 loading、空态、最新 100 条、顶部省略提示和 503；DOM 注入测试通过。
- termblog-web 只连接 public socket，路由无写入口。

### M4：部署收口

- `make deploy` 后各进程、两个 socket 权限、root-only 数据目录正确。
- target 清单由真实文章的直属目录去重生成，不给无直属文章的目录创建 FIFO。
- `tests/verify-comments.sh` 覆盖 FIFO → queue → approve → API，并检查会话初始快照存在；
  新会话启动 barrier 负责取得最新 approved 评论。

## 13. 实施落点

| 位置 | 变更 |
| --- | --- |
| `Cargo.toml` | 将新的 `crates/servers/commentd` 加入 workspace，统一依赖版本。 |
| `crates/config/src/lib.rs` | 新增 comments 配置结构、默认值和双 socket/data_dir 校验。 |
| `crates/servers/commentd/src/protocol.rs` | 定义两个 socket 的 kind、请求/响应、分页错误；不污染会话协议语义。 |
| `crates/servers/commentd/src/store.rs` | JSONL load/validate、索引、revision、COW commit、fault-injection 接口。 |
| `crates/servers/commentd/src/server.rs` | 双 listener、逐 socket kind 白名单、一连接一请求。 |
| `crates/servers/commentd/src/main.rs` | `commentd` / `commentctl` 多调用分发及显式 `commentd --init`。 |
| `crates/servers/jaild` | 在 backend/session 边界加入 pre-shell FIFO 打开和首次快照 barrier；pump 加 ack 分支与退出 drain。 |
| `crates/tools/jailbin/src/blog` | 复用 slug 算 target，统一渲染文章/列表评论，并在输出边界再过滤。 |
| `crates/tools/content-build` | 从 `Article.slug` 生成 target 清单；在每篇文章末尾和 `/blog/` 页底生成评论结构并加载独立入口。 |
| `crates/servers/web` | 在静态 fallback 前加入只读 API，只持有 public socket 路径。 |
| `frontend` | 增加 comments 入口/样式；首页覆盖式留言抽屉；`main.ts` 只增加文章/终端双向切换。 |
| `etc`、`deploy-scripts`、`tests` | rc.d、初始化/权限、模板 FIFO、协议客户端及端到端验证。 |

实现时先落共享数据类型和纯函数测试，再接 listener/文件系统/PTY；每个里程碑保持可独立
回归，M0 结论写入代码注释与测试名，不让临时 spike 成为未记录的生产假设。

## 14. 编码量估算（粗）

| 部件 | 行数 |
| --- | --- |
| commentd（协议、双 listener、COW store、分页） | ~650 |
| jaild（pre-open、固定 target、ack、快照、drain） | ~320 |
| jailbin（文章/列表渲染、二次过滤） | ~90 |
| termblog-web（只读 API） | ~70 |
| content-build（target 清单、文章/列表结构） | ~80 |
| frontend（comments bundle、视图切换、首页抽屉、样式） | ~180 |
| 模板、rc.d、config、deploy、verify | ~170 |
| 单元/集成/端到端测试 | ~300 |

合计约 **1860 行**。新增依赖预计只有 `sha2`；时间库优先复用 workspace 已有选择。
增加量主要来自持久化故障处理、分页一致性、双 socket 和安全回归测试。

## 15. 后续候选

评论静态烘焙供无 JS 访客/爬虫读取、`auto_approve`、推送通知、reply 嵌套、
`~/.comment-name` 会话粘性签名、评论数进入 `.index`/Atom、审核 Web UI、
跨会话增量快照缓存。
