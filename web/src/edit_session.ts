import { type Api, act } from "./api.ts";
import { byId, closeModal, openModal } from "./dom.ts";
import type { EditSession, SessionEditorModel, SessionInfo } from "./generated/index.ts";
import { state } from "./state.ts";

export function initSessionEditor(
  a: Api,
  reload: () => Promise<void>,
  restarted: (id: string) => void,
): void {
  api = a;
  refresh = reload;
  onRestart = restarted;
  byId<HTMLFormElement>("edit-form").addEventListener("submit", (e) => {
    e.preventDefault();
    void submit();
  });
  byId("edit-now").addEventListener("click", () => void save(true));
  byId("edit-later").addEventListener("click", () => void save(false));
  byId("edit-back").addEventListener("click", showFields);
}
async function submit(): Promise<void> {
  if (!session || !model || busy) return;
  busy = true;
  const baseline = model;
  try {
    const result = await act(
      api.planSessionEdit({
        model,
        draft: {
          title: field("title").value,
          program: select("program").value,
          section: select("section").value || null,
          keep_alive: field("keep-alive").checked,
          base: select("base").value,
        },
      }),
    );
    if (!result || model !== baseline || modal().classList.contains("hidden")) return;
    pending = result.value.edit;
    if (result.value.requires_restart) {
      byId("edit-fields").classList.add("hidden");
      byId("edit-warning").classList.remove("hidden");
      byId("edit-heading").textContent = "Restart session?";
      byId("edit-later").focus();
      return;
    }
  } finally {
    busy = false;
  }
  await save(false);
}
function showFields(): void {
  byId("edit-fields").classList.remove("hidden");
  byId("edit-warning").classList.add("hidden");
  byId("edit-heading").textContent = "Edit session";
}
async function save(restart: boolean): Promise<void> {
  if (!session || !pending || busy) return;
  const id = session.id;
  busy = true;
  const buttons = modal().querySelectorAll<HTMLButtonElement>("button");
  for (const b of buttons) b.disabled = true;
  try {
    const result = await act(api.editSession(id, { ...pending, restart }));
    if (!result) return;
    if (result.value.warnings.length) alert(result.value.warnings.join("\n"));
    closeModal(modal());
    if (result.value.restarted) onRestart(id);
    await refresh();
  } finally {
    busy = false;
    for (const b of buttons) b.disabled = false;
  }
}
export async function openSessionEditor(info: SessionInfo): Promise<void> {
  if (busy) return;
  session = info;
  pending = null;
  let options = null;
  let optionsError = "";
  try {
    options = await api.createOptions();
  } catch {
    optionsError =
      "Could not load program and section choices. Existing settings can still be saved.";
  }
  const result = await act(
    api.sessionEditorModel({
      session: info,
      peers: state.sessions,
      main_branch: state.projects.find((p) => p.id === info.project_id)?.main_branch ?? null,
      options,
    }),
  );
  if (!result || session !== info) return;
  model = result.value;
  showFields();
  field("title").value = model.original.title;
  field("keep-alive").checked = model.original.keep_alive;
  select("program").replaceChildren(...model.programs.map((p) => new Option(p.label, p.command)));
  select("program").value = model.original.program;
  select("section").replaceChildren(
    new Option("Automatic", ""),
    ...model.sections.map((s) => new Option(s, s)),
  );
  select("section").value = model.original.section ?? "";
  select("base").replaceChildren(...model.bases.map((b) => new Option(b.label, b.id)));
  select("base").value = model.original.base;
  byId("edit-options-error").textContent = optionsError;
  openModal(modal());
  field("title").focus();
}

let api: Api;
let refresh: () => Promise<void>;
let onRestart: (id: string) => void;
let session: SessionInfo | null = null;
let model: SessionEditorModel | null = null;
let pending: EditSession | null = null;
let busy = false;
const modal = () => byId("edit-modal");
const field = (id: string) => byId<HTMLInputElement>(`edit-${id}`);
const select = (id: string) => byId<HTMLSelectElement>(`edit-${id}`);
