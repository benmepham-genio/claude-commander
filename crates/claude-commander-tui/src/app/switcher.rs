//! The in-session switcher: the real quick-switch palette, drawn over a live
//! tmux pane.
//!
//! Ctrl+Space while attached used to run `tmux display-popup` on a second
//! process of this binary, which read `state.json` off disk and drew its own
//! miniature session list. That picker could not see remote backends, could not
//! run commands, and duplicated the palette's fuzzy/scroll/click handling. Here
//! the TUI keeps the attach suspended (see [`AttachSession`]) and paints
//! [`Modal::QuickSwitch`] itself, so there is exactly one palette in the app.
//!
//! Three things make drawing over someone else's screen work:
//!
//! 1. **A fixed viewport.** ratatui's `Viewport::Fixed` is documented for
//!    "a terminal layout managed by another renderer"
//!    (`ratatui-core/src/terminal/viewport.rs`) — here that renderer is tmux.
//! 2. **A one-time opaque pre-paint.** ratatui only emits cells that differ
//!    from its previous buffer, which starts blank — so a `Clear`ed modal
//!    interior emits *nothing* and the pane shows through. (Styling it
//!    `bg(Color::Reset)` does not help: `Cell::EMPTY` is already `Color::Reset`.)
//!    Painting the rectangle with spaces once, before the first draw, makes the
//!    blank screen match ratatui's blank buffer; every later frame then diffs
//!    correctly, erasing text that goes away.
//! 3. **A repaint on the way out**, via [`AttachRefresher`] — we never have to
//!    remember what we covered.
//!
//! Known limitation: nothing here enables mouse reporting. The TUI disabled its
//! own capture before attaching, so clicks and the wheel reach the palette only
//! when the attached program left the terminal reporting them (tmux with `mouse
//! on`, typically). Turning it on for the overlay would mean changing modes the
//! attached program owns and restoring them exactly on the way out; keyboard
//! navigation works regardless, so that trade isn't worth making.

use crossterm::event::KeyEventKind;
use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::{Backend, CrosstermBackend},
    layout::Rect,
};
use std::io::Write;
use tracing::{debug, info, warn};

use super::*;
use claude_commander_core::backend::AttachStreams;
use claude_commander_core::tmux::{AttachConfig, AttachOutcome, AttachResult, AttachSession};

/// How the palette overlay closed.
#[derive(Debug)]
pub(super) enum OverlayExit {
    /// Esc, or an activation that left nothing on screen — put the user straight
    /// back in the pane they came from.
    ///
    /// `toast` carries a status message the palette raised on its way out. The
    /// TUI's own status bar is not visible from inside a pane, so resuming would
    /// otherwise swallow it: the message expires on its 4s deadline long before
    /// the user next sees the tree, and a command whose entire outcome is a toast
    /// ("No cascade in progress") looks like the palette did nothing at all.
    Cancelled { toast: Option<String> },
    /// A session row was activated.
    Session { target: AttachTarget },
    /// A command row was activated and left something for the full TUI to show
    /// (a modal, an editor to launch, or a quit), so the attach ends.
    Command,
}

impl App {
    /// Drive one attach, servicing the in-session switcher.
    ///
    /// [`claude_commander_core::tmux::run_attach`] runs an attach straight through and cannot
    /// service [`AttachResult::OpenSwitcher`]; the TUI can, because it owns the
    /// palette. On a local pick this never tears the attach down — it hands the
    /// same tmux client to another session with `switch-client`, which is what
    /// keeps switching at Alt+Tab speed.
    ///
    /// Returns core's [`Result`](claude_commander_core::Result), not this crate's:
    /// starting the attach is the only fallible step and the failure is core's, so
    /// wrapping it in a [`TuiError`](crate::TuiError) would add a layer without
    /// adding information — the same call as `actions::load_branch_entries`.
    pub(super) async fn drive_attach(
        &mut self,
        streams: AttachStreams,
        cfg: AttachConfig,
    ) -> claude_commander_core::Result<AttachOutcome> {
        let mut session = AttachSession::start(streams, cfg)?;

        loop {
            let result = session.run().await;
            if result != AttachResult::OpenSwitcher {
                return Ok(session.finish(result).await);
            }

            let came_from = session.current_session().await;
            match self.run_switcher_overlay(&came_from).await {
                OverlayExit::Cancelled { toast } => {
                    debug!("Switcher dismissed; resuming the pane");
                    // Resume first: it repaints the covered region, which would
                    // otherwise wipe the message straight back off the screen.
                    session.resume().await;
                    if let Some(msg) = toast {
                        display_message_in_pane(&came_from, &msg).await;
                    }
                }
                OverlayExit::Session { target } => {
                    // Move the client in place only when there is a client of
                    // ours to move *and* the pick is on the same (local) server;
                    // otherwise re-attach. See `switch_client_in_place`.
                    let in_place = switch_client_in_place(
                        session.local_client_tty(),
                        self.pick_local_tmux_name(&target).as_deref(),
                    )
                    .map(|(tty, name)| (tty.to_string(), name.to_string()));

                    let switched = match &in_place {
                        Some((tty, name)) => self.switch_client_to(name, tty, &came_from).await,
                        None => false,
                    };
                    match (switched, in_place) {
                        (true, Some((_, name))) => {
                            session.set_current_session(name).await;
                            // An in-place switch keeps the same attach — and so
                            // the same injection channel — but changes what is
                            // on screen. Without this the dictation policy would
                            // keep planning against the pane the user left.
                            self.conversation
                                .injector
                                .set_pane(self.pane_info_for(&target));
                            session.resume().await;
                        }
                        // Either the pick can't be reached in place, or the
                        // switch failed (a dead session `switch-client` can't
                        // create, say). Re-attach rather than resuming onto a
                        // pane the user didn't ask for.
                        _ => {
                            self.ui_state.pending_switcher_target = Some(target);
                            return Ok(session.finish(AttachResult::OpenSwitcher).await);
                        }
                    }
                }
                OverlayExit::Command => {
                    return Ok(session.finish(AttachResult::OpenSwitcher).await);
                }
            }
        }
    }

    /// Open the palette over the pane and run it until it closes.
    async fn run_switcher_overlay(&mut self, came_from: &str) -> OverlayExit {
        let toast_before = self.status_message_text();
        self.open_quick_switch_with_mode(PaletteMode::Unified).await;
        self.preselect_previous_session(came_from);

        // A constant rectangle for the whole overlay. It is sized for a full
        // list rather than the current match count so that filtering the list
        // never *shrinks* the box — shrinking would blank cells we don't own,
        // punching holes in the pane that nothing repaints until the overlay
        // closes.
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let modal_area = overlay_area(cols, rows);
        paint_opaque(modal_area);

        let mut terminal = match Terminal::with_options(
            CrosstermBackend::new(std::io::stdout()),
            TerminalOptions {
                viewport: Viewport::Fixed(modal_area),
            },
        ) {
            Ok(t) => t,
            Err(e) => {
                warn!("Failed to build the switcher overlay terminal: {e}");
                self.ui_state.modal = Modal::None;
                return OverlayExit::Cancelled { toast: None };
            }
        };

        // The attach's stdin pump is parked, so the terminal is ours to read.
        self.event_loop.restart_input();
        let exit = self.switcher_event_loop(&mut terminal).await;

        // Wait for the reader thread to exit before the stdin pump resumes:
        // handing the terminal back while both read it means the first
        // keystroke after Esc goes to whichever wins, and a split escape
        // sequence reaches the pane as junk. The pre-attach path awaits this for
        // the same reason (`app/mod.rs`, before `flush_stdin`).
        self.event_loop.stop_input().await;

        match exit {
            OverlayExit::Cancelled { .. } => OverlayExit::Cancelled {
                toast: self
                    .status_message_text()
                    .filter(|now| Some(now) != toast_before.as_ref()),
            },
            // The other exits end the attach, so the TUI shows its own toast.
            other => other,
        }
    }

    /// The status-bar message currently pending, if any.
    fn status_message_text(&self) -> Option<String> {
        self.ui_state
            .status_message
            .as_ref()
            .map(|(msg, _)| msg.clone())
    }

    /// The overlay's event loop. Keys go through the ordinary
    /// [`Self::handle_input`], so with `Modal::QuickSwitch` open the palette's
    /// own handling — fuzzy filtering, navigation, Tab-completion, wheel and
    /// double-click — applies unchanged; there is no second key map to drift.
    ///
    /// Background results are not applied under the palette, but they are not
    /// dropped either: each is the only report of its task, and some clear an
    /// in-flight flag (`lfs_pull_in_flight`, `reviewed_poll_in_flight`) that
    /// nothing else resets. They are handed back to the event queue on exit, so
    /// the main loop applies them once the attach ends.
    pub(super) async fn switcher_event_loop<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
    ) -> OverlayExit {
        let mut deferred = Vec::new();
        let exit = self
            .switcher_event_loop_inner(terminal, &mut deferred)
            .await;
        self.event_loop.requeue(deferred);
        exit
    }

    async fn switcher_event_loop_inner<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        deferred: &mut Vec<AppEvent>,
    ) -> OverlayExit {
        loop {
            if let Err(e) = terminal.draw(|f| self.render_switcher_overlay(f)) {
                warn!("Switcher overlay render failed: {e}");
                self.ui_state.modal = Modal::None;
                return OverlayExit::Cancelled { toast: None };
            }

            let Some(event) = self.event_loop.next().await else {
                self.ui_state.modal = Modal::None;
                return OverlayExit::Cancelled { toast: None };
            };

            match event {
                // Release/repeat events would double a typed character and
                // double-fire Esc. The normal TUI never sees them (it doesn't ask
                // for `REPORT_EVENT_TYPES`), but the overlay reads the terminal in
                // whatever keyboard-protocol state the attached program negotiated,
                // so filter here rather than trusting that.
                AppEvent::Input(InputEvent::Key(k)) if k.kind != KeyEventKind::Press => continue,
                AppEvent::Input(input) => self.handle_input(input).await,
                // The tree isn't visible under the palette and the pane is frozen
                // anyway, so background results can wait until we resume; only
                // the palette's own state matters here. Hold them for the main
                // loop rather than dropping them (see `switcher_event_loop`).
                update @ AppEvent::StateUpdate(_) => {
                    deferred.push(update);
                    continue;
                }
                // A tick redraws (top of the loop) and advances the animation
                // clock — the status glyphs include a spinner, which would
                // otherwise sit frozen on whatever frame the TUI last drew.
                AppEvent::Tick => {
                    self.ui_state.tick_count = self.ui_state.tick_count.wrapping_add(1);
                    continue;
                }
                AppEvent::Quit => self.ui_state.should_quit = true,
            }

            if matches!(self.ui_state.modal, Modal::QuickSwitch { .. }) {
                continue;
            }
            return self.classify_overlay_exit();
        }
    }

    /// Work out what the palette did, now that it has closed.
    fn classify_overlay_exit(&mut self) -> OverlayExit {
        // A session row runs the ordinary `handle_select`, which stages an attach
        // request and asks the main loop to exit. We are not the main loop, so
        // take both back.
        if let Some(target) = self.ui_state.attach_request.take() {
            self.ui_state.should_quit = false;
            info!("Switcher picked {target:?}");
            return OverlayExit::Session { target };
        }
        match overlay_exit_kind(
            self.ui_state.should_quit,
            self.ui_state.editor_command.is_some(),
            matches!(self.ui_state.modal, Modal::None),
        ) {
            ExitKind::Command => OverlayExit::Command,
            // The toast is attached by `run_switcher_overlay`, which knows what
            // was pending before the palette opened.
            ExitKind::Cancelled => OverlayExit::Cancelled { toast: None },
        }
    }

    /// The tmux name a pick resolves to, but only when it lives on the **local**
    /// backend — the only sessions `tmux switch-client` can reach.
    fn pick_local_tmux_name(&self, target: &AttachTarget) -> Option<String> {
        (self.attach_target_backend(target) == LOCAL_BACKEND_ID)
            .then(|| self.attach_target_name(target))
            .flatten()
    }

    /// Render the palette into the overlay's fixed viewport. Unlike the in-TUI
    /// path this must not emit `Clear`: the rectangle was already painted opaque,
    /// and `Clear` would diff as "no change" and leave it transparent.
    fn render_switcher_overlay(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        // Published so a click maps onto exactly the rows drawn — the same
        // contract `render_modal` keeps for the in-TUI palette.
        let rows_area = overlay_rows_area(area);
        self.ui_state.modal_list_rect = Some(rows_area);

        let Modal::QuickSwitch {
            mode,
            query,
            matches,
            selected_idx,
            scroll,
            // The overlay palette is never opened over a review view — the
            // review is a TUI screen, not something an attached pane shows.
            review: _,
        } = &self.ui_state.modal
        else {
            return;
        };
        self.render_quick_switch(
            frame,
            area,
            rows_area,
            *mode,
            query,
            matches,
            *selected_idx,
            *scroll,
        );
    }

    /// Start the highlight on the first row that isn't the session we came from,
    /// preserving the Alt+Tab feel the popup switcher had: with an empty query
    /// the palette is in most-recently-attached order, and the session you are
    /// *in* is the most recent of all, so row 0 would be a no-op.
    fn preselect_previous_session(&mut self, came_from: &str) {
        // Resolve session ids to tmux names up front, so the index maths below
        // is a pure function of the palette's own rows.
        let came_from_ids: Vec<SessionId> = self
            .backends
            .iter()
            .flat_map(|h| h.view.snapshot.sessions.iter())
            .filter(|s| {
                s.tmux_session_name == came_from
                    // The Ctrl+\ shell pane counts as the same session: coming
                    // from `foo-sh`, `foo` is still where we already are.
                    || came_from.strip_suffix("-sh") == Some(s.tmux_session_name.as_str())
            })
            .map(|s| s.session_id)
            .collect();

        let Modal::QuickSwitch {
            matches,
            selected_idx,
            ..
        } = &mut self.ui_state.modal
        else {
            return;
        };
        *selected_idx = first_other_session(matches, &came_from_ids);
    }

    /// `tmux switch-client` **our** client (`client_tty`) onto `name`, reviving it
    /// first if its tmux session died (e.g. after a reboot) — `switch-client`
    /// cannot create sessions. Returns whether the client actually moved.
    async fn switch_client_to(&mut self, name: &str, client_tty: &str, came_from: &str) -> bool {
        // On a revive error, still try the raw name: the pick may exist in tmux
        // without being in commander's state.
        let target = match self.local_backend() {
            Some(local) => match local.service().ensure_attachable_by_tmux_name(name).await {
                Ok(revived) => revived,
                Err(e) => {
                    warn!("Failed to revive picked session {name}: {e}");
                    name.to_string()
                }
            },
            None => name.to_string(),
        };

        // `-c` names the client to move. Without it tmux picks "the current
        // client", which is resolved from the *environment* — so with commander
        // itself running inside tmux, or a second terminal attached to the same
        // server, an un-targeted switch can yank someone else's client to this
        // session and leave ours where it was.
        match tokio::process::Command::new("tmux")
            .args(["switch-client", "-c", client_tty, "-t", &target])
            .status()
            .await
        {
            Ok(s) if s.success() => true,
            Ok(s) => {
                warn!("tmux switch-client exited with {:?}", s.code());
                // Surface it in the pane the user is still on; without this a
                // failed switch looks like the palette did nothing.
                display_message_in_pane(
                    came_from,
                    &format!("Could not switch to session {target}"),
                )
                .await;
                false
            }
            Err(e) => {
                warn!("Failed to spawn tmux switch-client: {e}");
                false
            }
        }
    }
}

/// Show `msg` on the status line of the pane the user is sitting in.
///
/// Best-effort, and local-only: it runs against the operator's own tmux, so for
/// a remote session it simply targets a name that isn't there — the same trade
/// the voice-input feedback already makes.
async fn display_message_in_pane(target: &str, msg: &str) {
    let _ = tokio::process::Command::new("tmux")
        .args(["display-message", "-t", target, msg])
        .status()
        .await;
}

/// Whether the switcher may move the user in place with `tmux switch-client`,
/// and if so which client to move onto which session.
///
/// Both halves are required and neither is inferable from the other:
///
/// - `local_client_tty` is `Some` only for a **local** attach. A remote attach's
///   tmux client lives on the server, so a local `switch-client` cannot move it —
///   and is not merely a no-op, since it may succeed against an *unrelated* local
///   client and drag that one somewhere the user never asked to go, while the
///   remote pane the user is actually looking at stays put.
/// - `pick_local_tmux_name` is `Some` only when the *picked session* is on the
///   local backend, since that is all `switch-client` can address.
///
/// Anything else falls back to ending the attach and re-attaching, which is
/// slower but always correct.
pub(super) fn switch_client_in_place<'a>(
    local_client_tty: Option<&'a str>,
    pick_local_tmux_name: Option<&'a str>,
) -> Option<(&'a str, &'a str)> {
    local_client_tty.zip(pick_local_tmux_name)
}

/// What the palette left behind, once it is closed and not a session pick.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ExitKind {
    /// Something is waiting in the full TUI; end the attach so the user sees it.
    Command,
    /// Nothing to show: put the user straight back in the pane.
    Cancelled,
}

/// Decide between them from the state the palette left.
///
/// Deliberately *not* keyed on "was the closing key Esc". That reads the user's
/// intent from a keystroke, and gets it wrong in both directions: a command that
/// completes silently would tear the attach down to show the user nothing, and a
/// binding that resolved Esc to an action would be mistaken for a dismissal.
/// Asking what is actually waiting on screen answers the real question — is there
/// anything for the user to come back *to*?
pub(super) fn overlay_exit_kind(
    should_quit: bool,
    has_editor_command: bool,
    modal_is_none: bool,
) -> ExitKind {
    if should_quit || has_editor_command || !modal_is_none {
        ExitKind::Command
    } else {
        ExitKind::Cancelled
    }
}

/// Index of the first row that isn't one of `came_from`, or 0 when there is no
/// such row (an empty list, or one holding only that session).
///
/// This is what keeps the Alt+Tab feel the `display-popup` picker had. That one
/// filtered the attached session out of its list entirely; the palette instead
/// shows every session — it's the same palette everywhere — and just starts the
/// highlight past the one you're already in, so Enter is still a single
/// keystroke to the previous session.
///
/// Pure over the palette's own rows, so the behaviour is testable without a
/// terminal or a backend.
pub(super) fn first_other_session(matches: &[QuickSwitchItem], came_from: &[SessionId]) -> usize {
    matches
        .iter()
        .position(|item| match item {
            QuickSwitchItem::Session(m) => !came_from.contains(&m.session_id),
            // A command row is never "the session we came from".
            _ => true,
        })
        .unwrap_or(0)
}

/// The rectangle the overlay reserves over the pane, for a `cols`×`rows`
/// terminal.
///
/// Sized for a *full* list rather than the current match count, so filtering the
/// list never shrinks the box. A shrinking box would blank cells the overlay
/// doesn't own — punching holes in the pane that nothing repaints until it
/// closes — because ratatui erases whatever its previous frame drew.
pub(super) fn overlay_area(cols: u16, rows: u16) -> Rect {
    modals::quick_switch_areas(Rect::new(0, 0, cols, rows), actions::LIST_MAX_VISIBLE).0
}

/// The selectable rows inside [`overlay_area`]: border(1) + input(1) off the
/// top, border(1) off the bottom.
pub(super) fn overlay_rows_area(area: Rect) -> Rect {
    Rect {
        x: area.x + 1,
        y: area.y + 2,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(3),
    }
}

/// Paint `area` with spaces at the terminal's default colours, so the region the
/// overlay is about to draw into is opaque and matches ratatui's blank starting
/// buffer. See the module docs for why the first draw cannot do this itself.
fn paint_opaque(area: Rect) {
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(opaque_fill(area).as_bytes());
    let _ = stdout.flush();
}

/// The escape sequence [`paint_opaque`] writes. Split out so the cursor
/// addressing is pinned by a test: it is the one piece of raw ANSI here, an
/// off-by-one would misplace the whole overlay against the box ratatui then
/// draws, and nothing else in the codebase would catch it.
fn opaque_fill(area: Rect) -> String {
    if area.width == 0 || area.height == 0 {
        return String::new();
    }
    let mut out = String::with_capacity(area.height as usize * (area.width as usize + 8));
    // SGR reset first, so a colour left set by the pane doesn't tint the fill.
    out.push_str("\x1b[0m");
    let blank = " ".repeat(area.width as usize);
    for y in area.y..area.y + area.height {
        // Terminal cursor addressing is 1-based; ratatui's Rect is 0-based.
        out.push_str(&format!("\x1b[{};{}H{blank}", y + 1, area.x + 1));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_row(id: SessionId) -> QuickSwitchItem {
        QuickSwitchItem::Session(QuickSwitchMatch {
            session_id: id,
            title: "t".to_string(),
            branch: "b".to_string(),
            project_name: "p".to_string(),
            status: SessionStatus::Running,
            agent_state: None,
            unread: false,
            last_attached_at: None,
            workspace: None,
            other_workspace: None,
        })
    }

    fn command_row() -> QuickSwitchItem {
        QuickSwitchItem::Command(CommandEntry {
            action: claude_commander_core::config::keybindings::BindableAction::NewSession,
            label: "New Session",
            keys: "n".to_string(),
        })
    }

    // -- The switch-client fast path --

    #[test]
    fn switch_in_place_needs_our_own_client_and_a_local_pick() {
        assert_eq!(
            switch_client_in_place(Some("/dev/pts/3"), Some("cc-abc")),
            Some(("/dev/pts/3", "cc-abc")),
            "a local attach picking a local session switches in place"
        );
    }

    #[test]
    fn a_remote_attach_never_switches_a_local_client() {
        // Regression: the overlay is reachable from a REMOTE attach (unlike the
        // `display-popup` picker it replaced, which was gated off for remote).
        // Deciding from the *pick* alone would run `tmux switch-client` for a
        // session the WS attach cannot be moved to — and that is worse than a
        // no-op, because with any local client attached it exits 0 having dragged
        // an unrelated client away, after which the TUI tracks a session the user
        // is not looking at.
        assert_eq!(switch_client_in_place(None, Some("cc-abc")), None);
    }

    #[test]
    fn a_remote_pick_never_switches_in_place() {
        // `switch-client` only addresses the operator's own tmux server.
        assert_eq!(switch_client_in_place(Some("/dev/pts/3"), None), None);
        assert_eq!(switch_client_in_place(None, None), None);
    }

    // -- What the palette left behind --

    #[test]
    fn a_toast_raised_by_the_palette_is_bridged_into_the_pane() {
        // Only a message the palette raised *itself* should surface: an older
        // one still pending from before the overlay opened would be re-shown for
        // no reason. `run_switcher_overlay` decides this by comparing the pending
        // message before and after; pin that comparison.
        let bridged = |before: Option<&str>, after: Option<&str>| {
            after
                .map(str::to_string)
                .filter(|now| Some(now) != before.map(str::to_string).as_ref())
        };
        assert_eq!(
            bridged(None, Some("No cascade in progress")),
            Some("No cascade in progress".to_string()),
            "a message raised during the overlay reaches the pane"
        );
        assert_eq!(
            bridged(Some("older toast"), Some("older toast")),
            None,
            "a message already pending before the overlay is not re-shown"
        );
        assert_eq!(bridged(Some("older toast"), None), None);
        assert_eq!(bridged(None, None), None);
    }

    #[test]
    fn a_bare_dismissal_returns_to_the_pane() {
        // Esc, or any activation that left nothing on screen: there is nothing to
        // come back to, so don't tear the attach down.
        assert_eq!(overlay_exit_kind(false, false, true), ExitKind::Cancelled);
    }

    #[test]
    fn anything_left_on_screen_ends_the_attach() {
        // A command that opened a modal (Confirm, Settings, the review view).
        assert_eq!(overlay_exit_kind(false, false, false), ExitKind::Command);
        // Quit, picked from the palette while attached.
        assert_eq!(overlay_exit_kind(true, false, true), ExitKind::Command);
        // Open-in-editor stages a command for the outer loop to run.
        assert_eq!(overlay_exit_kind(false, true, true), ExitKind::Command);
    }

    // -- Alt+Tab preselection --

    #[test]
    fn preselection_skips_the_session_we_came_from() {
        let here = SessionId::new();
        let other = SessionId::new();
        let rows = vec![session_row(here), session_row(other)];
        // Empty query orders by most-recently-attached, and the session we are
        // *in* is the most recent of all — so without this, Enter would re-attach
        // to where we already are.
        assert_eq!(first_other_session(&rows, &[here]), 1);
    }

    #[test]
    fn preselection_stays_on_row_zero_when_we_came_from_elsewhere() {
        let a = SessionId::new();
        let b = SessionId::new();
        let rows = vec![session_row(a), session_row(b)];
        assert_eq!(first_other_session(&rows, &[SessionId::new()]), 0);
        assert_eq!(first_other_session(&rows, &[]), 0);
    }

    #[test]
    fn preselection_treats_a_command_row_as_selectable() {
        let here = SessionId::new();
        let rows = vec![session_row(here), command_row()];
        assert_eq!(first_other_session(&rows, &[here]), 1);
    }

    #[test]
    fn preselection_falls_back_to_zero_with_nothing_else_to_pick() {
        let here = SessionId::new();
        assert_eq!(first_other_session(&[], &[here]), 0);
        // The only row is the session we came from: no better answer exists, and
        // it must not panic or return an out-of-range index.
        assert_eq!(first_other_session(&[session_row(here)], &[here]), 0);
    }

    // -- Overlay geometry --

    #[test]
    fn overlay_box_does_not_move_or_resize_with_the_match_count() {
        // The reserved rectangle is a function of the terminal alone. If it
        // tracked the match count, filtering would shrink it and blank pane
        // cells the overlay never repaints.
        let full = overlay_area(120, 40);
        for n in [0usize, 1, 3, actions::LIST_MAX_VISIBLE, 500] {
            let sized = modals::quick_switch_areas(Rect::new(0, 0, 120, 40), n).0;
            if n == actions::LIST_MAX_VISIBLE {
                assert_eq!(sized, full);
            } else if n < actions::LIST_MAX_VISIBLE {
                assert!(
                    sized.height < full.height,
                    "the in-TUI palette does shrink; the overlay must not use that geometry"
                );
            }
        }
        assert_eq!(overlay_area(120, 40), full);
    }

    #[test]
    fn overlay_rows_sit_inside_the_border_below_the_input_line() {
        let area = Rect::new(10, 5, 40, 13);
        let rows = overlay_rows_area(area);
        assert_eq!(rows.x, 11, "inside the left border");
        assert_eq!(rows.y, 7, "below the top border and the query line");
        assert_eq!(rows.width, 38, "inside both borders");
        assert_eq!(rows.height, 10, "border + input + border removed");
        // Rows must stay strictly within the reserved box, or a click maps onto
        // a row that was never drawn.
        assert!(rows.y + rows.height <= area.y + area.height);
        assert!(rows.x + rows.width <= area.x + area.width);
    }

    // -- The opaque pre-paint --

    #[test]
    fn opaque_fill_addresses_the_box_one_based() {
        // ratatui Rects are 0-based, terminal cursor addressing is 1-based. Get
        // this wrong and the fill lands a row/column off the box ratatui draws,
        // leaving a bright edge of pane text along two sides of the palette.
        let fill = opaque_fill(Rect::new(10, 5, 4, 2));
        assert_eq!(fill, "\x1b[0m\x1b[6;11H    \x1b[7;11H    ");
    }

    #[test]
    fn opaque_fill_covers_every_row_of_the_box() {
        let area = Rect::new(0, 0, 3, 5);
        let fill = opaque_fill(area);
        // One cursor-position sequence per row, or part of the box stays
        // transparent and pane text shows through the palette.
        assert_eq!(fill.matches("\x1b[").count(), 1 + area.height as usize);
        for y in 1..=area.height {
            assert!(
                fill.contains(&format!("\x1b[{y};1H")),
                "row {y} not painted"
            );
        }
    }

    #[test]
    fn opaque_fill_of_an_empty_box_writes_nothing() {
        assert!(opaque_fill(Rect::new(0, 0, 0, 5)).is_empty());
        assert!(opaque_fill(Rect::new(0, 0, 5, 0)).is_empty());
    }

    #[test]
    fn overlay_rows_match_the_palettes_own_row_geometry() {
        // The overlay derives its rows from the drawn box, the in-TUI palette
        // from the screen. They must agree, or a click in the overlay selects a
        // different row than the one under the cursor. Would break silently if
        // the palette ever grew a hint line.
        let screen = Rect::new(0, 0, 120, 40);
        let (modal, rows) = modals::quick_switch_areas(screen, actions::LIST_MAX_VISIBLE);
        assert_eq!(overlay_area(120, 40), modal);
        assert_eq!(overlay_rows_area(modal), rows);
    }

    #[test]
    fn overlay_rows_survive_a_degenerate_box() {
        // A terminal too short for the palette must not underflow the u16 maths.
        let rows = overlay_rows_area(Rect::new(0, 0, 1, 1));
        assert_eq!(rows.width, 0);
        assert_eq!(rows.height, 0);
    }
}
