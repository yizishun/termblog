import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { ImageAddon } from "@xterm/addon-image";
import { WebLinksAddon } from "@xterm/addon-web-links";
import "@xterm/xterm/css/xterm.css";
import { installOsc } from "./osc";

// ── proto 帧编解码(格式与 crates/proto 完全一致: u8 kind | u32 len | payload) ──
const T_OPEN = 0x01;
const T_DATA = 0x02;
const T_RESIZE = 0x03;
const T_OPENED = 0x04;
const T_CLOSED = 0x05;
const T_REPLAY_END = 0x06;

const textEnc = new TextEncoder();
const textDec = new TextDecoder();

function frame(kind: number, payload: Uint8Array): ArrayBuffer {
  const b = new Uint8Array(5 + payload.length);
  const v = new DataView(b.buffer);
  v.setUint8(0, kind);
  v.setUint32(1, payload.length);
  b.set(payload, 5);
  return b.buffer;
}
const jsonFrame = (kind: number, v: unknown) => frame(kind, textEnc.encode(JSON.stringify(v)));

// ── 落地页接管状态机(M5): 镜像页带 source + route meta, 普通页面无 ──
// 时序常量: 接管兜底 5000ms; 自动命令延迟 200ms; 淡出 450ms(400ms 过渡 + 余量)。
// 镜像页 Open 带 fresh=1: 每次访问都是全新会话(网关回收闲置旧会话), 落地永远
// 是干净 shell → 自动敲 blog; 没有 attach 回镜像页的状态判断。attach 只发生在
// 首页刷新(终端浏览保留历史)。
const ARTICLE_SOURCE =
  document.querySelector<HTMLMetaElement>('meta[name="termblog-source"]')?.content?.trim() ?? "";
const ARTICLE_ROUTE =
  document.querySelector<HTMLMetaElement>('meta[name="termblog-route"]')?.content?.trim() ?? "";
const validSource = /^[a-z0-9-]+(?:\/[a-z0-9-]+)*\.md$/.test(ARTICLE_SOURCE);
const validRoute = /^\/[a-z0-9-]+(?:\/[a-z0-9-]+)*\/$/.test(ARTICLE_ROUTE);
const onMirror = validSource && validRoute;
if ((ARTICLE_SOURCE || ARTICLE_ROUTE) && !onMirror) {
  console.error("termblog article meta is incomplete or invalid");
}
const WANT = onMirror ? ARTICLE_ROUTE : "";
let takeoverDone = false; // 终端是否已完成首次接管
let autoArmed = false; // 新会话落地页: 等首帧数据后补敲 blog 命令
let autoSent = false; // 自动命令已发出(一次性)

// ── 终端内链接统一点击行为: 页面内确认后打开新标签 ──
// 不依赖 window.confirm 或 <a target="_blank"> 的默认动作：Safari 可能在链接回调中
// 静默压制它们，即使事件链和 xterm activate 都已正常触发。<dialog> 不受该策略影响；
// 用户点击“打开”按钮会产生一次新的真实手势，可同步创建新标签页。
let linkDialog: HTMLDialogElement | undefined;

function ensureLinkDialog(): HTMLDialogElement {
  if (linkDialog) return linkDialog;

  const dialog = document.createElement("dialog");
  dialog.className = "link-confirm";
  dialog.setAttribute("aria-labelledby", "link-confirm-title");
  dialog.innerHTML = `
    <form method="dialog">
      <h2 id="link-confirm-title">Open this link?</h2>
      <p>Link will open in a new tab:</p>
      <code class="link-confirm-url"></code>
      <div class="link-confirm-actions">
        <button value="cancel">Cancel</button>
        <button type="button" class="link-confirm-open">Open</button>
      </div>
      <p class="link-confirm-error" role="alert" hidden></p>
    </form>`;
  dialog.querySelector<HTMLButtonElement>(".link-confirm-open")!.addEventListener("click", (event) => {
    const button = event.currentTarget as HTMLButtonElement;
    const target = button.dataset.href;
    if (!target) return;

    // 必须在这次真实 click 的同步调用栈内创建窗口, Safari 才认可用户手势。
    const newWindow = window.open();
    if (!newWindow) {
      const error = dialog.querySelector<HTMLElement>(".link-confirm-error")!;
      error.textContent = "Unable to open link. Please copy the URL above.";
      error.hidden = false;
      return;
    }
    try { newWindow.opener = null; } catch { /* Safari 可能拒绝赋值, 忽略即可 */ }
    newWindow.location.href = target;
    dialog.close();
  });
  document.body.append(dialog);
  linkDialog = dialog;
  return dialog;
}

function confirmOpenLink(uri: string) {
  let url: URL;
  try {
    url = new URL(uri, window.location.href);
  } catch {
    return;
  }
  if (!["http:", "https:"].includes(url.protocol)) return;

  const dialog = ensureLinkDialog();
  dialog.querySelector<HTMLElement>(".link-confirm-url")!.textContent = url.href;
  dialog.querySelector<HTMLButtonElement>(".link-confirm-open")!.dataset.href = url.href;
  dialog.querySelector<HTMLElement>(".link-confirm-error")!.hidden = true;
  if (!dialog.open) dialog.showModal();
}

// ── 终端(视觉参数配合 style.css 的浅色 CRT 主题) ──
const term = new Terminal({
  fontFamily: '"Maple Mono", monospace',
  fontSize: 15,
  cursorBlink: true,
  scrollback: 2000,
  theme: {
    background: "#fafafa",
    foreground: "#2e3338",
    cursor: "#2e3338",
    // xterm 默认是 30% 白色选区，在浅色背景上几乎不可见。
    selectionBackground: "rgba(46, 51, 56, 0.20)",
    selectionInactiveBackground: "rgba(46, 51, 56, 0.12)",
  },
  linkHandler: {
    activate: (_event, uri) => confirmOpenLink(uri),
  },
});
// OSC 0/2 来自不可信 PTY：只允许短、非控制字符标题。document.title 按文本
// 赋值，不解释 HTML；zsh 用它报告当前目录，blog 阅读期间用文章标题覆盖。
const SAFE_TERMINAL_TITLE = /^[^\u0000-\u001f\u007f-\u009f]{1,512}$/u;
term.onTitleChange((title) => {
  if (SAFE_TERMINAL_TITLE.test(title)) document.title = title;
});
const fit = new FitAddon();
term.loadAddon(fit);
// 图片二期: iTerm2 Inline Images Protocol(像素内嵌显示)。选项名是
// iipSizeLimit(不是 sizeLimit), 单位字节; 1 MiB 与 jail 侧运行时硬上限
// (§5.6.4)呼应(> 单图 683 KiB 上限, 留余量; 同时小于 parser 保护)。
// sixelSupport 关闭(协议层只选 IIP)。xterm.js 6.0.0 默认 canvas renderer,
// IIP 可用; 若未来启用 WebglAddon, 必须重新验证图像渲染。
term.loadAddon(new ImageAddon({ sixelSupport: false, iipSizeLimit: 1 * 1024 * 1024 }));
// 裸 URL 可点(图片占位框里的链接等); 回调与 OSC8 linkHandler 共用同一确认逻辑
term.loadAddon(new WebLinksAddon((_event, uri) => confirmOpenLink(uri)));

const TOKEN_KEY = "termblog.attach_token";
let ws: WebSocket | undefined;

// ── 回放窗口: attach 会话在 Opened(attached=true)与 ReplayEnd 之间禁用 xterm stdin ──
// 服务端 attach 时会把原始 scrollback(128 KiB)原样回放给全新 xterm 实例。
// 历史里可能有终端查询(如 vim 启动时的 CSI 6n / DA / OSC 10;?): xterm.js 会把
// 它们当实时指令执行并生成回答, 而 onData 会把回答上行进 PTY, 落进 zsh 编辑行
// 变成乱码。disableStdin 能让 triggerDataEvent 整体静默(渲染不受影响), 等
// ReplayEnd 且所有回放 write 的解析完成后才恢复, 实时输出里的查询不受影响。
// 计数口径只含 ReplayEnd 之前收到的帧: 协议保证回放块全部先于 ReplayEnd 按序
// 送达, 之后的实时帧不进计数, 否则它们会把禁用窗口顺延(窗口内的实时查询
// 与用户按键会被吞掉)。fresh 会话没有回放, 不进这个窗口(不依赖 ReplayEnd 帧)。
let replayPending = 0; // 尚未解析完的回放 write 数(仅统计 ReplayEnd 之前的帧)
let replayEndSeen = false; // 是否已收到 ReplayEnd 帧

function showSessionRestore(show: boolean) {
  const host = document.getElementById("term-host");
  const status = document.getElementById("session-restore");
  host?.classList.toggle("restoring", show);
  if (status) status.hidden = !show;
}

function finishReplayIfReady() {
  if (replayEndSeen && replayPending <= 0) {
    term.options.disableStdin = false;
    showSessionRestore(false);
  }
}

function send(buf: ArrayBuffer) {
  if (ws?.readyState === WebSocket.OPEN) ws.send(buf);
}
const sendResize = () => send(jsonFrame(T_RESIZE, { cols: term.cols, rows: term.rows }));

function refit() {
  fit.fit();
  sendResize();
}

// ── 等 Web 字体真正就绪后再 open + fit ──
// 陷阱(清空缓存后的冷加载尤其如此): document.fonts.ready 在"没有任何元素
// 使用该字体"时会立即 resolve; document.fonts.load 只保证字体数据就绪、
// 不保证布局已经用新字体量出行高。若在字体就绪前就 open, renderer 首次
// 测量会用 fallback(≈14px) 算出错误行数(52 行), Maple Mono 换入后是
// 20px/行 → 52×20=1040px 撑破 730px 容器, 提示符被 overflow:hidden 裁掉。
// 所以先加载字体、强制一次 reflow, 再 open + fit, renderer 首次测量即为
// 20px/行, 行数一次到位, 不再依赖 loadingdone 事后纠正(那步跨浏览器不可靠)。
async function boot() {
  await Promise.all([
    document.fonts.load('15px "Maple Mono"'),
    document.fonts.load('italic 15px "Maple Mono"'),
  ]);
  await document.fonts.ready;
  void document.body.offsetHeight; // 强制同步布局, 让已加载的字体度量生效

  term.open(document.getElementById("term-screen")!);
  installOsc(term, handleOscUrl);
  refit();
  document.getElementById("term-host")!.classList.add("ready");
  connect();
}
boot();

// 双保险: 后续仍有字体换入(如 italic 延迟加载)就重 fit 一次
document.fonts.addEventListener("loadingdone", () => {
  void document.body.offsetHeight;
  refit();
});

// ── 连接(attach token: 刷新页面回原会话, shell 状态不丢) ──
function connect() {
  const wsProto = location.protocol === "https:" ? "wss" : "ws";
  const socket = new WebSocket(`${wsProto}://${location.host}/ws`);
  ws = socket;
  socket.binaryType = "arraybuffer";

  socket.onopen = () => {
    // fresh: 镜像页一律开新会话(旧会话由网关回收, 无需判断旧 shell 状态);
    // 首页不带 fresh, 带 token 尝试 attach 回原会话(终端浏览保留历史)
    socket.send(jsonFrame(T_OPEN, {
      cols: term.cols,
      rows: term.rows,
      attach_token: sessionStorage.getItem(TOKEN_KEY),
      fresh: onMirror,
      // 能力通告: 本终端链路支持 iTerm2 Inline Images Protocol(jaild 白名单
      // 过滤后映射为 jail 里的 TERMBLOG_IMG, blog 据此走 TUI 阅读器)
      caps: ["img-iterm2"],
    }));

    // 键入原样进 WS(不做行缓冲/命令拦截, 否则会与 zsh ZLE、vim 打架)
    term.onData((d) => send(frame(T_DATA, textEnc.encode(d))));

    window.addEventListener("resize", () => {
      fit.fit();
      sendResize();
    });
  };

  socket.onmessage = (ev: MessageEvent<ArrayBuffer>) => {
    const b = new Uint8Array(ev.data);
    const payload = b.subarray(5);
    switch (b[0]) {
      case T_DATA:
        // 只统计 ReplayEnd 之前收到的帧(即回放块): write 的 callback 在该块被
        // 解析器完整消费后触发, 保证 ReplayEnd 到达时所有回放块都已解析完、
        // 不会有查询回答漏出。ReplayEnd 之后的实时帧不计数, 不延长禁用窗口。
        if (!replayEndSeen) {
          replayPending += 1;
          term.write(payload, () => {
            replayPending -= 1;
            finishReplayIfReady();
          }); // 裸 PTY 字节, ANSI 原样上屏
        } else {
          term.write(payload);
        }
        if (autoArmed && !autoSent) {
          // 自动命令推迟到 zsh 已开始输出(MOTD/提示符已在画)之后才发:
          // 早发的字节会被 tty 驱动立刻回显在屏幕顶部(那时提示符还不存在),
          // 造成第一行孤儿回显 + 与提示符绘制交错的乱象。
          autoSent = true;
          setTimeout(() => {
            send(frame(T_DATA, textEnc.encode(`blog -- "$HOME/${ARTICLE_SOURCE}"\r`)));
          }, 200);
        }
        break;
      case T_OPENED: {
        const opened = JSON.parse(textDec.decode(payload));
        // 只有 attach 才有 scrollback 回放: 仅此时禁用 stdin, 回放里的终端查询
        // 不能再触发 xterm.js 回答上行(污染 shell 编辑行); ReplayEnd 会恢复。
        // fresh 会话(镜像页/新 token)无回放, stdin 从一开始就可用, 不依赖新帧。
        if (opened.attached) {
          term.options.disableStdin = true;
          showSessionRestore(true);
        } else {
          showSessionRestore(false);
        }
        replayPending = 0;
        replayEndSeen = false;
        // 恢复的会话: 屏幕清干净, 由 SIGWINCH 触发前台程序重绘; 新会话本来就是新画面
        term.reset();
        sessionStorage.setItem(TOKEN_KEY, opened.attach_token);
        // 横幅只在普通页面打(仅首页会 attach)
        if (opened.attached && !onMirror) term.write("\x1b[90m[Previous session restored]\x1b[0m\r\n");
        sendResize(); // attach 后服务端 winsize 可能还是旧值, 主动同步一次
        // 镜像页 fresh=1 必然是新会话: 发送推迟到首帧 T_DATA 之后(见上),
        // 避免回显抢在提示符之前。
        if (onMirror) autoArmed = true;
        armFallback(); // 5s 兜底(§9.2 末)
        break;
      }
      case T_REPLAY_END:
        replayEndSeen = true;
        finishReplayIfReady();
        break;
      case T_CLOSED: {
        // 兜底: 无论何种原因会话结束, 都要恢复 stdin, 防止协议异常时终端变砖
        term.options.disableStdin = false;
        showSessionRestore(false);
        let reason = textDec.decode(payload);
        try {
          reason = JSON.parse(reason).reason;
        } catch { /* 非 JSON 就直接显示 */ }
        term.write(`\r\n\x1b[31m[Session closed: ${reason}]\x1b[0m\r\n`);
        sessionStorage.removeItem(TOKEN_KEY);
        if (onMirror && !takeoverDone) revealStaticFallback();
        break;
      }
    }
  };

  socket.onclose = () => {
    term.options.disableStdin = false; // 断线兜底, 同上
    showSessionRestore(false);
    term.write("\r\n\x1b[90m[Connection closed, refresh page to reconnect]\x1b[0m\r\n");
    if (onMirror && !takeoverDone) revealStaticFallback();
  };
}

// ── 落地页接管: 等待层淡出交给终端 / 降级露出静态正文 ──
// 等待层(#mirror-cover)从首屏就盖住静态正文(正文不闪现, 由内容编译期写入
// 镜像页 HTML); 终端就绪的信号与 §9.2 一致 —— 本会话首条 OSC 7777。

function handleOscUrl(path: string) {
  // 地址栏: 普通页面与已接管页面任何合法路径照常同步(终端里读别的文章
  // 地址栏跟着走); 镜像页接管前只同步本页路径 —— 接管前只会出现自动命令
  // 的本页 OSC(镜像页是全新会话), 其它路径一律不劫持本页地址栏。
  if (!(onMirror && !takeoverDone) || path === WANT) {
    try {
      history.replaceState(null, "", path);
    } catch {
      /* 某些环境(如 about:blank)会抛 SecurityError, 忽略 */
    }
  }
  // 接管: 本篇文章的 OSC 到达 = 内容已在画。blog 先发 OSC、less 随后才画
  // 第一帧 —— 延迟 200ms 让内容画到等待层下面, 淡出时透出已画好的终端。
  if (onMirror && !takeoverDone && path === WANT) setTimeout(takeOver, 200);
}

function takeOver() {
  if (takeoverDone) return;
  takeoverDone = true;
  // 先藏正文再淡等待层：淡出过程透出的是终端，不是静态正文。
  document.getElementById("static-view")?.style.setProperty("display", "none");
  const cover = document.getElementById("mirror-cover");
  cover?.classList.add("fade-out");
  setTimeout(() => cover?.remove(), 450);
}

function revealStaticFallback() {
  // 会话死了 / WS 断了 / 5s 无 OSC：撤掉等待层，保留可读的静态正文。
  // 若 OSC 稍后到达，takeOver 仍会完成自动接管。
  document.getElementById("static-view")?.style.removeProperty("display");
  const cover = document.getElementById("mirror-cover");
  cover?.classList.add("fade-out");
  setTimeout(() => cover?.remove(), 450);
}

function armFallback() {
  setTimeout(() => {
    if (!takeoverDone) revealStaticFallback();
  }, 5000);
}
