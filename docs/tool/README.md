# Screenshot tooling

The images in [`docs/images/`](../images) are generated, not hand-captured. Both
capture scripts render the **same** hermetic demo workspace — three projects, ten
sessions, two PR stacks, a mix of agent states — so the terminal and client
images tell one consistent story and neither leaks anything from the machine
they were taken on.

| File | What it does |
|------|--------------|
| `fixture.sh` | Builds the demo workspace: temp XDG tree, throwaway tmux server, three git repos, ten sessions, PR metadata and stack links. Sourced by both capture scripts. |
| `demo-claude.sh` | Stand-in agent. Paints a plausible Claude Code transcript into the pane and parks — no real agent runs, so a capture costs nothing and looks the same every time. It is launched as a bare `claude` (both because the state detector only inspects panes whose harness it recognises by basename, and so the client's session header doesn't show a temp path), and reads its per-session mode and task from the fixture's manifest instead of from arguments. |
| `capture-tui.sh` | Runs the TUI on the isolated tmux server and exports pane captures as SVG. |
| `ansi-to-svg.py` | Renders a `tmux capture-pane -e` dump as a Rich terminal-window SVG (run through `uv`; no local Python setup needed). |
| `capture-client.sh` | Boots the server over the seeded tree and drives the real Flutter app (`client/integration_test/screenshots_test.dart`) to write the client PNGs. |

## Regenerating

Terminal UI — needs `tmux`, `cargo`, `uv`:

```sh
docs/tool/capture-tui.sh            # every view
docs/tool/capture-tui.sh stacked    # just docs/images/stacked.svg
```

Views are `stacked` (the default Section Stacks view), `board`, and `info` (the
session Info modal), writing `stacked.svg`, `board.svg` and `info-modal.svg`.
`CC_COLS` / `CC_ROWS` override the capture size.

Flutter client — needs the client nix shell:

```sh
nix develop .#clientCi -c docs/tool/capture-client.sh
```

`clientCi` is the shell to prefer: it carries `xvfb-run` plus software GL, so the
app renders headless instead of taking over the desktop. In `.#client` (no
`xvfb-run`) the app opens a real window; that works too, and `CC_NO_XVFB=1`
forces it. Writes `client-sessions.png`, `client-terminal.png`,
`client-lcars.png` (the same fleet list in the LCARS theme) and
`client-desktop.png`.

The capture also includes `client-edit-session.png` and
`client-edit-session-warning.png`. Capture just those dialogs with:

```sh
nix develop .#clientCi -c docs/tool/capture-client.sh edit
```

## Hermeticity

Both scripts follow the same contract as `client/tool/e2e.sh`:

- `XDG_CONFIG_HOME` / `XDG_DATA_HOME` are redirected into a `mktemp` tree, so
  `config.toml`, `state.json`, `tui.json` and the worktrees dir all live there;
  the real ones are never read or written.
- `TMUX_TMPDIR` points into that tree **and** `$TMUX`/`$TMUX_PANE` are unset.
  Both halves matter: tmux resolves `$TMUX` in preference to `$TMUX_TMPDIR`, so
  running this from inside tmux without the unset would put the demo sessions on
  your real tmux server — and the cleanup's `kill-server` would then take every
  session you have open with it.
- A `gh` stub that always fails shadows the real one, so the PR poller sees
  `FetchFailed` (cache-preserving) instead of authoritatively clearing the
  fixture's seeded PR numbers.
- `DO_NOT_TRACK=1`, so a capture never posts telemetry.

Everything — server, tmux server, temp tree — is torn down on exit, including on
failure.

## Changing what the images show

The workspace lives in one place: `cc_seed_fixture` in `fixture.sh`. Session
titles, projects, agent states, diff sizes, PR numbers, review decisions, labels
and stack links are all declared there. `docs/images/*.svg` and `*.png` are
tracked in Git LFS (see `.gitattributes`).
