#!/usr/bin/env node
// tests/e2e-image.mjs —— 图片二期 e2e(纯 Node, 无浏览器, CI 默认跑)。
//
// addon-image 依赖浏览器的 Terminal/renderer/Canvas, @xterm/headless 没有
// 像素渲染, 0.9.0 typings 也没有 onImage 回调 —— 所以 Node 测试只负责
// manifest、布局与 IIP 字节格式:
//
//   1. 跑 content-build → 断言 manifest 行号锚定(block_start 行以 ┌ 开头、
//      block_end-1 行以 └ 开头)、区间递增不重叠、asset 与 .rendered-assets
//      字节一致、w/h 与 asset 解码尺寸一致、indent/display 几何;
//   2. 调 blog --dump-image-frame 录制帧 → 提取 OSC 1337 序列 → 断言头部
//      字段合法、base64 解码 == 对应处理后字节、width/height 属性与 §5.6.3
//      几何计算一致、切片帧的裁剪窗口正确。
//
// 用法: node tests/e2e-image.mjs [content-dir] [dist-dir](默认仓库内路径)
// 前置: make build(cargo build --release -p content-build -p termblog-jailbin
// + 前端已构建); 本脚本自己跑 content-build。

import { spawnSync, execFileSync } from "node:child_process";
import {
  cpSync,
  existsSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  rmSync,
  symlinkSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const REPO = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const CONTENT = process.argv[2] ?? join(REPO, "jailtpl/content");
const DIST = process.argv[3] ?? join(REPO, "frontend/dist");
const BIN_CONTENT = join(REPO, "target/release/content-build");
const BIN_JAILBIN = join(REPO, "target/release/jailbin");

// 与 jailbin reader.rs 的默认 cell 尺寸一致(§5.6.3)
const CELL_W = 9;
const CELL_H = 20;

let pass = 0;
let fail = 0;
function check(cond, name, extra = "") {
  if (cond) {
    pass += 1;
    console.log(`✅ ${name}`);
  } else {
    fail += 1;
    console.log(`❌ ${name}${extra ? `\n   ${extra}` : ""}`);
  }
}
function assertEq(got, want, name) {
  check(got === want, name, `got ${JSON.stringify(got)}, want ${JSON.stringify(want)}`);
}

// ── 1. 跑 content-build(测试 fixture) ──
console.log("== 1. content-build + manifest ==");
execFileSync(BIN_CONTENT, ["--content", CONTENT, "--dist", DIST], { stdio: "inherit" });

const slug = "image-test";
const manifestPath = join(CONTENT, ".rendered", `${slug}.images.json`);
check(existsSync(manifestPath), "有图文章产 manifest(image-test.images.json)");
const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
assertEq(manifest.version, 1, "manifest version == 1");
check(Array.isArray(manifest.images) && manifest.images.length === 2, "image-test 有 2 个锚点(外链图不进 manifest)", JSON.stringify(manifest.images));

// 剥掉 SGR/OSC 序列后的可见文本(与 content-build 单测同规则)
function stripAnsi(s) {
  return s
    .replace(/\u001b\]8;;.*?(\u001b\\|\u0007)/g, "")
    .replace(/\u001b\[[0-9;?]*[a-zA-Z]/g, "")
    .replace(/\u001b\][^\u001b]*(\u001b\\|\u0007)/g, "");
}
const renderedLines = readFileSync(join(CONTENT, ".rendered", slug), "utf8").split("\n").map(stripAnsi);
let prevEnd = 0;
for (const img of manifest.images) {
  const top = renderedLines[img.block_start] ?? "";
  const bottom = renderedLines[img.block_end - 1] ?? "";
  check(top.startsWith("┌─ 图片"), `block_start=${img.block_start} 行以 ┌ 开头`, top);
  check(bottom.startsWith("└"), `block_end-1=${img.block_end - 1} 行以 └ 开头`, bottom);
  check(img.block_start >= prevEnd, `区间递增不重叠(${img.block_start} >= ${prevEnd})`);
  prevEnd = img.block_end;
  assertEq(img.indent_cols, 0, `顶层图 indent_cols=0(${img.asset})`);
  assertEq(img.display_cols, 76, `顶层图 display_cols=76(${img.asset})`);
  // asset 与 dist/blog 同字节
  const assetBytes = readFileSync(join(CONTENT, ".rendered-assets", img.asset));
  const distBytes = readFileSync(join(DIST, "blog", img.asset));
  check(
    assetBytes.equals(distBytes),
    `.rendered-assets/${img.asset} 与 dist/blog/${img.asset} 字节一致`,
  );
  // w/h 与 asset 解码尺寸一致
  const dims = parseDims(assetBytes, extOf(img.asset));
  assertEq(`${img.w}x${img.h}`, dims, `manifest w/h 与 asset 解码尺寸一致(${img.asset})`);
}
// 无图文章不产 manifest
check(!existsSync(join(CONTENT, ".rendered", "hello.images.json")), "无图文章(hello)不产 manifest");

// ── 2. dump-image-frame: IIP 字节格式 ──
console.log("== 2. blog --dump-image-frame ==");
const home = mkdtempSync(join(tmpdir(), "tb-e2e-"));
mkdirSync(join(home, ".rendered"));
cpSync(join(CONTENT, ".rendered"), join(home, ".rendered"), { recursive: true });
cpSync(join(CONTENT, ".rendered-assets"), join(home, ".rendered-assets"), { recursive: true });
symlinkSync(BIN_JAILBIN, join(home, "blog"));

function dump(rows, cols, row) {
  const r = spawnSync(join(home, "blog"), ["--dump-image-frame", slug, String(rows), String(cols), String(row)], {
    env: { ...process.env, HOME: home },
    maxBuffer: 64 * 1024 * 1024,
  });
  check(r.status === 0, `dump rows=${rows} cols=${cols} row=${row} 退出码 0`, r.stderr?.toString());
  return r.stdout;
}

// 提取 OSC 1337 序列。IIP 的 name 是 base64(UTF-8), size 是解码后图片
// 的原始字节数；addon-image 0.9.0 要求 size 非零，否则会静默丢弃图片。
const IIP_RE = /\u001b\]1337;File=name=([A-Za-z0-9+/=]+);size=(\d+);inline=1;width=(\d+)px;height=(\d+)px;preserveAspectRatio=0:([A-Za-z0-9+/=]*)\u0007/g;
function iips(buf) {
  const s = buf.toString("latin1");
  const out = [];
  for (const m of s.matchAll(IIP_RE)) {
    out.push({
      name: Buffer.from(m[1], "base64").toString("utf8"),
      size: Number(m[2]),
      wpx: Number(m[3]),
      hpx: Number(m[4]),
      b64: m[5],
    });
  }
  return out;
}

// §5.6.3 几何(JS 独立复算, 与 reader.rs 同公式)
const images = manifest.images; // pixel.png(2×2), photo.jpg(1080×607)
const geo = (img) => {
  const wcols = Math.max(1, Math.min(img.display_cols, 76 - img.indent_cols));
  const wpx = wcols * CELL_W;
  const hpx = Math.max(1, Math.floor((wpx * img.h) / img.w));
  const r = Math.max(1, Math.ceil(hpx / CELL_H));
  return { wcols, wpx, r };
};
const fullHpx = (img, wpx) => Math.max(1, Math.floor((wpx * img.h) / img.w));
const displaySlice = (img, wpx, rowOffset, visibleRows) => {
  const full = fullHpx(img, wpx);
  const y0 = Math.min(full, rowOffset * CELL_H);
  const y1 = Math.min(full, (rowOffset + visibleRows) * CELL_H);
  return { y0, y1 };
};

// 模型行号: 文本行与图像块(r+1 行)依次排列(与 reader.rs build_model 一致)
const blockRow = (i) => {
  let row = 0;
  let cursor = 0;
  for (let j = 0; j < i; j++) {
    row += images[j].block_start - cursor;
    row += geo(images[j]).r + 1;
    cursor = images[j].block_end;
  }
  row += images[i].block_start - cursor;
  return row;
};
check(blockRow(0) === 8, `pixel 块模型行 8(实际 ${blockRow(0)})`);
check(blockRow(1) === 8 + 36 + 3, `photo 块模型行 47(实际 ${blockRow(1)})`);

// F1: 整帧含完整可见图(rows=60): pixel 完整、photo 切片
{
  const buf = dump(60, 76, 0);
  const seqs = iips(buf);
  check(seqs.length === 2, `F1 两条 IIP(实际 ${seqs.length})`, seqs.map((s) => s.name).join(","));
  if (seqs.length === 2) {
    const [pix, photo] = seqs;
    assertEq(pix.name, "image-test/pixel.png", "F1: pixel 在 photo 前");
    const wpx = geo(images[0]).wpx;
    assertEq(pix.wpx, wpx, `F1 pixel width=${wpx}px`);
    assertEq(pix.hpx, fullHpx(images[0], wpx), `F1 pixel height=${fullHpx(images[0], wpx)}px`);
    const dec = Buffer.from(pix.b64, "base64");
    assertEq(pix.size, dec.length, "F1 pixel size == payload 解码字节数");
    check(dec.equals(readFileSync(join(home, ".rendered-assets", pix.name))), "F1 pixel 完整 payload == 处理后字节");
    // photo 切片(顶可见, 底裁): 先缩到显示尺寸, 再按显示像素裁。
    const g = geo(images[1]);
    const { y0, y1 } = displaySlice(images[1], g.wpx, 0, 60 - blockRow(1));
    check(photo.wpx === g.wpx && photo.hpx === y1 - y0, `F1 photo 切片 ${photo.wpx}x${photo.hpx}px`);
    const slice = Buffer.from(photo.b64, "base64");
    assertEq(photo.size, slice.length, "F1 photo size == 切片 payload 解码字节数");
    check(parseDims(slice, "jpg") === `${g.wpx}x${y1 - y0}`, `F1 photo 切片尺寸 ${g.wpx}x${y1 - y0}`, parseDims(slice, "jpg"));
  }
}

// F2: photo 完整可见(row=模型行, rows=24): 只有 photo 一条, payload == 原字节
{
  const r0 = blockRow(1);
  const buf = dump(24, 76, r0);
  const seqs = iips(buf);
  check(seqs.length === 1, `F2 一条 IIP(实际 ${seqs.length})`);
  if (seqs.length === 1) {
    const photo = seqs[0];
    const g = geo(images[1]);
    assertEq(photo.wpx, g.wpx, `F2 width=${g.wpx}px`);
    assertEq(photo.hpx, fullHpx(images[1], g.wpx), `F2 height=${fullHpx(images[1], g.wpx)}px`);
    const dec = Buffer.from(photo.b64, "base64");
    assertEq(photo.size, dec.length, "F2 photo size == payload 解码字节数");
    check(dec.equals(readFileSync(join(home, ".rendered-assets", photo.name))), "F2 完整 payload == photo.jpg 处理后字节");
  }
}

// F3: photo 切片(顶滚出视口): row = photo 模型行 + 4, 视口 24 行
{
  const r0 = blockRow(1);
  const buf = dump(24, 76, r0 + 4);
  const seqs = iips(buf);
  check(seqs.length === 1, `F3 一条 IIP(实际 ${seqs.length})`);
  if (seqs.length === 1) {
    const photo = seqs[0];
    const g = geo(images[1]);
    const k = 4; // 滚出的模型行
    const { y0, y1 } = displaySlice(images[1], g.wpx, k, g.r - k);
    const hpx = y1 - y0;
    assertEq(`${photo.wpx}x${photo.hpx}`, `${g.wpx}x${hpx}`, "F3 切片 width/height 属性与几何计算一致");
    const slice = Buffer.from(photo.b64, "base64");
    assertEq(photo.size, slice.length, "F3 photo size == 切片 payload 解码字节数");
    check(parseDims(slice, "jpg") === `${g.wpx}x${y1 - y0}`, `F3 裁剪窗口 ${g.wpx}x${y1 - y0}`, parseDims(slice, "jpg"));
  }
}

// F4: 2×2 小图顶部滚出 15 行。旧实现先映射回 2px 源图再裁剪, 会把
// 剩余切片错误地重新放大到 684px 高并盖住后面的“真实照片”文字。
{
  const scroll = blockRow(0) + 15;
  const buf = dump(24, 76, scroll);
  const seqs = iips(buf);
  check(seqs.length === 1, `F4 一条小图切片 IIP(实际 ${seqs.length})`);
  if (seqs.length === 1) {
    const pix = seqs[0];
    const g = geo(images[0]);
    const { y0, y1 } = displaySlice(images[0], g.wpx, 15, g.r - 15);
    assertEq(`${pix.wpx}x${pix.hpx}`, `${g.wpx}x${y1 - y0}`, "F4 小图按显示像素裁剪");
    const slice = Buffer.from(pix.b64, "base64");
    check(parseDims(slice, "png") === `${g.wpx}x${y1 - y0}`, `F4 PNG 实际尺寸 ${g.wpx}x${y1 - y0}`, parseDims(slice, "png"));
    const followingTextRow = (blockRow(0) + g.r + 2) - scroll;
    check(Math.ceil(pix.hpx / CELL_H) < followingTextRow, "F4 小图切片不会覆盖后续文字");
  }
}

// 每帧用 DEC 2026 同步输出包裹: 浏览器保留旧帧直到清屏、文本、图片都已
// 解析完成, 避免按 j 时出现空白帧闪烁。
{
  const buf = dump(24, 76, blockRow(1));
  const s = buf.toString("latin1");
  check(s.startsWith("\u001b[?2026h"), "帧以同步输出 BEGIN 开始");
  check(s.endsWith("\u001b[?2026l"), "帧以同步输出 END 提交");
}

rmSync(home, { recursive: true, force: true });

// ── 工具: 图片尺寸(PNG 头 / JPEG SOF / GIF 头) ──
function extOf(p) {
  return p.split(".").pop().toLowerCase();
}
function parseDims(buf, ext) {
  if (ext === "png") {
    if (buf.length < 24 || buf.readUInt32BE(0) !== 0x89504e47) return "?";
    return `${buf.readUInt32BE(16)}x${buf.readUInt32BE(20)}`;
  }
  if (ext === "gif") {
    if (buf.length < 10 || buf.toString("latin1", 0, 3) !== "GIF") return "?";
    return `${buf.readUInt16LE(6)}x${buf.readUInt16LE(8)}`;
  }
  // jpeg: 扫 SOF0/1/2/3 标记
  let i = 2;
  while (i + 9 < buf.length) {
    if (buf[i] !== 0xff) {
      i += 1;
      continue;
    }
    const marker = buf[i + 1];
    if ([0xc0, 0xc1, 0xc2, 0xc3].includes(marker)) {
      return `${buf.readUInt16BE(i + 7)}x${buf.readUInt16BE(i + 5)}`;
    }
    const len = buf.readUInt16BE(i + 2);
    i += 2 + len;
  }
  return "?";
}

console.log("");
console.log(`== 结果: ${pass} 通过, ${fail} 失败 ==`);
process.exit(fail === 0 ? 0 : 1);
