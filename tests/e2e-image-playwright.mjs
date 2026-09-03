#!/usr/bin/env node
// tests/e2e-image-playwright.mjs —— 图片二期像素渲染测试(唯一真正验证
// 像素渲染的路径, §7.3)。
//
// 门控: env TERMBLOG_PW=1 才真跑(需要 playwright + chromium headless,
// 人工/CI 按需); 未设 TERMBLOG_PW 或依赖缺失 → 跳过(退出 0 并说明)。
//
// 流程: 开 /blog/image-test/ → 等镜像页接管(cover 消失)→ 采样
// #term-screen canvas 像素 → 断言存在非背景/非前景的彩色像素
// (图像像素), 可选断言图片区域尺寸。
//
// 用法:
//   TERMBLOG_PW=1 BASE=http://127.0.0.1:8080 node tests/e2e-image-playwright.mjs
//
// 前置: 生产实例在跑(web + jaild + 已重建的模板, 见 plan_image_v2 §6);
//   npm i playwright(仓库外, 本脚本不引入仓库依赖)。

const BASE = process.env.BASE ?? "http://127.0.0.1:8080";

async function main() {
  if (process.env.TERMBLOG_PW !== "1") {
    console.log("⚠️ 跳过(像素渲染测试需要真实浏览器, TERMBLOG_PW=1 门控)");
    console.log(`   启用: TERMBLOG_PW=1 BASE=${BASE} node tests/e2e-image-playwright.mjs`);
    return;
  }
  let chromium;
  try {
    ({ chromium } = await import("playwright"));
  } catch {
    console.log("⚠️ 跳过: playwright 未安装(TERMBLOG_PW 已设但依赖缺失)");
    console.log("   安装: npm i playwright && npx playwright install chromium");
    return;
  }

  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
  let fail = 0;
  const check = (cond, name, extra = "") => {
    if (cond) console.log(`✅ ${name}`);
    else {
      fail += 1;
      console.log(`❌ ${name}${extra ? `\n   ${extra}` : ""}`);
    }
  };

  try {
    await page.goto(`${BASE}/blog/image-test/`, { waitUntil: "domcontentloaded" });
    // 镜像页接管 = 等待层移除(blog 先发 OSC 7777, 200ms 后淡出移除 cover)
    await page.waitForSelector("#mirror-cover", { state: "detached", timeout: 20000 });
    check(true, "镜像页接管完成(#mirror-cover 已移除)");
    // 等 TUI 首帧(首次图像解码 + 编码略慢于 less, 多等一些)
    await page.waitForTimeout(2500);

    const stats = await page.evaluate(() => {
      const canvas = document.querySelector("#term-screen canvas");
      if (!canvas) return null;
      const ctx = canvas.getContext("2d");
      if (!ctx) return null;
      const { width, height } = canvas;
      const data = ctx.getImageData(0, 0, width, height).data;
      const bg = [0xfa, 0xfa, 0xfa]; // style.css 背景
      const fg = [0x2e, 0x33, 0x38]; // 前景文本
      const near = (p, c, tol = 12) => Math.abs(p[0] - c[0]) < tol && Math.abs(p[1] - c[1]) < tol && Math.abs(p[2] - c[2]) < tol;
      let nonBg = 0;
      let colorful = 0; // 既非背景也非前景的像素(图像内容)
      const total = data.length / 4;
      for (let i = 0; i < data.length; i += 4) {
        const p = [data[i], data[i + 1], data[i + 2]];
        if (!near(p, bg)) {
          nonBg += 1;
          if (!near(p, fg)) colorful += 1;
        }
      }
      return { width, height, total, nonBg, colorful };
    });
    if (!stats) {
      check(false, "canvas 2D 采样(取不到 canvas 或 context)");
    } else {
      check(stats.total > 0, `canvas 采样 ${stats.width}×${stats.height} 像素`);
      check(
        stats.colorful > 500,
        `存在非背景/非前景的彩色像素(图像内容): ${stats.colorful} 个`,
        JSON.stringify(stats),
      );
    }
  } catch (e) {
    check(false, `流程执行: ${e.message}`);
  } finally {
    await browser.close();
  }
  console.log("");
  console.log(`== 结果: ${fail === 0 ? "通过" : `${fail} 失败`} ==`);
  process.exit(fail === 0 ? 0 : 1);
}

main();
