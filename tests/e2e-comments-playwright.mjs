#!/usr/bin/env node
// 评论 bundle 的真实 DOM 测试。默认跳过；TERMBLOG_PW=1 时使用本地 mock API，
// 不要求启动 termblog 服务，也不向仓库添加 Playwright 依赖。

import { createServer } from "node:http";
import { readFileSync, readdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const REPO = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const DIST_ASSETS = join(REPO, "frontend/dist/assets");

const response = {
  revision: "test",
  total: 5,
  omitted_earlier: 0,
  comments: [
    {
      number: 1,
      target: "/",
      author: "alice <img src=x>",
      text: "root",
      created_at: "2026-09-05T00:00:00Z",
    },
    {
      number: 2,
      target: "/",
      author: "second-root",
      text: "second",
      created_at: "2026-09-05T00:00:01Z",
    },
    {
      number: 3,
      target: "/",
      author: "bob",
      text: "reply",
      created_at: "2026-09-05T00:00:02Z",
      reply_to: { number: 1, author: "alice <img src=x>" },
    },
    {
      number: 4,
      target: "/",
      author: "carol",
      text: "nested",
      created_at: "2026-09-05T00:00:03Z",
      reply_to: { number: 3, author: "bob" },
    },
    {
      number: 5,
      target: "/",
      author: "dave",
      text: "sibling",
      created_at: "2026-09-05T00:00:04Z",
      reply_to: { number: 1, author: "alice <img src=x>" },
    },
  ],
  has_more: false,
};

async function main() {
  if (process.env.TERMBLOG_PW !== "1") {
    console.log("⚠️ 跳过(评论 DOM 测试需要真实浏览器, TERMBLOG_PW=1 门控)");
    console.log("   启用: cd frontend && npm run build && cd .. && TERMBLOG_PW=1 node tests/e2e-comments-playwright.mjs");
    return;
  }

  let chromium;
  try {
    ({ chromium } = await import("playwright"));
  } catch {
    console.log("⚠️ 跳过: playwright 未安装(TERMBLOG_PW 已设但依赖缺失)");
    return;
  }

  const bundleName = readdirSync(DIST_ASSETS).find(
    (name) => name.startsWith("comments-") && name.endsWith(".js"),
  );
  if (!bundleName) throw new Error("缺少 frontend/dist/assets/comments-*.js，请先构建前端");
  const bundle = readFileSync(join(DIST_ASSETS, bundleName));
  const server = createServer((request, reply) => {
    if (request.url?.startsWith("/api/comments?")) {
      reply.writeHead(200, { "content-type": "application/json" });
      reply.end(JSON.stringify(response));
      return;
    }
    if (request.url === "/comments.js") {
      reply.writeHead(200, { "content-type": "text/javascript" });
      reply.end(bundle);
      return;
    }
    reply.writeHead(200, { "content-type": "text/html; charset=utf-8" });
    reply.end(`<!doctype html><section data-comments-target="/" data-comments-fifo="~/comment">
      <p class="comments-status"></p><ol class="comment-list"></ol>
      <script type="module" src="/comments.js"></script></section>`);
  });
  await new Promise((resolveListen, rejectListen) => {
    server.once("error", rejectListen);
    server.listen(0, "127.0.0.1", resolveListen);
  });

  let browser;
  try {
    const address = server.address();
    if (!address || typeof address === "string") throw new Error("无法取得测试服务端口");
    browser = await chromium.launch();
    const page = await browser.newPage();
    await page.goto(`http://127.0.0.1:${address.port}/`, { waitUntil: "networkidle" });
    const ids = await page.locator(".comment-id").allTextContents();
    if (JSON.stringify(ids) !== JSON.stringify(["#1", "#2", "#3", "#4", "#5"])) {
      throw new Error(`评论线性顺序错误: ${JSON.stringify(ids)}`);
    }
    const bodies = await page.locator(".comment-text").allInnerTexts();
    if (bodies[2] !== "(In reply to alice <img src=x> from comment #1):\nreply") {
      throw new Error(`回复标签错误: ${bodies[2]}`);
    }
    if ((await page.locator("img, script:not([src])").count()) !== 0) {
      throw new Error("评论内容被解释成 HTML");
    }
    console.log("✅ 评论局部编号线性顺序、多层回复标签与文本节点 DOM 安全通过");
  } finally {
    if (browser) await browser.close();
    await new Promise((resolveClose) => server.close(resolveClose));
  }
}

main().catch((error) => {
  console.error(`❌ ${error.message}`);
  process.exit(1);
});
