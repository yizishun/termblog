# termblog 部署手册 —— 阿里云 FreeBSD 15.0(UFS 根) + 数据盘 zroot

> 目标机器实测快照(2026-01): FreeBSD 15.0-RELEASE-p4 GENERIC amd64, 2 vCPU / 2G 内存 / 40G 单盘
> (28G 空闲), UFS 根 + 1G swap, UEFI 引导, 无防火墙, 用户 yzs(wheel, sudo 需密码)。
> ZFS 内核模块(zfs.ko)已随 GENERIC 内核自带且已加载, 缺的只是一个叫 `zroot` 的池。
> 本方案: UFS 根不动, 在其上建**文件式 zpool** —— termblog 用到的全部 ZFS 特性
> (clone/snapshot/readonly/quota)都是数据集层操作, 与 vdev 类型无关; 性能代价经实测
> 落在 termblog 几乎不经过的路径上(详见会话讨论, 结论: 够用)。
>
> 全程只有一次不可避免的 reboot(racct 是 loader tunable, 运行期只读)。
> 所有 `sudo` 命令由管理员手工执行; 部署脚本(build-template.sh / deploy.sh)本身无需修改。
>
> **⚠️ 文件式池(方案 B)的适用边界**(来自 zpool-create(8) Example 4 原文: "While not
> recommended, a pool based on files can be useful for experimental purposes"):
> 上游立场是"功能完全支持, 生产不推荐"。相对分区式 vdev, 文件式严格多一层宿主
> 文件系统依赖(UFS 元数据管辖 backing file 的所有块), 且与系统盘共享空间
> (UFS 写满 = 池报错)。termblog 之所以尚可接受: 池里只有可再生数据(模板
> 十分钟可重建, 会话本就一次性), 评论/统计等不可再生数据全在 UFS 根上——
> 最坏情况是约十分钟的可用性事故, 不是数据丢失。**本手册默认方案 A(ESSD Entry
> 数据盘, 实测 ~¥47/年, 比预想的还便宜), 方案 B 仅作零成本起步备选。**

---

## 阶段 0 —— 系统准备(一次性, 结尾重启)

```sh
# ── 0.1 装 node + npm ──────────────────────────────────────────────────
# 注意: FreeBSD 15 的 node24 包已不再捆绑 npm(实测 pkg info -l node24 只含
# /usr/local/bin/node 一个可执行文件), npm 是独立包 npm-node24(npm 11.x)。
# 只装 node24 或反复重装它都不会有 npm —— 必须单独装。
sudo pkg install -y npm-node24

# 核验: 两条都必须出版本号
node -v && npm -v
# 备选路线(仅备忘, 不推荐): corepack enable npm 也能生成 npm shim,
# 但首次运行要联网现拉 npm, 部署脚本里不如独立包稳。


# ── 0.2 开机配置: ZFS 自启 + ARC 封顶 + racct ─────────────────────────
# zfs_enable=YES: 开机自动 import 池(文件式池靠 /boot/zfs/zpool.cache
#                里的绝对路径, 重启后自动找回, 无需手动 import)
sudo sysrc zfs_enable=YES

# loader.conf 追加三行(现有内容不动, 只追加):
#   zfs_load          —— 确定性加载 ZFS 模块(现在是被自动拉起的, 写死更稳)
#   vfs.zfs.arc_max   —— ARC 封顶 512M。默认按物理内存一半(~1G)吃,
#                        2G 内存的机器必须封顶, 否则和 64 个 zsh 会话抢内存
#   kern.racct.enable —— rctl 资源限额的总开关, jaild 对它 fail-closed:
#                        不开就拒绝交付任何会话。注意写法必须是不带引号的
#                        `kern.racct.enable=1`(deploy.sh 按此格式 grep,
#                        写成 ="1" 会让它重复追加一行)
sudo sh -c 'cat >> /boot/loader.conf <<EOF
zfs_load="YES"
vfs.zfs.arc_max="536870912"
kern.racct.enable=1
EOF'

# 人工核对三行都在
cat /boot/loader.conf


# ── 0.3 建 zroot 池(二选一; 此时建好, 重启正好验证自动 import) ────────
#
# 【方案 A · 推荐: 云数据盘】ESSD Entry 10GiB, 包年 ~¥47/年(杭州 2026-01 实价)。
#   容量账: 模板(lz4 后)~1.2-1.5G + 换面瞬态峰值 ~3G + 会话 churn ≤64M,
#   10G 余量 3×; 不够时云盘在线扩容 + zpool online -e 即可。
#   性能账: Entry 10G = 1880 IOPS / 101.5MB/s, 对 termblog 负载
#   (spawn 元数据事务 + 顺序写重建 + quota 封顶的小写入)有 10-30× 富余,
#   与系统盘实测吞吐(~104MB/s)同级。更高档(PL0/AutoPL)差异不可感知。
#   购买要点: 可用区必须与实例一致(cn-hangzhou-i) / 包年计费 /
#   不勾加密与预配置性能 / 挂载到实例后【忽略】控制台的
#   "初始化磁盘/分区格式化"指引 —— FreeBSD 整盘建池, 不分区不格式化。
sudo zpool create -o ashift=12 -O mountpoint=none -O compression=lz4 \
     zroot /dev/vtbd1        # 先用 sysctl kern.disks 确认新盘名(vtbd0 是系统盘!)
zpool status zroot           # 期望: pool: zroot, state: ONLINE, vdev 为 vtbd1
#
# 【方案 B · 零成本备选: 文件式池】适用边界见文首警示, 仅建议起步验证用。
# ashift=12: 4K 扇区对齐, 云盘标准; 底层是文件也一样要对齐。
# -O mountpoint=none: zroot 根数据集不挂载(池里只需要 zroot/jails/*,
#                      build-template.sh 自己建 /jails/template 挂载点)。
# -O compression=lz4: 模板以文本为主, lz4 压缩率可观且 CPU 代价近零;
#                      子数据集(jails/template 及其 clone)全部继承。
#sudo mkdir -p /usr/local/zfs
#sudo truncate -s 16G /usr/local/zfs/zroot.img
#sudo chmod 600 /usr/local/zfs/zroot.img  # 防呆: 该文件即整池载体, 误删=池蒸发
#sudo zpool create -o ashift=12 -O mountpoint=none -O compression=lz4 \
#     zroot /usr/local/zfs/zroot.img


# ── 0.4 重启(racct 生效的唯一途径) ─────────────────────────────────────
sudo shutdown -r now
```

## 阶段 1 —— 重启后核验(全绿才继续)

```sh
# racct 已生效?(deploy.sh 的第一道 fail-closed 检查)
sysctl -n kern.racct.enable        # 必须输出 1

# 池已被 zfs_enable 自动导入?
zpool status zroot                 # ONLINE
zfs get -o value mountpoint,compression zroot   # none / lz4

# ARC 封顶已生效?
sysctl -n vfs.zfs.arc_max          # 536870912
```

## 阶段 1.5 —— 固化 cloud-init(本机镜像特性, 必做)

> 本机阿里云 FreeBSD 镜像的 cloud-init 有两个坑(2026-09-06 实锤):
> ① cc_ssh 的 ssh_deletekeys 未在 cloud.cfg 配置, 默认 True → 每次运行都删光
>    /etc/ssh/ssh_host_* 重新生成; 而"每实例一次"的 sem 标记存在 /var/run,
>    FreeBSD 开机清空 → **每次开机主机密钥都变**, ssh 客户端必弹
>    "REMOTE HOST IDENTIFICATION HAS CHANGED"。
> ② cc_update_hostname(frequency: always)每次开机把 hostname 改写成
>    实例 ID 风格(iZbp...Z), 并覆盖 rc.conf 里的 hostname= 行。

```sh
# 止血: 三行配置让 cloud-init 不删/不生成主机密钥、不动主机名
sudo tee /usr/local/etc/cloud/cloud.cfg.d/99-termblog-keep-keys.cfg <<EOF
ssh_deletekeys: false
ssh_genkeytypes: []
preserve_hostname: true
EOF

# 恢复 hostname(cloud-init 已把它从 rc.conf 改掉)
sudo sysrc hostname=freebsd-server
sudo hostname freebsd-server
```

客户端侧(若已遇到指纹告警): `ssh-keygen -R freebsd-server` 后重连接受新指纹。

## 阶段 2 —— 部署前配置修正(repo 内两处, 服务器上的 ~/termblog)

```sh
# deploy.sh 每次运行都会用 repo 里这份覆盖 /usr/local/etc/termblog.toml
# (旧版自动备份为 .old), 所以改 repo 这份才是长久之计, 不是改 /usr/local/etc。
vim ~/termblog/etc/termblog.toml

# 必改 1: HTTPS 域名。site_url 与 domains 必须使用公网 DNS 名称。
site_url = "https://www.yizishun.com"
[web.tls]
enabled = true
listen = "0.0.0.0:443"
domains = ["www.yizishun.com"]
contacts = []
cache_dir = "/var/db/termblog/acme"
production = true
# 同时保留 [web] listen = "0.0.0.0:80" 承载 HTTP-01 与 HTTPS 跳转；
# 必须先完成阶段 6.5，否则 www 绑不上 80/443。

# 必改 2: max_total —— 2G 内存机器上 64 会话×128M 内存帽是超售,
#         降到 16 是稳妥值(rctl 是上限不是预留, 但没必要赌)
max_total = 16

# 核对(git diff 可见这份改动; 以后 git pull 前先 stash 或提交到分支)
grep -E "site_url|max_total" ~/termblog/etc/termblog.toml

# ── 2.5 先装一份配置到 /usr/local/etc(重要, 别跳过!) ───────────────────
# 运行时进程读取 /usr/local/etc/termblog.toml。构建脚本则显式读取仓库配置，
# 因此生成的 sitemap/atom/canonical 会在首次部署时直接使用 HTTPS URL。
sudo install -m 644 ~/termblog/etc/termblog.toml /usr/local/etc/termblog.toml
# deploy.sh 也会安装并备份运行时配置；上面提前安装可让首次启动配置明确可查。
```

## 阶段 3 —— 预编译(以 yzs 身份; 让 root 脚本里的编译步骤变增量秒回)

```sh
cd ~/termblog

# ── 3.1 国内网络: 先配 cargo/npm 镜像(一次性, 用户级配置, 不碰仓库) ──
# 实测(杭州 ECS): 直连 crates.io 时 cargo 卡在 "Updating crates.io index"
# 15 分钟仅消耗 3.5s CPU(纯网络等待); 换 rsproxy.cn 后 30 秒内开始编译。
mkdir -p ~/.cargo
cat > ~/.cargo/config.toml << "EOF"
[source.crates-io]
replace-with = "rsproxy-sparse"

[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"

[net]
git-fetch-with-cli = true
EOF
npm config set registry https://registry.npmmirror.com
# 回退: rm ~/.cargo/config.toml && npm config delete registry

# 全 workspace: 5 个守护进程 + jailbin + content-build
# 2C/2G 冷编译较久(可能压 swap), 属正常; 若 OOM 中断, 用单任务重跑:
#   CARGO_BUILD_JOBS=1 cargo build --release
cargo build --release

cd frontend
# 国内拉 npm 源慢的话加 --registry=https://registry.npmmirror.com
npm install
npm run build          # 产物进 frontend/dist
cd ..

# 内容编译必须在 vite build 之后跑(它要读 dist 里的入口资产)
# 产物: jailtpl/content/.rendered/(终端 ANSI 预渲染)+ 镜像页 + 评论清单
./target/release/content-build --content jailtpl/content --dist frontend/dist
```

## 阶段 4 —— 构建 jail 模板(root, 需网络)

```sh
# ── 4.1 预下载 base.txz(脚本见 /tmp/termblog-base.txz 存在即跳过下载) ──
# 必须 15.0-RELEASE: 与宿主内核同版本(jail 用户态不能比宿主内核新)。
# 脚本默认 URL 是 16.0-CURRENT 快照, 这就是为什么要预下载覆盖它。
# 文件保留在 /tmp 供以后 --replace 换面复用(约 800M, 磁盘预算内)。
fetch -o /tmp/termblog-base.txz \
  https://mirrors.aliyun.com/freebsd/releases/amd64/15.0-RELEASE/base.txz

# ── 4.2 校验(脚本自己不校验; hash 取自官方 MANIFEST, 镜像文件应一致) ──
test "$(sha256 -q /tmp/termblog-base.txz)" = \
  "ac0c933cc02ee8af4da793f551e4a9a15cdcf0e67851290b1e8c19dd6d30bba8" \
  && echo "base.txz 校验 OK" \
  || { echo "校验失败! 删除重下: rm /tmp/termblog-base.txz"; }

# ── 4.3 构建(首次; 模板已存在会拒绝, 防覆盖在跑会话) ──────────────────
# 内部: 以 yzs 增量编译(阶段 3 已热, 秒回)→ 建 zroot/jails/template →
#   解 base.txz → chroot pkg 装 zsh/less/tree → guest 用户 →
#   内容 + 评论 FIFO + /proc scope 目录 + jailbin → snapshot → readonly=on
sudo sh ~/termblog/deploy-scripts/build-template.sh

# ── 4.4 核验模板 ───────────────────────────────────────────────────────
zfs list -o name,used,readonly zroot/jails/template        # readonly=on
zfs list -t snapshot                                         # @release 在列
ls -l /jails/template/usr/local/bin/blog                    # jailbin 符号链接
ls /jails/template/usr/local/share/termblog/                # comment-targets.tsv + article-index.json
```

## 阶段 5 —— 全量部署(root)

```sh
sudo sh ~/termblog/deploy-scripts/deploy.sh
# 内部: racct 检查 → 模板检查 → 增量编译(已预编译, 秒回) →
#   安装 5 个二进制/rc 脚本/newsyslog/配置 → 发布静态镜像 →
#   初始化 commentd/statd 数据目录 → sysrc 开机自启 → 拉起 5 个守护进程。
# 结束时自动打印 socket 列表 + 进程表 + 访问地址。

# ── 5.2 快速自检 ───────────────────────────────────────────────────────
# socket 就位
ls -l /var/run/termblog.sock /var/run/commentd-public.sock /var/run/termblog-statd.sock
# 进程形态: jaild/commentd/statd=root, termblog-web/ssh=www
ps -axo user,pid,comm | grep -E "jaild|commentd|statd|termblog" | grep -v grep
# 本机 HTTP 连通(镜像页 HTML 应有内容)
fetch -q -o - http://127.0.0.1/ | head -5
# 本机 SSH 免密进 jail(blog 是虚拟用户名, 任意密码直接进; 22 直绑见阶段 6.5/6.6)
(printf 'id -un\n'; sleep 3) | timeout 10 \
  ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
      blog@127.0.0.1 2>/dev/null
# 期望输出: guest
```

## 阶段 6 —— 验收 + 公网放行

```sh
# M3(root, ~2 分钟): 进程形态 / 真实 jail / 会话隔离 / rctl 掐 fork bomb /
#                    每IP并发配额 / 断线即回收 zfs 无泄漏 / 4M 磁盘配额
sudo sh ~/termblog/tests/verify-m3.sh

# M5(非 root): 镜像页 / 发现链路 / feed / robots + ssh 侧 blog 行为
sh ~/termblog/tests/verify-m5.sh

# 评论链路(root): 双 socket / FIFO 投稿 / 审核 / 嵌套回复 / 会话快照
sudo sh ~/termblog/tests/verify-comments.sh

# 统计链路(root): statd socket/数据权限 / 会话 /proc/.../stat 快照
sudo sh ~/termblog/tests/verify-stats.sh

# ── 公网访问: 阿里云控制台 → ECS → 安全组 → 入方向放行 TCP 80 / 443 / 22 / 2222 ──
# (80=ACME/HTTPS 跳转, 443=HTTPS, 22=termblog-ssh, 2222=系统 sshd 管理)
# (系统内无 ipfw/pf, 只差安全组这一道)
# 浏览器: http://<公网IP>   访客终端: ssh blog@<公网IP>   管理: ssh -p 2222 yzs@<公网IP>
```

## 阶段 6.5 —— mac_portacl: www 直绑特权端口 80/443(web)与 22(termblog-ssh)

> 本次实际采用: 首发部署前直接配置 80+443+22, 不走 8080/2222 过渡。
> 硬性顺序: 6.5.1 与 6.6 的 sshd 迁移都必须早于 deploy.sh 启动 termblog-web/ssh。

www 用户直绑特权端口的正路。两个坑(均在 15.0 上对着 man page 核实过):
① 规则是 4 段格式 `idtype:id:protocol:port`, id 只认数字 UID(www=80),
   网上流传的 `www:tcp:80` 用户名写法静默不生效;
② portacl 管不到保留段(net.inet.ip.portrange.reservedlow/high, 默认 0-1023)
   内的端口 —— 目标端口必须先脱离保留段, 规则才生效(man page 原文明示)。
   80、443 和 22 都要绑 → reservedhigh 必须降到 **21**(只降到 79 不够:
   22 仍在保留段内, portacl 对 22 无效 —— 容易踩)。

设计上 portacl 是"接管者"而非"补丁": 加载后 1-1023 对非 root 默认全拒,
再按规则放行。最终安全态势 = 原状 + 仅"uid 80 可绑 tcp 80/443/22"三条;
root 经 suser_exempt(默认 1)照旧豁免(sshd 迁 2222 后是非特权端口, 无需豁免)。

```sh
# ── 6.5.1 内核侧(root, 免重启; 顺序有讲究: 任何时刻都不比现状更宽松) ──
sudo kldload mac_portacl
sudo sysctl security.mac.portacl.rules=uid:80:tcp:80,uid:80:tcp:443,uid:80:tcp:22
sudo sysctl net.inet.ip.portrange.reservedhigh=21
sysctl security.mac.portacl.suser_exempt          # 确认 root 豁免 = 1

# ── 6.5.2 持久化(重启后仍生效) ──
echo 'mac_portacl_load="YES"' | sudo tee -a /boot/loader.conf
# 机器上已有本节旧两行(80 单端口 + reservedhigh=79)时这样原位替换;
# 全新机器可直接 printf 追加同样的两行:
sudo sed -i.bak \
    -e 's|^security.mac.portacl.rules=.*|security.mac.portacl.rules=uid:80:tcp:80,uid:80:tcp:443,uid:80:tcp:22|' \
    -e 's|^net.inet.ip.portrange.reservedhigh=.*|net.inet.ip.portrange.reservedhigh=21|' /etc/sysctl.conf
# rules 是 mac_portacl 唯一不能写成 loader tunable 的变量, 只能进 sysctl.conf;
# 注意 rules 是"整表替换"语义 —— 以后增删端口要改这一行, 不能追加第二行
# (两行 rules 的话后行覆盖前行, 前面放行的端口会静默失效)。

# ── 6.5.3 应用侧(改仓库这份 etc/termblog.toml) ──
#   [web] listen       = "0.0.0.0:80"             # ACME HTTP-01 + 308
#   [web] site_url     = "https://www.yizishun.com"
#   [web.tls] enabled  = true
#   [web.tls] listen   = "0.0.0.0:443"
#   [web.tls] domains  = ["www.yizishun.com"]
#   [ssh] listen   = "0.0.0.0:22"
sudo install -m 644 ~/termblog/etc/termblog.toml /usr/local/etc/termblog.toml
sudo sh ~/termblog/deploy-scripts/deploy.sh
# 安全组放行 80 / 443 / 22 / 2222。verify-m5 默认读取 site_url 验证 HTTPS。
```

前提: 域名须已 ICP 备案(大陆 ECS 的 80/443 会拦未备案域名, 与本机制无关;
SSH 22 不受 ICP 影响)。termblog-web 原生使用 rustls + rustls-acme：80 提供
HTTP-01 并把其他请求 308 到 HTTPS，443 提供 HTTPS/WSS；证书账户、私钥与
续期状态缓存在 `/var/db/termblog/acme`，无需 nginx/certbot，真实访客 IP 也
天然保留。

## 阶段 6.6 —— 系统 sshd 迁 2222, 把 22 让给 termblog-ssh

访客敲 `ssh blog@www.yizishun.com` 免记端口; 管理入口挪到 2222。要点:
- 先在阿里云安全组放行 2222 再动 sshd(否则新连接进不来);
- 双端口过渡, 自检 2222 真在监听后才摘 22(已建立的会话不受 sshd 重启影响,
  但切换完成前别关当前终端; 最后兜底还有阿里云控制台 VNC);
- Port 行通常不受 cloud-init 影响; 若重启后失效, 先查 sshd_config 是否被改写。

```sh
# (前置: 安全组已放行 TCP 2222)
sudo sh -c 'grep -q "^Port " /etc/ssh/sshd_config || printf "Port 22\nPort 2222\n" >> /etc/ssh/sshd_config'
sudo service sshd restart
sleep 1
if sockstat -l | grep -q ':2222 '; then
    sudo sed -i.bak -e '/^Port 22$/d' /etc/ssh/sshd_config
    sudo service sshd restart
    echo "== sshd 已只在 2222, 22 已让出 =="
else
    echo "!! 2222 没监听, Port 22 保留未动 —— 停下排查(sshd_config 是否已有别的 Port 行)"
fi
sockstat -l | grep -E ':(22|2222) '    # 预期: 只剩 sshd 的 *:2222(ipv4+ipv6 两行)
# www 自测占 22(预期 exit=124; "Address already in use" = sshd 还占着):
sudo -u www timeout 3 nc -l 22; echo "exit=$?"
# 客户端侧: 以后管理用 ssh -p 2222 yzs@<ip>, 顺手更新本机 ~/.ssh/config
```

22 直接暴露公网 → termblog-ssh 会持续收到扫描器噪音。就代码实测
(crates/servers/ssh): 只接受用户名 blog, 认证失败直接 reject, 会话在 shell
请求阶段才创建 —— 垃圾连接不建 jail, 只耗日志(/var/log/termblog-ssh.log)与
少量 CPU; 但 max_total=16 的会话位可能被占满, 观察: 该日志与 `ls /jails`。
真被骚扰再考虑 pf 限速或退回 2222, 不必预先过度设计。

## 阶段 7 —— 日常运维

```sh
# ── 以后只改文章(新增/修改 jailtpl/content 下的 .md 后): 零停机发布 ──
# 全程不停服、不杀会话: 静态镜像立即换新, 模板旁路重建后换名上场,
# 旧会话继续用旧模板, 全部退出后旧模板自动回收。
cd ~/termblog && make content
# 等价于: deploy.sh --static-only + build-template.sh --replace

# ── 月度 scrub: ZFS 校验和主动巡检(两层栈里唯一能发现静默损坏的手段) ──
sudo zpool scrub zroot
zpool status zroot          # 进度看 "scrub:" 行, 完成后 errors: 0
# 可选自动化(root crontab: sudo crontab -e):
#   0 3 1 * * /sbin/zpool scrub zroot

# ── 容量水位(数据盘: 池 10G 封顶; 文件式另见下) ────────────────────────
zfs list -o used zroot
zpool list zroot                  # 池整体占用(SIZE/USED), 数据盘看这里就够
df -h /                           # UFS 侧余量(target/ 编译产物在这, 不能写满)
# 文件式方案才需要: ls -lh /usr/local/zfs/zroot.img(backing file 只增不缩)

# ── 数据盘扩容(将来不够时): 控制台在线扩容后 ─────────────────────────
# sudo zpool online -e zroot vtbd1

# ── 内存压力观察(ARC 封顶 512M 之后) ─────────────────────────────────
sysctl vfs.zfs.arc_summary | head -20
```

## 故障速查

| 症状 | 首查 |
| --- | --- |
| ssh 报 REMOTE HOST IDENTIFICATION HAS CHANGED | cloud-init 每次开机重生成主机密钥 → 见阶段 1.5; 修过后仍报则 `ssh-keygen -R freebsd-server` |
| deploy.sh 卡在 "kern.racct.enable is not yet in effect" | 阶段 0 的 reboot 没做 / loader.conf 行写成带引号 |
| zfs: no pools available(重启后) | `zpool import -a` 手动导入一次并检查 zfs_enable / cachefile |
| npm: not found | `sudo pkg install -y npm-node24`(node24 包不捆绑 npm) |
| cargo 编译被 Killed | 内存不足 → `CARGO_BUILD_JOBS=1 cargo build --release` |
| cargo 长时间卡在 Updating crates.io index | 国内直连 crates.io 慢 → 阶段 3.1 的 rsproxy.cn 镜像(实测 15 分钟 → 30 秒) |
| 绑 80/443/22 报 Permission denied(portacl 已配) | 规则误写用户名(必须使用数字 UID，并包含 `uid:80:tcp:80,uid:80:tcp:443,uid:80:tcp:22`)/ reservedhigh 没降到 21/ 模块没加载(`kldstat \| grep portacl`) |
| 重启后 80/443/22 又绑不上 | sysctl.conf 里 rules 被追加成了第二行(整表替换, 后行覆盖前行)/ loader.conf 缺 mac_portacl_load |
| HTTPS 一直拿不到证书 | DNS 未指向本机/安全组未同时开放 80 和 443/大陆 ECS 域名未备案；查 `/var/log/termblog-web.log` 的 ACME error |
| ssh -p 2222 连不上(管理入口) | 安全组没放行 2222 / sshd_config 缺 Port 2222 行 / cloud-init 重写了 sshd_config |
| build-template.sh 下载 16.0-CURRENT | 必须先做阶段 4.1 预下载(或显式传 15.0 URL 参数) |
| 公网不通但本机 fetch 通 | 阿里云安全组没放行 80/443/22/2222 中对应的入口端口 |
| 会话开不出, 查 jaild 日志 | `tail -50 /var/log/jaild.log` |

## 升级路径(备忘; 仅方案 B 起步者需要, 方案 A 天然就是真 vdev)

文件式池 → 数据盘/分区式池, 业务无感迁移:

```sh
# 新盘(如 vtbd1)上建同构池, 一条管道搬过去, 停服窗口 = 切换瞬间
sudo zpool create -o ashift=12 -O mountpoint=none -O compression=lz4 zroot2 /dev/vtbd1
sudo zfs snapshot -r zroot@migrate
sudo zfs send -R zroot@migrate | sudo zfs receive -F zroot2
# 停 jaild → zpool export zroot → 改名 zroot2 为 zroot → 起 jaild
```
