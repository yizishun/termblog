# 终端博客 —— 核心架构计划

> 目标：一个部署在 FreeBSD 15.0 真机上的博客。每个访客获得一个真实 jail 里的真实
> shell（zsh），Web 前端（xterm.js）与 SSH 接入是同一个 PTY 的两种显示方式。
> 本计划只覆盖核心功能：**web 接入、ssh 接入、jail 会话后端**。
> Anubis / comment / proc-blog / CTF 等后续叠加，不在本计划内，但架构为其留出扩展点。

---

## 1. 总体结构：三个解耦部分 + 一个共享核心 + 特权分离

用户说的 "web / ssh / jail 后端" 三部分是正确的切分，但 web 和 ssh 有大量共同逻辑
（会话创建、PTY 读写、resize、生命周期）。因此代码组织为 **一个共享核心 crate +
两个薄接入层**；同时，生产部署采用**特权分离的两进程模型**：

```
                       ┌────────────────────────────────────────┐
 浏览器 xterm.js ──WS──▶│  termblog-web (axum)      [降权 www]   │
                       │   · 静态文件服务(前端 glue)             │──┐
                       └────────────────────────────────────────┘  │ proto 帧
                       ┌────────────────────────────────────────┐  │ (Unix socket
 ssh 客户端 ──────────▶│  termblog-ssh (russh)     [降权 www]   │──┤  /var/run/
                       └────────────────────────────────────────┘  │  termblog.sock)
                                                                   ▼
                       ┌────────────────────────────────────────────────────┐
                       │  jaild                                    [root]   │
                       │   · SessionManager  会话表、配额、空闲回收          │
                       │   · Session         PTY 主端读写、广播、winsize     │
                       │   · JailBackend     ZFS clone → jail create →      │
                       │                     jail_attach + zsh → 销毁       │
                       └────────────────────────────────────────────────────┘
                                                                   │
                                              FreeBSD: openpty(2), jail(2)/jail(8),
                                              zfs(8), rctl(8), pf(4)
```

**为什么特权分离是一等设计而非"后续优化"**：这个系统给匿名访客发真 shell，
axum/russh 这两个直接解析不可信网络输入的进程绝不应携带 root 权限。
jail/zfs/rctl/openpty+jail_attach 必须 root，于是天然切成两侧，
边界就是一个权限受控的 Unix socket（0660, owner root:www）。
接入网关被打穿 ≠ 主机被打穿。

解耦边界：

- **web / ssh 互不知晓**，都只依赖 `termblog-core` 里定义的 proto 帧 +
  `SessionClient`（socket 客户端封装，API 形如 `SessionHandle`）。
- **jaild 不知道接入方式**：它只提供「创建会话 → 字节流双工通道 + resize」。
- **JailBackend 是 trait**：核心逻辑对 "shell 跑在哪" 无感。开发期可用
  `LocalBackend`（直接 forkpty 本机 zsh，无需 root / FreeBSD），生产用 `JailBackend`。
- **开发模式 = 单进程**：`backend = "local"` 时 web/ssh 直接在进程内调
  SessionManager（`SessionClient` 有 in-process 实现），不需要 socket 和 root，
  macOS/Linux 上就能跑 M1。生产模式才拆两进程。

## 2. 仓库布局（Cargo workspace + 前端）

```
termblog/
├── Cargo.toml                 # [workspace]
├── crates/
│   ├── proto/                 # 帧协议 + SessionClient（socket 客户端 / in-process 两种实现）
│   │   └── src/lib.rs         # 零重依赖（bytes + serde），三方共享的唯一契约
│   ├── core/                  # termblog-core：会话核心（被 jaild 使用）
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── session.rs     # Session, SessionManager, SessionHandle
│   │   │   ├── pty.rs         # openpty + 读写泵（tokio AsyncFd）
│   │   │   ├── backend.rs     # trait ShellBackend + LocalBackend
│   │   │   ├── jail.rs        # JailBackend: zfs clone / jail create / jail_attach / rctl / 销毁
│   │   │   └── config.rs      # 全局配置（TOML）
│   │   └── tests/
│   ├── jaild/                 # 特权守护进程 [root]：监听 Unix socket，收 proto 帧
│   │   └── src/main.rs        # 调 core::SessionManager
│   ├── web/                   # termblog-web [降权]: axum, WS 端点 + 静态资源
│   │   └── src/main.rs        # local 模式下可单进程内嵌 core + ssh（见 §7）
│   ├── ssh/                   # termblog-ssh [降权]: russh server（lib，由 web 的 bin 拉起或独立 bin）
│   │   └── src/lib.rs
│   └── webctl/                # 装进 jail 模板的 OSC 发射器（静态编译小二进制）
│       └── src/main.rs        # `webctl theme dark` → 打印 OSC 7777 序列（本期只留骨架）
├── frontend/                  # 唯一的非 Rust 部分，刻意做薄
│   ├── index.html
│   ├── src/main.ts            # xterm.js 挂载、WS 收发、resize、重连（~300 行）
│   ├── src/osc.ts             # OSC 7777 handler 注册点（本期只留接口）
│   └── vite.config.ts         # 构建产物嵌入 termblog-web（rust-embed）
├── jailtpl/                   # jail 模板构建脚本（sh）：base + zsh + webctl + 博客内容 → ZFS snapshot
│   └── build-template.sh
└── etc/
    ├── termblog.toml          # 运行配置样例
    └── rc.d/{jaild,termblog}  # FreeBSD rc 脚本（两个服务）
```

## 3. proto —— 接入层与 jaild 之间的帧协议

Unix socket 上的极简二进制帧：`u8 type | u32 len | payload`。

```
0x01 Open    { cols, rows, term, peer_ip, attach_token? }   # 接入方 → jaild
0x02 Data    raw bytes                                       # 双向
0x03 Resize  { cols, rows }                                  # 接入方 → jaild → TIOCSWINSZ
0x04 Opened  { session_id, attach_token }                    # jaild → 接入方
0x05 Closed  { reason: Exit | Timeout | Quota | Error }      # jaild → 接入方
```

设计要点：

- **Data 之后就是裸字节**：PTY 输出的 ANSI/OSC 原样穿透到 xterm.js / ssh 终端，
  这是 web 与 ssh 画面一致的根本保证，也是 webctl OSC 方案能工作的前提。
- **WS 线协议直接复用同一套帧**（binary frame 原样承载 proto 帧），
  termblog-web 因此是纯透传，零协议转换；ssh 侧只做 window-change → Resize、
  channel data → Data 的最小翻译。
- 配额判定在 jaild（peer_ip 随 Open 传入），接入层只做握手速率限制——
  单一事实来源，web/ssh 共享同一配额池。

## 4. termblog-core —— 会话核心（jaild 进程内使用）

### 4.1 关键类型

```rust
/// 接入层拿到的东西：与具体 backend / 传输方式无关
pub struct SessionHandle {
    pub id: SessionId,
    pub input: mpsc::Sender<Bytes>,          // 键入 → PTY master 写
    pub output: broadcast::Receiver<Bytes>,  // PTY master 读 → 所有观察者
    pub control: mpsc::Sender<Control>,      // Resize{cols,rows} / Close
}

pub enum Control { Resize { cols: u16, rows: u16 }, Close }

/// backend 抽象：核心只关心「给我一个接在 PTY 从端上的 shell 进程」
#[async_trait]
pub trait ShellBackend: Send + Sync {
    async fn spawn(&self, sid: SessionId, winsize: Winsize) -> Result<ShellChild>;
    async fn cleanup(&self, sid: SessionId) -> Result<()>;   // 会话结束时销毁 jail
}

pub struct ShellChild {
    pub master: OwnedFd,       // PTY master（core 负责读写泵与 TIOCSWINSZ）
    pub pid: Pid,              // 用于 SIGHUP/等待退出
}
```

### 4.2 SessionManager 职责

- `create(peer: PeerInfo) -> SessionHandle`：检查每 IP 并发上限与全局会话上限
  （超限直接回 `Closed{Quota}`，即背压——ZFS clone 风暴在入口处被拒绝而非排队堆积）→
  调 backend.spawn → 启动读写泵 task → 登记会话表。
- **读写泵**（每会话一个 task）：
  - PTY master 可读 → 读入 → `broadcast::send`（web/ssh 观察者各自消费）；
  - `input` mpsc 有数据 → 写 PTY master；
  - `Control::Resize` → `ioctl(TIOCSWINSZ)` + `SIGWINCH`；
  - shell 退出（EOF/SIGCHLD）→ 通知观察者 → `backend.cleanup` → 移出会话表。
- **空闲回收**：无任何观察者且超过 `idle_grace`（默认 60s）→ 销毁。这个宽限期
  即"刷新页面不丢会话"的重连窗口：WS 重连时带上 session token 可 attach 回原会话。
- **核心决策（默认）**：每个连接一个独立 jail 会话；重连 attach 仅限持有原 token
  的同一访客。web 与 ssh 不共享会话（未来若要"ssh 接管 web 会话"，broadcast 输出
  通道已天然支持多观察者，只需加认证即可，架构不用改）。

### 4.3 PTY 细节

- `openpty(2)`（`nix::pty::openpty`），master 设 non-blocking，用
  `tokio::io::unix::AsyncFd` 包装做异步读写（FreeBSD kqueue 支持良好）。
- 输出侧对慢消费者的策略：broadcast 缓冲 N KiB，落后则丢帧并让 xterm.js
  重绘（对交互终端可接受；不做无限缓冲防内存被打爆）。

## 5. JailBackend —— FreeBSD 会话隔离

每会话一个 **ephemeral jail**，生命周期与会话严格绑定：

```
spawn(sid):
  1. zfs clone zroot/jails/template@release  zroot/jails/s-<sid>     # 毫秒级
  2. jail -c name=s-<sid> path=/jails/s-<sid> host.hostname=blog \
       persist ip4=disable ip6=disable allow.* 全关                  # 无网络
  3. rctl -a jail:s-<sid>:memoryuse:deny=128M
     rctl -a jail:s-<sid>:vmemoryuse:deny=512M
     rctl -a jail:s-<sid>:maxproc:deny=32
     rctl -a jail:s-<sid>:openfiles:deny=256
     rctl -a jail:s-<sid>:pcpu:deny=25
  4. openpty; fork; 子进程: setsid → 从端设为控制终端 →
     jail_attach(2) → chdir("/home/guest") → exec zsh -l  (uid=guest)
  5. 返回 ShellChild{master, pid}

cleanup(sid):
  kill 进程组 → jail -r s-<sid> → zfs destroy zroot/jails/s-<sid>
  （幂等；启动时扫描残留 s-* 全部回收，防止上次崩溃泄漏）
```

实现选择：

- jail 创建/销毁优先用 `libjail-rs`（fubarnetes/jail crate）+ `rctl` crate；
  若版本对 FreeBSD 15 有兼容问题，降级为调用 `jail(8)`/`rctl(8)` 命令行——
  接口都封装在 `jail.rs` 内部，外界无感。
- `jail_attach(2)` 在 fork 后的子进程里直接 syscall（`nix` 或手写 libc 绑定），
  避免依赖 `jexec` 外部命令；这样 PTY 从端、uid 切换、attach 的顺序完全可控。
- **jail 模板**（`jailtpl/build-template.sh`）：FreeBSD base（裁剪）+ zsh + 常用
  工具（less/grep/tree…）+ **webctl**（OSC 发射器，本期只装骨架）+ 博客内容
  （markdown 渲染为终端友好格式，或直接源文件）
  + guest 用户 + 定制 zshrc（欢迎 MOTD、受限 PATH）→ `zfs snapshot`。
  模板只读，可写层来自 clone，销毁即回收。

## 6. termblog-web —— Web 接入层

- **axum**，两个职责：
  1. `GET /`、静态资源：前端 glue 构建产物，用 `rust-embed` 编进二进制，部署单文件。
  2. `GET /ws`：升级 WebSocket → 向 jaild 发 `Open`（或带 attach_token 重连）→
     之后 WS binary frame ↔ Unix socket **原样透传 proto 帧**（见 §3），零协议转换。
- 限流：握手速率 + 每 IP 连接数（tower 中间件）做第一道闸；会话配额的最终判定
  在 jaild（单一事实来源）。TLS 交给前置反代或 axum-server rustls（配置项，二选一）。

### frontend/（薄 glue，~300 行 TS）

- xterm.js + fit/webgl addon；打开 WS；`term.onData` → `Data` 帧；
  收 `Data` → `term.write`；`ResizeObserver`/fit → 发 `Resize` 帧
  （`transport.ts` 里 proto 帧的 TS 编解码只有几十行）。
- **不做客户端行缓冲/命令拦截**——按键字节原样进 WS，否则会与 zsh ZLE、vim 打架。
  前端独有命令走 OSC 通道（下条）。
- 断线重连：指数退避，带 attach_token 尝试恢复，失败则新会话。
- `osc.ts`：为后续"前端独有命令"预留 OSC handler 注册点
  （`term.parser.registerOscHandler(7777, …)`），与 jail 里的 `webctl` 二进制配对：
  `webctl theme dark` 打印 `ESC]7777;theme=dark BEL`，前端捕获执行，真 ssh 终端
  自动忽略——本期只留接口与 webctl 骨架，不实现具体命令。
- 旧 ghpage repo 仅复用视觉风格与 xterm 初始化配置；shell/content/apps 层不搬。

## 7. termblog-ssh —— SSH 接入层

- **russh** 实现 SSH server（不是宿主 sshd ForceCommand，也不是 jail 里跑 sshd——
  这样 web/ssh 走完全相同的 jaild 路径，配额、回收、审计一套逻辑，且宿主 sshd
  完全不暴露给访客）。
- 认证：`ssh guest@blog` 免密（`auth_none`/接受任意密码，仅用于开会话）。
- channel 事件映射（ssh-gw 是 SSH ↔ proto 的最小翻译器）：
  - `pty_req(term, cols, rows)` → 记录 winsize；`shell_req` → 向 jaild 发 `Open`
    → 泵 channel data ↔ `Data` 帧；
  - `window_change_req` → `Resize` 帧；
  - 收 `Closed` → `exit_status` + 关 channel；channel 关 → 断开 socket（触发宽限回收）。
- 禁 exec/subsystem/port-forward/agent-forward，只实现 shell 会话最小子集。
- host key 持久化在 `/var/db/termblog/ssh_host_ed25519`，首次启动自动生成。

## 8. 进程与部署形态

- **生产：两个守护进程**（见 §1）：
  - `jaild` [root]：唯一特权进程，监听 `/var/run/termblog.sock`（0660 root:www）。
  - `termblogd` [www]：一个降权进程同时拉起 axum listener 和 russh listener
    （ssh crate 是 lib；三部分**代码**解耦，降权侧**进程**不必拆成两个）。
    配置里可分别关掉 web 或 ssh。
- **开发：单进程**：`backend = "local"` 时 termblogd 进程内直接内嵌
  SessionManager + LocalBackend，无 socket、无 root、无 FreeBSD 依赖。
- `etc/rc.d/{jaild,termblog}` rc 脚本 + `etc/termblog.toml`：

```toml
[web]  listen = "127.0.0.1:8080"          # 前面可挂反代/Anubis（后续）
[ssh]  listen = "0.0.0.0:2222"
[session] max_total = 64
          max_per_ip = 3
          idle_grace_secs = 60
          hard_lifetime_secs = 7200
[jail] template = "zroot/jails/template@release"
       dataset_prefix = "zroot/jails/s-"
       path_prefix = "/jails"
       memory = "128M"
       vmemory = "512M"
       maxproc = 32
       openfiles = 256
       pcpu = 25
[backend] kind = "jail"                    # 或 "local"（开发模式，单进程无特权）
          socket = "/var/run/termblog.sock"
```

## 9. 里程碑（每步都可运行验证）

1. **M1 骨架**：proto + core(LocalBackend, 本机 zsh) + web + 前端 glue，单进程。
   浏览器里得到一个完整可用的本机终端。✔ 验收：vim/htop/resize 正常；
   **在 FreeBSD 15.0 真机上同步验证 forkpty + AsyncFd**（提前暴露 Tier-2 风险）。
2. **M2 SSH**：russh 接入层，走与 web 相同的会话路径。✔ 验收：`ssh -p 2222 guest@host`
   与 web 行为一致。
3. **M3 Jail + 特权分离**：JailBackend + jaild 独立进程（Unix socket）+ 模板构建
   脚本 + rctl + 启动残留回收；web/ssh 降权运行。
   ✔ 验收：并发多访客互不可见；fork bomb 被 rctl 掐死；断线 60s 后 jail 消失；
   `zfs list` 无泄漏；termblogd 以 www 用户运行。
4. **M4 打磨**：attach 重连、配额/限流、hard lifetime、日志（tracing）、rc 脚本、
   单文件部署。✔ 验收：真机 FreeBSD 15.0 上线跑通；破坏性自测
   （fork 炸弹、写盘打满、内存打满、会话占满）全部通过。

## 10. 明确不做（本期）

Anubis / dmesg 开机动画、comment、/proc/blog、CTF、webctl 具体命令的实现
（仅留 OSC handler 注册点与 webctl crate 骨架）、SEO 静态镜像、多观察者共享会话、
jail 预热池（秒开优化，接口上兼容，后续加）。

## 11. 风险与预案

| 风险 | 预案 |
|---|---|
| libjail-rs/rctl crate 对 FreeBSD 15 不兼容 | jail.rs 内降级为 `jail(8)`/`rctl(8)` 命令行，接口不变 |
| Rust 对 FreeBSD 是 Tier 2（nix/tokio/russh 边角） | M1 即在真机验证 forkpty + jail_attach + AsyncFd 组合；退路 blocking thread 泵 |
| ZFS clone 泄漏（进程崩溃） | 启动扫描回收 + cleanup 幂等 + hard lifetime 兜底 |
| 突发流量 → zfs clone 风暴 | 配额在 Open 入口直接拒绝（`Closed{Quota}` + 前端"客满"提示），不排队 |
| russh 的 PTY/agent 边角行为 | 只实现 shell 会话所需最小子集，禁 exec/subsystem/forward |
| 恶意占满会话 | 每 IP 上限 + 全局上限 + hard lifetime；后续 Anubis 前置 |
| 接入网关被打穿 | 网关降权 www；特权面收敛在 jaild，socket 权限 0660 |
