# Claude Commander client

Cross-platform Flutter client for [claude-commander-server](../crates/claude-commander-server) —
the GUI counterpart to the terminal UI, for whichever device you're not sitting at
a `claude-commander` shell on. Verified targets today are **Linux desktop** and
**Android**. **iOS** (simulator) and **macOS** desktop build and run, and both builds
are covered by CI; device builds still need a signing identity — see
[iOS / macOS](#ios--macos).
The app is kept Apple-safe as it grows: `reqwest` uses `rustls` (no OpenSSL to
cross-build), and Linux-only desktop dependencies are gated behind `isLinux` in the
dev shell.

The app talks to the server's HTTP REST API (`/api/…`) and its WebSocket terminal
attach endpoint (`/ws/attach`) — the same server the TUI drives locally or attaches
to remotely.

## Architecture

```
Flutter UI  ──frb──►  Rust cdylib  ──claude-commander-client──►  claude-commander-server
             (handle)  (registry)      (RemoteClient + Poller)
```

**Flutter UI** (`client/lib/`) — Material 3, dark theme. An [`AdaptiveShell`](lib/pages/adaptive_shell.dart)
renders a stacked phone flow (list → detail → terminal/review, via `Navigator.push`)
or a desktop master-detail layout — a server sidebar + grouped session list on the
left, a persistent detail pane on the right whose Detail/Terminal/Review tabs
switch in place. `useWideLayout` picks between them on **both** dimensions: the
master-detail layout needs `kWideBreakpoint` (900 logical px) of width *and* more
than `kShortViewportHeight` (500) of height, because a phone held sideways clears
the width on its own — a Pixel 8a is 914 × 411dp — while having no height to spend
on a workspace header and tab strip. The same page *bodies* (`SessionListBody`,
`SessionDetailBody`, `TerminalBody`, `ReviewBody`) back both layouts; only the
surrounding shell differs.

A [`CommanderStore`](lib/state/commander_store.dart) (`ChangeNotifier`) is the
single reactive source of truth for a connected server. It owns the opaque
per-server **handle** for its whole lifetime, refetches the workspace snapshot and
agent states whenever the server's change-feed generation counter bumps (no
wall-clock polling on the Dart side), and tracks a `ConnectionStateDto` off a
separate connection-health feed. Widgets rebuild via `ListenableBuilder`.

**Rust cdylib** (`client/rust/`) — `rust_lib_claude_commander_client`, compiled as
`cdylib` (Android/Linux `.so`) and `staticlib` (iOS, future). Its
[`api/registry.rs`](rust/src/api/registry.rs) is the opaque-handle seam: `connectServer`
builds a `claude-commander-client::RemoteClient` + background `Poller` and registers
both under a fresh UUID handle; every subsequent route/terminal/feed call is keyed
by that handle. One registry entry per connected server today — the seam a
multi-server client (design already written; see below) grows into without
changing the call shape.

- `api/simple.rs` — most HTTP routes: sessions, projects, create-options,
  programs, cascade/push-stack operations.
- `api/terminal.rs` — WebSocket attach bridge (a shared `tokio` multi-thread
  runtime drives the socket off the Dart isolate; events stream to Dart via
  `flutter_rust_bridge` `StreamSink`) plus the change-feed and connection-feed
  streams the store listens to.
- `api/review.rs` — review/diff and comments HTTP endpoints.
- `api/mirrors.rs` — `#[frb(mirror(…))]` declarations so frb generates typed Dart
  classes from the protocol types (compile-checked: a field mismatch is a Rust
  compile error, not a runtime surprise).

**`claude-commander-client` crate** (`crates/claude-commander-client`) — the shared
transport: `RemoteClient` speaks the wire DTOs against the server's `/api` surface
and `/ws/attach`, classifying failures into transport-neutral `ClientError`
categories, with a background `Poller` driving the change-feed generation counter
and connection-state machine (exponential backoff). This crate depends only on
`claude-commander-protocol` plus network crates — never on `claude-commander-core`
(tmux/gix don't cross-compile to Android) — so it backs *both* this cdylib and the
desktop TUI's remote-session support (via the thin `claude-commander-remote`
adapter). One transport, two frontends.

**`claude-commander-protocol` crate** (`crates/claude-commander-protocol`) — the
single source of truth for wire types (`SessionInfo`, `SessionDetail`,
`ReviewSnapshot`, `ClientControl`/`ServerControl` WS frames, etc.). Server, client
transport, and cdylib all depend on it; nobody maintains a private DTO mirror.

**DTO convention** — `flutter_rust_bridge` renders data-carrying Rust enums as Dart
`freezed` classes, which requires the `build_runner` toolchain. The client
deliberately avoids that dependency. Where the protocol types use data enums or
tuples (`ApplyOutcome`, `BinaryKind`, `line_range`), the cdylib converts them into
plain structs + unit enums before returning to frb. See `api/terminal.rs`
(`TerminalEvent`) and `api/review.rs` (`ApplyResult`, `ReviewFileDto`, etc.) for the
pattern.

**Auth** — `flutter_secure_storage` stores the server URL and bearer token in the
platform keystore (Android Keystore / Keychain / libsecret). The token is never
written to plain shared preferences.

## Features

- **Connect + auth** — server URL + bearer token, tested against `/health` before
  saving; reconnect goes through the same `CommanderStore` so a settings change
  can't abandon a handle.
- **Session list** — grouped by project in workspace order, with unread markers,
  live agent-state chips, and a connection-health indicator (connected /
  connecting / degraded) fed by the poller.
- **Session detail** — lifecycle actions (kill/restart/delete), rename, set
  section, keep-alive toggle; pane preview and diff stat fetched on demand.
- **Create session** — form driven by the server's create-options: a program
  picker (falls back to free text if options fail to load), section, base branch,
  optional initial prompt.
- **Live terminal** — WebSocket attach (agent or shell) rendered with the
  `xterm.dart` fork; a desktop pane on wide layouts, a pushed route on phone.
  Re-attaches on returning to the foreground, since a backgrounded (frozen)
  process can't answer the server's heartbeat pings and has its attach killed
  server-side. Only when the attach is *known* dead, though: either it already
  reported detached/error, or the app was away longer than
  `protocol::ws::attach_dead_after()` — the deadline no unanswered attach
  survives, read over the bridge rather than mirrored as a Dart constant. A
  shorter absence leaves the live socket alone, because re-attaching spawns a
  fresh `tmux attach-session` child and that loses a scrolled copy-mode view. The
  status bar's reconnect button is always enabled, so a half-open socket (network
  path gone without a TCP FIN, so no detach frame ever arrives) is never a dead
  end.
- **Image attach** — the terminal status bar's image button (agent attaches only)
  offers the photo library / a file dialog, a camera capture on Android, or a
  clipboard paste; on Linux **Ctrl+V** attaches a clipboard image directly and
  falls back to a normal text paste when the clipboard holds no image. The bytes
  go to `POST /sessions/{id}/paste-image`, which writes a temp file server-side
  and types its path into the agent pane without pressing Enter — so a prompt can
  be written around it. See [Image attach](#image-attach).
- **Review** — diff view with inline comments, snippet-based re-anchoring, and
  apply; on-demand blob loading for images, with per-file reviewed toggles.
- **Programs list editing** — a dedicated settings page (`ProgramsPage`) edits the
  server's launch-program list (`PUT /config/programs`), the same list the create
  form offers.
- **Session management** — unread markers on the list with mark-read on open,
  plus rename, set-section, and a keep-alive toggle from the detail view.
- **Projects** — a dedicated `ProjectsPage` (from the settings menu) lists,
  adds, removes, and scans server-side project paths and browses each project's
  branches (`addProject`/`removeProject`/`scanDirectory`/`listBranches`).
- **Cascade / push-stack** — triggered from the session detail view with their
  operation outcome reported (`cascadeMerge`/`pushStack`); a paused cascade shows
  a global resume/abandon banner (`cascadeResume`/`cascadeAbandon`).
- **Themes** — Mission Control or LCARS, plus per-role colour overrides
  (primary, working, attention, …) picked from theme swatches, a hex field or
  the clipboard. With two or more workspaces, each can have its own theme
  (a preset and/or overrides) over the usual one, applied live on a workspace
  switch. In a workspace's scope the picker marks the preset it inherits
  `USUAL` rather than checking it, with a **Pin this preset** button that pins
  it (shedding the usual overrides); **Inherit usual preset** unpins a preset
  while keeping the workspace's overrides. All of it is per device
  (`shared_preferences`), keyed by workspace name: a rename made on this device
  moves the key, a rename made elsewhere leaves the workspace on the usual
  theme. See `lib/theme/theme_prefs.dart`.
- **Window modes** (desktop) — fullscreen, a borderless frame where the app draws
  its own themed window bar, and a window size/position remembered across
  launches. See [Window modes](#window-modes).

## Image attach

Getting a screenshot to the agent works the same way it does from the desktop
TUI's Ctrl+V, and reuses the same route.

**Why a path, not an upload to the agent:** the Claude CLI accepts a plain-text
image path in its prompt. `CommanderService::paste_image` validates the bytes,
writes them to a pruned temp file, and `send-keys -l`s the absolute path into the
session's agent pane with **no Enter** — the user adds prompt text and submits.
The path therefore appears in the terminal view through the ordinary attach output
stream, which is why the UI shows no success confirmation.

**Where the rules live:** the accept allow-list (PNG/JPEG/GIF/WebP/BMP, sniffed
from magic bytes — never a filename or `Content-Type`) and the 10 MiB cap are in
`claude_commander_protocol::paste`, shared by the server's body limit, the
service's re-check, and `RemoteClient::paste_image`'s local pre-check. So an
invalid or oversized image is refused before it leaves the device, and
`imageMaxBytes()` lets the UI reject an oversized pick from its file length
without reading a large photo into memory.

**Agent attaches only.** The path is injected into the *agent* pane, so the button
is hidden — and Ctrl+V left to `xterm` — on a shell attach, where it would
otherwise type into a pane the user cannot see.

**Plugins:** `image_picker` (Android: native photo picker + camera; Linux:
"limited" support via the endorsed `image_picker_linux` → `file_selector`, i.e. a
GTK dialog with no camera) and `super_clipboard` for clipboard *image* reads,
which Flutter's own text-only `Clipboard` cannot do. Both are behind the
`ImagePickerService` / `ClipboardImageReader` seams in `lib/services/` so widget
tests can substitute fakes instead of driving platform channels.

**Ctrl+V interception.** `xterm` binds Ctrl+V to `PasteTextIntent`, handled by a
`TerminalActions` widget *inside* `TerminalView` — so an outer `Actions` override
is shadowed. The hook is `TerminalView.onKeyEvent`, which outranks both its
shortcuts and its input handler. Pre-empting the key means the text-paste
fallback has to be reproduced in `_pasteClipboard`; don't remove it, or Ctrl+V
stops pasting text when the clipboard holds none.

**Gradle note.** `super_clipboard` pulls `irondash_engine_context`, which still
declares `compileSdkVersion 31`. The Nix SDK is read-only, so Gradle cannot
install that platform — `android/build.gradle.kts` therefore pins every
subproject's `compileSdk` to `ANDROID_COMPILE_SDK` (exported by the client dev
shell), alongside the pre-existing NDK pin. Without it the APK build fails with
"The SDK directory is not writable".

**Known limitation.** Android may kill the activity during a camera capture under
memory pressure; `image_picker` exposes `retrieveLostData()` for that, which we
don't call — by the time the app restarts the terminal attach is gone, so there
is nothing to re-target the upload at. The capture is simply lost and the user
retries.

## Multi-server

The app connects to one server today. The handle-per-server registry (cdylib
side) and the reserved server-sidebar slot (`_ServerSidebar` in `adaptive_shell.dart`,
currently rendering a single row) are the seams a multi-server client grows into —
a design for that exists but isn't built; it's a follow-up branch.

## Dev environment

Enter the client dev shell with:

```sh
nix develop .#client
```

This is a separate dev shell in the root `flake.nix`; the default shell (`nix develop`)
is lean and never pulls the Flutter/Android toolchain.

The shell provides: Flutter/Dart, Rust (stable) + four Android cross-compile
targets (`aarch64`, `armv7`, `x86_64`, `i686` Linux Android) via fenix, the Android
SDK (platforms 34/35/36, build-tools, NDK r28 `28.0.13004108`, emulator + x86\_64
system images), JDK 17, `cargo-ndk`, `flutter_rust_bridge_codegen`, CMake/Ninja/Clang
for the native build, and `pkg-config`/`libclang`.

Each host's extra native stack is appended conditionally, since neither desktop's
libs are any use to the other:

- **Linux** — the GTK/X11 stack Flutter needs for the desktop target (`gtk3`, `glib`,
  `pcre2`, `libepoxy`, `libx11`, `libsecret`), gated behind `isLinux`.
- **macOS** — CocoaPods (Flutter shells out to `pod install` for both Apple runners),
  the Apple Rust targets (`clientAppleTargets` in `flake.nix`), an `xcrun` shim, and
  Xcode-toolchain overrides for the iOS triples.
  Cocoa itself comes from Xcode, which is a host prerequisite rather than a Nix input
  — it can't be packaged, which is why the Apple side can't be hermetic the way
  Android is. The shellHook warns at entry if no Xcode is selected.

**What the shellHook sets up:**

- `ANDROID_HOME`, `ANDROID_SDK_ROOT`, `ANDROID_NDK_ROOT/HOME`, `ANDROID_NDK_VERSION`, `JAVA_HOME` — pointed at the Nix-provided SDK.
- `flutter config --android-sdk` — points Flutter at the Nix SDK (read-only; no auto-install).
- `LD_LIBRARY_PATH=/usr/lib:…` — lets Nix-built libepoxy find the system Mesa EGL at runtime (Linux desktop only).
- `flutter config --enable-ios` / `--enable-macos-desktop` (macOS only) — both are opt-in in Flutter's own per-user config (`~/.config/flutter_settings`), so checking the runners into the repo doesn't switch them on.
- `CC_`/`CXX_`/`AR_`/`CARGO_TARGET_*_LINKER` for `aarch64-apple-ios`, `aarch64-apple-ios-sim` and `x86_64-apple-ios` (macOS only) — pointed at Xcode's toolchain, since Nix's cc wrapper injects `-mmacos-version-min` (which clang rejects alongside `-miphoneos-version-min`) and `NIX_LDFLAGS` (which drags the macOS `libiconv` into an iOS link). The darwin triples are left on Nix's toolchain — that host is what it is built for.
- `IPHONEOS_DEPLOYMENT_TARGET` (macOS only) — read out of the Runner's own pbxproj (**15.0** today, lowest configuration wins) rather than restated, so the two cannot drift. Unpinned, cc-rs takes the SDK's default while rustc still defaults these triples to iOS 10, and the mismatch fails the link on compiler-rt symbols (`___chkstk_darwin`) that libSystem no longer carries.
- `PATH` gets a `cc-xcode-env` wrapper (macOS only) — run anything that drives Xcode *directly* through it: `cc-xcode-env flutter build ios --simulator`. It points `DEVELOPER_DIR` at the selected Xcode (the stdenv's is the Nix apple-sdk, which is not an Xcode), puts Apple's `xcodebuild` and `/usr/bin/xcrun` ahead of this shell's stubs, and applies the same environment scrub the `xcrun` shim uses when it delegates. Without it, `xcodebuild -showBuildSettings` returns nothing, `productBundleIdentifier` is null, and Flutter exits with "Application not configured for iOS" — which reads as a missing `client/ios`. **This bites on Apple silicon only:** `xcodeproj.dart` prefixes every Xcode command with `/usr/bin/arch -arm64e` on a `darwin_arm64` host, and the shim is a shell script whose interpreter is Nix's bash — plain arm64, no arm64e slice — so `arch` cannot exec it. An Intel Mac never takes that path, which is why this surfaced first in CI.
- `PATH` gets an `xcrun` shim ahead of the stdenv's (macOS only) — see `clientXcrunShim` in `flake.nix`. Dart's native-assets build hooks run each `hooks/build.dart` under a filtered environment that keeps `PATH` but drops `DEVELOPER_DIR`, and nixpkgs' `xcrun` stub can only find an SDK through that variable. Without the shim it returns `error: unable to find sdk: 'macosx'`, the string is passed to clang as `-isysroot`, and any hook-built C/ObjC source dies on a missing `assert.h`/`Foundation.h`. `objective_c` is a transitive dependency, so this took plain `flutter test` down on macOS — nothing Apple-specific required.
- `client/rust/target/release → debug` symlink — frb's `ioDirectory` looks for `release/`; the symlink means a debug build is found immediately.
- Creates the Android AVD `cctest` (android-35, google\_apis, x86\_64, Pixel 6) on first entry if it doesn't already exist.

> The Nix SDK is read-only. Every SDK component the build needs must be declared in `flake.nix` (in `platformVersions`, `buildToolsVersions`, `ndkVersions`, etc.) rather than being auto-installed by Gradle or `flutter doctor`.

## Golden tests

`test/goldens/` holds reference images for the chrome layer — each chrome form on
its own, plus the session list, settings and wide shell — rendered in **both**
themes. They run as part of `flutter test`; the Nix-pinned Flutter is what makes
that mostly safe, since identical Skia and font versions mean CI rasterises
almost exactly as your machine does. Not *exactly*: the LCARS phone-shell golden
disagreed with the runner on label antialiasing alone, with the geometry
byte-identical, and was deleted in #280 rather than given a tolerance.

So run them the way CI does — from the repo root, with the pinned toolchain
forced, because the images are rasteriser-sensitive:

```sh
CC_FORCE_NIX=1 scripts/verify.sh --goldens            # run them
CC_FORCE_NIX=1 scripts/verify.sh --goldens --update   # regenerate them
```

Then **look at the diff before committing** — an unexplained image change is the
entire point. Running that lane before pushing a golden change is a repo rule;
see the Golden tests section of the root `CLAUDE.md` for what it does and does
not prove, and for what to do when a golden fails only on CI.

Three things the harness (`test/support/golden.dart`) has to do, each learned by
getting it wrong first:

- **Load the bundled fonts.** `flutter test` does not; without `loadCommanderFonts()`
  every glyph is an Ahem box and the references pin a layout the app never draws.
- **Load `MaterialIcons` from `FLUTTER_ROOT`.** Otherwise every icon is the same
  notdef square — the window bar's maximise and restore goldens came out byte-for-byte
  identical, pinning nothing.
- **Pump inside a real `Scaffold`.** A `Text` with no `Material` ancestor picks up
  `DefaultTextStyle.fallback`, which Flutter draws with a yellow double underline.

What they can't do: they render at `devicePixelRatio` 1 in logical pixels, so an
artifact that only exists in a rasterised window at fractional DPI (see the LCARS
eyebrow's padding comment) renders identically here and passes. They also can't
see the session state glyphs, which no bundled face contains — see the note in
`golden.dart`.

## Window modes

Desktop only, via `window_manager`. Two independent settings plus remembered
geometry, all persisted through `SharedPreferences` and applied **before the first
frame** — the runner shows the window on its first Flutter frame, so a late restore
is a visible jump from the default 1280x720.

| | Shortcut | Settings row | Default |
|---|---|---|---|
| Fullscreen | `F11` | WINDOW → Full screen | off |
| Window frame | `Shift+F11` | WINDOW → Window frame | **hidden** (borderless) |

**Why borderless is the default.** GTK3 has no `xdg-decoration` support, so a GTK3
window on Wayland is always client-side decorated — KWin will never draw its own
title bar for this app, and the "native" frame is a GNOME-style GTK header bar even
in a KDE session. The app's own bar is the only frame that can match the desktop,
and each theme draws its own: a flat 32px bar in Mission Control, a run of blocks
(`MIN` / `MAX` / `CLOSE`) in LCARS. Drag the bar to move the window, double-click to
maximise.

Four details that are load-bearing rather than incidental:

- **`setTitleBarStyle(hidden)`, never `setAsFrameless`.** On the runner's header-bar
  path — Wayland, and X11 under GNOME — the former hides the header *widget* and
  leaves the window GTK-decorated, so its invisible client-side resize border
  survives and borderless needs no resize grips of its own. **This does not hold on
  the other path**: with no header bar the plugin falls through to
  `gtk_window_set_decorated(false)` (`window_manager_plugin.cc:512-519`), which is
  what `setAsFrameless` does, so borderless on X11 outside GNOME leaves a window
  with no resize border — resizable only via the WM (KWin: `Meta`+right-drag).
  Deliberately not accommodated: X11 is deprecated and out of scope here. Switch
  the frame to Native in Settings if you are on it.
- **Window *position* is X11-only.** Wayland does not let a client place its own
  window, so the `x,y` in `commander.window.bounds` is honoured only on X11; the
  size restores everywhere.
- **F11 is a `FocusManager` *early* key handler.** The terminal view maps F11 to
  `TerminalKey.f11` and forwards the escape sequence to the remote PTY, so the key
  has to be taken before the focus walk reaches it. Neither obvious option does
  that: a `Shortcuts` widget sits above the focused terminal, and a
  `HardwareKeyboard` handler — which *does* run first — only answers the engine,
  because `KeyEventManager` dispatches to the focus tree regardless of its result.
  An early handler runs before the walk and `KeyEventResult.handled`
  short-circuits it. Every F11 event is claimed, repeats and release included;
  only the press acts.
- **Geometry is never saved while fullscreen or maximised.** Those bounds are the
  screen, and restoring them would leave nothing to un-maximise back to.

No Android path: `window_manager` declares only linux/macos/windows, so
`createWindowService()` returns null there — which means no `WindowController`, and
therefore no window bar and no key handler, without a platform check in the UI.

## Build and run

`scripts/dev-run.sh` wraps both flows below, including the AVD boot-and-wait and
the build → install → launch chain; it enters the right dev shell itself, so it
works from a bare terminal. The raw commands are kept here because they are what
the script runs and what you need when debugging a step of it.

### Linux desktop

```sh
scripts/dev-run.sh linux          # or, by hand:
cd client && flutter run -d linux
```

Requires a display (`DISPLAY` or `WAYLAND_DISPLAY`). The `release → debug` symlink in the shellHook means `flutter run` (debug mode) finds the cdylib without a separate `cargo build` step.

### Android emulator

```sh
scripts/dev-run.sh android              # boot if needed, build, install, launch
scripts/dev-run.sh android --release    # the release APK instead
scripts/dev-run.sh android --device 1A2B3C4D   # a physical handset
scripts/dev-run.sh emulator start|stop|status  # AVD lifecycle on its own
```

By hand, boot the AVD the shellHook created (KVM-accelerated, Linux only):

```sh
emulator -avd cctest -no-window -gpu swiftshader_indirect \
         -no-audio -no-boot-anim -accel on &
adb wait-for-device
until adb shell getprop sys.boot_completed 2>/dev/null | grep -q 1; do sleep 3; done
```

Then run:

```sh
cd client
flutter run -d emulator-5554
```

`cargo-ndk` cross-compiles the cdylib for the emulator's x86\_64-linux-android target; Gradle links it into the APK. The `ANDROID_NDK_VERSION` env var (set by the shellHook) pins Gradle to the Nix-provided NDK in both `android/app/build.gradle.kts` and `rust_builder/android/build.gradle`.

### iOS / macOS

Both build: `flutter build ios --simulator` and `flutter build macos` run green, and
CI's `client-apple` job runs both on every PR. Everything comes from
`.#clientApple` — Rust toolchain, CocoaPods, `cc-xcode-env`, the `xcrun` shim —
**except the Flutter SDK**, which comes from the Flutter action pinned to the
version the flake provides (read back out of the shell, so the two cannot
drift).

That exception applies to the **iOS build only** — `flutter pub get` and the
macOS build still use the flake's Flutter — and it is a measured upstream bug
rather than a preference. In `flutter-artifacts-ios-aarch64-darwin`, the path
`bin/cache/artifacts/engine/ios/Flutter.xcframework/ios-arm64_x86_64-simulator/Flutter.framework/Flutter`
holds a **26,320-byte, non-executable** stub whose fat header claims an arm64
slice past EOF, so `ld` stops at "X86_64 slice extends beyond end of file". The
same path in the `x86_64-darwin` artifact is the real **78,153,424-byte** engine,
and on `aarch64-darwin` that binary is present but filed under `extension_safe/`.
Check it yourself, no Mac required:

```
nix store ls -lR --store https://cache.nixos.org \
  $(nix derivation show -r "$(nix eval --raw nixpkgs#flutter341.drvPath)" \
    | grep -oE '/nix/store/[a-z0-9]{32}-flutter-artifacts-ios-aarch64-darwin' | sort -u | head -1) \
  | grep 'ios-arm64_x86_64-simulator/Flutter.framework/Flutter$'
```

This is **not** "nixpkgs Flutter is broken on Apple silicon": Android, web,
desktop and the macOS engine artifacts are all intact (the macOS engine is the
same size on both darwin systems). Only the iOS artifact set is affected, which
is also why iOS builds fine from the dev shell on an Intel Mac. The job prints
`lipo -detailed_info` for that binary every run, so the day nixpkgs ships an
intact artifact, dropping the action is a one-line change. That is the SDK only: everything else in the job — the Rust toolchain, CocoaPods,
`cc-xcode-env`, the `xcrun` shim — still comes from the shell. Building iOS
locally through `nix develop .#clientApple` works on an Intel Mac (verified:
stock *and* Nix Flutter both produce a simulator `Runner.app` there); on Apple
silicon the Nix Flutter's engine hits the malformed slice above, so use a Flutter
on `PATH` there — the repo's scripts already prefer one. The minimum iOS is **15.0** — the
oldest floor that costs nothing: iOS 13, 14 and 15 all shipped to the same devices
(iPhone 6s and later), iOS 16 is where Apple dropped them, and 15 is also the oldest
simulator Xcode 26 can run and the point where rustc's `aarch64-apple-ios-sim` floor
(14.0) stops forcing a version-mismatch warning into every simulator link. Flutter's
own hard floor is 13.0 (`ios_deployment_target_migration.dart` rewrites anything
below it), so 15.0 needs no compatibility shims. macOS minimum is 15.0.

What exists:

- `client/ios` and `client/macos` runners, from `flutter create --platforms=ios,macos
  --org com.claudecommander .`. Bundle id `com.claudecommander.claudeCommanderClient`
  (no underscores allowed, so it can't match the Android `applicationId` exactly);
  display name "Claude Commander" on both, matching the Android `android:label`.
- App icons for both, generated from their own masters — see
  [`tool/icon/README.md`](tool/icon/README.md) for why the Apple platforms can't
  share `icon_full.svg`.
- Dev-shell support: CocoaPods, the four Apple Rust targets, the `xcrun` shim, and
  `flutter config --enable-ios/--enable-macos-desktop`.
- `rust_builder/{ios,macos}` podspecs, which ship with cargokit — `pod install` runs
  `build_pod.sh` to produce the Rust staticlib, the same way the Android Gradle
  plugin drives `cargo-ndk`.

The Rust half is verified: `cargo build --target <triple>` produces the staticlib for
each Apple triple from inside the shell with no extra environment. That needs Xcode's
C toolchain rather than the shell's, because rustls resolves to `aws-lc-rs` here and
`aws-lc-sys` compiles C and arm64 assembly — the shellHook exports `CC_`/`CXX_`/`AR_`
and `CARGO_TARGET_*_LINKER` for the iOS triples to arrange that (see `flake.nix` for
what each one is working around).

The Xcode half is proven too: `pod install` has run for both platforms (their
`Podfile.lock`s are committed), cargokit's `build_pod.sh` produces the staticlib the
Xcode build links, and the iOS app boots in a simulator. A fresh Mac still needs
`sudo xcodebuild -runFirstLaunch` before the first build.

What has **not** happened: no signed device build, and nothing is launched in CI —
device builds need a signing identity that a fork's PR cannot have, and booting a
simulator runtime in CI is slow enough to want its own task. The macOS app is sandboxed
(`macos/Runner/*.entitlements`), so anything new that touches the network, the
keychain or user-selected files needs its entitlement added there or it fails only at
runtime — never at build time, which is why CI cannot catch it.

> `flutter create` copies its templates straight out of the read-only Nix store, so
> the files it writes are mode `444`. Run `chmod -R u+w client/ios client/macos` after
> adding a platform, or the next tool to touch them (`flutter_launcher_icons` was the
> first here) fails with `Permission denied`.

## frb codegen loop

`flutter_rust_bridge` generates the Dart FFI glue from the Rust API surface. After editing any file under `client/rust/src/api/`:

```sh
cd client
flutter_rust_bridge_codegen generate   # regenerates lib/src/rust/* and rust/src/frb_generated.rs
cargo build                            # verify the cdylib still compiles
```

Commit the regenerated files (`client/lib/src/rust/` and `client/rust/src/frb_generated.rs`) together with the Rust changes. The generated files are checked in so contributors without the full toolchain can still run `flutter analyze` and widget tests.

> **The runtime and the codegen are pinned to the same version, 2.13.0.** The runtime is `=2.13.0` (`client/rust/Cargo.toml`, `pubspec.yaml`) to match the `flutter_rust_bridge_codegen` the `.#client` dev shell takes from the nixpkgs pin (`nix develop .#client -c flutter_rust_bridge_codegen --version`). The generated glue stamps the codegen version that wrote it (`frb_generated.rs:41`, `frb_generated.dart:75`), so a nixpkgs bump that moves the codegen means bumping both runtime pins and regenerating in the same commit. `.#clientCi` carries no codegen at all.

## Testing

Four layers, all runnable from the client dev shell (`nix develop .#client`, or the
slim `.#clientCi` used by CI):

| Layer | Where | What it covers |
|-------|-------|----------------|
| cdylib unit | `client/rust/src/api/*.rs` `#[cfg(test)]` | pure helpers (URL mapping, id/DTO parsing, handle registry) |
| cdylib ↔ server integration | `client/rust/tests/server_flows.rs` | every blocking HTTP fn against a real in-process server (connect, create/list/detail/kill, restart/delete, join-by-prefix, review round-trip) |
| Dart widget | `client/test/*_test.dart` | each page with a hand-rolled `FakeCommanderApi` (no live bridge), plus `CommanderStore` unit tests |
| Full-stack e2e | `client/integration_test/app_flows_test.dart` | the real app on `-d linux` against a hermetic server |

`scripts/verify.sh --client` runs the first three layers (plus `dart format` and
`flutter analyze`) in one go; `scripts/verify.sh --e2e` adds the fourth. The
per-layer commands:

```sh
# Dart widget tests (fast; no Rust bridge, no server):
cd client && flutter test

# cdylib unit + integration tests (needs tmux; boots a hermetic server in-process):
cd client/rust && cargo test

# Full-stack e2e (boots a hermetic, XDG-isolated server, then drives the app on
# the Linux desktop target). Needs a display; use xvfb-run when headless:
client/tool/e2e.sh              # local, with a display
xvfb-run -a client/tool/e2e.sh  # headless / CI
```

`client/tool/e2e.sh` redirects `XDG_CONFIG_HOME`/`XDG_DATA_HOME` **and** `TMUX_TMPDIR`
into a `mktemp` dir, so the server it launches touches neither your real
config/state/worktrees nor your default tmux server — the whole tree (including the
isolated tmux server) is torn down on exit, even if a test fails mid-run. The
integration harness (`crates/claude-commander-test-support`)
is shared by the server's own integration tests and the cdylib's. CI runs all four
layers via the `client` job in `.github/workflows/ci.yml`.

The integration/e2e server tests self-skip when tmux is absent (a runtime check, not
`#[ignore]`), so they run in CI where tmux is present.

## Feature status

| Phase | Feature | Status |
|-------|---------|--------|
| 0 | `claude-commander-protocol` crate, shared wire types | Done |
| 1 | Connect + auth, session list | Done |
| 2 | Session detail + lifecycle (kill/restart/delete/create) | Done |
| 3 | Live terminal (WebSocket, `xterm.dart`) | Done |
| 4 | Review/diff + inline comments, apply | Done |
| 5 | iOS / macOS | iOS simulator + macOS desktop build and run, CI-covered; no signed device build |
| 6 | Shared `claude-commander-client` transport crate (also backs the TUI's remote sessions) | Done |
| 7 | Adaptive desktop shell (master-detail), programs-list editing, multi-server seams | Done / in progress |
| 8 | Image attach (picker, camera, clipboard, Ctrl+V) | Done |

**Measured throughput (Phase 3 spike, debug builds):**
- Linux desktop: 23.7 MB/s end-to-end (frb stream → UTF-8 decode → xterm.dart VT parse/write → paint)
- Android emulator (x86\_64, KVM): 21.6 MB/s

Real PTY output sustains well under 2 MB/s, so both targets have comfortable headroom.

Review images load on demand (`GET /sessions/{id}/blob`) and per-file reviewed marks are togglable (`POST /sessions/{id}/files/reviewed`). Non-image binaries still show a placeholder.

## xterm.dart fork

The terminal view uses a fork of xterm.dart pinned to `github.com/sizeak/xterm.dart`, branch `commander` (at the v4.0.0 commit). The upstream package is lightly maintained; the fork carries mobile/touch fixes and lets the project cherry-pick community PRs without waiting on upstream. Carry local patches on the `commander` branch and upstream them where possible.

The pubspec dependency:

```yaml
xterm:
  git:
    url: https://github.com/sizeak/xterm.dart.git
    ref: commander
```
