# plan_comment —— 评论系统第一版: 设备写入即评论

> 性质: M5 后的第一个大功能。访客在 jail 里往指定的特殊文件写一行文本即提交评论,
> 经站长审核后显示在终端与网页。本文是设计定案 + 实施计划; 所有关键决策已与作者
> 逐条确认(见 §2「决策记录」)。第一版目标是**代码量最小**: 有多条路时一律选行数少的。

## 1. 范围

**做**:

1. `commentd` 新服务: 单进程, Unix socket, 评论存储 + 审核状态机 + 只读/审核消息
2. 评论设备: jail 模板预埋 FIFO —— `~/blog/**/comment`(每个目录一个)+ `~/comment`(全局)
3. jaild 评论泵: 持有全部读端、写入时定位归属目录、限流、提交 commentd、ack 注入 PTY、
   `~/.comments` 同步
4. `jailbin blog` 渲染评论段(文章页 + 留言板)
5. web: `GET /api/comments` 转译 + 前端渲染模块 + mirror 页评论槽注入
6. `commentctl`: `queue` / `approve` / `reject` 三个命令(审核最简形态)
7. 配置节 `[comments]` + rc.d + deploy.sh + `tests/verify-comments.sh` 端到端

**不做**(第一版, 部分留位):

- 嵌套回复(平铺; 想回复就写 `回复 #12: ...`)
- 审核之外的任何自动策略(防滥用只靠「默认 pending + 限流 + 去重」; `auto_approve`
  留配置项, 默认关)
- 评论构建期快照(爬虫读实时烘烤进去 —— 值得做, 放第二版)
- 推送通知(poll 已够); kqueue `NOTE_RENAME` 定位优化(遍历已够)
- HTML 表单投稿、Anubis 联动、评论计数进 `.index`/atom、每目录独立 index 页

## 2. 决策记录(与作者确认)

| # | 决策 |
| --- | --- |
| D1 | 特殊文件 = **FIFO(mkfifo)**, 不是内核字符设备(devfs 规则集无法创建节点, 内核模块过重) |
| D2 | 全局设备 = `~/comment`(**A1**), 不叫 `/dev/comment` |
| D3 | 逐目录设备: blog 树**每个目录**一个 `comment` FIFO, 构建期预埋; 全局设备在家目录根 |
| D4 | 归属 = **写入时设备所在目录的 URL 路径**(位置即目录, 不定文章; mv 无所谓, 看当前路径) |
| D5 | 目标映射: `~/blog/X/` → `/blog/X/`; `~/blog/` → `/blog/`; `~/` → `/`(首页=留言板) |
| D6 | 正文只走 FIFO; PTY 只负责 ack; jail 无网络, 永远接触不到 commentd |
| D7 | 存储 = 单写者 JSONL + 内存索引 + 变更时原子重写, 查询按 target/status |
| D8 | 审核 = `pending / approved / deleted` 三态; 默认 pending; commentctl 三命令 |
| D9 | 匿名写作: 行内前缀 `名字: 内容`; IP 只存带盐哈希(盐在数据目录, 600 权限) |
| D10 | 协议: 复用 proto 帧编解码 + core::Link; 语义独立(kind 是新命名空间) |
| D11 | commentd 进程模型: **一连接 = 一请求 → 一响应 → 断开**(无 req_id、无长连接状态机) |

## 3. 数据模型与存储

单文件 `/var/db/termblog/comments.jsonl`(路径可配), 一行一条, 可 grep:

```json
{"id":42,"target":"/blog/hello/","author":"alice","text":"好文",
 "ip_hash":"sha256…","created_at":"2026-09-01T12:34:56Z","status":"pending"}
```

- `id`: 单调递增, 服务端发, 审核/引用(文本里的 `#42`)不重号。
- `target`: 归属页面的 URL 路径(§2 D5), 值域同 slug 白名单再加 `/` 与合法尾斜杠,
  长度 ≤ 200。
- `author`: 行内 `名字: 内容` 前缀解析(commentd 内, 单一事实源); 无名 = `guest`;
  长度 ≤ 32, 清洗后存。
- `text`: 清洗后纯文本, ≤ 512 字节 UTF-8。**所有 C0 控制字符(< 0x20)、DEL(0x7f)、
  ESC(0x1b) 一律剥掉**—— ESC 剥离后残留的 `[31m` 之类只剩无害字面字符。
- `ip_hash`: `sha256(salt + ip)`, salt 在数据目录里; 文件里永不落明文 IP。
- `status`: `pending`(默认)/ `approved` / `deleted`(reject 即删档, 软删保 id)。
- `created_at`: 服务端时钟, 不信任访客。

**存储实现**(注释即规格):

```
load():   启动读全文件建 Vec + HashMap<target, Vec<idx>> + HashMap<status, …>
全套变更:  收集所需改动 → 内存状态更新 → 写 tmp → fsync → rename(原子)
```

单写者 = commentd 独占文件, 无常驻锁、无 WAL、无恢复协议。变更频率(评论 + 审核
动作)毫秒级重写几千行毫无压力。存储收敛到 trait(`submit / set_status / by_target /
by_status / by_id / since`), 换 sqlite 是机械替换。启动时文件损坏: 原文件改名
`.broken-<ts>` 留证, 以空库启动并告警, 不丢不吞。

## 4. commentd 设计

```
listen /var/run/termblog-commentd.sock (SEQPACKET, 0660 root:www)
  └─ accept 循环
       └─ 每连接一个 tokio task:
            收 1 帧 → Arc<Mutex<Store>> 处理 → 回 1 帧 → 关连接
```

- 依赖 `termblog-core`(Link 收发、Seqpacket)与 `termblog-proto`(仅 `encode/decode_one`
  做编解码, kind 无语义, 完全可复用)。
- 语义消息在**本 crate** 定义, kind 编号从 1 起(独立 socket = 独立命名空间,
  与 proto 的会话词语毫无关系):

| kind | 请求 | 响应 |
| --- | --- | --- |
| 1 submit | `{target, author, text, ip}` | `{ok, id?, notice}` |
| 2 query | `{status:"approved"\|"pending", target?}` | `{ok, comments:[{id,target,author,text,created_at}]}` |
| 3 approve | `{ids:[…]}` | `{ok, changed}` |
| 4 reject | `{ids:[…]}` | `{ok, changed}` |

- **submit 处理链**: 解析 `名字: ` 前缀 → 清洗 author/text(§3)→ 空文本拒绝 →
  限流(同 ip_hash 每小时 10 条; 同 ip_hash+target+text 300s 内判重)→ 入库
  `status=pending` → 回 `{id, notice}`。notice 是给访客看的一句话, jaild 原样
  注入 PTY(例: `[#42] 已投入待审队列, 归属 /blog/hello/`)。
- notice 文案由 commentd 出(单一事实源: "归属哪个页面"这里最权威), jaild 不做文案。
- 审核限制: approve/reject 不校验调用者身份(Unix socket 权限即凭证), 与 jaild
  socket 同思路: 文件权限 0660 root:www, commentctl 以 root/sudo 跑。

## 5. jaild 侧(评论泵)

**spawn 时**(在现有 session 创建流程里追加, 失败不影响会话可用):

1. 枚举设备: 遍历克隆 `blog/**` 的每个目录与家目录根, 把名为 `comment` 的 FIFO
   全部 `open(O_RDONLY|O_NONBLOCK)` 读端(**宿主路径** `/jails/s-<sid>/...`);
2. 每个 fd 记 `(fd, fstat(dev,ino))`, 起一个读 task: `readable()` → `read`;
   **`read` 返回 0 = 暂无写者, 继续等**(不是 EOF, 写者随时会出现);
3. 按 fd 累积字节, 满 512 B 未遇 `\n` → 丢弃并 ack"评论超长"; 遇 `\n` → 成行,
   经 mpsc 交给每会话的评论处理器(限流计数器也在处理器上)。

**定位归属**(§2 D4 的机器实现):

```
fd.fstat() 得 (dev,ino)
遍历 ~/blog/** 与家目录根下所有名为 comment 的条目, stat 比对 (dev,ino)
  找到 -> target = 该条目所在目录的 URL 路径(D5)
  找不到(被 rm/改名/mv 出范围) -> ack「无法定位此设备, 未发送」, 丢弃
```

每次只跑一个会话的树(几十个目录、毫秒级), 限流压顶(每会话 8 条)后总负载为零。
**inode 为键**: mv 到任何仍受遍历覆盖的位置, 归属自动跟随新路径——位置语义的物理来源。

**提交与回执**: 评论处理器按 D11 开一次性连接 submit, 把 notice 组帧经新 ack 通道
交给 pump(现有 select 循环加一条分支, 写 PTY master = 访客屏幕上印字)。commentd
不在时: ack「评论服务暂不可用, 请稍后再试」, 不阻塞不重试(第一版放弃排队重试)。

**`~/.comments` 同步**: 每会话一个同步 task —— 会话起时、每 60s、自家评论提交成功后:
query approved → 在克隆里原子重写 `~/.comments`(JSONL: `{target, author, date10, text}`;
date10 由 jaild 从 created_at 取日期部分)。访客只读, 644 即可。

**生命周期**: 所有评论 task、ack 通道、同步 task 随会话 pump 同生共死; 会话回收
路径(现有 cleanup)不变, 无任何跨会话长寿资源。fd 数量级: 64 会话 × (1 + 目录数),
几百个睡在 `readable()` 上的 task, 对 tokio 是零成本。

## 6. jailbin 侧(blog 渲染)

- 归属目标: 阅读文章时 target = 文章文件所在目录按 D5 映射(blog 根目录文章 →
  `/blog/`); 裸 `blog` 列表视图的「留言板」段 = `target ∈ {"/", "/blog/"}`。
- 位置: **less 退出之后**打印在正文后(第一版不进分页器; "嵌入随读随滚"留 §14);
  裸 `blog` 打印在文章列表之后。

格式定稿(作者已确认):

```
── 评论 (2) ─────────────────────────────
#3  alice · 2026-09-01
    好文, 学习了

#4  guest · 2026-09-01
    沙发
```

递推规则:

- 段头 `── 评论 (N) ──` / `── 留言板 (N) ──`: 加粗, 后接 dim 短横线补足 76 列
  (与文章分隔线同款语汇)
- 每条首行 `#id  author · 日期`:`#id` 与日期 dim, author 加粗; 正文缩进 4 格;
  条与条之间空一行
- 排序: 时间**正序**(最早在前, 留言板如日志; 已确认)
- `#id` 必显: 「回复 #12」的引用约定靠它
- 每段上限 100 条, 超出时末行 dim 提示 `… 更早的评论省略`
- 空态(dim): `(暂无评论 —— echo '...' > comment 写第一条)`
- 折行: 第一版交给终端自动折行(§13 R3 观察), 不做 76 列重排
- 评论文本已在 commentd 剥净控制字符, 原样打印, 无终端注入面

## 7. web + 前端 + content-build

- **termblog-web**: 新路由 `GET /api/comments?target=<路径>`(挂在 ServeDir fallback
  之前)。处理器按 D11 一次性连接 commentd 发 query(approved, target), 转成 JSON
  数组(`[{id,author,text,created_at}]`)返回; commentd 不在 → 503 JSON。不加 POST、
  不加任何写入面(访客评论只有一个入口: 写设备)。
- **content-build**: 每个 mirror 页注入评论槽(文章页 → 文章目录路径; 列表页 →
  `/blog/`; 首页 → `/`):

  ```html
  <section id="comments" data-comments-target="…">
    <h2>评论 (N)</h2>
    <ol class="comment-list"></ol>
  </section>
  ```

- **frontend** `comments.ts`: 加载时若存在 `#comments` → fetch →
  逐条 `textContent` 填 `<li class="comment">`: meta 行 `author · <time
  datetime=完整RFC3339>到分钟</time> · #id`(弱灰 #6a737d/13px, 沿用 .post-meta /
  #blog-index 语汇); 正文行; max-width 80ch 居中, 条目靠间距分隔无重边框;
  空态「暂无评论 —— 打开终端: cd …; echo '...' > comment」; 失败态「评论暂不可用」。
- 时间显示(已确认): 网页到分钟, 终端只显日期。
- 展示端只取 approved: pending/rejected 在任何展示面都不可能出现。

## 8. commentctl

与 commentd 同 crate 的**多合一二进制**(jailbin 同款手法): `commentctl` 是
`commentd` 的符号链接, 按 argv[0] 分发。三个命令, 每次一次性连接:

```
commentctl queue          # 列出 pending(id/target/author/时间/正文企首行)
commentctl approve <id…>  # 或 --all
commentctl reject <id…>
```

不做面板、不做编辑器、不做规则。将来想要自动通过, 配置加 `auto_approve` 即可。

## 9. 配置与部署

```toml
# etc/termblog.toml 新增
[comments]
socket     = "/var/run/termblog-commentd.sock"
data_dir   = "/var/db/termblog/comments"
```

- `data_dir/`: `comments.jsonl` + `salt`(600)。zroot 里随系统快照备份; newsyslog
  不必管(文件自管理)。
- `etc/rc.d/commentd`: 手动 rc 脚本风格同 jaild/termblog。
- `deploy-scripts/deploy.sh`: 编译/安装 commentd + commentctl 符号链接 + rc.d +
  (首次)生成 salt + 起服务; `Makefile` 相应目标补齐。
- `deploy-scripts/build-template.sh`: 模板构建追加 —— 遍历 `~/blog` **每个目录**
  `mkfifo comment` + 家目录 `mkfifo comment`, 全部 `chown guest`(FIFO 写不落盘,
  不占配额、不受只读模板影响)。
- 注意: FIFO **不进仓库**(jailtpl/content 仍是纯内容源), 只在模板构建期生成。

## 10. 失败域与安全

| 场景 | 表现 |
| --- | --- |
| commentd 宕机 | 访客 ack「评论服务暂不可用」; `/api/comments` 503; 同步 task 跳过本周期; 不阻塞访客 shell |
| jaild 宕机 | 会话全灭(现状), 评论自然无口 |
| 访客 rm 设备后 `echo > comment` | 创建的是普通文件, 字节留在文件里, 无人收到(自伤) |
| 访客自己 mkfifo 一个无读端的管道再写 | **shell 永久挂住**(已知边界; 只挂自己, 会话回收即解) |
| 访客移动设备到遍历范围外 | ack「无法定位此设备, 未发送」 |
| 访客读设备(`cat comment` 读端) | 与 jaild 抢读, 最多偷走自己的字, 无害 |
| 存储文件损坏 | 留证改名, 空库启动 + 告警 |
| 防滥用 | 默认 pending(垃圾永不自动上线)+ 8 条/会话 + 10 条/时/ip_hash + 300s 重复判定 + 512B/条 |
| 终端/网页注入 | commentd 剥控制字符; 前端 textContent; 终端侧只有 commentd 洗过的字节 |

## 11. 编码量估算(粗)

| 部件 | 行数 |
| --- | --- |
| commentd(crate: lib 语义 + bin server + commentctl 分发) | ~450 |
| jaild(枚举/读 task/定位/限流/ack 通道/同步 task) | ~220 |
| jailbin blog(目标映射 + 两处渲染) | ~70 |
| termblog-web(/api/comments) | ~50 |
| content-build(评论槽注入) | ~40 |
| frontend(comments.ts) | ~110 |
| 模板/rc.d/config/deploy/verify | ~130 |
| 测试(单测 + 端到端) | ~180 |

合计 **约 1250 行**, 是现有 web 网关量级。新增依赖仅 `sha2`(纯 Rust, ip 盐哈希用;
chrono 与 content-build 共用), 无 C 依赖、无新系统包。

## 12. 里程碑与验收

- **M1 commentd + commentctl**。验收: 单测(前缀解析/清洗/存储重写/限流/判重);
  手工用 `nc -U` 连 socket 走 submit → queue → approve → query 全链。
- **M2 web 转译 + 前端渲染 + mirror 槽**。验收: 手工 approve 一条后, 浏览器里
  文章页/列表页/首页三处均可见; 空态与 503 态正常。
- **M3 设备 + jaild 泵 + blog 渲染(核心)**。验收: 访客 `echo` 写入 → ack 回显 →
  queue 可见 → approve → 重新 `blog` 评论段可见; mv 设备到另一目录再写, 归属
  跟随; rm/超长/超限各得其所。此里程碑动工首日先跑 §13 两项先验。
- **M4 部署收口**。验收: `make deploy` 拉起 commentd(进程/权限正确); 模板重建后
  新会话目录里出现 `comment` 设备; `tests/verify-comments.sh` 端到端一键过:
  写 FIFO → queue → approve → `blog` 输出与 `/api/comments` 双端可见。

## 13. 动工首日先验(30 分钟桩, 不过则换路)

- **R1 FIFO 跨克隆独立性**: 两个会话克隆同一模板中的 FIFO, 各自写入互不可见、
  各自读端互不串台。若 FreeBSD ZFS 上不成立 → 退路: jaild 每会话现场 `mkfifo`
  再开读端(代价极小, 只是设备出现时机从"模板"挪到"会话出生")。
- **R2 读端先占不阻塞写者**: `O_RDONLY|O_NONBLOCK` 打开 FIFO 后, 访客写端打开
  即时返回。异常 → 退路: jaild 用 `O_RDWR` 打开(读端单持有, 不影响访客 write)。
- **R3 终端自动折行效果**: 观察长评论在 xterm/ssh 两端观感, 决定何时补 76 列
  重排。

## 14. 后续候选(本计划明确不做, 防忘)

构建期评论快照(爬虫读到评论)、`auto_approve`、推送通知、reply 嵌套、
`~/.comment-name` 会话粘性签名、评论数进 `.index`、目录设备自愈(rm 后重启恢复)、
kqueue 定位增量表。