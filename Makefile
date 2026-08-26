# termblog —— 编译 / 启动 / 停止
# 用法: make build | make run(前台调试) | make start/stop/restart/status(后台服务)

BIN       := target/release/termblog-web
PID_FILE  := .termblog.pid
LOG_FILE  := termblog.log
ADDR      := 127.0.0.1:8080

.PHONY: all build build-frontend run start stop restart status logs clean dev dev-web

all: build

# ── 前端: 依赖装过就跳过, 构建产物在 frontend/dist(后端 ServeDir 直接读它) ──
frontend/node_modules: frontend/package.json
	cd frontend && npm install

build-frontend: frontend/node_modules
	cd frontend && npm run build

# ── 后端 ──
build: build-frontend
	cargo build --release

# ── 前台运行(Ctrl-C 停止, 调试用) ──
run: build
	./$(BIN)

# ── 后台启动(幂等: 已在运行则不重复启动) ──
start: build
	@if [ -f $(PID_FILE) ] && kill -0 `cat $(PID_FILE)` 2>/dev/null; then \
		echo "termblog-web 已在运行 (pid `cat $(PID_FILE)`)  http://$(ADDR)"; \
	else \
		nohup ./$(BIN) > $(LOG_FILE) 2>&1 & echo $$! > $(PID_FILE); \
		sleep 0.5; \
		kill -0 `cat $(PID_FILE)` 2>/dev/null \
			&& echo "已启动 (pid `cat $(PID_FILE)`)  http://$(ADDR)" \
			|| { echo "启动失败, 日志:"; tail -20 $(LOG_FILE); exit 1; }; \
	fi

stop:
	@if [ -f $(PID_FILE) ] && kill -0 `cat $(PID_FILE)` 2>/dev/null; then \
		kill `cat $(PID_FILE)` && echo "已停止 (pid `cat $(PID_FILE)`)"; \
		rm -f $(PID_FILE); \
	else \
		echo "未在运行"; rm -f $(PID_FILE); \
	fi

restart: stop start

status:
	@if [ -f $(PID_FILE) ] && kill -0 `cat $(PID_FILE)` 2>/dev/null; then \
		echo "运行中 (pid `cat $(PID_FILE)`)  http://$(ADDR)"; \
	else \
		echo "未在运行"; \
	fi

logs:
	tail -f $(LOG_FILE)

# ── 开发模式: 两个终端分别跑 make dev(后端 debug) 和 make dev-web(vite 热更新, 5173 端口已代理 /ws) ──
dev:
	cargo run -p termblog-web

dev-web:
	cd frontend && npm run dev

clean:
	cargo clean
	rm -f $(PID_FILE) $(LOG_FILE)
	rm -rf frontend/dist
