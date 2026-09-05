#!/usr/bin/env node
// content/HOME 路径模型回归：fixture 故意完全不创建 blog/ 源目录。

import { spawnSync } from "node:child_process";
import {
  copyFileSync,
  existsSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  renameSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const repo = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const binary = process.env.CONTENT_BUILD_BIN ?? join(repo, "target/debug/content-build");
if (!existsSync(binary)) {
  throw new Error(`缺少 ${binary}；请先运行 cargo build -p content-build`);
}

// 2×2 lossless webp(用与 content-build 同版本的 image crate 生成, 保证可解码)。
// 转 png 后与同目录已有 pixel.png 声称同一输出路径 → 触发 webp 冲突分支。
const WEBP_B64 =
  "UklGRm4AAABXRUJQVlA4TGEAAAAvAUAAEM1VICICHogEAAAAAIABAAAAAAAMAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAQAAHggAQAAAAAA5x8AAAAAAAAAAAAAAAAAAAAAAAAEAAAAAAAAAAAAAAAAEJEPAA==";

const root = mkdtempSync(join(tmpdir(), "termblog-content-paths-"));
const content = join(root, "content");
const dist = join(root, "dist");
const assets = join(dist, "assets");
const serverConfig = join(root, "termblog.toml");

function write(path, body) {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, body);
}

function build(expectSuccess = true) {
  const result = spawnSync(
    binary,
    [
      "--content",
      content,
      "--dist",
      dist,
      "--config",
      serverConfig,
      "--site-url",
      "https://example.test",
    ],
    { cwd: repo, encoding: "utf8" },
  );
  const details = `${result.stdout ?? ""}${result.stderr ?? ""}`;
  if (expectSuccess && result.status !== 0) {
    throw new Error(`content-build 应成功，实际退出 ${result.status}\n${details}`);
  }
  if (!expectSuccess && result.status === 0) {
    throw new Error(`content-build 应失败，实际成功\n${details}`);
  }
  return details;
}

function check(condition, message) {
  if (!condition) throw new Error(message);
  console.log(`✅ ${message}`);
}

try {
  mkdirSync(assets, { recursive: true });
  mkdirSync(join(content, "empty-comments-dir"), { recursive: true });
  write(serverConfig, "");
  write(join(assets, "index-fixture.js"), "export {};\n");
  write(join(assets, "index-fixture.css"), "/* fixture */\n");
  write(join(assets, "comments-fixture.js"), "export {};\n");
  write(join(dist, "index.html"), "<!doctype html><title>fixture</title>\n");
  write(join(dist, "style.css"), "/* fixture */\n");
  write(join(dist, "blog.css"), "/* fixture */\n");
  write(join(dist, "favicon.svg"), "<svg xmlns=\"http://www.w3.org/2000/svg\"/>\n");

  write(join(content, "help.md"), "# Help\n\nRoot article.\n");
  write(join(content, "notes/unix.md"), "# Unix notes\n\nNested article.\n");
  write(join(content, "demos/boot.cast"), '{"version":2,"width":80,"height":24}\n');
  write(join(content, "proc/readme.txt"), "HOME proc remains ordinary content.\n");
  write(join(content, ".draft.md"), "# Hidden\n");
  write(join(content, "notes/.secret.md"), "# Hidden nested\n");
  write(
    join(content, ".termblog.toml"),
    '[scopes]\ndirectories = ["", "empty-comments-dir"]\n',
  );

  build();
  check(!existsSync(join(content, "blog")), "fixture 不含 blog/ 源目录");
  check(existsSync(join(content, "proc/readme.txt")), "HOME 下的 proc 路径不被统计系统占用");
  check(existsSync(join(dist, "help/index.html")), "根文章生成 /help/");
  check(existsSync(join(dist, "notes/unix/index.html")), "嵌套文章生成 /notes/unix/");
  check(existsSync(join(dist, "blog/index.html")), "无 blog/ 源目录仍生成 /blog/ 全站列表");
  check(!existsSync(join(dist, "draft/index.html")), "隐藏 Markdown 不参与文章发现");

  const helpHtml = readFileSync(join(dist, "help/index.html"), "utf8");
  const notesHtml = readFileSync(join(dist, "notes/unix/index.html"), "utf8");
  check(helpHtml.includes('name="termblog-source" content="help.md"'), "HTML 写入明确 source meta");
  check(helpHtml.includes('name="termblog-route" content="/help/"'), "HTML 写入明确 route meta");
  check(helpHtml.includes('data-comments-target="/"'), "根 attachment 注入根文章");
  check(!notesHtml.includes("data-comments-target="), "无 attachment 的文章不生成评论 section");

  const index = JSON.parse(readFileSync(join(content, ".rendered/.index.json"), "utf8"));
  const mappings = index.articles.map(({ source_rel, key, route }) => ({ source_rel, key, route }));
  check(
    JSON.stringify(mappings) ===
      JSON.stringify([
        { source_rel: "help.md", key: "help", route: "/help/" },
        { source_rel: "notes/unix.md", key: "notes/unix", route: "/notes/unix/" },
      ]),
    "机器索引保存 source/key/route 的确定映射",
  );
  check(existsSync(join(content, ".rendered/notes/unix")), "嵌套 ANSI 产物使用完整 key");
  check(
    readFileSync(join(content, ".comment-targets.tsv"), "utf8") ===
      "comment\t/\nempty-comments-dir/comment\t/empty-comments-dir/\n",
    "无文章目录也能生成显式 comment attachment",
  );
  check(readFileSync(join(dist, "sitemap.xml"), "utf8").includes("/notes/unix/"), "sitemap 使用明确 route");
  check(readFileSync(join(dist, "atom.xml"), "utf8").includes("/help/"), "Atom 覆盖根文章 route");

  write(join(content, ".termblog.toml"), "[scopes]\ndirectories = []\n");
  build();
  check(readFileSync(join(content, ".comment-targets.tsv"), "utf8") === "", "评论可整体禁用并生成空清单");
  check(
    !readFileSync(join(dist, "help/index.html"), "utf8").includes("data-comments-target="),
    "评论禁用时根文章也不生成评论 section",
  );
  write(
    join(content, ".termblog.toml"),
    '[scopes]\ndirectories = ["", "empty-comments-dir"]\n',
  );
  build();

  renameSync(join(content, "notes"), join(content, "archive"));
  build();
  check(!existsSync(join(dist, "notes/unix/index.html")), "移动目录后旧 Web 页面被清理");
  check(existsSync(join(dist, "archive/unix/index.html")), "移动目录后新 Web 页面被生成");
  const movedHtml = readFileSync(join(dist, "archive/unix/index.html"), "utf8");
  check(movedHtml.includes('content="archive/unix.md"'), "移动后 source meta 使用新相对路径");
  check(movedHtml.includes('content="/archive/unix/"'), "移动后 route meta 使用新相对路径");
  check(!existsSync(join(content, ".rendered/notes/unix")), "移动目录后旧 ANSI 产物被清理");
  check(existsSync(join(content, ".rendered/archive/unix")), "移动目录后新 ANSI 产物被生成");
  check(existsSync(join(dist, "assets/index-fixture.js")), "清理不会删除 Vite/public 文件");

  const stableHtml = readFileSync(join(dist, "help/index.html"), "utf8");
  const stableManifest = readFileSync(join(content, ".web-outputs.tsv"), "utf8");
  write(join(content, "assets.md"), "# Reserved route\n");
  const conflict = build(false);
  check(conflict.includes("Web path conflict"), "系统路由冲突在提交前失败");
  check(readFileSync(join(dist, "help/index.html"), "utf8") === stableHtml, "冲突失败保留旧 Web 页面");
  check(
    readFileSync(join(content, ".web-outputs.tsv"), "utf8") === stableManifest,
    "冲突失败保留旧输出清单",
  );
  rmSync(join(content, "assets.md"));

  // 其余系统保留路由同样在提交前失败, 旧产物保持不动(D14: blog.md 与全站列表冲突)。
  for (const [file, marker] of [
    ["blog.md", "system article list /blog/"],
    ["ws.md", "WebSocket system route /ws"],
    ["api/comments.md", "HTTP API prefix /api/"],
  ]) {
    write(join(content, file), "# Reserved route\n");
    const reserved = build(false);
    check(reserved.includes(marker), `${file} 与系统保留路径冲突: ${marker}`);
    check(
      readFileSync(join(dist, "help/index.html"), "utf8") === stableHtml,
      `${file} 冲突失败保留旧页面`,
    );
    rmSync(join(content, file));
  }
  rmSync(join(content, "api"), { recursive: true, force: true });

  write(join(content, "new.md"), "# File-directory collision\n");
  mkdirSync(join(dist, "new/index.html"), { recursive: true });
  const structuralConflict = build(false);
  check(structuralConflict.includes("Web output conflict"), "文件与已有空目录的结构冲突被拒绝");
  check(readFileSync(join(dist, "help/index.html"), "utf8") === stableHtml, "结构冲突失败保留旧页面");
  rmSync(join(content, "new.md"));
  rmSync(join(dist, "new"), { recursive: true });

  // webp 产物统一转 png 后, 与已有 png 声称同一输出 → 图片阶段 fail-fast(计划 §7.3)。
  // 先 write webp 让 helper 创建 pix/ 目录, 再 copyFileSync png(它不建父目录)。
  write(join(content, "pix/a.webp"), Buffer.from(WEBP_B64, "base64"));
  copyFileSync(
    join(repo, "jailtpl/content/tests/image-test/pixel.png"),
    join(content, "pix/a.png"),
  );
  write(join(content, "clash.md"), "# WebP collision\n\n![w](pix/a.webp)\n\n![p](pix/a.png)\n");
  const webpClash = build(false);
  check(
    webpClash.includes("Web output conflict") && webpClash.includes("pix/a.png"),
    "webp 转 png 与已有 png 目标冲突在提交前失败",
  );
  check(
    readFileSync(join(dist, "help/index.html"), "utf8") === stableHtml,
    "图片冲突失败保留旧页面",
  );
  rmSync(join(content, "clash.md"));
  rmSync(join(content, "pix"), { recursive: true, force: true });

  symlinkSync(join(content, "help.md"), join(content, "linked.md"));
  const symlinkFailure = build(false);
  check(symlinkFailure.includes("content does not support symlinks"), "content 符号链接被拒绝");
  rmSync(join(content, "linked.md"));

  rmSync(join(content, ".comment-targets.tsv"));
  mkdirSync(join(content, ".comment-targets.tsv"));
  const controlPathFailure = build(false);
  check(controlPathFailure.includes("content control path type error"), "隐藏控制路径类型错误在提交前失败");

  console.log("\ncontent path e2e: 全部通过");
} finally {
  rmSync(root, { recursive: true, force: true });
}
