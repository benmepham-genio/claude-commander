import type { SessionInfo } from "./generated/index.ts";

/**
 * The confirm shown before restarting `s`, or `null` when none is needed. A
 * restart is when a deferred (pending) program change takes effect, and that
 * switch starts a fresh conversation — so it is the one restart worth a warning,
 * as the TUI's `reset_confirm_message` and the Flutter client also say.
 */
export function restartConfirmation(
  s: Pick<SessionInfo, "title" | "pending_program">,
): string | null {
  if (s.pending_program === null) return null;
  return `Restart "${s.title}" with "${s.pending_program}"? This switches the program and clears the conversation.`;
}
