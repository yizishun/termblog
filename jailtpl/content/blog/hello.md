# 你好, 世界

这是 termblog 的第一篇文章。你现在读到的, 是同一份 markdown 的两种投影之一:
搜索引擎看到的是这个静态镜像, 终端用户看到的是 `blog hello` 的排版输出。

## 两种入口, 一个源头

- 网页:打开 `/blog/hello/`,几秒后由真实终端接管
- SSH:`ssh -p 2222 blog@<host>`,然后敲 `blog hello`

> 文章本体只有一个:`jailtpl/content/blog/hello.md`,其余全是编译产物。

## 排版自检

```sh
$ blog hello
```

行内代码 `less -R`、**粗体**、*斜体* 与 [链接](https://example.com) 会被两种
渲染器各自正确呈现;中文段落按 76 列折行,连字符与标点悬挂不做特殊处理。

---

这里是分隔线之后的一段,验证块间距。
