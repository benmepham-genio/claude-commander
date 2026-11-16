import type { Page } from "@playwright/test";
import {
  api,
  connect,
  expect,
  openSession,
  REVIEW_FILE,
  REVIEW_SESSION,
  sessionByTitle,
  test,
} from "./support";

interface Snapshot {
  reviewed: string[];
  comments: { file: string; side: string; line_range: [number, number]; comment: string }[];
}

/** Closes the review, then reopens it over a synthetic 5000-line single-file diff. */
async function openLargeDiff(page: Page): Promise<void> {
  await page.locator("#review-close").click();
  await page.route("**/api/sessions/*/review", async (route) => {
    const response = await route.fetch();
    const snapshot = await response.json();
    snapshot.diff.files[0].hunks = [
      {
        old_start: 1,
        old_lines: 0,
        new_start: 1,
        new_lines: 5000,
        header: "",
        lines: Array.from({ length: 5000 }, (_, i) => ({
          origin: "addition",
          old_lineno: null,
          new_lineno: i + 1,
          content: `large-line-${i}`,
        })),
      },
    ];
    await route.fulfill({ response, json: snapshot });
  });
  await page.locator("#review-btn").click();
}

test.describe("review view", () => {
  test.beforeEach(async ({ page }) => {
    await connect(page);
    await openSession(page, REVIEW_SESSION);
    await page.locator("#review-btn").click();
    await expect(page.locator("#review-view")).toBeVisible();
    await expect(page.locator("#review-title")).toHaveText(`Review — ${REVIEW_SESSION}`);
  });

  test("large single-file diffs render only the rows near the viewport", async ({ page }) => {
    await openLargeDiff(page);
    await expect(page.locator(".rv-line").first()).toBeVisible();
    const rendered = (text: string) =>
      page
        .locator(".rv-line", { hasText: new RegExp(`${text}$`) })
        .evaluate((n) => n.checkVisibility({ contentVisibilityAuto: true }));
    expect(await rendered("large-line-0")).toBe(true);
    expect(await rendered("large-line-4999")).toBe(false);
    await page.locator("#review-body").evaluate((el) => {
      el.scrollTop = el.scrollHeight;
    });
    await expect(page.locator(".rv-line", { hasText: "large-line-4999" })).toBeVisible();
    await expect.poll(() => rendered("large-line-0")).toBe(false);
  });

  test("a far-offscreen row of a large diff is in the page for the browser's find", async ({
    page,
  }) => {
    await openLargeDiff(page);
    await expect(page.locator(".rv-line").first()).toBeVisible();
    // Without scrolling: the last row must already be in the DOM, or Ctrl+F
    // cannot see it, yet its rendering must be skipped while it is offscreen.
    const last = page.locator(".rv-line", { hasText: /large-line-4999$/ });
    await expect(last).toHaveCount(1);
    expect(await last.evaluate((n) => n.checkVisibility({ contentVisibilityAuto: true }))).toBe(
      false,
    );
    // window.find runs the browser's text search over the DOM; headless
    // Chromium's window.find selects a match without scrolling to it, so
    // reveal the selection as find-in-page would and check the row renders.
    const found = await page.evaluate(() => {
      const hit = (window as unknown as { find(s: string): boolean }).find("large-line-4999");
      getSelection()?.anchorNode?.parentElement?.scrollIntoView({ block: "center" });
      return hit;
    });
    expect(found).toBe(true);
    await expect(last).toBeInViewport();
    await expect
      .poll(() => last.evaluate((n) => n.checkVisibility({ contentVisibilityAuto: true })))
      .toBe(true);
  });

  test("a reload keeps a deep scroll position in a large diff", async ({ page }) => {
    await openLargeDiff(page);
    // Scroll down a page at a time so every chunk above the target has been
    // rendered at its real height, as it has for a reader who scrolled there.
    const target = await page.locator("#review-body").evaluate(async (el) => {
      const frame = () => new Promise((r) => requestAnimationFrame(r));
      const rendered = () =>
        [...el.querySelectorAll(".rv-line")].some(
          (r) =>
            r.textContent?.endsWith("large-line-2500") &&
            r.checkVisibility({ contentVisibilityAuto: true }),
        );
      do {
        el.scrollTop += el.clientHeight;
        await frame();
        await frame();
      } while (!rendered());
      const box = el.getBoundingClientRect();
      const hit = document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2);
      return hit?.closest(".rv-line")?.textContent?.match(/large-line-\d+$/)?.[0] ?? "";
    });
    expect(target).toMatch(/^large-line-\d+$/);
    const row = page.locator(".rv-line", { hasText: new RegExp(`${target}$`) });
    await expect(row).toBeInViewport();
    const before = await row.elementHandle();
    // Refresh rebuilds the view the same way a comment add/delete/apply does.
    await page.locator("#review-refresh").click();
    await expect.poll(() => before?.evaluate((n) => n.isConnected)).toBe(false);
    await expect(row).toBeInViewport();
  });

  test("a reload with a composer open above the viewport keeps rows in place", async ({ page }) => {
    await openLargeDiff(page);
    await page.locator(".rv-line", { hasText: /large-line-10$/ }).click();
    await expect(page.locator(".rv-composer")).toBeVisible();
    const body = page.locator("#review-body");
    const frames = () =>
      page.evaluate(
        () => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r))),
      );
    // Deep enough that the composer's chunk is offscreen, and stops rendering.
    await body.evaluate((el) => {
      el.scrollTop = 30_000;
    });
    await frames();
    const row = page.locator(".rv-line", { hasText: /large-line-2000$/ });
    const y = () => row.evaluate((n) => n.getBoundingClientRect().top);
    const before = await y();
    const old = await row.elementHandle();
    await page.locator("#review-refresh").click();
    await expect.poll(() => old?.evaluate((n) => n.isConnected)).toBe(false);
    await expect(page.locator(".rv-composer")).toHaveCount(0);
    expect(await y()).toBeCloseTo(before, 0);
    // Rendering the composer's chunk again must not change the height it was
    // rebuilt at: return to the same scrollTop and the row is where it was.
    const top = await body.evaluate((el) => el.scrollTop);
    await body.evaluate((el) => {
      el.scrollTop = 0;
    });
    await frames();
    await body.evaluate((el, t) => {
      el.scrollTop = t;
    }, top);
    await frames();
    expect(await y()).toBeCloseTo(before, 0);
  });

  test("a reload with an unfocused composer in a skipped chunk keeps rows in place", async ({
    page,
  }) => {
    await openLargeDiff(page);
    await page.locator(".rv-line", { hasText: /large-line-10$/ }).click();
    const composer = page.locator(".rv-composer");
    await expect(composer).toBeVisible();
    // With neither focus nor the click's caret in it, as after the reader
    // clicks elsewhere, nothing holds the composer's chunk relevant, so once
    // it is offscreen its rendering is skipped: size containment then answers
    // its height with the stand-in, which does not shrink as the composer goes.
    await composer.locator("textarea").evaluate((ta) => {
      (ta as HTMLTextAreaElement).blur();
      getSelection()?.removeAllRanges();
    });
    const body = page.locator("#review-body");
    const frames = () =>
      page.evaluate(
        () => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r))),
      );
    await body.evaluate((el) => {
      el.scrollTop = 30_000;
    });
    await frames();
    await expect
      .poll(() => composer.evaluate((n) => n.checkVisibility({ contentVisibilityAuto: true })))
      .toBe(false);
    const row = page.locator(".rv-line", { hasText: /large-line-2000$/ });
    const y = () => row.evaluate((n) => n.getBoundingClientRect().top);
    const before = await y();
    const old = await row.elementHandle();
    await page.locator("#review-refresh").click();
    await expect.poll(() => old?.evaluate((n) => n.isConnected)).toBe(false);
    await expect(composer).toHaveCount(0);
    expect(await y()).toBeCloseTo(before, 0);
    // And the composer's chunk was rebuilt at its height without the composer:
    // rendering it again leaves the row where it was.
    const top = await body.evaluate((el) => el.scrollTop);
    await body.evaluate((el) => {
      el.scrollTop = 0;
    });
    await frames();
    await body.evaluate((el, t) => {
      el.scrollTop = t;
    }, top);
    await frames();
    expect(await y()).toBeCloseTo(before, 0);
  });

  test("renders the session's diff against its base", async ({ page }) => {
    const file = page.locator(".rv-file").filter({ hasText: REVIEW_FILE });
    await expect(file.locator(".rv-file-path")).toHaveText(REVIEW_FILE);
    await expect(file.locator(".rv-file-status")).toHaveText("modified");
    await expect(file.locator(".rv-hunk-header").first()).toContainText("@@");
    // cc_dirty_worktree's edit: one line inserted into acquire(), plus reapers.
    await expect(
      file.locator(".rv-line.addition", { hasText: "self.reap_expired();" }),
    ).toHaveCount(1);
    // The gutter is the row's ::before: an addition shows only its new number.
    const gutter = await file
      .locator(".rv-line.addition", { hasText: "self.reap_expired();" })
      .evaluate((n) => getComputedStyle(n, "::before").content);
    expect(gutter).toMatch(/^"\s+\d+"$/);
    // The code column starts where comments and the composer are indented to
    // (`.rv-comment`'s left margin), so a comment lines up under its code.
    // `*`'s border-box does not reach a pseudo-element, so this pins it.
    const codeIndent = await file
      .locator(".rv-line.addition", { hasText: "self.reap_expired();" })
      .evaluate((row) => {
        const content = row.querySelector(".rv-content");
        if (!content) throw new Error("row has no .rv-content");
        return content.getBoundingClientRect().left - row.getBoundingClientRect().left;
      });
    expect(codeIndent).toBe(84);
    await expect(page.locator("#review-status")).toContainText("1 file(s)");

    await page.locator("#review-close").click();
    await expect(page.locator("#review-view")).toBeHidden();
  });

  test("add a comment on a diff line", async ({ page, request, dialogs }) => {
    const s = await sessionByTitle(request, REVIEW_SESSION);
    const text = `e2e comment ${Date.now()}`;

    await page.locator(".rv-line.addition", { hasText: "self.reap_expired();" }).click();
    const composer = page.locator(".rv-composer");
    await expect(composer).toBeVisible();
    await composer.locator("textarea").fill(text);
    await composer.getByRole("button", { name: "Comment" }).click();

    await expect(composer).toBeHidden();
    await expect(page.locator(".rv-comment", { hasText: text })).toBeVisible();
    const snap = await api<Snapshot>(request, "GET", `/sessions/${s.id}/review`);
    const c = snap.comments.find((x) => x.comment === text);
    expect(c).toMatchObject({ file: REVIEW_FILE, side: "new" });
    expect(dialogs).toEqual([]);
  });

  test("toggle a file reviewed and back", async ({ page, request }) => {
    const s = await sessionByTitle(request, REVIEW_SESSION);
    const reviewed = async () =>
      (await api<Snapshot>(request, "GET", `/sessions/${s.id}/review`)).reviewed;
    const box = page
      .locator(".rv-file")
      .filter({ hasText: REVIEW_FILE })
      .locator(".rv-reviewed input[type=checkbox]");

    await expect(box).not.toBeChecked();
    await box.check();
    await expect.poll(reviewed).toContain(REVIEW_FILE);

    // Survives a reload of the review from the server.
    await page.locator("#review-refresh").click();
    await expect(box).toBeChecked();

    await box.uncheck();
    await expect.poll(reviewed).not.toContain(REVIEW_FILE);
  });
});
