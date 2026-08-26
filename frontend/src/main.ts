import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";

// ── proto 帧编解码(格式与 crates/proto 完全一致: u8 kind | u32 len | payload) ──
const T_OPEN = 0x01;
const T_DATA = 0x02;
const T_RESIZE = 0x03;
const T_OPENED = 0x04;
const T_CLOSED = 0x05;

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

// ── 终端(视觉参数配合 style.css 的浅色 CRT 主题) ──
const term = new Terminal({
  fontFamily: '"Maple Mono", monospace',
  fontSize: 15,
  cursorBlink: true,
  scrollback: 2000,
  theme: { background: "#fafafa", foreground: "#2e3338", cursor: "#2e3338" },
});
const fit = new FitAddon();
term.loadAddon(fit);
term.open(document.getElementById("term-screen")!);

// 等 Web 字体真正加载完再 fit + 淡入。
// 陷阱: document.fonts.ready 在"没有任何元素使用该字体"时会立即 resolve,
// 此时 fit 用 fallback 字体的行高(≈14px)算出错误行数(52 行); Maple Mono
// 实际是 20px/行(只该有 36 行), 多余的行被 overflow:hidden 裁掉,
// 提示符永远画在可见区域外。所以必须显式 fonts.load 触发加载。
function refit() {
  fit.fit();
  sendResize();
}
Promise.all([
  document.fonts.load('15px "Maple Mono"'),
  document.fonts.load('italic 15px "Maple Mono"'),
]).then(() => {
  refit();
  document.getElementById("term-host")!.classList.add("ready");
});
// 双保险: 任何字体加载完成(如 italic 后续换入)都重新 fit 一次
document.fonts.addEventListener("loadingdone", refit);

// ── 连接(attach token: 刷新页面回原会话, shell 状态不丢) ──
const TOKEN_KEY = "termblog.attach_token";

const wsProto = location.protocol === "https:" ? "wss" : "ws";
const ws = new WebSocket(`${wsProto}://${location.host}/ws`);
ws.binaryType = "arraybuffer";

function send(buf: ArrayBuffer) {
  if (ws.readyState === WebSocket.OPEN) ws.send(buf);
}
const sendResize = () => send(jsonFrame(T_RESIZE, { cols: term.cols, rows: term.rows }));

ws.onopen = () => {
  // 带上次拿到的 token 尝试恢复原会话; 没有/已失效则服务端自动开新会话
  ws.send(jsonFrame(T_OPEN, {
    cols: term.cols,
    rows: term.rows,
    attach_token: sessionStorage.getItem(TOKEN_KEY),
  }));

  // 键入原样进 WS(不做行缓冲/命令拦截, 否则会与 zsh ZLE、vim 打架)
  term.onData((d) => send(frame(T_DATA, textEnc.encode(d))));

  window.addEventListener("resize", () => {
    fit.fit();
    sendResize();
  });
};

ws.onmessage = (ev: MessageEvent<ArrayBuffer>) => {
  const b = new Uint8Array(ev.data);
  const payload = b.subarray(5);
  switch (b[0]) {
    case T_DATA:
      term.write(payload); // 裸 PTY 字节, ANSI 原样上屏
      break;
    case T_OPENED: {
      const opened = JSON.parse(textDec.decode(payload));
      // 恢复的会话: 屏幕清干净, 由 SIGWINCH 触发前台程序重绘; 新会话本来就是新画面
      term.reset();
      sessionStorage.setItem(TOKEN_KEY, opened.attach_token);
      if (opened.attached) term.write("\x1b[90m[已恢复原会话]\x1b[0m\r\n");
      sendResize(); // attach 后服务端 winsize 可能还是旧值, 主动同步一次
      break;
    }
    case T_CLOSED: {
      let reason = textDec.decode(payload);
      try {
        reason = JSON.parse(reason).reason;
      } catch { /* 非 JSON 就直接显示 */ }
      term.write(`\r\n\x1b[31m[会话结束: ${reason}]\x1b[0m\r\n`);
      sessionStorage.removeItem(TOKEN_KEY);
      break;
    }
  }
};

ws.onclose = () => term.write("\r\n\x1b[90m[连接已断开, 刷新页面重连]\x1b[0m\r\n");
