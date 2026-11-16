// The sidebar's project → session tree.
//
// `TreeView` decides whether a poll needs a re-render (pure, tested);
// `renderTree` builds the DOM from a `TreeModel`.

import { clear, h, type MenuItem, showContextMenu } from "./dom.ts";
import type {
  AgentState,
  ProjectInfo,
  SessionId,
  SessionInfo,
  SessionStatus,
} from "./generated/index.ts";
import { agentStateFor, humanize } from "./state.ts";

/** Everything the tree renders — and nothing else. */
export interface TreeModel {
  projects: readonly ProjectInfo[];
  sessions: readonly SessionInfo[];
  agentStates: Partial<Record<SessionId, AgentState>>;
  selectedId: string | null;
  collapsed: ReadonlySet<string>;
}

export interface TreeHandlers {
  onSelect(id: string): void;
  onToggleProject(id: string): void;
  onNewSession(p: ProjectInfo): void;
  onRemoveProject(p: ProjectInfo): void;
  sessionMenu(s: SessionInfo): MenuItem[];
  projectMenu(p: ProjectInfo): MenuItem[];
}

/** Sessions grouped by project id, in snapshot order. */
export function groupByProject(sessions: readonly SessionInfo[]): Map<string, SessionInfo[]> {
  const by = new Map<string, SessionInfo[]>();
  for (const s of sessions) {
    const list = by.get(s.project_id);
    if (list) list.push(s);
    else by.set(s.project_id, [s]);
  }
  return by;
}

/**
 * A cheap fingerprint of a model: equal data gives an equal key even though
 * every poll parses fresh objects. The whole model is serialised (not a
 * hand-picked subset of fields), so a field the tree starts rendering later
 * can't be forgotten here.
 */
export function treeKey(model: TreeModel): string {
  return JSON.stringify([
    model.projects,
    model.sessions,
    model.agentStates,
    model.selectedId,
    [...model.collapsed].sort(),
  ]);
}

/**
 * Renders the tree only when its model changes. The page polls every 1.5 s,
 * and rebuilding the tree each time would reset hover state, restart CSS
 * transitions and churn the DOM for nothing.
 */
export class TreeView {
  private readonly render: (model: TreeModel) => void;
  private lastKey: string | null = null;

  constructor(render: (model: TreeModel) => void) {
    this.render = render;
  }

  /** Returns whether it re-rendered. */
  update(model: TreeModel): boolean {
    const key = treeKey(model);
    if (key === this.lastKey) return false;
    this.lastKey = key;
    this.render(model);
    return true;
  }
}

// ---- DOM ---------------------------------------------------------------------

function statusBadge(status: SessionStatus): HTMLElement {
  return h("span", { className: `badge ${status}`, text: humanize(status) });
}

function agentBadge(st: AgentState): HTMLElement {
  return h("span", { className: `agent ${st}` }, h("span", { className: "dot" }), humanize(st));
}

function sessionRow(s: SessionInfo, model: TreeModel, on: TreeHandlers): HTMLElement {
  const li = h(
    "li",
    { className: `session${s.id === model.selectedId ? " active" : ""}` },
    h(
      "div",
      { className: "session-row" },
      h("span", { className: "session-name", text: s.title }),
      statusBadge(s.status),
    ),
    h("div", {
      className: "session-meta",
      text: `${s.branch}${s.pr_number ? ` · PR #${s.pr_number}` : ""}`,
    }),
    agentBadge(agentStateFor(model.agentStates, s)),
  );
  li.dataset.id = s.id;
  li.addEventListener("click", () => on.onSelect(s.id));
  li.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    showContextMenu(e, on.sessionMenu(s));
  });
  return li;
}

function iconButton(className: string, text: string, title: string, onClick: () => void) {
  const btn = h("button", { className: `icon-btn ${className}`, text, title });
  btn.addEventListener("click", (e) => {
    e.stopPropagation();
    onClick();
  });
  return btn;
}

function projectGroup(
  p: ProjectInfo,
  sessions: readonly SessionInfo[],
  model: TreeModel,
  on: TreeHandlers,
): HTMLElement {
  const collapsed = model.collapsed.has(p.id);
  const name = h("span", { className: "pname", text: p.name, title: p.repo_path });
  const header = h(
    "div",
    { className: "project-header" },
    h("span", { className: "twisty", text: collapsed ? "▶" : "▼" }),
    name,
    h("span", { className: "pcount", text: sessions.length ? String(sessions.length) : "" }),
    h(
      "span",
      { className: "phover" },
      iconButton("add", "＋", `New session in ${p.name}`, () => on.onNewSession(p)),
      iconButton("del", "✕", `Remove ${p.name}`, () => on.onRemoveProject(p)),
    ),
  );
  header.addEventListener("click", () => on.onToggleProject(p.id));
  header.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    showContextMenu(e, on.projectMenu(p));
  });

  const group = h("div", { className: "project-group" }, header);
  if (!collapsed) {
    const ul = h("ul", { className: "project-sessions" });
    if (sessions.length === 0) ul.append(h("li", { className: "empty", text: "no sessions" }));
    else for (const s of sessions) ul.append(sessionRow(s, model, on));
    group.append(ul);
  }
  return group;
}

const projectNodes = new WeakMap<HTMLElement, Map<string, { key: string; node: HTMLElement }>>();

export function renderTree(container: HTMLElement, model: TreeModel, on: TreeHandlers): void {
  if (model.projects.length === 0) {
    clear(container);
    projectNodes.delete(container);
    container.append(
      h("div", {
        className: "tree-empty",
        text: "No projects yet. Use ＋ add to register a repo.",
      }),
    );
    return;
  }
  const previous = projectNodes.get(container) ?? new Map();
  const next = new Map<string, { key: string; node: HTMLElement }>();
  const byProject = groupByProject(model.sessions);
  const nodes = model.projects.map((project) => {
    const sessions = byProject.get(project.id) ?? [];
    const key = JSON.stringify([
      project,
      sessions,
      model.collapsed.has(project.id),
      sessions.map((session) => [model.agentStates[session.id], model.selectedId === session.id]),
    ]);
    const cached = previous.get(project.id);
    const entry =
      cached?.key === key ? cached : { key, node: projectGroup(project, sessions, model, on) };
    next.set(project.id, entry);
    return entry.node;
  });
  // Reuse unchanged project subtrees, preserving their hover/focus state.
  for (const [index, node] of nodes.entries()) {
    const at = container.children.item(index);
    if (at !== node) container.insertBefore(node, at);
  }
  const retained = new Set(nodes);
  for (const child of [...container.children]) {
    if (!retained.has(child as HTMLElement)) child.remove();
  }
  projectNodes.set(container, next);
}
