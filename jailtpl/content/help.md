# 欢迎来到 yzs 的 blog

这里既是个人博客，也是一个真实、临时的 FreeBSD jail。你可以像使用普通终端一样阅读、
浏览和留言；退出后，本次会话中创建的文件会被丢弃。

English version: `blog help-en.md`

## 快速开始

```sh
blog                  # 列出文章
blog help.md          # 阅读中文帮助
blog help-en.md       # Read the English guide
less ~/help.md        # 查看 Markdown 原文
play                  # 列出终端录像
play demo             # 播放本站演示
cat /proc/stat        # 查看根目录的访问、访客和评论统计快照
exit                  # 结束会话
```

阅读文章时按 `j`/`k` 向下/向上滚动，按 `q` 返回终端。播放录像时，
按 Space 暂停或继续，暂停时按 `.` 单步播放，按 `q` 或 Ctrl-C 停止。

## 留言与回复

每次打开并写入当前目录的 `comment` FIFO，会提交一条留言：

```sh
echo 'alice: hello' > ~/comment
echo 'bob: #1: thanks for sharing' > ~/comment
```

格式是 `名字: 内容`；省略名字时会显示为 `guest`。`#1` 是当前目录中已公开留言的局部
编号，上面的第二条命令会回复第 1 条留言。也可以先编辑一个多行文件，再提交一次：

```text
bob: #1: How can I comment/reply with multiple lines?
1. Write a file. 2. cat file > comment.
```

```sh
cat reply.txt > ~/comment
```

一次 `cat` 的完整内容只会成为一条留言，文件中的换行会原样保留。每条留言最多 512
字节，提交后先进入审核队列；当前会话中的评论与统计都是启动时的快照，审核通过后请
重新连接查看。

## 图片

这是我的博客的图片演示。

![termblog 图片演示](demo.png)
