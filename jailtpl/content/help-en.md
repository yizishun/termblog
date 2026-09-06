# Welcome to ~yzs's termblog

This is both a personal blog and a real, temporary FreeBSD jail. Read, explore, and leave a
message as you would in a normal terminal. Files created during your session disappear when
you exit.

Chinese guide: `blog help.md`

## Quick start

```sh
blog                  # List articles
blog help.md          # Read the Chinese guide
blog help-en.md       # Read this English guide
less ~/help-en.md     # View the Markdown source
play                  # List terminal recordings
play demo             # Play the site demo
cat /proc/stat        # View the root scope's frozen statistics snapshot
exit                  # End the session
```

While reading, press `j`/`k` to scroll down/up and `q` to return to the shell. During playback,
Space pauses or resumes, `.` advances one frame while paused, and `q` or Ctrl-C stops playback.

## Comments and replies

Each time you open and write the current directory's `comment` FIFO, one comment is submitted:

```sh
echo 'alice: hello' > ~/comment
echo 'bob: #1: thanks for sharing' > ~/comment
```

The format is `name: message`; without a name, the author is `guest`. `#1` is the local number
of an approved comment in this directory, so the second command replies to comment 1. You can
also prepare a multiline file and submit it once:

```text
bob: #1: How can I comment/reply with multiple lines?
1. Write a file. 2. cat file > comment.
```

```sh
cat reply.txt > ~/comment
```

The complete contents of one `cat` command become one comment, with internal newlines preserved.
A comment may contain at most 512 bytes. Submissions enter a moderation queue. Comments and
statistics in this session are frozen at connection time, so reconnect after approval to see
changes.

## Images

Reference an image in the same directory with a relative Markdown path. The Web mirror shows
the image directly; capable terminals render it inline, while other terminals show a clickable
placeholder.

![termblog image demo](demo.png)
