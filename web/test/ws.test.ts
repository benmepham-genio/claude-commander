import assert from "node:assert/strict";
import { describe, test } from "node:test";
import { WS_ERR_AUTH, WS_ERR_NO_SESSION } from "../src/generated/constants.ts";
import {
  ATTACH_STABLE_MS,
  AttachLifecycle,
  attachFrame,
  GONE_PROBE_BASE_MS,
  GONE_PROBE_MAX_MS,
  GoneRecovery,
  parseControl,
  RECONNECT_MAX_MS,
  refreshFrame,
  resizeFrame,
  selectAction,
  wsAttachUrl,
} from "../src/ws.ts";

describe("attach frames", () => {
  test("the URL follows the page's scheme", () => {
    assert.equal(wsAttachUrl({ protocol: "http:", host: "h:1" }), "ws://h:1/ws/attach");
    assert.equal(wsAttachUrl({ protocol: "https:", host: "h" }), "wss://h/ws/attach");
  });

  test("the agent pane is the wire default and is omitted", () => {
    assert.deepEqual(attachFrame("s1", "agent", { cols: 80, rows: 24 }), {
      type: "attach",
      session_id: "s1",
      cols: 80,
      rows: 24,
    });
  });

  test("a shell attach names its kind; an unmeasured terminal sends no size", () => {
    assert.deepEqual(attachFrame("s1", "shell", null), {
      type: "attach",
      session_id: "s1",
      kind: "shell",
    });
    assert.deepEqual(attachFrame("s1", "agent", { cols: 0, rows: 0 }), {
      type: "attach",
      session_id: "s1",
    });
  });

  test("resize carries the new geometry; refresh is a bare repaint request", () => {
    assert.deepEqual(resizeFrame(120, 40), { type: "resize", cols: 120, rows: 40 });
    assert.deepEqual(refreshFrame(), { type: "refresh" });
  });

  test("parseControl rejects what is not a control frame", () => {
    assert.deepEqual(parseControl('{"type":"ready","session":"x"}'), {
      type: "ready",
      session: "x",
    });
    assert.equal(parseControl("not json"), null);
    assert.equal(parseControl("42"), null);
    assert.equal(parseControl("null"), null);
  });
});

describe("AttachLifecycle: what a close means", () => {
  const error = (message: string) => ({ type: "error", message }) as const;

  test("an auth error goes to the connect screen and never retries", () => {
    const l = new AttachLifecycle();
    l.onControl(error(WS_ERR_AUTH));
    assert.deepEqual(l.onClose(), { kind: "auth" });
  });

  test("an unknown session is final and says so", () => {
    for (const message of [WS_ERR_NO_SESSION, `${WS_ERR_NO_SESSION}: abc123`]) {
      const l = new AttachLifecycle();
      l.onControl(error(message));
      assert.deepEqual(l.onClose(), { kind: "gone", message });
    }
  });

  test("any other close reconnects, backing off to a cap", () => {
    const l = new AttachLifecycle();
    const delays = Array.from({ length: 10 }, () => {
      const d = l.onClose();
      assert.equal(d.kind, "reconnect");
      return d.kind === "reconnect" ? d.delayMs : -1;
    });
    for (let i = 1; i < delays.length; i++) {
      assert.ok((delays[i] ?? 0) >= (delays[i - 1] ?? 0), `${delays}`);
    }
    assert.ok((delays[1] ?? 0) > (delays[0] ?? 0), `${delays}`);
    assert.equal(delays.at(-1), RECONNECT_MAX_MS);
  });

  test("another error frame still reconnects", () => {
    const l = new AttachLifecycle();
    l.onControl(error("attach failed: tmux exited"));
    assert.equal(l.onClose().kind, "reconnect");
  });

  test("an attach that stayed up resets the backoff", () => {
    let now = 0;
    const l = new AttachLifecycle(() => now);
    const first = l.onClose();
    l.onClose();
    l.onClose();
    l.onControl({ type: "ready", session: "s1" });
    now += ATTACH_STABLE_MS;
    assert.deepEqual(l.onClose(), first);
  });

  test("a ready that drops at once does not reset the backoff", () => {
    let now = 0;
    const l = new AttachLifecycle(() => now);
    const delay = () => {
      const d = l.onClose();
      return d.kind === "reconnect" ? d.delayMs : -1;
    };
    const first = delay();
    const second = delay();
    l.onControl({ type: "ready", session: "s1" });
    now += 100; // dropped straight after attaching
    assert.ok(second > first);
    assert.ok(delay() > second, "a flapping attach keeps backing off");
  });

  test("the session ending is final, not a reconnect", () => {
    const l = new AttachLifecycle();
    l.onControl({ type: "ready", session: "s1" });
    l.onControl({ type: "detached", reason: "session_ended" });
    assert.deepEqual(l.onClose(), { kind: "gone", message: "session ended" });
  });

  test("a tmux detach stops instead of reconnecting", () => {
    // Another viewer opened the session (or Ctrl-b d). Reconnecting would
    // re-attach with `-d` and displace that viewer in turn, back and forth.
    const l = new AttachLifecycle();
    l.onControl({ type: "ready", session: "s1" });
    l.onControl({ type: "detached", reason: "client_detached" });
    assert.deepEqual(l.onClose(), { kind: "detached" });
  });

  test("a transport detach still reconnects", () => {
    const l = new AttachLifecycle();
    l.onControl({ type: "detached", reason: "transport" });
    assert.equal(l.onClose().kind, "reconnect");
  });
});

describe("selectAction: clicking a session row", () => {
  test("a different session attaches", () => {
    assert.equal(selectAction("a", "b", null), "attach");
    assert.equal(selectAction(null, "b", null), "attach");
  });

  test("the selected session is a no-op while its attach is live", () => {
    assert.equal(selectAction("a", "a", null), "none");
  });

  test("the selected session re-attaches once its attach ended for good", () => {
    // e.g. the TUI restarted it.
    assert.equal(selectAction("a", "a", "gone"), "reattach");
    // Displaced by another viewer: clicking takes the session back.
    assert.equal(selectAction("a", "a", "detached"), "reattach");
    // A rejected token waits on the connect screen, which resumes itself.
    assert.equal(selectAction("a", "a", "auth"), "none");
  });
});

describe("GoneRecovery: re-attaching after the session ended", () => {
  test("a session seen down and then running again is re-attached at once", () => {
    let now = 0;
    const r = new GoneRecovery(() => now);
    r.onGone();
    assert.equal(r.onPoll("stopped"), false, "killed: nothing to attach to");
    now += 100;
    assert.equal(r.onPoll("running"), true, "restarted (another tab, the TUI)");
  });

  test("a session that never looked down is probed, backing off", () => {
    // A restart faster than a poll, or a Ctrl-b d: the snapshot says running
    // throughout, so only trying tells whether a pane is there.
    let now = 0;
    const r = new GoneRecovery(() => now);
    r.onGone();
    assert.equal(r.onPoll("running"), false, "not straight away");
    now += GONE_PROBE_BASE_MS;
    assert.equal(r.onPoll("running"), true);
    // That probe ended "gone" again: the next waits longer.
    r.onGone();
    now += GONE_PROBE_BASE_MS;
    assert.equal(r.onPoll("running"), false);
    now += GONE_PROBE_BASE_MS;
    assert.equal(r.onPoll("running"), true);
  });

  test("probing backs off to a cap, never stops", () => {
    let now = 0;
    const r = new GoneRecovery(() => now);
    for (let i = 0; i < 20; i++) {
      r.onGone();
      now += GONE_PROBE_MAX_MS;
      assert.equal(r.onPoll("running"), true, `probe ${i}`);
    }
  });

  test("nothing happens until the attach has actually gone", () => {
    const r = new GoneRecovery(() => 1e9);
    assert.equal(r.onPoll("running"), false);
    assert.equal(r.onPoll(undefined), false);
  });

  test("a session missing from the snapshot is not re-attached", () => {
    let now = 0;
    const r = new GoneRecovery(() => now);
    r.onGone();
    now += GONE_PROBE_MAX_MS;
    assert.equal(r.onPoll(undefined), false);
  });

  test("a probe that attached resets the backoff for the next time", () => {
    let now = 0;
    const r = new GoneRecovery(() => now);
    // Several failed probes build the delay up...
    for (let i = 0; i < 4; i++) {
      r.onGone();
      now += GONE_PROBE_MAX_MS;
      assert.equal(r.onPoll("running"), true);
    }
    // ...then one reaches `ready`: the pane is back.
    r.onReady();
    // A later, unrelated end starts over from the base delay.
    r.onGone();
    now += GONE_PROBE_BASE_MS;
    assert.equal(r.onPoll("running"), true);
  });
});
