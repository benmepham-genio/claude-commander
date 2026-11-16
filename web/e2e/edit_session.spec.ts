import type { CreatedId, SessionId } from "../src/generated/index.ts";
import {
  api,
  connect,
  expect,
  openSession,
  PROJECT,
  paneText,
  sessionByTitle,
  test,
  workspace,
} from "./support";

for (const restart of [false, true]) {
  test(`edit session: program change restart=${restart}`, async ({ page, request }) => {
    const ws = await workspace(request);
    const project = ws.projects.find((p) => p.name === PROJECT);
    if (!project) throw new Error("Fixture project missing");
    const title = `edit-${restart}-${Date.now()}`;
    const created = await api<CreatedId<SessionId>>(request, "POST", "/sessions", {
      project_path: project.repo_path,
      title,
      program: "cat",
    });
    try {
      await page.route("**/api/create-options", (route) =>
        route.fulfill({
          json: {
            default_program: "cat",
            programs: [
              { label: "Echo alias", command: "cat" },
              { label: "Idle alias", command: "sleep 60" },
            ],
            sections: [],
          },
        }),
      );
      await connect(page);
      await openSession(page, title);
      await page.locator("#edit-session-btn").click();
      const modal = page.locator("#edit-modal");
      await expect(page.locator("#edit-title")).toHaveValue(title);
      await expect(page.locator("#edit-program")).toHaveValue("cat");
      await expect(page.locator("#edit-program option:checked")).toHaveText("Echo alias");
      await page.locator("#edit-title").fill(`${title}-renamed`);
      await page.locator("#edit-program").selectOption({ label: "Idle alias" });
      await modal.getByRole("button", { name: "Save", exact: true }).click();
      await expect(page.locator("#edit-heading")).toHaveText("Restart session?");
      expect((await sessionByTitle(request, title)).program).toBe("cat");
      await page.locator(restart ? "#edit-now" : "#edit-later").click();
      await expect(modal).toBeHidden();
      const edited = await sessionByTitle(request, `${title}-renamed`);
      expect(edited.program).toBe(restart ? "sleep 60" : "cat");
      expect(edited.pending_program).toBe(restart ? null : "sleep 60");
      expect(edited.status).toBe("running");
      if (!restart) {
        // cat echoes text only while the original pane remains running.
        await page.locator("#terminal").click();
        await page.keyboard.type("still-running");
        await page.keyboard.press("Enter");
        await expect.poll(() => paneText(request, created.id)).toContain("still-running");
        await page.locator("#restart-btn").click();
      }
    } finally {
      await api(request, "DELETE", `/sessions/${created.id}`);
    }
  });
}

test("edit session: cancel pending change to an unconfigured live program", async ({
  page,
  request,
}) => {
  const ws = await workspace(request);
  const project = ws.projects.find((p) => p.name === PROJECT);
  if (!project) throw new Error("Fixture project missing");
  const title = `edit-cancel-pending-${Date.now()}`;
  const created = await api<CreatedId<SessionId>>(request, "POST", "/sessions", {
    project_path: project.repo_path,
    title,
    program: "cat",
  });
  try {
    await api(request, "PATCH", `/sessions/${created.id}`, {
      op: "edit",
      title,
      program: "sleep 60",
      section: null,
      keep_alive: false,
      restart: false,
      base: null,
    });
    await page.route("**/api/create-options", (route) =>
      route.fulfill({
        json: {
          default_program: "sleep 60",
          programs: [{ label: "Idle alias", command: "sleep 60" }],
          sections: [],
        },
      }),
    );
    await connect(page);
    await openSession(page, title);
    await page.locator("#edit-session-btn").click();
    await expect(page.locator("#edit-program option:checked")).toHaveText("Idle alias");
    await expect(page.locator("#edit-program option[value='cat']")).toHaveText("cat");
    await page.locator("#edit-program").selectOption("cat");
    await page.locator("#edit-modal").getByRole("button", { name: "Save", exact: true }).click();
    await expect(page.locator("#edit-modal")).toBeHidden();
    const edited = await sessionByTitle(request, title);
    expect(edited.program).toBe("cat");
    expect(edited.pending_program).toBeNull();
    expect(edited.status).toBe("running");
  } finally {
    await api(request, "DELETE", `/sessions/${created.id}`);
  }
});

test("cancelling during a delayed editor plan does not save", async ({ page, request }) => {
  const project = (await workspace(request)).projects.find((p) => p.name === PROJECT);
  if (!project) throw new Error("fixture project missing");
  const title = `edit-cancel-plan-${Date.now()}`;
  const created = await api<CreatedId<SessionId>>(request, "POST", "/sessions", {
    project_path: project.repo_path,
    title,
    program: "cat",
  });
  try {
    await connect(page);
    await openSession(page, title);
    await page.locator("#edit-session-btn").click();
    await page.locator("#edit-title").fill(`${title}-cancelled`);
    let release = () => {};
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    await page.route("**/api/session-editor/plan", async (route) => {
      await gate;
      await route.continue();
    });
    const planning = page.waitForRequest("**/api/session-editor/plan");
    await page.locator("#edit-modal").getByRole("button", { name: "Save", exact: true }).click();
    await planning;
    await page.locator("#edit-modal").getByRole("button", { name: "Cancel", exact: true }).click();
    const planned = page.waitForResponse("**/api/session-editor/plan");
    release();
    await (await planned).finished();
    await page.evaluate(() => new Promise((resolve) => requestAnimationFrame(resolve)));
    await expect(page.locator("#edit-modal")).toBeHidden();
    expect((await sessionByTitle(request, title)).title).toBe(title);
  } finally {
    await api(request, "DELETE", `/sessions/${created.id}`);
  }
});
