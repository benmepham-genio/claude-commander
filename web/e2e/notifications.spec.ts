import { connect, ECHO_SESSION, expect, sessionByTitle, test } from "./support";

for (const supported of [true, false]) {
  test(`hidden tabs notify promptly with change feed ${supported ? "enabled" : "unavailable"}`, async ({
    page,
    request,
  }) => {
    const session = await sessionByTitle(request, ECHO_SESSION);
    let waiting = false;
    let baselineCaptured = false;
    let release: () => void = () => {};
    const parked = new Promise<void>(() => {});
    const changed = new Promise<void>((resolve) => {
      release = resolve;
    });
    await page.route("**/api/changes?*", async (route) => {
      if (!supported) {
        await route.fulfill({ status: 404, body: "{}" });
        return;
      }
      if (new URL(route.request().url()).searchParams.get("since") === "1") {
        await parked;
        return;
      }
      await changed;
      await route.fulfill({ contentType: "application/json", body: "1" });
    });
    await page.route("**/api/changes", async (route) => {
      if (!supported) {
        await route.fulfill({ status: 404, body: "{}" });
        return;
      }
      await route.fulfill({ contentType: "application/json", body: "0" });
    });
    await page.route("**/api/agent-states*", async (route) => {
      if (!route.request().headers().authorization) {
        await route.continue();
        return;
      }
      const response = await route.fetch();
      expect(response.ok()).toBeTruthy();
      const body = await response.json();
      body.states[session.id] = waiting ? "waiting_for_input" : "idle";
      await route.fulfill({ response, json: body });
      if (!waiting) baselineCaptured = true;
    });
    await page.addInitScript(() => {
      const received: string[] = [];
      Object.assign(window, { testNotifications: received });
      window.Notification = class {
        static permission = "granted";
        constructor(title: string) {
          received.push(title);
        }
        close() {}
      } as unknown as typeof Notification;
    });
    await connect(page);
    await expect.poll(() => baselineCaptured).toBe(true);
    await page.evaluate(() => {
      Object.defineProperty(document, "hidden", { configurable: true, get: () => true });
      Object.defineProperty(document, "visibilityState", {
        configurable: true,
        get: () => "hidden",
      });
      document.dispatchEvent(new Event("visibilitychange"));
    });
    waiting = true;
    release();
    await expect
      .poll(
        () =>
          page.evaluate(
            () => (window as unknown as { testNotifications: string[] }).testNotifications,
          ),
        { timeout: 5000 },
      )
      .toContain(`${ECHO_SESSION} — needs your input`);
  });
}
