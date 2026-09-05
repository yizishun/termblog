type ReplySummary = {
  number: number;
  author: string;
};

type CommentItem = {
  number: number;
  target: string;
  author: string;
  text: string;
  created_at: string;
  reply_to?: ReplySummary;
};

type CommentsResponse = {
  total: number;
  omitted_earlier: number;
  comments: CommentItem[];
};

function emptyHint(target: string, fifo: string): string {
  const noun = target === "/" ? "messages" : "comments";
  return `No ${noun} yet — write the first one with: echo 'alice: Great post' > ${fifo}`;
}

function renderComment(item: CommentItem): HTMLLIElement {
  const li = document.createElement("li");
  li.className = "comment-item";
  const meta = document.createElement("p");
  meta.className = "comment-meta";
  const number = document.createElement("span");
  number.className = "comment-id";
  number.textContent = `#${item.number}`;
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
  meta.append(number, "  ", author, " · ", time);
  const text = document.createElement("p");
  text.className = "comment-text";
  if (item.reply_to) {
    const context = document.createElement("span");
    context.className = "comment-reply-context";
    context.textContent = `(In reply to ${item.reply_to.author} from comment #${item.reply_to.number}):`;
    text.append(context, document.createElement("br"), item.text);
  } else {
    text.textContent = item.text;
  }
  li.append(meta, text);
  return li;
}

async function loadSection(section: HTMLElement): Promise<void> {
  const target = section.dataset.commentsTarget;
  const fifo = section.dataset.commentsFifo;
  const status = section.querySelector<HTMLElement>(".comments-status");
  const list = section.querySelector<HTMLOListElement>(".comment-list");
  if (!target || !fifo || !status || !list) return;
  status.textContent = "Loading…";
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
      status.textContent = emptyHint(target, fifo);
      return;
    }
    status.textContent = data.omitted_earlier > 0 ? `${data.omitted_earlier} earlier comments omitted` : "";
    for (const item of data.comments) {
      list.append(renderComment(item));
    }
  } catch {
    status.textContent = "Comments temporarily unavailable";
  }
}

for (const section of document.querySelectorAll<HTMLElement>("[data-comments-target]")) {
  void loadSection(section);
}
