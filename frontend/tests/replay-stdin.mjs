// 回放副作用回归测试(无浏览器): 用 @xterm/headless 直接验证修复机制 ——
// 回放窗口(Opened→ReplayEnd)内 disableStdin 会吞掉所有终端回答, 渲染不受影响;
// ReplayEnd 恢复后, 同样的查询能正常收到回答(实时 vim 等程序不受影响)。
// 查询字节来自 jailtpl/content/demo.cast:210-211(vim 启动查询)。
//
// 说明: headless 版不含 CoreBrowserTerminal 的 OSC 颜色上报监听器, 因此这里
// 只能断言 CPR/DA2 回答; OSC 10/11 上报走同一 triggerDataEvent 闸门
// (CoreBrowserTerminal.ts:213 → CoreService.triggerDataEvent), 闸门行为相同。
import pkg from "@xterm/headless";
const { Terminal } = pkg;

// ESC[6n ESC[6n ESC[>c ESC]10;?BEL ESC]11;?BEL
const QUERIES = new Uint8Array([
  0x1b, 0x5b, 0x36, 0x6e,
  0x1b, 0x5b, 0x36, 0x6e,
  0x1b, 0x5b, 0x3e, 0x63,
  0x1b, 0x5d, 0x31, 0x30, 0x3b, 0x3f, 0x07,
  0x1b, 0x5d, 0x31, 0x31, 0x3b, 0x3f, 0x07,
]);

const writeDone = (term, data) => new Promise((resolve) => term.write(data, resolve));

let pass = 0, fail = 0;
const check = (name, ok) => { console.log(`${ok ? "✅" : "❌"} ${name}`); ok ? pass++ : fail++; };

const term = new Terminal({
  cols: 80,
  rows: 24,
  allowProposedApi: true,
  // 与 frontend/src/main.ts 相同的浅色主题(颜色上报路径在浏览器版中依赖主题)
  theme: { background: "#fafafa", foreground: "#2e3338" },
});
let out = [];
term.onData((d) => out.push(d));

// ── 回放窗口: disableStdin=true, 历史里的查询必须一个回答都不产生 ──
term.options.disableStdin = true;
await writeDone(term, QUERIES);
check("回放期间无任何上行回答", out.length === 0);

// 渲染不受影响: 回放内容应照常上屏
await writeDone(term, "hello");
const visible = term.buffer.active.getLine(0)?.translateToString(true) ?? "";
check("回放期间渲染仍正常(可见文本上屏)", visible.includes("hello"));

// ── ReplayEnd: 恢复 stdin, 实时查询必须收到完整回答 ──
term.options.disableStdin = false;
out = [];
await writeDone(term, QUERIES);
check(
  "恢复后收到 3 条回答(headless 支持: 2×CPR + DA2)",
  out.length === 3,
);
check(
  "两次 CPR 光标位置回答",
  out.slice(0, 2).every((d) => /^\x1b\[\d+;\d+R$/.test(d)),
);
check("DA2 回答 = ESC[>0;276;0c", out[2] === "\x1b[>0;276;0c");

console.log(`\n${pass} passed, ${fail} failed`);
process.exit(fail ? 1 : 0);
