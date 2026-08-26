# termblog —— 编译 / 运行 / 服务管理
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
#   TERMBLOG_LISTEN       web, 默认 127.0.0.1:8080
#   TERMBLOG_SSH_LISTEN   ssh, 默认 0.0.0.0:22(特权端口; 开发建议 127.0.0.1:2222)

BIN_WEB := target/release/termblog-web
BIN_SSH := target/release/termblog-ssh
PID_WEB := .termblog-web.pid
PID_SSH := .termblog-ssh.pid
LOG_WEB := termblog-web.log
LOG_SSH := termblog-ssh.log
URL_WEB := http://127.0.0.1:8080
URL_SSH := ssh://0.0.0.0:22

.PHONY: all build build-frontend \
        run run-ssh \
        start start-ssh stop stop-ssh restart restart-ssh \
        status status-ssh logs logs-ssh clean

all: build

# ── 前端: 依赖装过即跳过; 产物在 frontend/dist(web 的 ServeDir 直接读) ──
frontend/node_modules: frontend/package.json
	cd frontend && npm install

build-frontend: frontend/node_modules
	cd frontend && npm run build

# ── 构建: 一次产出 web + ssh 两个二进制 ──
build: build-frontend
	cargo build --release

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
