import {
  api,
  connect,
  expect,
  fixture,
  PROJECT,
  type ProjectInfo,
  projectGroup,
  projectHeader,
  test,
} from "./support";

test.describe("projects", () => {
  test.beforeEach(async ({ page }) => {
    await connect(page);
  });

  test("add a project by path", async ({ page, request, dialogs }) => {
    // An unchanged project's tree node survives the refresh that adds another.
    const original = await projectGroup(page, PROJECT).elementHandle();
    await page.locator("#add-project-btn").click();
    const modal = page.locator("#project-modal");
    await expect(modal).toBeVisible();
    await page.locator("#project-path").fill(fixture.addRepo);
    await modal.getByRole("button", { name: "Add repo" }).click();

    await expect(modal).toBeHidden();
    await expect(projectHeader(page, "beta")).toBeVisible();
    // A project with no sessions shows the empty placeholder.
    await expect(projectGroup(page, "beta").locator("li.empty")).toHaveText("no sessions");
    expect(await projectGroup(page, PROJECT).evaluate((node, old) => node === old, original)).toBe(
      true,
    );
    const projects = await api<ProjectInfo[]>(request, "GET", "/projects");
    expect(projects.map((p) => p.repo_path)).toContain(fixture.addRepo);
    expect(dialogs).toEqual([]);
  });

  test("scan a directory for projects", async ({ page, request, dialogs }) => {
    await page.locator("#add-project-btn").click();
    const modal = page.locator("#project-modal");
    await page.locator("#project-path").fill(fixture.scanDir);
    await modal.getByRole("button", { name: "Scan directory" }).click();

    await expect(modal).toBeHidden();
    await expect.poll(() => dialogs).toContainEqual("Scan complete: 2 added, 0 skipped.");
    await expect(projectHeader(page, "gamma")).toBeVisible();
    await expect(projectHeader(page, "delta")).toBeVisible();
    const names = (await api<ProjectInfo[]>(request, "GET", "/projects")).map((p) => p.name);
    expect(names).toEqual(expect.arrayContaining(["gamma", "delta"]));
  });
});
