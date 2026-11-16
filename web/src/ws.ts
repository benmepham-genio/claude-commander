// The /ws/attach protocol, minus the socket: frame builders, control-frame
// parsing, and `AttachLifecycle`, which decides what a close means. Pure, so
// `node --test` covers it; terminal.ts owns the actual WebSocket.
//
// Binary frames are raw PTY bytes; text frames are JSON `ClientControl` (sent)
// and `ServerControl` (received).

import { WS_ERR_AUTH, WS_ERR_NO_SESSION } from "./generated/constants.ts";
import type { AttachKind, ClientControl, ServerControl } from "./generated/index.ts";

export function wsAttachUrl(loc: Pick<Location, "protocol" | "host">): string {
  const proto = loc.protocol === "https:" ? "wss:" : "ws:";
  return `${proto}//${loc.host}/ws/attach`;
}

export function authFrame(token: string): ClientControl {
  return { type: "auth", token };
}

/**
 * The attach handshake. It carries the terminal's size so the server spawns
 * `tmux attach` at the right geometry: sizing with a resize *after* it lets
 * tmux paint an 80x24 screen first, which xterm reflows and tmux's incremental
 * repaint never clears (#281).
 */
export function attachFrame(
  sessionId: string,
  kind: AttachKind,
  size: { cols: number; rows: number } | null,
): ClientControl {
  const frame: Extract<ClientControl, { type: "attach" }> = {
    type: "attach",
    session_id: sessionId,
  };
  // `agent` is the default and omitted on the wire (protocol's AttachKind).
  if (kind === "shell") frame.kind = "shell";
  if (size?.cols && size.rows) {
    frame.cols = size.cols;
    frame.rows = size.rows;
  }
  return frame;
}

export function resizeFrame(cols: number, rows: number): ClientControl {
  return { type: "resize", cols, rows };
}

/**
 * Ask the server to force a full repaint of the pane (`tmux refresh-client`).
 * Sent after a resize: a local xterm `fit()` reflows the browser buffer to the
 * new width *before* tmux is told the new size, and tmux's post-resize repaint
 * is incremental (no full-screen clear), so any line xterm re-wrapped at the
 * new width but tmux still believes is at the old one is never corrected —
 * words run together and glyphs overlap. A full repaint clears that desync.
 */
export function refreshFrame(): ClientControl {
  return { type: "refresh" };
}

export function parseControl(text: string): ServerControl | null {
  try {
    const msg = JSON.parse(text) as ServerControl;
    return msg && typeof msg === "object" && typeof msg.type === "string" ? msg : null;
  } catch {
    return null;
  }
}

/**
 * How to read a `ServerControl.error` message. The two final ones are
 * protocol constants (`ws::WS_ERR_AUTH`, `ws::WS_ERR_NO_SESSION` — the latter
 * may carry a `: detail` suffix, which the Rust client's `handshake_error`
 * accepts too); anything else is transient.
 */
export function classifyError(message: string): "auth" | "no_session" | "other" {
  if (message === WS_ERR_AUTH) return "auth";
  if (message === WS_ERR_NO_SESSION || message.startsWith(`${WS_ERR_NO_SESSION}: `)) {
    return "no_session";
  }
  return "other";
}

/** What a close of the attach socket should lead to. */
export type CloseDecision =
  /** Transient (network blip, server restart, backgrounded tab): try again. */
  | { kind: "reconnect"; delayMs: number }
  /** The token was rejected: back to the connect screen; retrying can't help. */
  | { kind: "auth" }
  /**
   * The session does not exist, or its program exited (`detached` with
   * `session_ended`): say so; retrying can't help. A restart re-attaches
   * explicitly.
   */
  | { kind: "gone"; message: string }
  /**
   * The pane was detached while its session lives on (`detached` with
   * `client_detached`): another viewer opened it, or Ctrl-b d. Stop, and wait
   * for a click — every attach detaches the others, so reattaching on our own,
   * now or on a probe, would only displace that viewer in turn.
   */
  | { kind: "detached" };

export const RECONNECT_BASE_MS = 1000;
export const RECONNECT_MAX_MS = 15_000;
/**
 * How long an attach must stay up after `ready` before it counts as a success
 * that resets the backoff. Without it an attach that is accepted and then
 * dropped at once (a crash-looping pane) would reconnect every second forever.
 */
export const ATTACH_STABLE_MS = 5000;

/**
 * One attach — a session + pane — across however many sockets it takes. Fed
 * each control frame, asked on every close what to do next. Reconnects back
 * off exponentially (capped) until an attach has stayed up for
 * `ATTACH_STABLE_MS`, so a server that is down, or an attach that keeps
 * dropping, is not hammered at a fixed rate.
 */
export class AttachLifecycle {
  private failures = 0;
  private final: CloseDecision | null = null;
  private readyAt: number | null = null;
  private readonly now: () => number;

  constructor(now: () => number = () => Date.now()) {
    this.now = now;
  }

  /** A line to show in the terminal for this control frame, if any. */
  onControl(msg: ServerControl): string | null {
    switch (msg.type) {
      case "ready":
        // The pane streams in over binary frames; nothing to show.
        this.readyAt = this.now();
        return null;
      case "detached":
        // Sent by the server's pump as it tears an attach down
        // (crates/claude-commander-server/src/ws/attach.rs, `pump`). Once its
        // attach client has gone it reads why from the client's exit line:
        // `session_ended` if the session was destroyed, `client_detached` if
        // it lives on but the pane was taken over.
        if (msg.reason === "session_ended") {
          this.final = { kind: "gone", message: "session ended" };
        } else if (msg.reason === "client_detached") {
          this.final = { kind: "detached" };
        }
        return `\r\n\x1b[90m[detached: ${msg.reason ?? ""}]\x1b[0m\r\n`;
      case "error": {
        const message = msg.message ?? "";
        const kind = classifyError(message);
        if (kind === "auth") this.final = { kind: "auth" };
        else if (kind === "no_session") this.final = { kind: "gone", message };
        return `\r\n\x1b[91m[error: ${message}]\x1b[0m\r\n`;
      }
      default:
        return null;
    }
  }

  onClose(): CloseDecision {
    if (this.final) return this.final;
    if (this.readyAt !== null && this.now() - this.readyAt >= ATTACH_STABLE_MS) this.failures = 0;
    this.readyAt = null;
    const delayMs = Math.min(RECONNECT_BASE_MS * 2 ** this.failures, RECONNECT_MAX_MS);
    this.failures++;
    return { kind: "reconnect", delayMs };
  }
}

/** Why the current attach stopped for good, if it did. */
export type Halt = "auth" | "gone" | "detached" | null;

/**
 * What clicking session `clicked` does, given the current selection and how
 * its attach stands. Re-clicking the selected session is how a user retries an
 * attach that ended ("session ended" after the TUI restarted it) or takes back
 * one that was detached (another viewer opened it, a Ctrl-b d);
 * while it is live, or waiting on the connect screen, it does nothing.
 */
export function selectAction(
  selected: string | null,
  clicked: string,
  halt: Halt,
): "attach" | "reattach" | "none" {
  if (selected !== clicked) return "attach";
  return halt === "gone" || halt === "detached" ? "reattach" : "none";
}

export const GONE_PROBE_BASE_MS = 3000;
export const GONE_PROBE_MAX_MS = 60_000;

/**
 * Brings back an attach that ended "gone" once there is a pane to attach to
 * again, fed the selected session's status from each poll.
 *
 * The session's pane can come back without this tab doing anything: the TUI,
 * the CLI or another tab restarts it. (A pane that was only detached ends
 * "detached", not "gone", and is never probed: re-attaching would displace
 * whoever took it.) So:
 * - seen not running and then running again (killed, then restarted):
 *   re-attach at once;
 * - running throughout (a restart faster than a poll): the snapshot can't
 *   tell, so probe — re-attach after a delay that doubles, to a cap, with
 *   each probe that ends "gone" again;
 * - missing from the snapshot (deleted): never.
 */
export class GoneRecovery {
  private readonly now: () => number;
  private goneAt: number | null = null;
  private sawDown = false;
  private probes = 0;

  constructor(now: () => number = () => Date.now()) {
    this.now = now;
  }

  /** The attach just ended "gone" (including a probe that found nothing). */
  onGone(): void {
    this.goneAt = this.now();
    this.sawDown = false;
  }

  /**
   * An attach reached `ready`: the pane is back, so a later end starts the
   * probe delay over from the base rather than where the last outage left it.
   */
  onReady(): void {
    this.goneAt = null;
    this.sawDown = false;
    this.probes = 0;
  }

  /** A poll saw the selected session with `status`; true means re-attach now. */
  onPoll(status: string | undefined): boolean {
    if (this.goneAt === null || status === undefined) return false;
    if (status !== "running") {
      this.sawDown = true;
      return false;
    }
    const wait = Math.min(GONE_PROBE_BASE_MS * 2 ** this.probes, GONE_PROBE_MAX_MS);
    if (!this.sawDown && this.now() - this.goneAt < wait) return false;
    if (!this.sawDown) this.probes++;
    this.goneAt = null;
    return true;
  }
}
