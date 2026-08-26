import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";

// ── proto 帧编解码(格式与 crates/proto 完全一致: u8 kind | u32 len | payload) ──
const T_OPEN = 0x01;
const T_DATA = 0x02;
const T_RESIZE = 0x03;
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

// 等 Web 字体加载完再 fit + 淡入, 避免字体切换引起的重排闪烁(配合 style.css)
document.fonts.ready.then(() => {
  fit.fit();
  document.getElementById("term-host")!.classList.add("ready");
  sendResize(); // Open 可能在 fit 前已发出(初始尺寸不准), 这里纠正一次
});

// ── 连接 ──
const wsProto = location.protocol === "https:" ? "wss" : "ws";
const ws = new WebSocket(`${wsProto}://${location.host}/ws`);
ws.binaryType = "arraybuffer";

function send(buf: ArrayBuffer) {
  if (ws.readyState === WebSocket.OPEN) ws.send(buf);
}
const sendResize = () => send(jsonFrame(T_RESIZE, { cols: term.cols, rows: term.rows }));

ws.onopen = () => {
  ws.send(jsonFrame(T_OPEN, { cols: term.cols, rows: term.rows }));

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
    case T_CLOSED: {
      let reason = textDec.decode(payload);
      try {
        reason = JSON.parse(reason).reason;
      } catch { /* 非 JSON 就直接显示 */ }
      term.write(`\r\n\x1b[31m[会话结束: ${reason}]\x1b[0m\r\n`);
      break;
    }
  }
};

ws.onclose = () => term.write("\r\n\x1b[90m[连接已断开, 刷新页面重连]\x1b[0m\r\n");
