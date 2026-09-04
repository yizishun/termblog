type CommentItem = {
  id: number;
  target: string;
  author: string;
  text: string;
  created_at: string;
};

type CommentsResponse = {
  total: number;
  omitted_earlier: number;
  comments: CommentItem[];
};

function emptyHint(target: string): string {
  if (target === "/") return "暂无留言 —— echo 'alice: 你好' > ~/comment 写第一条";
  if (target === "/blog/") return "暂无评论 —— echo 'alice: 好文' > ~/blog/comment 写第一条";
  const slug = target.slice("/blog/".length, -1);
  return `暂无评论 —— echo 'alice: 好文' > ~/blog/${slug}/comment 写第一条`;
}

function renderComment(item: CommentItem, ordinal: number): HTMLLIElement {
  const li = document.createElement("li");
  li.className = "comment-item";
  const meta = document.createElement("p");
  meta.className = "comment-meta";
  const id = document.createElement("span");
  id.className = "comment-id";
  id.textContent = `#${ordinal}`;
  const author = document.createElement("strong");
  author.textContent = item.author;
  const time = document.createElement("time");
  time.dateTime = item.created_at;
  const date = new Date(item.created_at);
  time.textContent = Number.isNaN(date.valueOf())
    ? item.created_at
    : new Intl.DateTimeFormat("zh-CN", {
        year: "numeric",
        month: "2-digit",
        day: "2-digit",
        hour: "2-digit",
        minute: "2-digit",
        hour12: false,
      }).format(date);
  meta.append(id, "  ", author, " · ", time);
  const text = document.createElement("p");
  text.className = "comment-text";
  text.textContent = item.text;
  li.append(meta, text);
  return li;
}

async function loadSection(section: HTMLElement): Promise<void> {
  const target = section.dataset.commentsTarget;
  const status = section.querySelector<HTMLElement>(".comments-status");
  const list = section.querySelector<HTMLOListElement>(".comment-list");
  if (!target || !status || !list) return;
  status.textContent = "正在加载…";
  list.replaceChildren();
  try {
    const query = new URLSearchParams({ target, limit: "100" });
    const response = await fetch(`/api/comments?${query.toString()}`, {
      headers: { Accept: "application/json" },
    });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const data = (await response.json()) as CommentsResponse;
    if (!Array.isArray(data.comments) || typeof data.total !== "number") {
      throw new Error("bad response");
    }
    if (data.comments.length === 0) {
      status.textContent = emptyHint(target);
      return;
    }
    status.textContent = data.omitted_earlier > 0 ? `还有 ${data.omitted_earlier} 条更早评论` : "";
    for (const [index, item] of data.comments.entries()) {
      list.append(renderComment(item, data.omitted_earlier + index + 1));
    }
  } catch {
    status.textContent = "评论暂不可用";
  }
}

for (const section of document.querySelectorAll<HTMLElement>("[data-comments-target]")) {
  void loadSection(section);
}
