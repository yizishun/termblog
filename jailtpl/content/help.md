# 欢迎来到 termblog

这里既是博客首页，也是一个临时的 FreeBSD 终端。每次连接都会得到独立、短暂的环境。

## 阅读文章

- `blog`：列出所有文章
- `blog hello`：阅读一篇文章，并在文末显示评论
- `blog ~/help.md`：再次打开这份首页说明和首页留言
- `less ~/blog/hello.md`：查看未经排版的 Markdown 原文

## 留言

留言格式是 `名字: 内容`，例如：

```sh
echo 'alice: 你好' > ~/comment
```

提交后会进入审核队列；当前会话的评论快照不会变化，审核通过后重新连接终端即可看到。

文章评论绑定到文章所在的直属目录；同目录的多篇文章共享评论。例如 `~/blog/hello.md` 位于 `~/blog/`：

```sh
echo 'alice: 好文' > ~/blog/comment
```

如果文章是 `~/blog/topic/one.md`，则向它所在的目录投稿：

```sh
echo 'alice: 好文' > ~/blog/topic/comment
```

## 终端录像

- `play`：列出录像
- `play hello/demo`：播放录像（空格暂停，`q` 退出）
