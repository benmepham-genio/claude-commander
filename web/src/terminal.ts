// The terminal: an xterm.js view onto a real PTY attach over /ws/attach.
//
// Owns the xterm instance, the attach socket and its reconnects, copy/select,
// the on-screen key bar and dictation. What a socket close *means* is decided
// by ws.ts's AttachLifecycle; this module only carries it out.

import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import type { Auth } from "./api.ts";
import { clearStickyConn, els, flashConn, setConn, stickConn } from "./dom.ts";
import type { AttachKind, ClientControl } from "./generated/index.ts";
import { state } from "./state.ts";
import {
  AttachLifecycle,
  attachFrame,
  authFrame,
  GoneRecovery,
  type Halt,
  parseControl,
  refreshFrame,
  resizeFrame,
  wsAttachUrl,
} from "./ws.ts";

export interface TerminalHooks {
  /**
   * The attach was refused for `token` (the one the socket authenticated
   * with): take the user to the connect screen, unless it has been replaced.
   */
  onAuthRejected(token: string | null): void;
  onRecoveryChanged?(): void;
}

let auth: Auth;
let hooks: TerminalHooks;
let term: Terminal | null = null;
let fit: FitAddon | null = null;
let ws: WebSocket | null = null;
let lifecycle = new AttachLifecycle();
/** Why the current attach ended for good, if it did: don't reopen it on our own. */
let halt: Halt = null;
/** Brings a "gone" attach back when the session's pane returns (ws.ts). */
let recovery = new GoneRecovery();
let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
let resizeTimer: ReturnType<typeof setTimeout> | null = null;
let attachKind: AttachKind = "agent";
let ctrlPending = false;
let pendingCopy = false;

export function initTerminal(a: Auth, h: TerminalHooks): void {
  auth = a;
  hooks = h;
  els.shellBtn.addEventListener("click", toggleShell);
  els.kbdBtn.addEventListener("click", () => term?.focus());
  wireKeyBar();
  els.micBtn.addEventListener("click", toggleDictation);
  document.addEventListener("visibilitychange", onVisibilityChange);
}

function ensureTerm(): Terminal {
  if (term) return term;
  const t = new Terminal({
    cursorBlink: true,
    disableStdin: false,
    fontFamily: "Menlo, Monaco, 'Courier New', monospace",
    fontSize: 13,
    scrollback: 5000,
    theme: { background: "#000000" },
    // When the remote app has mouse reporting on (Claude's TUI does), a plain
    // drag is forwarded to the app and never becomes a selection. These let the
    // user force a real selection to copy: Option(⌥)+drag on macOS, Shift+drag
    // elsewhere; right-click selects the word under the cursor.
    macOptionClickForcesSelection: true,
    rightClickSelectsWord: true,
  });
  term = t;
  fit = new FitAddon();
  t.loadAddon(fit);
  t.open(els.terminal);

  // Stop mobile keyboards mangling input (autocorrect/predictive re-sends).
  const ta = t.textarea;
  if (ta) {
    ta.setAttribute("autocorrect", "off");
    ta.setAttribute("autocapitalize", "off");
    ta.setAttribute("autocomplete", "off");
    ta.setAttribute("spellcheck", "false");
  }
  requestAnimationFrame(() => fitNow());

  // Keystrokes → raw PTY bytes (binary frame). A sticky Ctrl (from the key bar)
  // rewrites the next char to its control code. Paste arrives here too (multi-
  // char, so the sticky-Ctrl transform is skipped) and is forwarded verbatim.
  t.onData((data) => {
    let out = data;
    if (ctrlPending && data.length === 1) {
      out = String.fromCharCode(data.charCodeAt(0) & 0x1f);
      armCtrl(false);
    }
    sendData(out);
  });

  // Ctrl/Cmd+C copies the selection instead of sending SIGINT — but only when
  // there IS a selection, so a bare ^C still reaches the program. (Paste is
  // handled natively by xterm's paste event → onData above.)
  t.attachCustomKeyEventHandler((e) => {
    if (e.type !== "keydown") return true;
    // Ctrl+\ toggles between the agent pane and the paired shell, matching the
    // native client (the toggle is a client-side re-attach, so the combo can't
    // just go down the PTY).
    if (e.ctrlKey && (e.key === "\\" || e.code === "Backslash")) {
      toggleShell();
      return false;
    }
    const mod = e.ctrlKey || e.metaKey;
    if (mod && (e.key === "c" || e.key === "C") && t.hasSelection()) {
      if (copySelection()) return false;
    }
    return true;
  });

  // Copy-on-select. onSelectionChange can fire from a requestAnimationFrame
  // (outside a user gesture, where a clipboard write is blocked), so it only
  // *arms* a pending copy; the write happens on the next mouseup/touchend, a
  // real gesture. On document, so a drag released outside the terminal counts.
  t.onSelectionChange(() => {
    pendingCopy = t.hasSelection();
  });
  document.addEventListener("mouseup", flushCopyOnSelect);
  document.addEventListener("touchend", flushCopyOnSelect);
  return t;
}

// ---- clipboard ---------------------------------------------------------------

function flushCopyOnSelect(): void {
  if (!pendingCopy) return;
  pendingCopy = false;
  copySelection();
}

// Prefers the async Clipboard API (secure contexts: HTTPS or localhost) and
// falls back to execCommand for plain-HTTP pages, where navigator.clipboard is
// undefined.
function copySelection(): boolean {
  if (!term?.hasSelection()) return false;
  const sel = term.getSelection();
  if (!sel) return false;
  if (navigator.clipboard?.writeText) {
    navigator.clipboard
      .writeText(sel)
      .then(flashCopied)
      .catch(() => execCopy(sel));
  } else {
    execCopy(sel);
  }
  return true;
}

// Legacy clipboard write via a throwaway textarea — works without a secure
// context. Must run inside a user gesture. Restores terminal focus after.
function execCopy(text: string): void {
  try {
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.setAttribute("readonly", "");
    ta.style.position = "fixed";
    ta.style.top = "-1000px";
    ta.style.opacity = "0";
    document.body.append(ta);
    ta.select();
    ta.setSelectionRange(0, text.length);
    const ok = document.execCommand("copy");
    ta.remove();
    if (ok) flashCopied();
  } catch {
    // Nothing more to try.
  }
  term?.focus();
}

function flashCopied(): void {
  flashConn("copied", 1000);
}

// ---- sending -----------------------------------------------------------------

function sendControl(frame: ClientControl): void {
  if (ws && ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(frame));
}

/** Raw bytes to the PTY, as a binary frame. */
export function sendData(data: string): void {
  if (ws && ws.readyState === WebSocket.OPEN) ws.send(new TextEncoder().encode(data));
}

export function fitNow(): void {
  try {
    fit?.fit();
  } catch {
    // Not laid out yet (hidden); the next fit catches up.
  }
}

/** Debounced resize frame, for bursts of viewport events. */
export function sendResize(): void {
  if (resizeTimer) clearTimeout(resizeTimer);
  resizeTimer = setTimeout(sendResizeNow, 150);
}

function sendResizeNow(): void {
  if (!term?.cols || !term.rows) return;
  sendControl(resizeFrame(term.cols, term.rows));
  // Force a full repaint at the new size. A local xterm fit() has already
  // reflowed the browser buffer to this width, but tmux only repaints
  // incrementally after a resize, so without this any line it re-wrapped at
  // the new width stays desynced (words merge, glyphs overlap). Frames are
  // ordered, so the server applies the resize before this refresh.
  sendControl(refreshFrame());
}

// ---- the attach socket -------------------------------------------------------

/** Attach the terminal to a (newly selected) session, on its agent pane. */
export function attach(id: string): void {
  attachKind = "agent";
  updateShellButton();
  const t = ensureTerm();
  closeSocket();
  t.reset();
  startAttach(id);
}

/** Drop the attach entirely (the session is gone or deselected). */
export function detach(): void {
  closeSocket();
  halt = null;
  hooks.onRecoveryChanged?.();
}

/** How the current attach stands (for a re-click on the selected session). */
export function haltState(): Halt {
  return halt;
}

/**
 * Fed the selected session's status from every poll: an attach that ended
 * "gone" re-attaches once the session has a pane again (another tab or the TUI
 * restarted it). One that ended "detached" waits for a click instead.
 */
export function onSelectedStatus(status: string | undefined): void {
  if (halt === "gone" && state.selectedId && recovery.onPoll(status)) {
    closeSocket();
    term?.reset();
    startAttach(state.selectedId, false);
  }
}

/**
 * Re-attach after an attach that ended for good (e.g. the token was rejected
 * and the user has since reconnected). A no-op while an attach is live.
 */
export function resume(): void {
  if (halt && state.selectedId) {
    term?.reset();
    startAttach(state.selectedId);
  }
}

/**
 * Attach afresh to the selected session's current pane, whatever state the old
 * attach was in. For after a restart: the old pane's session ended (a final
 * close), and the new one needs a new attach.
 */
export function reattach(): void {
  if (!state.selectedId) return;
  closeSocket();
  term?.reset();
  startAttach(state.selectedId);
}

/**
 * A fresh attach: new lifecycle (backoff, final errors), then its first socket.
 * A user action (select, toggle, reconnect, restart) clears a sticky "session
 * ended" from the header and starts recovery over; an automatic re-attach
 * (`onSelectedStatus`) keeps both, so a failed probe backs off and the header
 * only changes once an attach is actually `ready`.
 */
function startAttach(id: string, user = true): void {
  lifecycle = new AttachLifecycle();
  halt = null;
  hooks.onRecoveryChanged?.();
  if (user) {
    recovery = new GoneRecovery();
    clearStickyConn();
  }
  openSocket(id);
}

function closeSocket(): void {
  if (reconnectTimer) {
    clearTimeout(reconnectTimer);
    reconnectTimer = null;
  }
  if (ws) {
    ws.onclose = null;
    ws.onmessage = null;
    ws.onerror = null;
    try {
      ws.close();
    } catch {
      // Already closing.
    }
    ws = null;
  }
}

// Open (or reopen) the attach socket for `id`. Reconnects on an unexpected drop
// (backgrounded mobile tab, network blip, server restart) while it stays
// selected; tmux replays the pane on re-attach, so it's seamless. A rejected
// token or a missing session ends the attach instead (see AttachLifecycle).
function openSocket(id: string): void {
  closeSocket();
  const sock = new WebSocket(wsAttachUrl(location));
  sock.binaryType = "arraybuffer";
  ws = sock;
  const life = lifecycle;
  // The token this socket authenticates with, so a refusal is pinned to it.
  let sentToken: string | null = null;

  sock.onopen = () => {
    setConn("ok", "connected");
    // Authenticate in-band (browsers can't set headers on the upgrade).
    sentToken = auth.token;
    if (sentToken) sock.send(JSON.stringify(authFrame(sentToken)));
    fitNow();
    const size = term?.cols && term.rows ? { cols: term.cols, rows: term.rows } : null;
    sock.send(JSON.stringify(attachFrame(id, attachKind, size)));
    // An older server ignores the handshake size; this resize covers it and is
    // a no-op for a server that already sized the PTY.
    sendResizeNow();
  };

  sock.onmessage = (ev: MessageEvent<string | ArrayBuffer>) => {
    if (typeof ev.data === "string") {
      const msg = parseControl(ev.data);
      if (msg?.type === "ready") {
        clearStickyConn();
        recovery.onReady();
      }
      const note = msg && life.onControl(msg);
      if (note) term?.write(note);
    } else {
      term?.write(new Uint8Array(ev.data));
    }
  };

  sock.onclose = () => {
    if (state.selectedId !== id || ws !== sock) return;
    ws = null;
    const next = life.onClose();
    switch (next.kind) {
      case "reconnect":
        setConn("error", "reconnecting…");
        scheduleReconnect(id, next.delayMs);
        break;
      case "auth":
        // Refused a token the user has since replaced: try again with the new
        // one rather than halting (the connect submit's resume() has already
        // run and found nothing halted).
        if (sentToken !== auth.token) {
          startAttach(id);
          break;
        }
        halt = "auth";
        stickConn("error", "disconnected");
        hooks.onAuthRejected(sentToken);
        break;
      case "gone":
        // The terminal already shows the error line; stop there, and keep
        // saying so in the header until the user attaches again.
        halt = "gone";
        recovery.onGone();
        hooks.onRecoveryChanged?.();
        stickConn("error", next.message);
        break;
      case "detached":
        // Deliberately no `recovery.onGone()`: its probe would re-attach and
        // displace whoever took the pane. The user clicks to take it back.
        halt = "detached";
        stickConn("error", "detached — click the session to re-attach");
        break;
    }
  };

  sock.onerror = () => setConn("error", "stream error");
}

function scheduleReconnect(id: string, delayMs: number): void {
  if (reconnectTimer) return;
  reconnectTimer = setTimeout(() => {
    reconnectTimer = null;
    if (state.selectedId === id && document.visibilityState === "visible") openSocket(id);
  }, delayMs);
}

function onVisibilityChange(): void {
  if (document.visibilityState !== "visible" || !state.selectedId || halt) return;
  if (!ws || ws.readyState > WebSocket.OPEN) {
    if (reconnectTimer) {
      clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
    openSocket(state.selectedId);
  } else if (ws.readyState === WebSocket.OPEN) {
    fitNow();
    sendResize();
  }
}

// ---- agent / shell pane ------------------------------------------------------

// Toggle between the agent pane and the paired shell by re-attaching with the
// other `kind`. The shell pane is created on demand server-side; tmux replays it
// on attach, so switching is just a fresh attach.
export function toggleShell(): void {
  if (!state.selectedId) return;
  attachKind = attachKind === "shell" ? "agent" : "shell";
  updateShellButton();
  term?.reset();
  closeSocket();
  startAttach(state.selectedId);
}

function updateShellButton(): void {
  const inShell = attachKind === "shell";
  els.shellBtn.textContent = inShell ? "Agent" : "Shell";
  els.shellBtn.title = inShell ? "Back to the agent pane (Ctrl+\\)" : "Toggle shell pane (Ctrl+\\)";
  els.shellBtn.classList.toggle("sticky-active", inShell);
}

// ---- mobile: on-screen keys --------------------------------------------------

const KEY_SEQ: Record<string, string> = {
  esc: "\x1b",
  tab: "\t",
  enter: "\r",
  up: "\x1b[A",
  down: "\x1b[B",
  right: "\x1b[C",
  left: "\x1b[D",
};

function armCtrl(on: boolean): void {
  ctrlPending = on;
  els.ctrlKey.classList.toggle("sticky-active", on);
}

function wireKeyBar(): void {
  for (const btn of els.keyBar.querySelectorAll<HTMLButtonElement>("button[data-key]")) {
    btn.addEventListener("mousedown", (e) => e.preventDefault());
    btn.addEventListener("click", () => {
      const key = btn.dataset.key ?? "";
      if (key === "ctrl") {
        armCtrl(!ctrlPending);
        return;
      }
      const seq = KEY_SEQ[key];
      if (seq) sendData(seq);
    });
  }
}

// ---- microphone dictation (Web Speech API) -----------------------------------

// Not in lib.dom: Chromium ships it prefixed, others not at all.
interface SpeechRecognitionLike {
  lang: string;
  continuous: boolean;
  interimResults: boolean;
  onresult: ((e: SpeechResultEvent) => void) | null;
  onend: (() => void) | null;
  onerror: ((e: { error: string }) => void) | null;
  start(): void;
  stop(): void;
}
interface SpeechResultEvent {
  resultIndex: number;
  results: ArrayLike<{ isFinal: boolean; 0: { transcript: string } }>;
}
type SpeechRecognitionCtor = new () => SpeechRecognitionLike;

let recognition: SpeechRecognitionLike | null = null;
let dictating = false;

function updateMicButton(): void {
  els.micBtn.classList.toggle("mic-active", dictating);
  els.micBtn.title = dictating ? "Stop dictation" : "Dictate (microphone)";
}

function toggleDictation(): void {
  if (dictating) {
    recognition?.stop();
    return;
  }
  const w = window as unknown as {
    SpeechRecognition?: SpeechRecognitionCtor;
    webkitSpeechRecognition?: SpeechRecognitionCtor;
  };
  const SR = w.SpeechRecognition ?? w.webkitSpeechRecognition;
  if (!window.isSecureContext || !SR) {
    alert(
      "Microphone dictation needs a secure (HTTPS) connection. Reach this UI over " +
        "HTTPS (e.g. via Tailscale) and the mic button will work.",
    );
    return;
  }
  const r = new SR();
  recognition = r;
  r.lang = navigator.language || "en-US";
  r.continuous = true;
  r.interimResults = false;
  r.onresult = (e) => {
    for (let i = e.resultIndex; i < e.results.length; i++) {
      const res = e.results[i];
      if (res?.isFinal) sendData(res[0].transcript);
    }
  };
  r.onend = () => {
    dictating = false;
    recognition = null;
    updateMicButton();
  };
  r.onerror = (e) => {
    if (e.error !== "aborted" && e.error !== "no-speech") {
      flashConn(`mic: ${e.error}`, 2000, "error");
    }
  };
  try {
    r.start();
    dictating = true;
  } catch {
    dictating = false;
    recognition = null;
  }
  updateMicButton();
}
