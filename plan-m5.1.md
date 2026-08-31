# plan-m5.1 —— M5 之后的工程整理

> 性质: 纯整理 + 少量无感增强, 不改任何对外行为(除新增 `web.site_title` 配置项与
> jailbin 多合一二进制外, 访客体验零变化)。
> 背景决策(与作者逐条确认): 见文末「决策记录」。

## 范围

**做**:

1. Rust workspace 重组: `crates/{config, servers/{proto,core,web,ssh,jaild}, tools/{content-build,jailbin}}`
2. 把 `config` 从 `termblog-core` 拆出为独立 crate `termblog-config`(content-build 只依赖它)
3. `web.site_title` 进配置
4. 新 crate `termblog-jailbin`: busybox 式多合一 jail 二进制, 移植 `jailtpl/bin/{blog,webctl}`
5. 部署脚本收敛为两件套: `deploy-scripts/build-template.sh`(含 `--replace`)+ `deploy-scripts/deploy.sh`(含 `--static-only`)
6. 测试脚本归置到 `tests/`: `verify-jail.sh` → `verify-m3.sh`, 另含 `verify-m5.sh`、`e2e-reconnect.mjs`
7. Makefile 增加 `tpl` / `deploy` / `content` 目标并清理过时内容
8. 文档与引用同步

**不做**(本轮范围外):

- 会话内读者即时看到新内容(内容仍烘进模板快照, 旧会话读到旧内容; 拆内容数据集属 M6 候选)——模板替换本身已改为零停机 swap(见 §5.1)
- ops 脚本语言迁移(保持 shell)
- Makefile 之外的开发流程大改
- git 历史改写(前三条已合并进 M5, 已完成)

---

## 1. 目标目录布局

```
termblog/
├── Cargo.toml                     # members 更新(见 §2)
├── Makefile                       # 新目标 tpl/deploy/content + 清理(见 §6)
├── crates/
│   ├── config/                    # 新 crate: termblog-config(见 §3)
│   ├── servers/
│   │   ├── proto/                 # 自 crates/proto 移入
│   │   ├── core/                  # 自 crates/core 移入, 移除 config 模块
│   │   ├── web/  ssh/  jaild/     # 自 crates/ 移入
│   └── tools/
│       ├── content-build/         # 自 crates/content-build 移入
│       └── jailbin/               # 新 crate(见 §4)
├── deploy-scripts/
│   ├── build-template.sh          # 自 jailtpl/build-template.sh 移入, 新增 --replace(见 §5.1)
│   └── deploy.sh                  # deploy-root.sh + update-content.sh 合并(见 §5.2)
├── tests/
│   ├── verify-m3.sh               # 原 scripts/verify-jail.sh(见 §6 前身归置)
│   ├── verify-m5.sh
│   └── e2e-reconnect.mjs
├── jailtpl/
│   └── content/                   # bin/ 与 build-template.sh 移除(仅剩内容资产)
├── scripts/                       # 删除
└── ...(其余目录不动)
```

---

## 2. workspace 重组(纯移动, 先做)

2.1 `Cargo.toml` members 改为显式列表:

```toml
members = [
  "crates/config",
  "crates/servers/proto", "crates/servers/core",
  "crates/servers/web", "crates/servers/ssh", "crates/servers/jaild",
  "crates/tools/content-build", "crates/tools/jailbin",
]
```

(workspace.dependencies 不动。)

2.2 移动(用 `git mv` 保留历史):

- `crates/{proto,core,web,ssh,jaild}` → `crates/servers/`
- `crates/content-build` → `crates/tools/content-build`

2.3 修正所有相对 path 依赖(移动后的位置关系):

| crate | 依赖 | 新 path |
|---|---|---|
| servers/core | termblog-proto | `../proto` |
| servers/web | termblog-core / termblog-proto | `../core` / `../proto` |
| servers/ssh | 同上 | 同上 |
| servers/jaild | termblog-core / termblog-proto | `../core` / `../proto` |
| tools/content-build | termblog-config(新, 见 §3) | `../../config` |

2.4 验证: `cargo build --release` 与 `cargo test` 全绿后再进入下一步。

---

## 3. config 拆 crate + site_title

### 3.1 拆分

- 新建 `crates/config/`:

```toml
[package]
name = "termblog-config"
version = "0.1.0"
edition = "2021"

[dependencies]
anyhow.workspace = true
serde.workspace = true
toml.workspace = true
```

- 把 `crates/servers/core/src/config.rs` 整体搬为 `crates/config/src/lib.rs`;`DEFAULT_CONFIG`、`Config::load`、`WebConfig/SshConfig/SessionConfig/JailConfig` 及全部默认值原样保留;文件头注释「web / ssh / jaild 三个二进制共用」改为「servers 三二进制 + content-build 共用」。
- `crates/servers/core/src/lib.rs`: 删除 `pub mod config;` 与 `pub use config::{...};`;doc 注释中「jaild 也依赖本 crate 的 link/config/handle」的 `config` 删去。
- `crates/servers/core/Cargo.toml`: 删除 `toml.workspace = true` 与 `serde.workspace = true`(二者仅 config.rs 使用;若编译报错则按需保留, 由实现者确认)。
- 机械替换 import(逐文件):
  - `crates/servers/web/src/main.rs`: `use termblog_core::{Config, Control, SessionClient};` → `Config` 改从 `termblog_config` 引入
  - `crates/servers/ssh/src/main.rs`: 同上
  - `crates/servers/jaild/src/main.rs` 与 `crates/servers/jaild/src/jail.rs`: `Config` / `JailConfig` 改从 `termblog_config` 引入(`jail.rs` 现为 `use termblog_core::config::JailConfig;`)
  - `crates/tools/content-build/src/main.rs`: `termblog_core::Config::load` → `termblog_config::Config::load`;Cargo.toml 删除 `termblog-core` 依赖, 新增 `termblog-config = { path = "../../config" }`
- web/ssh/jaild 三个 Cargo.toml 新增 `termblog-config = { path = "../../config" }`。
- content-build 对 core 的全部使用只有 `Config::load` + `cfg.web.site_url`(已确认), 拆分后依赖面即干净。

### 3.2 web.site_title 进配置

- `WebConfig` 增加字段 `pub site_title: String`, 默认 `"~yzs"`(与 content-build 现 CLI 默认一致, 保证无配置时产物零漂移)。
- `content-build/src/main.rs`: `--site-title` 改为可选, 优先级 `--site-title` > `cfg.web.site_title` > 内置默认 `"~yzs"`(现实现是把默认值直接写进 Cli 结构体, 调整为 Option 即可)。
- `etc/termblog.toml` 的 `[web]` 段增加注释与取值:

```toml
# 站点标题: 镜像页 <title> / og:site_name / atom feed 标题用
site_title = "~yzs"
```

### 3.3 验证

`cargo build --release`、`cargo test` 全绿;`target/release/content-build --content jailtpl/content --dist frontend/dist` 产物与改动前逐字一致(未配置 site_title 时)。

---

## 4. jailbin(busybox 式多合一, 对访客无感)

### 4.1 crate 骨架

- `crates/tools/jailbin/Cargo.toml`:

```toml
[package]
name = "termblog-jailbin"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "jailbin"
path = "src/main.rs"

[dependencies]
anyhow.workspace = true
```

- 单 `main.rs`, 按 `argv[0]` basename 分发: `blog` → blog 子命令, `webctl` → webctl 子命令, 其它 → usage + 非零退出。模块划分 `blog.rs` / `webctl.rs`(纯函数化, 便于单测);未来新命令 = 新模块 + 模板里多一条 symlink。

### 4.2 blog 移植规格

**逐条等价于现 `jailtpl/bin/blog`, 行为一字不差**:

1. 无参: `$HOME/.rendered/.index` 存在则原样输出(等同于 cat);不存在则 stderr 输出「暂无文章(模板里没有 .rendered/.index, 重建模板试试)」, exit 0。
2. 有参: 候选解析顺序 `[原样, $HOME/blog/<arg>, $HOME/blog/<arg>.md]`;全部不存在 → stderr「blog: 没有这个文件: <arg> (敲 blog 看列表)」, exit 1;相对路径用 current_dir 拼成绝对路径。
3. slug: 绝对路径以 `$HOME/blog/` 为前缀且以 `.md` 结尾 → 去前缀、去后缀;白名单 `[a-z0-9/-]`, 空串/以 `/` 结尾/含 `//` → slug 置空(仅阅读, 不同步地址栏)。
4. slug 有效 → 输出 OSC 进入序列, 内容与 `webctl url "/blog/<slug>/"` 一致(见 4.3)。
5. 内容源: slug 有效且 `$HOME/.rendered/<slug>` 存在 → 读预渲染;否则读原始文件(有 slug 时 stderr 提示「blog: 「<slug>」无预渲染产物, 显示原始 markdown(重建模板后即排版)」)。
6. 分页: stdin 是 tty → `Command::new("less").args(["-RXc", src]).status()`(继承 stdio, **不 exec** —— less 退出后还要发复位 OSC);stdin 非 tty → 直接写文件内容到 stdout。
7. 分页退出且 slug 有效 → 输出 OSC 复位(`url=/`)。

### 4.3 webctl 移植规格

- 子命令 `url` + 参数以 `/` 开头 → stdout 输出字节 `\x1b]7777;url=<参数>\x07`(ESC ] 7777 ; url=… BEL)。
- 参数不以 `/` 开头 / 缺参数 / 未知子命令 → usage 到 stderr, exit 2。

### 4.4 测试与运行环境

- 单测: slug 白名单正反例、候选路径解析、OSC 字符串拼装。
- `ldd target/release/jailbin` 必须只见 FreeBSD base 库(libc/libthr/libm/libutil/libgcc_s 等);若出现 `/usr/local` 依赖, 给 release profile 加 `+crt-static` 或等效静态方案。jail 内无 pkg 依赖是硬约束。
- 模板安装(替换现 build-template.sh 第 7.5 步):

```sh
install -m 555 "$REPO/target/release/jailbin" "$MOUNT/usr/local/bin/jailbin"
ln -s jailbin "$MOUNT/usr/local/bin/blog"
ln -s jailbin "$MOUNT/usr/local/bin/webctl"
```

- 删除 `jailtpl/bin/`。
- 无感验收: `blog hello`、裸 `blog`、读文章的 OSC 进入/复位、ANSI 粗体与 shell 版逐字一致(`tests/verify-m5.sh` 第 4 组全过);前端自动敲的 `blog ~/blog/<slug>.md` 与 zshrc MOTD 一字不改。

---

## 5. 部署脚本两件套

**总原则**: 脚本内部不得调用 make/gmake/Makefile(cargo/npm/content-build 直接调用);`REPO` 从脚本自身路径推导;以 yzs 身份编译的部分用 `su -l yzs -c` 包裹(避免 target/ 被 root 污染, 沿用 deploy-root.sh 的既有做法)。

### 5.1 deploy-scripts/build-template.sh(自 jailtpl/build-template.sh 移入)

- 保留原构建流程全部步骤: 父数据集 → base.txz(缓存 /tmp/termblog-base.txz)→ devfs → resolv.conf → pkg(zsh/less/tree)→ guest 用户 → .zshrc → 内容拷贝 → snapshot + readonly=on。路径引用改为 `CONTENT="$REPO/jailtpl/content"`, `REPO=$(dirname "$SCRIPT_DIR")`。
- 第 7.5 步按 §4.4 改为安装 jailbin + 两条 symlink。
- **新增自包含前置**(root 跑, 编译部分降权 yzs): 若 `$REPO/target/release/jailbin` 或 `$REPO/jailtpl/content/.rendered` 缺失, 则:

```sh
su -l yzs -c "cd $REPO && cargo build --release -p content-build -p termblog-jailbin \
  && [ -d frontend/dist ] || (cd frontend && npm install && npm run build) \
  && ./target/release/content-build --content jailtpl/content --dist frontend/dist"
```

  (content-build 需要 frontend/dist 里的 entry 资产;前置失败即停。)
- **新增 `--replace` 模式**(替代 update-content.sh 第三步, 供内容更新用; **零停机换模板**):
  1. 清理上次残留(构建中途失败可能留下): `zfs destroy -r zroot/jails/template.new 2>/dev/null || true`。
  2. 构建到旁路名 `zroot/jails/template.new`: 构建流程与正常模式完全一致, 仅 `DATASET`/`MOUNT` 临时为 `zroot/jails/template.new` / `/jails/template.new`(构建期间旧模板与在线会话全程不受影响)。
  3. 构建完成(snapshot + readonly=on 之后)换名上场:
     - 旧名让位: 若 `zroot/jails/template.old` 残留 → 先尝试 `zfs destroy -r`(无会话 pin 则成功); 仍 busy → `zfs rename zroot/jails/template.old zroot/jails/template.old-$(date +%s)` 让位;
     - `zfs unmount zroot/jails/template 2>/dev/null || true`;
     - `zfs rename zroot/jails/template zroot/jails/template.old`;
     - `zfs rename zroot/jails/template.new zroot/jails/template`;
     - `zfs set mountpoint=/jails/template zroot/jails/template`;`zfs set mountpoint=none zroot/jails/template.old`;`zfs mount zroot/jails/template 2>/dev/null || true`(保持旧行为: 构建后模板挂载于 /jails/template)。
  4. **全程不停服、不杀会话、不重启**: 旧会话继续跑自己的 clone(内容保持旧——固有语义), 新会话自动 clone 新模板(内容新);jaild 每次会话即时解析 `cfg.template`(jail.rs:132), 名字未变, 无需重启。
  5. 清理: 对 `zroot/jails/template.old*` 逐个尝试 `zfs destroy -r`, 被 pin 的跳过(最后一个旧会话退出后由下一次更新回收; 会话硬寿命 7200s 兜底)。
  6. 已知窗口: 两条 rename 之间 `template` 名字瞬时不存在, 恰逢其会的新会话 clone 会失败 → jaild fail-closed, 访客重试即可, 不做处理。
- 无 `--replace` 且模板数据集已存在: 维持现行为(拒绝并提示先销毁或加 `--replace`)。
- 前提已验证(2026-08-31 本机实验, rtest 测试数据集已清理): 带 dependent clone 的数据集执行 `zfs rename` 放行, 换名方案成立。

### 5.2 deploy-scripts/deploy.sh(deploy-root.sh + update-content.sh 合并)

默认全量部署(原 deploy-root.sh 第 1–6 步, 顺序与逻辑原样保留), 差异点:

1. root 检查;racct 检查(loader tunable 一段原样保留)。
2. **模板存在检查**: `zfs list zroot/jails/template@release` 不存在 → 报错「请先运行 deploy-scripts/build-template.sh」并退出(两脚本模型: 部署不再顺手建模板)。
3. 编译(以 yzs): `cargo build --release`(全 workspace, 自动含 jailbin)+ `cd frontend && npm install && npm run build` + 运行 content-build。
4. 安装(原 deploy-root 第 4/5 步原样): sbin 三二进制(含 `jaild` 命名)、frontend dist、termblog.toml(.sample 与备份刷新逻辑)、rc.d 脚本、newsyslog、sysrc 自启、/var/db/termblog 与日志文件。
5. 发布静态镜像(原 update-content.sh 第 2 步): `rm -rf "$STATIC_DIR/blog"`;`cp -R frontend/dist/. "$STATIC_DIR/"`;`chmod -R a+rX "$STATIC_DIR"`(保留 600 权限事故的注释)。
6. 起服务: 原 start_daemon 三段(`daemon -H -p -P` + pid 文件逻辑)原样保留。
7. 收尾输出: socket/进程状态、网页与 ssh 地址、验收提示改为指向 `tests/verify-m3.sh` 与 `tests/verify-m5.sh`。

**`--static-only` 旗标**(语义 = update-content.sh --static-only, 保留): 只做「编译 content-build + 运行 content-build + 发布静态镜像」, 跳过其余全部步骤;结尾输出「完成(仅镜像)。jail 侧将在下次模板重建时跟进。」——纯文件替换, 零进程重启、零会话中断。

### 5.3 删除

`scripts/deploy-root.sh`、`scripts/update-content.sh`、`jailtpl/build-template.sh`(均已迁移)。

---

## 6. tests/ 归置与 Makefile

### 6.1 测试脚本

- `git mv scripts/verify-jail.sh tests/verify-m3.sh`(逻辑零改动;头部注释中「已由 scripts/e2e-reconnect.mjs 覆盖」改为 `tests/e2e-reconnect.mjs`)
- `git mv scripts/verify-m5.sh tests/verify-m5.sh`
- `git mv scripts/e2e-reconnect.mjs tests/e2e-reconnect.mjs`
- 删除 `scripts/` 目录。

### 6.2 Makefile

- Makefile 整体改为 **bmake(FreeBSD 默认 make)风格**(去掉 define/endef + $(call) 等
  GNU 语法, `$(shell …)` 改 `!=` 赋值), 直接 `make <目标>`, 不再要求 gmake。
- 部署目标内嵌 sudo(直接 `make content` 即可, 会提示输入密码):
  - `tpl`: `sudo sh deploy-scripts/build-template.sh`
  - `deploy`: `sudo sh deploy-scripts/deploy.sh`
  - `content`: `sudo sh -c 'sh deploy-scripts/deploy.sh --static-only && sh deploy-scripts/build-template.sh --replace'`
    (`content` = 只改文章的部署: 静态发布 + 模板零停机换面, 全程不停服、不杀会话;只发镜像、不碰模板时走 `deploy.sh --static-only`。)
- 保留开发目标: `build / build-frontend / build-content / run / run-ssh / start / start-ssh / stop / stop-ssh / restart / restart-ssh / status / status-ssh / logs / logs-ssh / clean`。
- 清理过时部分: 删除 `install` target(生产安装归 deploy.sh, 消除双事实源);更新头部注释(部署入口三目标说明);`clean` 的 `jailtpl/content/.rendered` 路径不变。

---

## 7. 文档与引用更新

- `docs/m5-explained.md`: §7 首句「blog 是装进 jail 模板的一个 shell 脚本」改为「Rust 多合一二进制 jailbin, blog/webctl 为其 symlink」;§9 部署段改为 build-template.sh / deploy.sh(含 --static-only)/ make content 与 tests/ 下验收脚本路径。
- `README.md`: 已过时(仍在描述 scripts/build-content.ts 时代), 全量重写: 项目定位、新目录树、三个部署入口(make tpl / make deploy / make content)、两脚本职责、验收位置。
- `etc/termblog.toml`: 见 §3.2。
- `jailtpl/content/README.md`: 「构建时 build-template.sh 把两块分别拷进 jail」→「deploy-scripts/build-template.sh …」。
- `plan_m5.md` 为历史记录, 不改。
- 收尾检查: `grep -rn "scripts/|deploy-root|update-content|verify-jail|jailtpl/build-template"`(排除 `.git`、`plan_m5.md`、`target`、`m5.diff`), 无残留引用。

---

## 8. 实施顺序(每步独立可验证)

1. §2 + §3.1(config 拆分)→ `cargo build --release` + `cargo test` 绿
2. §3.2(site_title)→ 无配置时产物零漂移确认
3. §4(jailbin)→ 单测 + ldd 检查
4. §5.1(build-template.sh 迁移 + --replace)+ §5.2(deploy.sh 合并)→ 此步完成后再删 scripts/ 与 jailtpl/bin/
5. §6(tests 归置 + Makefile)+ §7(文档)

建议提交拆分: ①workspace 重组 ②config 拆分 + site_title ③jailbin ④部署脚本重构 ⑤tests/Makefile/文档(可按实际粒度微调)。

---

## 9. 验收清单(must-pass)

- [ ] `cargo build --release`、`cargo test` 全绿
- [ ] 干净环境可走通首次部署: 构建输入 → `deploy-scripts/build-template.sh` → `deploy-scripts/deploy.sh`
- [ ] 改一篇文章跑 `make content`: 镜像页更新;在线会话全程无中断(旧会话继续运行、读到旧内容);新开会话 `blog` 可见新文;无进程重启;全部旧会话退出后 `template.old*` 可销毁回收
- [ ] 连续两次 `make content` 均成功(验证 `template.old` 残留时的让位/清理逻辑)
- [ ] 改一篇文章跑 `deploy-scripts/deploy.sh --static-only`: 镜像即时更新、零进程重启、零会话中断
- [ ] `tests/verify-m3.sh` 全绿(jailbin 切换未影响 M3 语义)
- [ ] `tests/verify-m5.sh` 全绿(含 ssh 侧 blog 行为: 列表/OSC 进出/ANSI 粗体, 证明 jailbin 与 shell 版等价)
- [ ] `node tests/e2e-reconnect.mjs` 通过
- [ ] `deploy-scripts/*.sh` 内无 make/gmake/Makefile 依赖
- [ ] 访客无感: `blog hello` / 前端自动命令 / zshrc MOTD 与改造前完全一致
- [ ] git 状态: `scripts/` 与 `jailtpl/bin/` 已删除;全仓库无旧路径引用(除 `plan_m5.md` 历史文档)

---

## 附: 决策记录(与本计划一一对应)

1. `site_title` 进配置(`web.site_title`), content-build 消费。
2. core/proto 属服务器侧 → 归入 `crates/servers/`;config 被 servers 与 content-build 共用 → 独立 `crates/config`(作者原话「弄一个公用的 config 在 crates 根」)。
3. ops 部署脚本保持 shell, 不进 crates;`crates/tools/` 仅 content-build 与 jailbin。
4. 部署脚本不得依赖 Makefile;Makefile 依赖它们(`tpl` / `deploy` / `content` 薄入口)。
5. `--static-only` 保留(零停机改文章, 只发镜像)。
6. jail 侧内容更新 = `deploy.sh --static-only` + `build-template.sh --replace`(由 `make content` 组合);模板替换采用 build-aside + rename swap, 全程零停机(2026-08-31 本机实验: 带 dependent clone 的数据集 `zfs rename` 放行)。
7. jailbin 采用 busybox 式 argv[0] 分派 + symlink, 访客无感。
