# plan_image_v1 —— 博客图片支持(第一期:资源管线 + 镜像页真图 + 终端占位框)

> 目标读者:实现者(可能是另一个 AI)。本文是自包含规格,不需要读对话历史。
> 仓库根:`/home/yzs/termblog`。先通读 `README.md` 了解架构。
>
> 这是两期计划的第一期。第二期(终端内嵌像素图,`plan_image_v2.md`)以本期
> 产物为降级形态(fallback),**本期的占位框渲染必须稳定、格式确定**,v2 的
> sidecar manifest 直接锚定它。

## 1. 问题定义:图片在三个环节上都是断的

1. **资源管线不存在**。`crates/tools/content-build/src/main.rs` 的 `scan_blog`
   (~L76-107)只收 `*.md`,非 md 文件仅打「忽略非 markdown 文件」告警——图片
   永远不会进入 `frontend/dist`,镜像页里的 `<img>` 必然 404。
2. **URL 相对基准错位**。写作时 `blog/hello.md` 引用 `![alt](hello/img.png)`,
   相对基准是 md 所在目录(`blog/`);而镜像页 URL 是 `/blog/hello/`,浏览器会
   把 `hello/img.png` 解析成 `/blog/hello/hello/img.png`。源基准与页面基准差
   一级,必须构建期重写。
3. **终端投影无图片概念**。`crates/tools/content-build/src/ansi.rs` 的
   `parse_inline` 把图片降级为一行 dim 文本 `image: alt url`(L236-244),
   不可点、无结构。

已有铺垫:`frontend/public/blog.css` 已有 `#static-view article img { max-width: 100% }`;
`meta.rs` 的 excerpt 已把图片 alt 计入摘要(无需改动)。

## 2. 目标 / 非目标

**目标**:
- 作者在 `jailtpl/content/blog/` 下放图片并在 md 里相对引用,`make content`
  之后:镜像页(爬虫/无 JS/降级视图)显示真图;终端投影出现格式确定的
  占位框,框内 URL 是可点击的 OSC 8 超链接;构建期强制图片预算(尺寸/字节),
  保证任何文章的资源开销有硬上限。
- 所有失败都是**构建期 fail-fast**(宁构建失败,不线上 404)。

**非目标**(属 v2 或明确不做):
- 终端内嵌像素图(sixel/iTerm2/kitty)→ `plan_image_v2.md`。
- 前端 lightbox / 自定义 linkHandler。
- svg(**明确拒绝**:同域直接打开 svg 会执行其中脚本,能读 sessionStorage 里
  的 attach_token;不值得为它做净化)、avif(image crate 编码支持不成熟)、
  自动转 webp、sitemap image 扩展、atom enclosure。

## 3. 写作约定(写进 `jailtpl/content/README.md`)

```
jailtpl/content/blog/
  hello.md            # 文章
  hello/              # 同名资源目录(沿用 demo.cast 的先例)
    arch.png
    diag.webp
```

- 引用:`![架构图](hello/arch.png)` —— **相对 md 所在目录**的相对路径。
- 格式白名单(扩展名,大小写不敏感):`png jpg jpeg webp gif`。
- 资源文件路径(相对 `blog/`)字符集限 `[a-z0-9/._-]`(与 slug 白名单同风格),
  违规构建失败——这样 URL 无需 percent-encode。
- 预算(构建期强制,处理**后**字节):
  - 单张位图 ≤ **256 KiB**;gif ≤ **512 KiB**(gif 不重编码,见 §5.2);
  - 单篇文章图片总量 ≤ **1.5 MiB**;
  - 位图宽度 > **1080 px** 自动缩小到 1080。
- 外部图片(`https://…` 绝对 URL):原样透传,不校验、不复制、不计预算。

## 4. 总览:改动面

| 组件 | 文件 | 改动 |
| --- | --- | --- |
| content-build | 新 `src/img.rs` | 白名单 / resolve / 处理(缩放重编码)/ 预算 / 尺寸读取 |
| content-build | `src/main.rs` | scan 收集资源;引用校验;调 img 处理;复制进 dist;Article 加 `image_meta` |
| content-build | `src/html.rs` | Image 事件 → 手写 `<img>`(src 重写 + 宽高 + lazy);og:image |
| content-build | `src/ansi.rs` | `Seg` 加 `link`;OSC 8 发射;图片占位框 Block;行内图降级 |
| 前端 | `frontend/package.json` / `src/main.ts` | `@xterm/addon-web-links`(裸 URL 可点) |
| 前端 | `frontend/public/blog.css` | img 样式补 `height:auto` 等 |
| 测试 | `crates/tools/content-build/src/*.rs` 单测 + `tests/verify-m5.sh` | 见 §8 |
| 文档 | `README.md` / `jailtpl/content/README.md` | 写作约定 |
| 部署脚本 | **零改动** | 见 §7 |

`Cargo.toml`(content-build)新增依赖:`image = { version = "0.25", default-features = false, features = ["png", "jpeg", "webp", "gif"] }`。
(gif 只需解码读尺寸;若 gif 重编码路径不用可再裁剪 feature。)

## 5. 详细规格

### 5.1 扫描与收集(main.rs / img.rs)

`scan_blog` 扩展为返回两个列表:md 文章(现状)+ 资源文件。资源分类:

- 扩展名 ∈ 白名单 → 收进 `assets: Vec<PathBuf>`(相对 blog/ 的路径);
- 扩展名 == `.cast` → 已知类型,静默(现状的「忽略非 markdown 文件」告警对它闭嘴);
- 其它非 md → 维持现状告警。

对 `assets` 逐个校验路径字符集 `[a-z0-9/._-]`(含目录部分),违规列入
slug_errors 一并报出、统一 fail。

### 5.2 图片处理管线(img.rs)

对每张**被引用**的本地图(未被引用的资源仅告警,不处理不复制——如 demo.cast):

```
读原字节 → 按扩展名分支:
  gif: 原样采用;image crate 解码首帧只为拿 (w, h);> 512KiB → fail
  png/jpg/jpeg/webp:
    decode → 若宽 > 1080: resize 到宽 1080(imageops::resize, FilterType::Triangle
      即可,Lanczos3 更慢更细,任选并固定)
    重编码: jpg/jpeg → JPEG q80;png → PNG 默认;webp → WebP
      ⚠️ image 0.25 的 WebPEncoder 只有 lossless——重编码后可能比原文件大。
      规则:重编码结果 > 原字节数 ⇒ 回退用原字节。
    若未触发缩放(宽 ≤ 1080) ⇒ 直接用原字节,不重编码。
  最终字节数 > 256KiB(gif > 512KiB)→ fail,
    报错须含:文章 slug、文件路径、实际大小、建议(自行压缩/缩小)。
每篇文章按 slug 累计处理后字节,> 1.5MiB → fail(同样带明细)。
```

产出 per 图片:`{ 最终字节 Vec<u8>, 宽 u32, 高 u32, dist 相对路径(如 "hello/arch.png") }`。

**尺寸必须来自真实解码**,不解析文件头手写(防错)。

### 5.3 URL 重写(img.rs,html/ansi 共用)

```rust
/// slug: "a/b/c"(对应 blog/a/b/c.md);dest: md 里写的 dest_url 原文。
/// 返回站点绝对路径 "/blog/a/b/xxx.png";外链原样返回(用返回值可区分)。
enum ResolvedImage { Local(String /* /blog/... */), External(String) }

fn resolve_image_url(slug: &str, dest: &str) -> Result<ResolvedImage>
```

规则:
1. `dest` 匹配 `^[a-zA-Z][a-zA-Z0-9+.-]*:`(http: https: mailto: data: …)→ External,原样。
2. 以 `/` 开头 → External 处理(原样)+ 告警「站点绝对路径图片不受管线管理」。
3. 否则:剥掉 `?`/`#` 后缀(保留到结果末尾);`base = dirname(slug)`
   (`"hello"` → `""`,`"a/b/c"` → `"a/b"`);拼接 `base + "/" + dest` 后按
   `/` 分段规范化:`.` 丢弃,`..` 弹栈、栈空则 **fail**「图片路径逃逸 blog 根」;
   结果 = `/blog/` + 规范化路径 + 后缀。
4. 本地图的文件存在性在 main.rs 校验(用规范化后的路径 join blog_dir),
   不存在 → 收集所有缺失后统一 fail。

注意 pulldown-cmark 可能对 dest_url 做 percent-encoding;由于 §3 限制了文件名字符集,
本地路径不应出现 `%`;出现了按 fail 处理(提示改名)。

### 5.4 HTML 投影(html.rs)

当前 `body_html`(L27-51)把事件流 `map(escape_raw_html)` 后整交 `push_html`。
改造为**两遍**:

1. 预处理 `Vec<Event>`:遍历时
   - 原始 `Event::Html`/`Event::InlineHtml` → 照旧 `escape_raw_html`(安全红线不变);
   - `Event::Start(Tag::Image{dest_url, title, ..})` → 消费到配对
     `Event::End(TagEnd::Image)`,期间收集 alt 纯文本(Text/Code 拼接,
     SoftBreak/HardBreak → 空格);生成**一个** `Event::InlineHtml`,内容为
     手写 `<img>` 标签(见下)。此 InlineHtml 是可信生成物,**不再过 escape**。
2. 其余照旧:剥首块 H1 → `push_html`。

`<img>` 标签规格(所有属性值过 `attr_escape`):

```html
<img src="/blog/hello/arch.png" alt="架构图" width="1080" height="640" loading="lazy" decoding="async" />
```

- src:Local 用 `/blog/...`;External 原样。
- width/height:Local 从处理结果查表(见 §5.7 的 `image_meta`);External 无尺寸则省略两属性。
- `title`:md 里写了 title 则输出。

**og:image**:`render_mirror_page` 增加入参 `first_image: Option<String>`(本站绝对
路径,取文章第一张 Local 图);有 `site_url` 且有首图时模板加
`<meta property="og:image" content="{{SITE_URL}}{{OG_IMAGE}}" />`。
模板条件块复用现有 `{{#if ...}}` 机制(新条件名 `og_image`)。

### 5.5 ANSI 终端投影(ansi.rs)—— v2 的 fallback,格式必须稳定

**(a) Seg 加链接维度:**

```rust
struct Seg { s: u8, t: String, link: Option<String> }
```

`push_seg` 合并条件追加「link 相等」。`render_line` 发射 OSC 8:

```
开:link 段开头输出 `\x1b]8;;<url>\x1b\\`(ST 终止;与 SGR 的顺序:先 SGR 后 OSC8)
闭:link 段结束(或行尾仍有活动 link)输出 `\x1b]8;;\x1b\\`(在 `\x1b[0m` 之前)
```

折行:与 SGR 重发同理,`wrap_segments` 不动 link,`render_line` 逐行开闭,
多行链接天然正确(less 与 xterm.js 均接受)。

**(b) 独占段落图片 → 占位框 Block:**

`Block` 新增 `Image { alt: String, url: String }`。判定:`parse_blocks` 的
Paragraph 分支里,若该段落的 inline 事件**恰好只有** `Start(Image)..End(Image)`
一个元素 → 生成 `Block::Image` 而非 Paragraph(引用块/列表项内的段落同样适用,
`parse_blocks` 递归已覆盖)。渲染(76 列,无前缀缩进场景;在 Quote/List 内时
沿用 `ind.first`/`ind.cont` 前缀):

```
\x1b[2m┌─ 图片 ──────…(─ 补齐到 76 列)\x1b[22m
\x1b[2m│\x1b[22m <alt,空则用文件名>          ← 可折行,内容宽 73,续行同前缀
\x1b[2m│\x1b[22m <url>                       ← CYAN + OSC8 link,可折行
\x1b[2m└───…(─ 补齐到 76 列)\x1b[22m
```

- `url`:有 `site_url` → 完整 URL(`https://host/blog/hello/arch.png`,SSH 用户
  可直接复制进浏览器);无 `site_url` → `/blog/...` 站点路径(web 端 OSC8 点击
  相对当前域仍可达)。`render_ansi` 需要拿到 base:改签名为
  `render_ansi(events, link_base: &str, …)`,main.rs 传入
  `site_url.as_deref().unwrap_or("")`,拼 URL = `link_base + resolved_path`。
- 占位框行数随折行变化,**不做固定行数假设**;v2 靠行号区间锚定。

**(c) 行内图(段落中夹图)**:替代现在的 `image: alt url`——

```
[图: <alt,空则文件名>]      style = DIM, link = url(同上规则)
```

**(d) 现有单测 `wrap_*` 不受影响;新增测试见 §8。**

### 5.6 资源复制与清理(main.rs)

- 复制:每张被引用的 Local 图,把处理后字节写到
  `frontend/dist/blog/<规范化相对路径>`(目录随写随建)。`dist/blog` 每次构建
  已整体先清后建(现状 L237-241),僵尸资源天然清理。
- `.rendered/` 产物不变(v2 才加 sidecar)。
- 摘要输出加一行:`图片: N 张, 共 X KiB(预算 1536 KiB/篇)`。

### 5.7 Article 结构(main.rs)

```rust
pub struct Article {
    …现有字段…
    /// 本地图 dest_url 原文 → (宽, 高, 重写后 /blog/ 路径);html.rs 查表用
    pub image_meta: std::collections::HashMap<String, (u32, u32, String)>,
}
```

外链不进表(html.rs 查不到 = External,省略宽高)。

## 6. 前端

1. `frontend/package.json` 加依赖 `@xterm/addon-web-links`(版本与
   `@xterm/xterm ^5.5.0` 配套,如 `^0.11.0`);`src/main.ts`:

   ```ts
   import { WebLinksAddon } from "@xterm/addon-web-links";
   term.loadAddon(new WebLinksAddon());
   ```

   OSC 8 链接 xterm.js 原生支持(悬停下划线、点击 `window.open`),无需自定义
   handler。web-links 让占位框里的裸 URL 在 OSC8 被 less 吃掉的环境下仍可点。
2. `frontend/public/blog.css`:`#static-view article img` 补
   `height: auto; display: block; margin: 1em auto;`
   (写了 width/height 属性后必须 `height:auto` 防拉伸)。

## 7. 部署与模板:零改动(论证)

- `deploy-scripts/deploy.sh` 的两条发布路径都是 `cp -R frontend/dist/. → STATIC_DIR`,
  图片随 dist 自动发布;`chmod -R a+rX` 已兜底权限。
- `deploy-scripts/build-template.sh` L100 已 `cp -R content/blog/. → ~/blog/`,
  图片随模板进 jail(只读模板,不占访客 4M 写配额),`blog` 命令读的
  `.rendered` 产物已含占位框。**jailbin 本期零改动。**

## 8. 测试规格

### 8.1 cargo 单测(content-build)

img.rs:
- resolve:嵌套 slug(`a/b/c` + `img.png` → `/blog/a/b/img.png`)、资源目录
  (`hello` + `hello/x.png` → `/blog/hello/x.png`)、`./x.png`、`../shared/x.png`
  (→ `/blog/shared/x.png`)、`../../etc` → fail、外链透传、`?v=1`/`#frag` 后缀保留;
- 路径字符集拒绝 `中文.png`、`a b.png`;
- 处理:>1080 宽被缩小(构造内存 PNG 喂管线)、>256KiB fail、预算累计超限 fail;
- gif 不缩放、超 512KiB fail。

html.rs:
- `<img>` 输出:src 重写、alt 转义(含 `"<>&` 的 alt)、width/height/loading/decoding 齐;
- 外链图无宽高属性;
- 首图进 og:image(有 site_url)/ 无 site_url 不出 og:image;
- 现有全部测试保持绿(事件流改造不破坏 H1 剥离等)。

ansi.rs:
- 独占图 → 占位框:含 `┌─ 图片`、`│` 前缀、`└`;URL 行带
  `\x1b]8;;…\x1b\\ … \x1b]8;;\x1b\\`;
- alt 折行续行带 `│ ` 前缀;长 URL 折行后**每行**都有 OSC8 开闭;
- 行内图 → `[图: alt]` + link;
- 无 site_url 时 URL 为 `/blog/...` 形态;
- strip_sgr 辅助函数需要同时剥 OSC8(扩展它,测试断言用)。

### 8.2 测试 fixture

仓库内新增一篇带图文章作为长期回归 fixture:
`jailtpl/content/blog/image-test.md` + `image-test/pixel.png`(2×2 PNG,
用脚本生成后提交二进制;注意文件名符合字符集)。文章里同时覆盖:独占图、
行内图、外链图(`https://` 占位)。**.gitignore 检查**别排除它。

### 8.3 verify-m5.sh 扩展(镜像页一节内追加)

```sh
curl -sf "$BASE/blog/image-test/" | grep -q '<img src="/blog/image-test/pixel.png"'
check $? "镜像页含重写后的 <img>"
curl -sf -o /dev/null -w '%{http_code}' "$BASE/blog/image-test/pixel.png" | grep -q 200
check $? "图片资源 HTTP 200"
```

ssh 一节追加:`blog image-test` 输出含 `┌─ 图片` 与 `]8;;`(cat -v 后 grep)。

## 9. 验收 checklist(人工)

1. `make build` 绿;`cargo test` 全绿;`sh tests/verify-m5.sh` 全绿。
2. 浏览器开 `/blog/image-test/`:无 JS(curl)看到真图;有 JS 接管后终端里
   看到占位框,悬停 URL 有下划线,点击开新标签显示图。
3. `ssh -p 2222 blog@host` 后 `blog image-test`:占位框整齐(76 列不溢出),
   URL 完整可读。
4. 故意引用不存在的图 → `make build` 失败且报错可读。

## 10. 已知坑(实现前读)

1. **image crate 的 WebP 编码只有 lossless**(0.25),重编码可能变大——§5.2
   的「变大回退原字节」规则就是为此。
2. OSC 8 用 **ST(`\x1b\\`)终止**(不用 BEL),与 iTerm2/kitty 惯例一致;
   xterm.js 与现代 less 均认。
3. `push_html` 会原样透传 InlineHtml——我们生成的 `<img>` 绝不能混进
   `escape_raw_html` 的路径,否则被转义成文本(§5.4 的两遍顺序)。
4. pulldown-cmark 对 alt 里的事件嵌套(如 `![**粗**](x.png)`)会展开成多个
   inline 事件,收集 alt 时按 §5.4 规则拍平即可。
5. `image_meta` 的 key 是 **md 里的 dest_url 原文**,不是 resolve 后的路径
   (html.rs 查表时手里只有原文)。
6. gif 不做 resize(会丢动画),只做尺寸读取与字节预算。
