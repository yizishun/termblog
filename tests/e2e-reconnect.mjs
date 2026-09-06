// 断线重连 E2E 验证脚本: 连 ws://127.0.0.1:18080/ws
// 1) 开新会话, 跑带标记的命令; 2) 断开; 3) 带 token 重连, 验证
//    attached=true / 同一会话 / scrollback 回放 / shell 状态延续。
// 协议边界(回放副作用修复): ReplayEnd 帧必须存在 —— 新会话(空回放)与
// attach 回放之后都要有; 回放内容必须全部排在 ReplayEnd 之前, 前端据此
// 在 Opened→ReplayEnd 之间禁用 xterm stdin, 防止历史里的终端查询
// (vim 启动时的 CSI 6n / DA / OSC 10;?)触发 xterm.js 回答上行污染 shell。
const T_OPEN = 0x01, T_DATA = 0x02, T_OPENED = 0x04, T_CLOSED = 0x05, T_REPLAY_END = 0x06;
const enc = new TextEncoder(), dec = new TextDecoder();
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function frame(kind, payload) {
  const b = new Uint8Array(5 + payload.length);
  const v = new DataView(b.buffer);
  v.setUint8(0, kind);
  v.setUint32(1, payload.length);
  b.set(payload, 5);
  return b.buffer;
}
const jsonFrame = (k, v) => frame(k, enc.encode(JSON.stringify(v)));
const concat = (chunks) => {
  let n = 0; for (const c of chunks) n += c.length;
  const b = new Uint8Array(n); let o = 0;
  for (const c of chunks) { b.set(c, o); o += c.length; }
  return b;
};

function open(attach_token) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket("ws://127.0.0.1:18080/ws");
    ws.binaryType = "arraybuffer";
    ws.chunks = [];        // 全部 DATA 帧(实时 + 回放)
    ws.preReplay = [];     // ReplayEnd 之前的 DATA 帧(应恰好是回放)
    ws.opened = null;
    ws.replayEnd = false;
    ws.onopen = () => ws.send(jsonFrame(T_OPEN, { cols: 80, rows: 24, attach_token }));
    ws.onmessage = (ev) => {
      const b = new Uint8Array(ev.data);
      const p = b.subarray(5);
      if (b[0] === T_OPENED) ws.opened = JSON.parse(dec.decode(p));
      else if (b[0] === T_DATA) {
        const copy = new Uint8Array(p);
        ws.chunks.push(copy);
        if (!ws.replayEnd) ws.preReplay.push(copy);
      }
      else if (b[0] === T_REPLAY_END) ws.replayEnd = true;
      else if (b[0] === T_CLOSED) console.log("  CLOSED frame:", dec.decode(p));
    };
    ws.onerror = () => reject(new Error("ws error"));
    const t = setInterval(() => { if (ws.opened) { clearInterval(t); resolve(ws); } }, 20);
    setTimeout(() => reject(new Error("open timeout")), 5000);
  });
}

let pass = 0, fail = 0;
const check = (name, ok) => { console.log(`${ok ? "✅" : "❌"} ${name}`); ok ? pass++ : fail++; };

// 1) 新会话
const c1 = await open(null);
check("首次连接 attached=false", c1.opened.attached === false);
const token = c1.opened.attach_token;
c1.send(frame(T_DATA, enc.encode("X=hello; echo E2E_$((40+2))\n")));
await sleep(1200);
check("标记命令输出 E2E_42", dec.decode(concat(c1.chunks)).includes("E2E_42"));
// 新会话没有回放, 但 ReplayEnd 仍必须发送(否则前端 xterm stdin 永不恢复)
check("新会话收到 ReplayEnd(空回放)", c1.replayEnd === true);
c1.close();
await sleep(500);

// 2) 带 token 重连
const c2 = await open(token);
check("重连 attached=true", c2.opened.attached === true);
check("session_id 相同", c2.opened.session_id === c1.opened.session_id);
await sleep(300);
// scrollback 回放: 断开前(上面 echo E2E_42)的输出应随 attach 原样回放过来
check("scrollback 回放含断开前输出 E2E_42", dec.decode(concat(c2.chunks)).includes("E2E_42"));
// 回放边界: 断开前的输出必须全部出现在 ReplayEnd 之前, 且 ReplayEnd 确实到达
check("重连收到 ReplayEnd", c2.replayEnd === true);
check("回放内容都在 ReplayEnd 之前", dec.decode(concat(c2.preReplay)).includes("E2E_42"));

// 3) shell 状态延续(变量 X 还在 => 是原来那个 zsh)
c2.send(frame(T_DATA, enc.encode("echo STATE_$X\n")));
await sleep(1200);
check("shell 状态延续 STATE_hello", dec.decode(concat(c2.chunks)).includes("STATE_hello"));
check("ReplayEnd 之后的实时输出仍正常送达", dec.decode(concat(c2.chunks)).includes("STATE_hello"));
c2.close();

// 4) 失效 token => 自动开新会话
const c3 = await open("deadbeef".repeat(4));
check("失效 token 回落新会话 attached=false", c3.opened.attached === false);
await sleep(300);
check("回落新会话也收到 ReplayEnd", c3.replayEnd === true);
c3.close();

console.log(`\n${pass} passed, ${fail} failed`);
process.exit(fail ? 1 : 0);
