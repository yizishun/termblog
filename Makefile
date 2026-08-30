# termblog —— 编译 / 运行 / 服务管理
# 注意: GNU make 语法(define/endef + $(shell)); FreeBSD 请用 gmake 调用。
#
# 约定: 不带后缀的目标操作 web, 带 -ssh 后缀的对应操作 ssh。
#   run/run-ssh            前台运行(Ctrl-C 停止, 调试用)
#   start/start-ssh        后台启动(幂等: 已在运行则不重复启动)
#   stop/stop-ssh          停止
#   restart/restart-ssh    重启
#   status/status-ssh      查看状态
#   logs/logs-ssh          跟踪日志
#
# 监听地址可用环境变量覆盖:
#   TERMBLOG_LISTEN       web, 默认 0.0.0.0:8080
#   TERMBLOG_SSH_LISTEN   ssh, 默认 0.0.0.0:2222(降权 www 跑不了特权端口 22)
#   TERMBLOG_SOCKET       jaild 的 Unix socket 路径
#   TERMBLOG_CONFIG       TOML 配置文件路径

BIN_WEB := target/release/termblog-web
BIN_SSH := target/release/termblog-ssh
BIN_JAILD := target/release/termblog-jaild
BIN_CONTENT := target/release/content-build
PID_WEB := .termblog-web.pid
PID_SSH := .termblog-ssh.pid
LOG_WEB := termblog-web.log
LOG_SSH := termblog-ssh.log
URL_WEB := http://$(shell hostname):8080
URL_SSH := ssh://0.0.0.0:2222

PREFIX ?= /usr/local
ETCDIR ?= $(PREFIX)/etc

.PHONY: all build build-frontend build-content install \
        run run-ssh \
        start start-ssh stop stop-ssh restart restart-ssh \
        status status-ssh logs logs-ssh clean

all: build

# ── 前端: 依赖装过即跳过; 产物在 frontend/dist(web 的 ServeDir 直接读) ──
frontend/node_modules: frontend/package.json
	cd frontend && npm install

build-frontend: frontend/node_modules
	cd frontend && npm run build

# ── 内容编译器: md → HTML 镜像(进 dist) + ANSI 预渲染(进 jailtpl/content/.rendered) ──
# 注意顺序: 必须在 vite build 之后跑(产物写入 dist 且需读 assets/index-*.js)
build-content:
	cargo build --release -p content-build
	$(BIN_CONTENT) --content jailtpl/content --dist frontend/dist

# ── 构建: 一次产出 web + ssh + jaild 三个二进制 + 前端 + 内容镜像 ──
build: build-frontend
	cargo build --release
	$(BIN_CONTENT) --content jailtpl/content --dist frontend/dist

# ── 部署(需要 root): 二进制 -> sbin, 前端 -> share, rc 脚本 -> etc/rc.d ──
install: build
	install -d $(DESTDIR)$(PREFIX)/sbin
	install -d $(DESTDIR)$(PREFIX)/share/termblog/frontend
	install -d $(DESTDIR)$(ETCDIR)/rc.d
	install -m 555 $(BIN_WEB) $(DESTDIR)$(PREFIX)/sbin/termblog-web
	install -m 555 $(BIN_SSH) $(DESTDIR)$(PREFIX)/sbin/termblog-ssh
	install -m 555 $(BIN_JAILD) $(DESTDIR)$(PREFIX)/sbin/jaild
	cp -R frontend/dist/. $(DESTDIR)$(PREFIX)/share/termblog/frontend/
	install -m 644 etc/termblog.toml $(DESTDIR)$(ETCDIR)/termblog.toml.sample
	install -m 555 etc/rc.d/jaild etc/rc.d/termblog $(DESTDIR)$(ETCDIR)/rc.d/
	@echo ">> 已安装到 $(DESTDIR)$(PREFIX)"
	@echo ">> 配置样例: $(ETCDIR)/termblog.toml.sample (复制为 termblog.toml 后按需修改)"
	@echo ">> 启用: sysrc jaild_enable=YES termblog_enable=YES"

# ── 前台运行(Ctrl-C 停止) ──
run: build
	./$(BIN_WEB)

run-ssh: build
	./$(BIN_SSH)

# ── 后台服务: web/ssh 各持独立 pid+log, 逻辑同一套(幂等) ──
# 参数: $(1)=pid 文件  $(2)=二进制  $(3)=日志  $(4)=服务名  $(5)=地址
define start_service
	@if [ -f $(1) ] && kill -0 $$(cat $(1)) 2>/dev/null; then \
		echo "$(4) 已在运行 (pid $$(cat $(1)))  $(5)"; \
	else \
		nohup ./$(2) > $(3) 2>&1 & echo $$! > $(1); \
		sleep 0.5; \
		if kill -0 $$(cat $(1)) 2>/dev/null; then \
			echo "$(4) 已启动 (pid $$(cat $(1)))  $(5)"; \
		else \
			echo "$(4) 启动失败, 日志:"; tail -20 $(3); exit 1; \
		fi; \
	fi
endef

define stop_service
	@if [ -f $(1) ] && kill -0 $$(cat $(1)) 2>/dev/null; then \
		kill $$(cat $(1)) && echo "已停止 $(2) (pid $$(cat $(1)))"; \
	else \
		echo "$(2) 未在运行"; \
	fi; \
	rm -f $(1)
endef

define status_service
	@if [ -f $(1) ] && kill -0 $$(cat $(1)) 2>/dev/null; then \
		echo "$(2) 运行中 (pid $$(cat $(1)))  $(3)"; \
	else \
		echo "$(2) 未在运行"; \
	fi
endef

start: build
	$(call start_service,$(PID_WEB),$(BIN_WEB),$(LOG_WEB),termblog-web,$(URL_WEB))

start-ssh: build
	$(call start_service,$(PID_SSH),$(BIN_SSH),$(LOG_SSH),termblog-ssh,$(URL_SSH))

stop:
	$(call stop_service,$(PID_WEB),termblog-web)

stop-ssh:
	$(call stop_service,$(PID_SSH),termblog-ssh)

restart: stop start

restart-ssh: stop-ssh start-ssh

status:
	$(call status_service,$(PID_WEB),termblog-web,$(URL_WEB))

status-ssh:
	$(call status_service,$(PID_SSH),termblog-ssh,$(URL_SSH))

logs:
	tail -f $(LOG_WEB)

logs-ssh:
	tail -f $(LOG_SSH)

clean:
	cargo clean
	rm -f $(PID_WEB) $(PID_SSH) $(LOG_WEB) $(LOG_SSH)
	rm -rf frontend/dist
	rm -rf jailtpl/content/.rendered
