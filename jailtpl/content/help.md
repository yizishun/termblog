# 欢迎来到 termblog

这里既是博客首页，也是一个临时的 FreeBSD 终端。每次连接都会得到独立、短暂的环境。

## 阅读文章

- `blog`：列出所有文章
- `blog path/to/article`：按列表给出的 HOME 相对 article key 阅读文章
- `blog ~/help.md`：再次打开这份首页说明和首页留言
- `less ~/path/to/article.md`：查看未经排版的 Markdown 原文

## 留言

留言格式是 `名字: 内容`，例如：

```sh
echo 'alice: 你好' > ~/comment
```

提交后会进入审核队列；当前会话的评论快照不会变化，审核通过后重新连接终端即可看到。

评论目录由站点配置显式启用，不从目录名猜测。文章绑定了评论时，阅读器会在文末显示对应的投稿路径；同一 attachment 下的多篇文章共享评论。例如提示路径为 `~/path/to/comment` 时：

```sh
echo 'alice: 好文' > ~/path/to/comment
```

## 终端录像

- `play`：列出录像
- `play path/to/demo`：按列表给出的 HOME 相对 key 播放录像（空格暂停，`q` 退出）
