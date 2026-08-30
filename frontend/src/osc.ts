// OSC 7777: 终端 → 浏览器地址栏。payload 不可信(jail 内任意进程可 printf),
// 这里只做解析 + 白名单, 把合法路径交给 main.ts 决策(是否 replaceState /
// 是否接管 / 是否当作会话状态心跳), 天花板 = 地址栏显示一个站内路径。
import type { Terminal } from "@xterm/xterm";

// 站内纯路径白名单: / 或 /a/b/ 形, 禁止 ? # . .. 与非 ASCII
const PATH_RE = /^\/[a-z0-9-]*(\/[a-z0-9-]+)*\/?$/;

export function installOsc(term: Terminal, onUrl: (path: string) => void): void {
  term.parser.registerOscHandler(7777, (payload) => {
    // 只认 url= 子命令, 其余一律吞掉
    if (!payload.startsWith("url=")) return true;
    const v = payload.slice(4);
    if (v.length < 1 || v.length > 512 || !PATH_RE.test(v)) return true;
    onUrl(v);
    return true;
  });
}
