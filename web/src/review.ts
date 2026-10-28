// The full-screen review view: the session's diff against its base, inline
// comments, per-file reviewed marks, and applying the comments to the agent.

import { type Api, act, Unauthorized } from "./api.ts";
import { byId, clear, h } from "./dom.ts";
import type {
  Comment,
  CommentSide,
  DiffLine,
  FileDiff,
  ReviewSnapshot,
  SessionId,
  SessionInfo,
} from "./generated/index.ts";

let api: Api;
let sessionId: SessionId | null = null;
let snapshot: ReviewSnapshot | null = null;
let composerEl: HTMLElement | null = null;

const rv = {
  get view() {
    return byId("review-view");
  },
  get body() {
    return byId("review-body");
  },
  get title() {
    return byId("review-title");
  },
  get status() {
    return byId("review-status");
  },
};

export function initReview(a: Api): void {
  api = a;
  byId("review-close").addEventListener("click", closeReview);
  byId("review-refresh").addEventListener("click", reloadReview);
  byId("review-apply").addEventListener("click", applyComments);
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && !rv.view.classList.contains("hidden")) closeReview();
  });
}

/** The path a file is known by in comments and reviewed marks. */
export function displayPath(f: Pick<FileDiff, "status" | "old_path" | "new_path">): string {
  return f.status === "deleted" ? f.old_path : f.new_path;
}

/** The side and line number a diff line is commented on, if it has one. */
export function lineAnchor(line: DiffLine): { side: CommentSide; lineno: number } | null {
  const side: CommentSide = line.origin === "deletion" ? "old" : "new";
  const lineno = side === "old" ? line.old_lineno : line.new_lineno;
  return lineno == null ? null : { side, lineno };
}

function emptyNote(text: string): HTMLElement {
  return h("div", { className: "rv-empty", text });
}

export async function openReview(s: SessionInfo): Promise<void> {
  sessionId = s.id;
  rv.title.textContent = `Review — ${s.title}`;
  rv.status.textContent = "loading…";
  clear(rv.body);
  rv.view.classList.remove("hidden");
  try {
    snapshot = await api.review(s.id);
    renderReview();
  } catch (e) {
    if (!(e instanceof Unauthorized)) {
      rv.body.replaceChildren(emptyNote(`Failed to load review: ${(e as Error).message}`));
      rv.status.textContent = "";
    }
  }
}

function closeReview(): void {
  rv.view.classList.add("hidden");
  sessionId = null;
  snapshot = null;
  clear(rv.body);
}

async function reloadReview(): Promise<void> {
  if (!sessionId) return;
  try {
    snapshot = await api.review(sessionId);
    renderReview();
  } catch {
    // Keep showing the last snapshot.
  }
}

function renderReview(): void {
  const snap = snapshot;
  if (!snap) return;
  clear(rv.body);
  const files = snap.diff?.files ?? [];
  rv.status.textContent = `${files.length} file(s) · ${snap.comments.length} comment(s)`;
  if (files.length === 0) {
    rv.body.append(emptyNote(`No changes against ${snap.base}.`));
    return;
  }
  const reviewed = new Set(snap.reviewed ?? []);
  for (const f of files) rv.body.append(renderFile(f, snap.comments, reviewed));
}

function renderFile(f: FileDiff, comments: readonly Comment[], reviewed: Set<string>): HTMLElement {
  const dp = displayPath(f);
  const cb = h("input");
  cb.type = "checkbox";
  cb.checked = reviewed.has(dp);
  cb.addEventListener("change", () => toggleReviewed(dp, cb));

  const header = h(
    "div",
    { className: "rv-file-header" },
    h("span", {
      className: "rv-file-path",
      text: f.status === "renamed" ? `${f.old_path} → ${f.new_path}` : dp,
    }),
    h("span", { className: "rv-file-status", text: f.status }),
    h(
      "span",
      {},
      h("span", { className: "rv-stat-add", text: `+${f.added}` }),
      " ",
      h("span", { className: "rv-stat-del", text: `-${f.removed}` }),
    ),
    h("label", { className: "rv-reviewed" }, cb, "reviewed"),
  );
  const wrap = h("div", { className: "rv-file" }, header);

  if (f.binary) {
    wrap.append(h("div", { className: "rv-binary", text: "Binary file not shown." }));
    return wrap;
  }

  for (const hunk of f.hunks ?? []) {
    wrap.append(
      h("div", {
        className: "rv-hunk-header",
        text: `@@ -${hunk.old_start},${hunk.old_lines} +${hunk.new_start},${hunk.new_lines} @@${
          hunk.header ? ` ${hunk.header}` : ""
        }`,
      }),
    );
    for (const line of hunk.lines) {
      wrap.append(renderLine(dp, line));
      const at = lineAnchor(line);
      if (!at) continue;
      for (const c of comments) {
        if (c.file === dp && c.side === at.side && c.line_range[0] === at.lineno) {
          wrap.append(renderComment(c));
        }
      }
    }
  }
  return wrap;
}

function renderLine(file: string, line: DiffLine): HTMLElement {
  const o = line.old_lineno ?? "";
  const n = line.new_lineno ?? "";
  const row = h(
    "div",
    { className: `rv-line ${line.origin}` },
    h("span", {
      className: "rv-gutter",
      text: `${String(o).padStart(4)} ${String(n).padStart(4)}`,
    }),
    h("span", { className: "rv-content", text: line.content }),
  );
  row.addEventListener("click", () => openComposer(row, file, line));
  return row;
}

function renderComment(c: Comment): HTMLElement {
  const [start, end] = c.line_range;
  const range = end !== start ? `${start}-${end}` : `${start}`;
  const del = h("button", { className: "rv-comment-del", text: "✕", title: "Delete comment" });
  del.addEventListener("click", () => deleteComment(c.id));
  return h(
    "div",
    { className: `rv-comment ${c.status}` },
    h(
      "div",
      { className: "rv-comment-meta" },
      h("span", { text: `${c.side}:${range} · ${c.status}` }),
      del,
    ),
    h("div", { text: c.comment }),
  );
}

function openComposer(afterRow: HTMLElement, file: string, line: DiffLine): void {
  composerEl?.remove();
  const at = lineAnchor(line);
  if (!at) return;
  const sid = sessionId;
  if (!sid) return;

  const ta = h("textarea");
  ta.rows = 2;
  ta.placeholder = `Comment on ${at.side} line ${at.lineno}…`;
  const cancel = h("button", { className: "ghost", text: "Cancel" });
  const save = h("button", { className: "primary", text: "Comment" });
  const box = h(
    "div",
    { className: "rv-composer" },
    ta,
    h("div", { className: "rv-composer-actions" }, cancel, save),
  );
  cancel.addEventListener("click", () => {
    box.remove();
    composerEl = null;
  });
  save.addEventListener("click", async () => {
    const text = ta.value.trim();
    if (!text) return;
    const ok = await act(
      api.addComment(sid, {
        file,
        side: at.side,
        line_range: [at.lineno, at.lineno],
        snippet: line.content,
        comment: text,
      }),
    );
    if (ok) {
      box.remove();
      composerEl = null;
      reloadReview();
    }
  });
  afterRow.insertAdjacentElement("afterend", box);
  composerEl = box;
  ta.focus();
}

async function deleteComment(cid: string): Promise<void> {
  if (sessionId && (await act(api.deleteComment(sessionId, cid)))) reloadReview();
}

async function toggleReviewed(dp: string, cb: HTMLInputElement): Promise<void> {
  if (!sessionId) return;
  // Send the state the box now shows rather than toggling the store, which
  // may hold a mark this page isn't showing (set by the CLI since it loaded).
  const res = await act(api.toggleReviewed(sessionId, { display_path: dp, reviewed: cb.checked }));
  if (res && typeof res.value?.reviewed === "boolean") cb.checked = res.value.reviewed;
  else cb.checked = !cb.checked; // revert on failure
}

async function applyComments(): Promise<void> {
  if (!sessionId) return;
  const res = await act(api.applyComments(sessionId));
  if (!res) return;
  const out = res.value;
  switch (out.outcome) {
    case "applied":
      alert(`Applied ${out.count} comment(s) — brief sent to the agent.`);
      break;
    case "deferred":
      alert(
        `Wrote ${out.count} comment(s), but the agent wasn't ready — re-apply when it's at a prompt.`,
      );
      break;
    case "blocked":
      alert(`Blocked: ${out.drifted.length} comment(s) drifted. Refresh and re-anchor.`);
      break;
    default:
      alert("No staged comments to apply.");
  }
  reloadReview();
}
