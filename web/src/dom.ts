// DOM helpers shared by the page's modules.
//
// Nothing here touches `document` at import time (element lookups are lazy
// getters), so modules that import this stay loadable under `node --test`.
//
// Server data only ever reaches the page as text: `h()` sets `textContent`,
// and no module assigns `innerHTML`.

import { type ConnClass, ConnStatus } from "./status.ts";

export function byId<T extends HTMLElement = HTMLElement>(id: string): T {
  const el = document.getElementById(id);
  if (!el) throw new Error(`#${id} is missing from index.html`);
  return el as T;
}

type Child = Node | string | null | undefined | false;

interface Props {
  className?: string;
  text?: string;
  title?: string;
}

/** Build an element; string children become text nodes, never markup. */
export function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  props: Props = {},
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  if (props.className) el.className = props.className;
  if (props.text !== undefined) el.textContent = props.text;
  if (props.title !== undefined) el.title = props.title;
  for (const c of children) {
    if (c === null || c === undefined || c === false) continue;
    el.append(c);
  }
  return el;
}

/** Remove every child (the `innerHTML = ""` idiom, without innerHTML). */
export function clear(el: Element): void {
  el.replaceChildren();
}

export const els = {
  get tree() {
    return byId("tree");
  },
  get conn() {
    return byId("conn-status");
  },
  get newBtn() {
    return byId<HTMLButtonElement>("new-btn");
  },
  get settingsBtn() {
    return byId<HTMLButtonElement>("settings-btn");
  },
  get refresh() {
    return byId<HTMLButtonElement>("refresh-btn");
  },
  get addProjectBtn() {
    return byId<HTMLButtonElement>("add-project-btn");
  },
  // toolbar
  get menuBtn() {
    return byId<HTMLButtonElement>("menu-btn");
  },
  get title() {
    return byId("session-title");
  },
  get micBtn() {
    return byId<HTMLButtonElement>("mic-btn");
  },
  get kbdBtn() {
    return byId<HTMLButtonElement>("kbd-btn");
  },
  get shellBtn() {
    return byId<HTMLButtonElement>("shell-btn");
  },
  get reviewBtn() {
    return byId<HTMLButtonElement>("review-btn");
  },
  get infoBtn() {
    return byId<HTMLButtonElement>("info-btn");
  },
  get editSession() {
    return byId<HTMLButtonElement>("edit-session-btn");
  },
  get restart() {
    return byId<HTMLButtonElement>("restart-btn");
  },
  get kill() {
    return byId<HTMLButtonElement>("kill-btn");
  },
  get delete() {
    return byId<HTMLButtonElement>("delete-btn");
  },
  // main
  get placeholder() {
    return byId("terminal-placeholder");
  },
  get terminal() {
    return byId("terminal");
  },
  get keyBar() {
    return byId("key-bar");
  },
  get ctrlKey() {
    return byId<HTMLButtonElement>("ctrl-key");
  },
  get backdrop() {
    return byId("sidebar-backdrop");
  },
  get infoPanel() {
    return byId("info-panel");
  },
  get infoList() {
    return byId("info-list");
  },
  // modals
  get newModal() {
    return byId("new-modal");
  },
  get newForm() {
    return byId<HTMLFormElement>("new-form");
  },
  get newProject() {
    return byId<HTMLInputElement>("new-project");
  },
  get newProjectName() {
    return byId<HTMLInputElement>("new-project-name");
  },
  get newTitle() {
    return byId<HTMLInputElement>("new-title");
  },
  get newProgram() {
    return byId<HTMLInputElement>("new-program");
  },
  get newSection() {
    return byId<HTMLSelectElement>("new-section");
  },
  get newBase() {
    return byId<HTMLInputElement>("new-base");
  },
  get newPrompt() {
    return byId<HTMLTextAreaElement>("new-prompt");
  },
  get projectModal() {
    return byId("project-modal");
  },
  get projectPath() {
    return byId<HTMLInputElement>("project-path");
  },
  get addPathBtn() {
    return byId<HTMLButtonElement>("add-path-btn");
  },
  get scanDirBtn() {
    return byId<HTMLButtonElement>("scan-dir-btn");
  },
  get settingsModal() {
    return byId("settings-modal");
  },
  get settingsForm() {
    return byId<HTMLFormElement>("settings-form");
  },
  get connectModal() {
    return byId("connect-modal");
  },
  get connectForm() {
    return byId<HTMLFormElement>("connect-form");
  },
  get connectToken() {
    return byId<HTMLInputElement>("connect-token");
  },
  get connectError() {
    return byId("connect-error");
  },
};

// The header's connection indicator; see status.ts for the layering.
const conn = new ConnStatus();

function renderConn(): void {
  const v = conn.view();
  els.conn.className = `conn ${v.cls}`;
  els.conn.textContent = v.text;
}

/** The poll's (or a live socket's) view of the connection. */
export function setConn(cls: ConnClass, text: string): void {
  conn.setBase(cls, text);
  renderConn();
}

/**
 * A terminal state ("session ended", a rejected token) that the next poll
 * must not overwrite. Held until `clearStickyConn`, i.e. the user's next
 * attach.
 */
export function stickConn(cls: ConnClass, text: string): void {
  conn.setSticky(cls, text);
  renderConn();
}

export function clearStickyConn(): void {
  conn.clearSticky();
  renderConn();
}

/** Show a transient status (e.g. "copied"), then whatever is underneath. */
export function flashConn(text: string, ms: number, cls: ConnClass = "ok"): void {
  const id = conn.flash(cls, text);
  renderConn();
  setTimeout(() => {
    conn.endFlash(id);
    renderConn();
  }, ms);
}

// ---- modals ------------------------------------------------------------------

export function openModal(el: HTMLElement): void {
  el.classList.remove("hidden");
}

export function closeModal(el: HTMLElement): void {
  el.classList.add("hidden");
}

/** Wire the shared modal behaviour: [data-close] buttons, backdrop click, Escape. */
export function wireModals(): void {
  for (const btn of document.querySelectorAll<HTMLElement>("[data-close]")) {
    btn.addEventListener("click", () => {
      const target = btn.dataset.close;
      if (target) closeModal(byId(target));
    });
  }
  for (const overlay of document.querySelectorAll<HTMLElement>(".modal-overlay")) {
    overlay.addEventListener("click", (e) => {
      // The connect modal is mandatory (no dismiss by backdrop).
      if (e.target === overlay && overlay.id !== "connect-modal") closeModal(overlay);
    });
  }
  document.addEventListener("keydown", (e) => {
    if (e.key !== "Escape") return;
    for (const m of document.querySelectorAll<HTMLElement>(".modal-overlay:not(.hidden)")) {
      if (m.id !== "connect-modal") closeModal(m);
    }
  });
}

// ---- right-click context menu ------------------------------------------------

export type MenuItem =
  | { type: "label"; text: string }
  | { type: "sep" }
  | { type?: "item"; text: string; danger?: boolean; onClick: () => void };

let ctxMenuEl: HTMLElement | null = null;

export function hideContextMenu(): void {
  ctxMenuEl?.remove();
  ctxMenuEl = null;
}

export function showContextMenu(e: MouseEvent, items: MenuItem[]): void {
  hideContextMenu();
  const menu = h("div");
  menu.id = "context-menu";
  for (const item of items) {
    if (item.type === "sep") {
      menu.append(h("div", { className: "ctx-sep" }));
    } else if (item.type === "label") {
      menu.append(h("div", { className: "ctx-label", text: item.text }));
    } else {
      const el = h("div", {
        className: `ctx-item${item.danger ? " danger" : ""}`,
        text: item.text,
      });
      el.addEventListener("click", () => {
        hideContextMenu();
        item.onClick();
      });
      menu.append(el);
    }
  }
  document.body.append(menu);
  const rect = menu.getBoundingClientRect();
  const x = Math.min(e.clientX, window.innerWidth - rect.width - 8);
  const y = Math.min(e.clientY, window.innerHeight - rect.height - 8);
  menu.style.left = `${Math.max(8, x)}px`;
  menu.style.top = `${Math.max(8, y)}px`;
  ctxMenuEl = menu;
}

export function wireContextMenu(): void {
  document.addEventListener("click", (e) => {
    if (ctxMenuEl && !ctxMenuEl.contains(e.target as Node)) hideContextMenu();
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") hideContextMenu();
  });
  window.addEventListener("blur", hideContextMenu);
  els.tree.addEventListener("scroll", hideContextMenu);
}

export async function copyToClipboard(text: string, okMsg = "copied"): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
    flashConn(okMsg, 1200);
  } catch {
    window.prompt("Copy:", text);
  }
}

// ---- mobile drawer -----------------------------------------------------------

export function isMobile(): boolean {
  return window.matchMedia("(max-width: 768px)").matches;
}

export function openDrawer(): void {
  document.body.classList.add("drawer-open");
  els.backdrop.classList.remove("hidden");
}

export function closeDrawer(): void {
  document.body.classList.remove("drawer-open");
  els.backdrop.classList.add("hidden");
}
