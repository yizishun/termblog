# termblog —— 编译 / 运行 / 服务管理
# bmake(FreeBSD 默认 make)风格, 直接 `make <目标>` 即可, 无需 gmake。
#
# 部署目标(需要 root, sudo 已内嵌, 会提示输入密码):
#   make tpl      构建 jail 模板(build-template.sh; 加 --replace 零停机换面)
#   make deploy   全量生产部署(deploy.sh; 需模板已构建)
#   make content  只改文章的部署: 静态发布 + 模板零停机换面
#
# 开发期目标(不带后缀的操作 web, 带 -ssh 后缀的对应操作 ssh):
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
BIN_CONTENT := target/release/content-build
PID_WEB := .termblog-web.pid
PID_SSH := .termblog-ssh.pid
LOG_WEB := termblog-web.log
LOG_SSH := termblog-ssh.log
HOSTNAME != hostname
URL_WEB := http://$(HOSTNAME):8080
URL_SSH := ssh://0.0.0.0:2222

.PHONY: all build build-frontend build-content \
        tpl deploy content verify-comments verify-content-paths \
        run run-ssh \
        start start-ssh stop stop-ssh restart restart-ssh \
        status status-ssh logs logs-ssh clean

all: build

# ── 前端: 依赖装过即跳过; 产物在 frontend/dist(web 的 ServeDir 直接读) ──
frontend/node_modules: frontend/package.json
	cd frontend && npm install

build-frontend: frontend/node_modules
	cd frontend && npm run build

# ── 内容编译器: content 内可见 md → route 对应 HTML + ANSI 预渲染 ──
# 注意顺序: 必须在 vite build 之后跑(产物写入 dist 且需读 assets/index-*.js)
build-content:
	cargo build --release -p content-build
	$(BIN_CONTENT) --content jailtpl/content --dist frontend/dist

# ── 构建: 一次产出全部二进制(含 jailbin)+ 前端 + 内容镜像 ──
build: build-frontend
	cargo build --release
	$(BIN_CONTENT) --content jailtpl/content --dist frontend/dist

# ── 部署入口(需要 root): 薄入口, sudo 已内嵌; 逻辑在 deploy-scripts/ ──
tpl:
	sudo sh deploy-scripts/build-template.sh

deploy:
	sudo sh deploy-scripts/deploy.sh

# 只改文章的部署: 静态发布 + 模板零停机换面(全程不停服、不杀会话)
content:
	sudo sh -c 'sh deploy-scripts/deploy.sh --static-only && sh deploy-scripts/build-template.sh --replace'

# ── 前台运行(Ctrl-C 停止) ──
run: build
	./$(BIN_WEB)

run-ssh: build
	./$(BIN_SSH)

# ── 后台服务: web/ssh 各持独立 pid+log(幂等) ──
start: build
	@if [ -f $(PID_WEB) ] && kill -0 $$(cat $(PID_WEB)) 2>/dev/null; then \
		echo "termblog-web 已在运行 (pid $$(cat $(PID_WEB)))  $(URL_WEB)"; \
	else \
		nohup ./$(BIN_WEB) > $(LOG_WEB) 2>&1 & echo $$! > $(PID_WEB); \
		sleep 0.5; \
		if kill -0 $$(cat $(PID_WEB)) 2>/dev/null; then \
			echo "termblog-web 已启动 (pid $$(cat $(PID_WEB)))  $(URL_WEB)"; \
		else \
			echo "termblog-web 启动失败, 日志:"; tail -20 $(LOG_WEB); exit 1; \
		fi; \
	fi

start-ssh: build
	@if [ -f $(PID_SSH) ] && kill -0 $$(cat $(PID_SSH)) 2>/dev/null; then \
		echo "termblog-ssh 已在运行 (pid $$(cat $(PID_SSH)))  $(URL_SSH)"; \
	else \
		nohup ./$(BIN_SSH) > $(LOG_SSH) 2>&1 & echo $$! > $(PID_SSH); \
		sleep 0.5; \
		if kill -0 $$(cat $(PID_SSH)) 2>/dev/null; then \
			echo "termblog-ssh 已启动 (pid $$(cat $(PID_SSH)))  $(URL_SSH)"; \
		else \
			echo "termblog-ssh 启动失败, 日志:"; tail -20 $(LOG_SSH); exit 1; \
		fi; \
	fi

stop:
	@if [ -f $(PID_WEB) ] && kill -0 $$(cat $(PID_WEB)) 2>/dev/null; then \
		kill $$(cat $(PID_WEB)) && echo "已停止 termblog-web (pid $$(cat $(PID_WEB)))"; \
	else \
		echo "termblog-web 未在运行"; \
	fi; \
	rm -f $(PID_WEB)

stop-ssh:
	@if [ -f $(PID_SSH) ] && kill -0 $$(cat $(PID_SSH)) 2>/dev/null; then \
		kill $$(cat $(PID_SSH)) && echo "已停止 termblog-ssh (pid $$(cat $(PID_SSH)))"; \
	else \
		echo "termblog-ssh 未在运行"; \
	fi; \
	rm -f $(PID_SSH)

restart: stop start

restart-ssh: stop-ssh start-ssh

status:
	@if [ -f $(PID_WEB) ] && kill -0 $$(cat $(PID_WEB)) 2>/dev/null; then \
		echo "termblog-web 运行中 (pid $$(cat $(PID_WEB)))  $(URL_WEB)"; \
	else \
		echo "termblog-web 未在运行"; \
	fi

status-ssh:
	@if [ -f $(PID_SSH) ] && kill -0 $$(cat $(PID_SSH)) 2>/dev/null; then \
		echo "termblog-ssh 运行中 (pid $$(cat $(PID_SSH)))  $(URL_SSH)"; \
	else \
		echo "termblog-ssh 未在运行"; \
	fi

logs:
	tail -f $(LOG_WEB)

logs-ssh:
	tail -f $(LOG_SSH)

clean:
	cargo clean
	rm -f $(PID_WEB) $(PID_SSH) $(LOG_WEB) $(LOG_SSH)
	rm -rf frontend/dist
	rm -rf jailtpl/content/.rendered jailtpl/content/.rendered-assets
	rm -f jailtpl/content/.comment-targets.tsv jailtpl/content/.web-outputs.tsv

verify-comments:
	sudo sh tests/verify-comments.sh

verify-content-paths:
	cargo build -p content-build
	node tests/e2e-content-paths.mjs
