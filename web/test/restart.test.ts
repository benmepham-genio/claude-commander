import assert from "node:assert/strict";
import { describe, test } from "node:test";
import { restartConfirmation } from "../src/restart.ts";

describe("restartConfirmation: the warning before a restart", () => {
  test("a pending program is named, with the conversation it discards", () => {
    const msg = restartConfirmation({ title: "task", pending_program: "codex" });
    assert.ok(msg, "a pending program must be confirmed");
    assert.match(msg, /"task"/);
    assert.match(msg, /"codex"/);
    assert.match(msg, /clears the conversation/);
  });

  test("no pending program restarts without asking", () => {
    assert.equal(restartConfirmation({ title: "task", pending_program: null }), null);
  });
});
