import 'dart:async';
import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:uuid/uuid.dart';
import 'package:xterm/xterm.dart';

import '../chrome/chrome.dart';
import '../services/clipboard_image_reader.dart';
import '../services/commander_api.dart';
import '../services/image_picker_service.dart';
import '../src/rust/api/mirrors.dart';
import '../state/commander_store.dart';
import '../state/commander_store_scope.dart';
import '../theme/terminal_theme.dart';
import '../theme/tokens.dart';
import '../util/ctrl_chord.dart';
import '../util/error_text.dart';
import '../util/viewport.dart';

/// The terminal's own status/throughput line, so a test can measure the chrome
/// it costs without depending on what is drawn in it.
const terminalStatusBar = ValueKey('terminal-status-bar');

/// The on-screen modifier/arrow row, likewise.
const terminalModifierBar = ValueKey('terminal-modifier-bar');

/// The back control [TerminalBody] draws when the page has no title bar to hold
/// one — see [TerminalBody.onBack].
const terminalBackButton = ValueKey('terminal-back');

/// Live attached terminal, layout-agnostic (no Scaffold, no route). Streams raw
/// PTY bytes from the cdylib WS bridge into an `xterm.dart` [Terminal], forwards
/// keystrokes/resize back, shows a compact status/throughput bar with a reconnect
/// action, and — only when [showModifierBar] is set (touch/narrow) — an on-screen
/// modifier bar.
///
/// Each attach uses a fresh per-attach id (a UUID) that keys its control channel
/// in the cdylib, so several attaches can be live against one server. The id is
/// registered with the [CommanderStore] (when one is in scope) so a
/// reconnect/dispose of the store tears the attach down before releasing the
/// handle.
///
/// The pane's size reaches the server twice over. The *initial* size rides in
/// the attach handshake, which is why the attach waits for the first frame
/// before opening: the server sizes the PTY before spawning
/// `tmux attach-session`, so tmux's very first paint is already at the width
/// that will render it. (It has to be this early — tmux paints a whole screen
/// the moment the attach starts, and its later repaints are incremental with no
/// full-screen clear, so a paint that arrived too wide is wrapped here and never
/// corrected.) Subsequent changes come from `xterm`'s [Terminal.onResize],
/// which fires from the widget's actual laid-out size.
///
/// The attach is also re-opened when the app returns to the foreground, because a
/// backgrounded process cannot keep the attach alive: the server pings every
/// attached socket and kills the attach once too many pings go unanswered, and a
/// frozen Android process answers none of them. See [_onResumed] for why the
/// resume only re-attaches when the attach is *known* dead rather than always.
class TerminalBody extends StatefulWidget {
  final CommanderApi api;

  /// The live server handle, used to resolve the transport client for the attach.
  final String handle;
  final SessionInfo session;

  /// Which pane to attach to: the agent pane (default) or the paired shell.
  final AttachKind kind;

  /// Show the on-screen modifier/arrow bar (mobile/touch only). Desktop relies
  /// on the physical keyboard, so this is false there.
  final bool showModifierBar;

  /// The session name to carry in the status bar, or null when the page's own
  /// title bar is already showing it. Set by [TerminalPage] in a short viewport,
  /// where the page drops its title to give the pane the height.
  final String? barTitle;

  /// Pops the route, or null when the page's chrome already offers a way back.
  /// Same origin as [barTitle]: a titleless Mission Control page has no app bar
  /// and therefore no back button, so the status bar grows one.
  final VoidCallback? onBack;

  /// Image sources for the attach-image action. Injectable because both drive
  /// platform channels a widget test cannot exercise; `null` means "use the real
  /// platform implementation".
  final ImagePickerService? imagePicker;
  final ClipboardImageReader? clipboardImages;

  /// Wall clock for the how-long-were-we-away measurement, injectable so a widget
  /// test can cross the heartbeat deadline without waiting a real minute; `null`
  /// means [DateTime.now].
  final DateTime Function()? clock;

  /// Builds the emulator each (re)attach writes into. Injectable so a test can
  /// drive the emulator-failure path of [_TerminalBodyState._write], which by
  /// definition has no reachable trigger through the real one: any sequence that
  /// still threw would be a bug to fix in the pinned `xterm` fork, not a fixture
  /// to build a test on. `null` means the real emulator.
  final Terminal Function()? terminalFactory;

  const TerminalBody({
    super.key,
    required this.api,
    required this.handle,
    required this.session,
    this.kind = AttachKind.agent,
    this.showModifierBar = true,
    this.barTitle,
    this.onBack,
    this.imagePicker,
    this.clipboardImages,
    this.clock,
    this.terminalFactory,
  });

  @override
  State<TerminalBody> createState() => _TerminalBodyState();
}

class _TerminalBodyState extends State<TerminalBody>
    with WidgetsBindingObserver {
  late Terminal _terminal;

  /// Bumped whenever [_terminal] is replaced, and used as the [TerminalView]'s
  /// key so Flutter tears the old view state down and builds a fresh one.
  ///
  /// Not needed for *rebinding*: an ordinary rebuild already follows a swapped
  /// terminal, because `_TerminalView.updateRenderObject` assigns
  /// `renderObject.terminal` (terminal_view.dart:517-533) and `RenderTerminal`'s
  /// setter moves the change listener, re-resizes and marks needs-layout
  /// (ui/render.dart:54-61). What the key buys is the view *state* that a
  /// rebuild keeps: `_TerminalViewState` owns its `ScrollController`
  /// (terminal_view.dart:164,173 — replaced only when the *widget's* changes)
  /// and its IME `_composingText` (terminal_view.dart:160,390). Carrying a
  /// scroll offset from the buffer we just discarded, or a half-composed IME
  /// string, into a blank grid is exactly the stale state this reset exists to
  /// clear.
  int _terminalGeneration = 0;

  /// Owned rather than left to `TerminalView` to create, so the Ctrl+V
  /// text-paste fallback can clear the selection the way xterm's own paste
  /// action does. Because we pass it in, we own disposing it.
  final TerminalController _terminalController = TerminalController();
  StreamSubscription<TerminalEvent>? _sub;
  CommanderStore? _store;

  /// A fresh id per attach: keys this attach's control channel in the cdylib.
  /// Regenerated on every (re)connect so a reconnect never collides with the
  /// entry a just-ended attach is still tearing down.
  String _attachId = const Uuid().v4();

  // Stateful UTF-8 decoder: PTY chunks can split a multibyte codepoint across
  // WS frames, so a chunked decoder buffers the partial tail until it completes.
  // Replaced alongside [_terminal], since a stream cut mid-codepoint would
  // otherwise prefix the fresh attach's output with the old one's dangling tail.
  late ByteConversionSink _decoder;

  String _status = 'connecting…';

  /// True once the attach has ended (detach/transport/error), so the UI offers
  /// a reconnect instead of pretending it's still live.
  bool _ended = false;

  /// True from the moment an attach-image action starts until it finishes —
  /// covering the clipboard read, the picker round trip and the upload. Set
  /// **synchronously** at each entry point, before the first `await`, so two
  /// fast Ctrl+V presses (or a press racing the bottom sheet) can't both get
  /// through and inject the path twice. A flag set only once the upload began
  /// would leave exactly that window open, since the clipboard read is itself a
  /// platform round trip with a multi-second timeout.
  bool _imageBusy = false;

  /// True while the on-screen Ctrl key is armed, i.e. the next character the
  /// keyboard produces is to be folded into its control byte. Held here rather
  /// than in [_ModifierBar] because it is consumed by terminal *output* (see
  /// [_sendText]), which the bar never sees.
  bool _ctrlArmed = false;

  /// True only around the upload itself, which is what the spinner reports.
  /// Deliberately narrower than [_imageBusy]: a spinner while the bottom sheet
  /// or the OS picker is in front of the user tells them nothing, and an
  /// indeterminate progress indicator animates forever, so widening it would
  /// also stop `pumpAndSettle` ever settling in widget tests.
  bool _uploading = false;

  late final ImagePickerService _imagePicker =
      widget.imagePicker ?? PlatformImagePicker();
  late final ClipboardImageReader _clipboardImages =
      widget.clipboardImages ?? const SuperClipboardImageReader();

  /// Whether this attach can take an image. The server always injects the path
  /// into the session's *agent* pane, so offering it on a shell attach would
  /// type into a pane the user isn't looking at.
  bool get _canAttachImage => widget.kind == AttachKind.agent;

  /// Wall clock, not a [Stopwatch]: on Android a device in deep sleep stops
  /// advancing the monotonic clock a stopwatch reads, while the server's
  /// heartbeat teardown happens in real time — so a monotonic measure would
  /// under-report exactly the long absences this exists to catch.
  late final DateTime Function() _now = widget.clock ?? DateTime.now;

  /// How long a *silent* client can be away before the server has certainly
  /// killed the attach, from the shared wire contract. Null until the bridge
  /// answers (or if it fails), and treated as "don't guess": without it, a resume
  /// only re-attaches something already reported dead.
  Duration? _deadAfter;

  /// When the app last dropped out of the foreground, or null while it is in
  /// front.
  DateTime? _leftForegroundAt;

  /// How many times the emulator has thrown while parsing output. Non-zero means
  /// the grid no longer matches the pane and cannot catch up on its own — see
  /// [_write].
  int _writeFailures = 0;

  // Throughput meter: bytes this second, refreshed on a 1s tick.
  int _totalBytes = 0;
  int _windowBytes = 0;
  final ValueNotifier<String> _throughput = ValueNotifier('0 B/s · 0 KB');
  Timer? _meter;

  @override
  void initState() {
    super.initState();
    _resetEmulator();
    _connect();

    WidgetsBinding.instance.addObserver(this);
    unawaited(_loadDeadAfter());

    _meter = Timer.periodic(const Duration(seconds: 1), (_) {
      if (!mounted) return;
      _throughput.value =
          '${_fmtRate(_windowBytes)} · ${_totalBytes ~/ 1024} KB';
      _windowBytes = 0;
    });
  }

  /// Install a blank terminal emulator and a matching UTF-8 decoder, discarding
  /// whatever the previous one held.
  ///
  /// Call sites are [initState] and the explicit reconnect. It is the escape
  /// hatch from a *desynchronised* pane: tmux repaints incrementally and its
  /// stream carries no full-screen clear, so once this end's grid stops matching
  /// tmux's model of the screen — anything from a dropped frame to a paint that
  /// arrived at the wrong width — nothing in the byte stream ever puts it right.
  /// A fresh attach repaints the whole screen, so a blank grid to paint it onto
  /// is all that is needed; the emulator cannot do it for us, as this fork's
  /// parser leaves RIS (`ESC c`) unimplemented (core/escape/parser.dart:104).
  void _resetEmulator() {
    _terminal = widget.terminalFactory?.call() ?? Terminal(maxLines: 10000);
    // The controller outlives the terminal (we own it), but its selection is a
    // pair of `CellAnchor`s holding `BufferLine`s of the buffer being discarded
    // (ui/controller.dart:21-22, core/buffer/line.dart:367-381). Nothing
    // detaches them when the terminal goes, so a selection made before the
    // reset would keep reporting a live range — painting a phantom highlight
    // over unrelated content on the new grid, and pinning the old lines.
    _terminalController.clearSelection();
    // Forward each decoded chunk to the terminal as it arrives. A plain
    // `Sink<String>` emits per-`add` (unlike `StringConversionSink.withCallback`,
    // which only fires its callback on `close`), while the chunked UTF-8 decoder
    // still buffers a partial multibyte codepoint split across WS frames until
    // it completes.
    _decoder = utf8.decoder.startChunkedConversion(_ChunkSink(_write));
    _terminal.onOutput = _sendText;
    _terminal.onResize = (cols, rows, pixelWidth, pixelHeight) {
      unawaited(
        widget.api.terminalResize(attachId: _attachId, cols: cols, rows: rows),
      );
    };
  }

  /// Feed one decoded chunk to the emulator, containing a parser failure to the
  /// chunk that caused it.
  ///
  /// An emulator that throws mid-parse abandons the rest of the chunk, and tmux
  /// repaints incrementally with no full-screen clear — so those bytes never come
  /// again and the grid is wrong from that point on for the life of the attach.
  /// Left to propagate, the throw is also an uncaught async error *per chunk*,
  /// and reporting one (a stack trace built and dumped on the UI thread) for
  /// every screenful of a busy pane is itself enough to starve the frame budget
  /// on a phone. That pair is what an `omp` pane looked like before the pinned
  /// fork stopped `CSI 1 K` at column 0 throwing out of `Terminal.write`
  /// (guarded by `terminal_page_test.dart`, and by `BufferLine.eraseRange`'s own
  /// tests in the fork).
  ///
  /// So: report the first one, keep the attach up, and say on the status bar that
  /// the pane is stale. Nothing here can repair the grid — only a fresh attach
  /// repaints the whole screen onto a blank one, which is exactly what the
  /// reconnect button already does.
  void _write(String text) {
    try {
      _terminal.write(text);
    } catch (e, stack) {
      final first = _writeFailures == 0;
      _writeFailures++;
      if (first) {
        FlutterError.reportError(
          FlutterErrorDetails(
            exception: e,
            stack: stack,
            library: 'claude-commander',
            context: ErrorDescription(
              'writing pane output to the terminal emulator (further '
              'failures on this attach are counted, not reported)',
            ),
          ),
        );
      }
      if (mounted) setState(() {});
    }
  }

  /// Cache the heartbeat deadline. No [setState]: nothing renders from it.
  Future<void> _loadDeadAfter() async {
    try {
      final deadAfter = await widget.api.attachDeadAfter();
      if (mounted) _deadAfter = deadAfter;
    } catch (_) {
      // Leave it null. A resume then only re-attaches an attach we were *told*
      // had ended — never one we merely suspect, since without the contract's
      // deadline we'd be guessing at the cost of the user's scrollback position.
    }
  }

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) {
    switch (state) {
      case AppLifecycleState.paused:
        // `paused` is the marker, not `inactive`: it is the earliest point at
        // which Android may freeze the process and stop our heartbeat pongs. It
        // is only an upper bound on when they actually stop (see [_onResumed]).
        // `inactive` would be worse: it also fires for a pulled-down notification
        // shade or a permission dialog, where the app keeps answering pings and
        // the attach is fine, so starting the clock there would re-attach healthy
        // sockets. `??=` keeps the earliest of a repeated pause.
        _leftForegroundAt ??= _now();
      case AppLifecycleState.resumed:
        _onResumed();
      // `hidden`/`inactive` are transient steps on the way to and from `paused`,
      // and `detached` means we're being torn down.
      case AppLifecycleState.hidden:
      case AppLifecycleState.inactive:
      case AppLifecycleState.detached:
        break;
    }
  }

  /// Back in the foreground: re-open the attach when it is dead, or long enough
  /// gone that it almost certainly is.
  ///
  /// Two triggers. Either the attach already reported detached/error (possibly
  /// delivered while we were away), which is certain; or we were away longer than
  /// the server's heartbeat tolerance, which no *silent* attach survives. The
  /// second is what a half-open socket needs: when the network path vanishes
  /// without a TCP FIN, no detach frame ever arrives and the UI would otherwise
  /// sit on a frozen pane that still claims to be attached.
  ///
  /// The second trigger is a heuristic, not a proof, because `paused` is not the
  /// same event as *frozen*: Android does not stop the cdylib's tokio threads at
  /// `paused`, so they keep answering pings until the cached-app freezer actually
  /// hits — which can lag by minutes, or never come (a paused-but-visible app in
  /// legacy split-screen, some OEM/charging configurations). Such a resume
  /// re-attaches a live socket and costs a scrolled copy-mode position. There is
  /// no client-observable freeze signal to do better with, and the alternative
  /// failure — coming back to a permanently dead pane — is the bug being fixed.
  ///
  /// A shorter absence deliberately changes nothing: the attach is probably still
  /// live, and re-attaching spawns a fresh `tmux attach-session` child, so a glance
  /// at a notification must not cost the user their place in the scrollback.
  void _onResumed() {
    final leftAt = _leftForegroundAt;
    _leftForegroundAt = null;
    if (_ended) {
      _connect();
      return;
    }
    final deadAfter = _deadAfter;
    if (leftAt == null || deadAfter == null) return;
    if (_now().difference(leftAt) > deadAfter) _connect();
  }

  @override
  void didChangeDependencies() {
    super.didChangeDependencies();
    // Register the current attach with the store (if one is in scope) so its
    // reconnect/dispose tears the attach down before releasing the handle.
    _store = CommanderStoreScope.of(context);
    _store?.setActiveTerminalAttach(_attachId);
  }

  /// Open (or re-open) the WS attach with a fresh attach id. A re-attach replays
  /// tmux's pane, so output simply continues appending.
  ///
  /// The outgoing attach is detached explicitly, because cancelling `_sub` alone
  /// does not stop it: its cdylib registry entry keeps the pump's control sender
  /// alive, so the pump's `rx.recv()` never ends, and the pump otherwise only
  /// learns Dart is gone by failing to push an Output frame — which never comes on
  /// an idle pane, and *never* on the half-open socket the reconnect button exists
  /// to escape. That left a zombie pump holding the WS open and still answering
  /// the server's pings, so the server kept its `tmux attach-session` child alive.
  /// Both callers can now run against a live attach (the always-enabled button, a
  /// resume past the deadline), so this is no longer a dead-attach-only path.
  void _connect() {
    _sub?.cancel();
    // A documented no-op for an id that was never attached, which covers both the
    // initial call from `initState` and an already-ended attach.
    unawaited(widget.api.terminalDetach(attachId: _attachId));
    _attachId = const Uuid().v4();
    _store?.setActiveTerminalAttach(_attachId);
    setState(() {
      _status = 'connecting…';
      _ended = false;
    });
    // Open the attach only once the view has been laid out, because the
    // handshake carries our cols/rows and a `Terminal` that has never been laid
    // out still reports xterm's default 80x24 (terminal.dart:116-118). That
    // default is not merely stale, it is *exactly* the server's own fallback
    // geometry — so handing it over would reproduce the pre-fix bug byte for
    // byte, which is what makes this deferral load-bearing rather than tidy.
    // Announcing a size after the handshake is not good enough: the server
    // spawns `tmux attach-session` on the spot and tmux paints a whole screen
    // into the socket, so a wrong size in the handshake means one paint at that
    // wrong width — which this end wraps at its own width and tmux's
    // incremental repaint never clears.
    final id = _attachId;
    WidgetsBinding.instance.addPostFrameCallback((_) {
      // A second `_connect` may have superseded us between the two frames (a
      // resume racing the button); that call owns the attach now.
      if (!mounted || _attachId != id) return;
      _openAttach(id);
    });
  }

  void _openAttach(String id) {
    _sub = widget.api
        .attachTerminal(
          handle: widget.handle,
          attachId: id,
          sessionId: widget.session.id,
          kind: widget.kind,
          cols: _terminal.viewWidth,
          rows: _terminal.viewHeight,
        )
        .listen(
          _onEvent,
          onError: (Object e) => setState(() {
            _status = 'stream error: $e';
            _ended = true;
          }),
        );
  }

  /// The user-driven reconnect. Unlike the lifecycle-driven [_connect], this
  /// also throws away the emulator: the button's job is to rescue a pane that is
  /// wrong on screen, and a stale grid is one of the ways it gets that way. The
  /// automatic resume path deliberately keeps its buffer — re-attaching already
  /// costs the scrollback position, and wiping it on every glance at a
  /// notification would compound that.
  void _reconnect() {
    setState(() {
      _resetEmulator();
      _terminalGeneration++;
    });
    _connect();
  }

  void _onEvent(TerminalEvent e) {
    switch (e.kind) {
      case TerminalEventKind.output:
        _totalBytes += e.bytes.length;
        _windowBytes += e.bytes.length;
        _decoder.add(e.bytes);
      case TerminalEventKind.ready:
        setState(() => _status = 'attached: ${e.text}');
        // Belt and braces. The handshake already carried our size, so against a
        // current server the PTY has it and this changes nothing — on Linux an
        // ioctl setting the size a PTY already has raises no SIGWINCH
        // (`tty_do_resize` in drivers/tty/tty_ioctl.c returns early when the
        // winsize is unchanged, before it signals). It still matters
        // against a server predating the handshake's cols/rows, which starts
        // the PTY at its own default and learns our size only from a Resize —
        // and xterm's onResize fires only on a *change*, so on a same-size
        // (re)connect nothing else would announce it.
        unawaited(
          widget.api.terminalResize(
            attachId: _attachId,
            cols: _terminal.viewWidth,
            rows: _terminal.viewHeight,
          ),
        );
      case TerminalEventKind.detached:
        setState(() {
          _status = 'detached: ${e.text}';
          _ended = true;
        });
      case TerminalEventKind.error:
        setState(() {
          _status = 'error: ${e.text}';
          _ended = true;
        });
    }
  }

  void _send(List<int> bytes) => unawaited(
    widget.api.terminalSendInput(attachId: _attachId, bytes: bytes),
  );

  /// Everything the emulator wants to send: typed characters, the escape
  /// sequences its input handler builds for special keys, pastes, and its own
  /// replies to device queries. The single funnel is what lets an armed Ctrl
  /// (see [_ctrlArmed]) reach a chord the on-screen row has no key for — the
  /// soft keyboard's characters arrive here and nowhere else.
  void _sendText(String data) {
    final chord = _ctrlArmed ? ctrlChord(data) : null;
    if (chord != null && chord.consumed) {
      setState(() => _ctrlArmed = false);
    }
    _send(utf8.encode(chord?.output ?? data));
  }

  // -- image attach --------------------------------------------------------

  /// Offer the available image sources and act on the choice. Camera only
  /// appears where the platform supports it (Android); Linux gets the file
  /// dialog and the clipboard.
  Future<void> _attachImage() async {
    if (_imageBusy) return;
    setState(() => _imageBusy = true);
    try {
      await _pickAndAttach();
    } finally {
      if (mounted) setState(() => _imageBusy = false);
    }
  }

  Future<void> _pickAndAttach() async {
    final source = await showModalBottomSheet<_ImageSource>(
      context: context,
      backgroundColor: CommanderTokens.of(context).canvasRaised,
      builder: (sheetContext) => SafeArea(
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            _sheetTile(
              sheetContext,
              Icons.photo_library_outlined,
              _imagePicker.supportsCamera ? 'Photo library' : 'Choose file',
              _ImageSource.gallery,
            ),
            if (_imagePicker.supportsCamera)
              _sheetTile(
                sheetContext,
                Icons.photo_camera_outlined,
                'Take photo',
                _ImageSource.camera,
              ),
            _sheetTile(
              sheetContext,
              Icons.content_paste,
              'Paste from clipboard',
              _ImageSource.clipboard,
            ),
          ],
        ),
      ),
    );
    if (source == null) return;
    await _attachFrom(source);
  }

  Widget _sheetTile(
    BuildContext sheetContext,
    IconData icon,
    String label,
    _ImageSource source,
  ) => ListTile(
    leading: Icon(
      icon,
      color: CommanderTokens.of(sheetContext).textMuted,
      size: 20,
    ),
    title: Text(label),
    onTap: () => Navigator.of(sheetContext).pop(source),
  );

  /// Resolve `source` to bytes and upload them. Cancellation is silent; every
  /// other failure surfaces as a snackbar.
  Future<void> _attachFrom(_ImageSource source) async {
    try {
      final bytes = source == _ImageSource.clipboard
          ? await _readClipboardImage()
          : await _readPickedImage(source);
      if (bytes == null) return;
      await _uploadImage(bytes);
    } catch (e) {
      _notify('Could not attach image: ${errorText(e, capitalize: false)}');
    }
  }

  /// Clipboard bytes, or null (with a note) when it holds no image.
  Future<Uint8List?> _readClipboardImage() async {
    final bytes = await _clipboardImages.readImage();
    if (bytes == null) {
      _notify('No image on the clipboard');
    }
    return bytes;
  }

  /// Picked-file bytes, or null when the user cancelled or the file is over the
  /// cap. Size is checked from the file *length* first, so a huge phone photo is
  /// refused without being read into memory.
  Future<Uint8List?> _readPickedImage(_ImageSource source) async {
    final file = await _imagePicker.pick(
      source == _ImageSource.camera
          ? ImagePickSource.camera
          : ImagePickSource.gallery,
    );
    if (file == null) return null; // cancelled
    final maxBytes = await widget.api.imageMaxBytes();
    final length = await file.length();
    if (length > maxBytes) {
      _notify(
        'Image is ${_fmtSize(length)} — the limit is ${_fmtSize(maxBytes)}',
      );
      return null;
    }
    return file.readAsBytes();
  }

  /// Upload to the agent pane. No success message: the server types the path
  /// into the pane, so it arrives on screen through the attach output stream.
  /// The re-entrancy guard ([_imageBusy]) is owned by the callers
  /// ([_attachImage] / [_pasteClipboard]), which set it before their first
  /// `await`; this only drives the spinner.
  Future<void> _uploadImage(Uint8List bytes) async {
    if (mounted) setState(() => _uploading = true);
    try {
      await widget.api.pasteImage(
        handle: widget.handle,
        id: widget.session.id,
        bytes: bytes,
      );
    } finally {
      if (mounted) setState(() => _uploading = false);
    }
  }

  /// Ctrl+V: attach a clipboard image if there is one, otherwise fall back to
  /// the plain text paste that `xterm` would have done.
  ///
  /// `xterm` binds Ctrl+V to `PasteTextIntent`, handled by a `TerminalActions`
  /// widget *inside* `TerminalView` — so an outer `Actions` override would be
  /// shadowed. `TerminalView.onKeyEvent` has higher priority than both its
  /// shortcuts and its input handler, which makes it the one place this can be
  /// intercepted; that also means the text-paste fallback has to be reproduced
  /// here, since pre-empting the key skips xterm's own handler.
  KeyEventResult _onKeyEvent(FocusNode node, KeyEvent event) {
    // Alt/Meta must be excluded, not ignored: xterm's own activator requires
    // them absent, so Ctrl+Meta+V previously reached the PTY as 0x16. Matching
    // loosely here would silently steal that.
    final keyboard = HardwareKeyboard.instance;
    final isPasteChord =
        event.logicalKey == LogicalKeyboardKey.keyV &&
        keyboard.isControlPressed &&
        !keyboard.isShiftPressed &&
        !keyboard.isAltPressed &&
        !keyboard.isMetaPressed;
    if (!isPasteChord || !_canAttachImage || _ended) {
      return KeyEventResult.ignored;
    }
    // `KeyRepeatEvent` is a *sibling* of `KeyDownEvent`, not a subclass, so a
    // held key must be matched explicitly — and it must still be swallowed.
    // xterm's `SingleActivator` defaults to `includeRepeats: true`, so letting a
    // repeat through would fire its text paste on every tick while our upload
    // was still running.
    if (event is! KeyDownEvent && event is! KeyRepeatEvent) {
      return KeyEventResult.ignored;
    }
    if (event is KeyRepeatEvent || _imageBusy) return KeyEventResult.handled;
    unawaited(_pasteClipboard());
    return KeyEventResult.handled;
  }

  /// Sets [_imageBusy] synchronously before its first `await`, so a second press
  /// arriving during the clipboard read is dropped by [_onKeyEvent].
  Future<void> _pasteClipboard() async {
    if (_imageBusy) return;
    setState(() => _imageBusy = true);
    try {
      final image = await _clipboardImages.readImage();
      if (image != null) {
        await _uploadImage(image);
        return;
      }
    } catch (e) {
      _notify('Could not attach image: ${errorText(e, capitalize: false)}');
      return;
    } finally {
      if (mounted) setState(() => _imageBusy = false);
    }
    // No image — behave exactly as xterm's own Ctrl+V would have, including
    // clearing the selection (see `TerminalActions`' PasteTextIntent handler).
    // Pre-empting the key skips xterm's handler, so this fidelity is ours to keep.
    final text = (await Clipboard.getData(Clipboard.kTextPlain))?.text;
    if (text != null && text.isNotEmpty) {
      _terminal.paste(text);
      _terminalController.clearSelection();
    }
  }

  /// `maybeOf`, not `of`: this widget also renders inside desktop panes, and a
  /// missing messenger must not turn a minor notice into a crash.
  void _notify(String message) {
    if (!mounted) return;
    ScaffoldMessenger.maybeOf(
      context,
    )?.showSnackBar(SnackBar(content: Text(message)));
  }

  /// MiB, not MB: the cap is a binary quantity (`MAX_IMAGE_BYTES` is
  /// `10 * 1024 * 1024`), so dividing by 1024² and calling it "MB" would misstate
  /// the limit the user is being held to.
  static String _fmtSize(int bytes) =>
      '${(bytes / (1024 * 1024)).toStringAsFixed(1)} MiB';

  @override
  void dispose() {
    WidgetsBinding.instance.removeObserver(this);
    _meter?.cancel();
    _throughput.dispose();
    unawaited(widget.api.terminalDetach(attachId: _attachId));
    // Guarded clear: if the wide pane already swapped in another attach (agent↔
    // shell), its initState registered the new id before this dispose runs, so
    // only clear when we're still the registered attach.
    _store?.clearActiveTerminalAttach(_attachId);
    _sub?.cancel();
    _decoder.close();
    // Ours to dispose: `TerminalView` only disposes a controller it created
    // itself, and we pass one in.
    _terminalController.dispose();
    super.dispose();
  }

  String _fmtRate(int bytesPerSec) {
    if (bytesPerSec >= 1024 * 1024) {
      return '${(bytesPerSec / (1024 * 1024)).toStringAsFixed(1)} MB/s';
    }
    if (bytesPerSec >= 1024) {
      return '${(bytesPerSec / 1024).toStringAsFixed(1)} KB/s';
    }
    return '$bytesPerSec B/s';
  }

  @override
  Widget build(BuildContext context) {
    // How much of us the soft keyboard covers. Zero when there is no keyboard —
    // and also zero when an ancestor Scaffold already consumed the inset by
    // shrinking us (`resizeToAvoidBottomInset: true`), which makes the panning
    // below a no-op. That is the case in the wide shell, which therefore still
    // resizes the pane on a soft keyboard — a known limitation, and a
    // touch-device-only one, since a desktop has no soft keyboard.
    final obscured = MediaQuery.viewInsetsOf(context).bottom;
    final t = CommanderTokens.of(context);
    // Sideways on a phone, the two bars this widget draws are 96dp of a 360dp
    // screen (48 apiece); compacted they are 71. Keyed off the viewport's
    // *size*, which a soft keyboard does not change (it moves `viewInsets`) —
    // so the pane's row count cannot move with the keyboard, which is the one
    // thing this whole page is built around.
    final short = isShortViewport(context);

    return ColoredBox(
      color: t.terminalBg,
      child: Column(
        children: [
          // Fixed: the status line stays put while the pane pans beneath it.
          _statusBar(context, short),
          Expanded(
            // Pan, don't resize. `xterm` derives the PTY's cols/rows from the
            // view's laid-out size, so letting the keyboard shrink the view
            // would resize the remote pane — and tmux answers a resize by
            // sliding a scrolled copy-mode view forward by a viewport height (it
            // doesn't compensate for the lines the shrink pushes into the
            // history), losing the user's place for good.
            //
            // So the pannable stack always fills this box — its height is a
            // function of the body alone, which the page holds constant — and we
            // translate it up instead. The pane's geometry is therefore constant
            // *by construction*: no keyboard-dependent arithmetic to get wrong,
            // and nothing to overflow when the keyboard is taller than the space
            // we have (landscape), where the worst case is simply that the pane
            // slides out of view rather than being resized.
            child: ClipRect(
              child: Transform.translate(
                offset: Offset(0, -obscured),
                child: Column(
                  children: [
                    Expanded(
                      child: TerminalView(
                        _terminal,
                        key: ValueKey(_terminalGeneration),
                        autofocus: true,
                        backgroundOpacity: 1,
                        theme: terminalThemeFor(t),
                        textStyle: TerminalStyle(fontFamily: t.mono),
                        padding: const EdgeInsets.symmetric(
                          horizontal: 14,
                          vertical: 8,
                        ),
                        controller: _terminalController,
                        onKeyEvent: _onKeyEvent,
                      ),
                    ),
                    // Rides up with the pane, landing just above the keyboard.
                    if (widget.showModifierBar)
                      _ModifierBar(
                        onSend: _send,
                        ctrlArmed: _ctrlArmed,
                        compact: short,
                        onToggleCtrl: () =>
                            setState(() => _ctrlArmed = !_ctrlArmed),
                      ),
                  ],
                ),
              ),
            ),
          ),
        ],
      ),
    );
  }

  /// The dot colour reflects the link state: the working accent while attached,
  /// danger once the attach has ended (reconnect offered), attention while
  /// connecting.
  Color get _statusColor {
    final t = CommanderTokens.of(context);
    if (_ended) return t.danger;
    if (_status.startsWith('attached')) return t.working;
    return t.attention;
  }

  /// The status line: link state, what the attach is doing, throughput, and the
  /// two actions (attach image, reconnect).
  ///
  /// [compact] brings it from 48dp down to 35 for a short viewport, by dropping
  /// the throughput readout and sizing the icon buttons to [_shortActionSize]
  /// instead of Material's 40dp minimum. It also picks up the two things a
  /// titleless page hands over — [TerminalBody.onBack] and
  /// [TerminalBody.barTitle] — so the bar the pane already pays for carries
  /// them rather than a second bar being drawn to.
  Widget _statusBar(BuildContext context, bool compact) {
    final t = CommanderTokens.of(context);
    final onBack = widget.onBack;
    final title = widget.barTitle;
    return Container(
      key: terminalStatusBar,
      padding: EdgeInsets.only(
        left: onBack != null ? 0 : (compact ? 10 : 14),
        right: compact ? 2 : 4,
        top: compact ? 1 : 4,
        bottom: compact ? 1 : 4,
      ),
      decoration: BoxDecoration(
        color: t.canvasRaised,
        border: Border(bottom: BorderSide(color: t.borderSubtle)),
      ),
      child: Row(
        children: [
          if (onBack != null)
            _barAction(
              context,
              compact: compact,
              icon: const Icon(Icons.arrow_back),
              tooltip: 'Back',
              onPressed: onBack,
              key: terminalBackButton,
            ),
          Container(
            width: compact ? 6 : 7,
            height: compact ? 6 : 7,
            decoration: BoxDecoration(
              color: _statusColor,
              shape: BoxShape.circle,
            ),
          ),
          SizedBox(width: compact ? 6 : 8),
          Expanded(
            child: Text(
              // The name only when the page is not showing it, so the bar never
              // says it twice.
              title == null ? _status : '$title · $_status',
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              style: t.meta(size: 10, color: t.textMuted),
            ),
          ),
          // Only ever visible after the emulator has thrown, which the fork's
          // own tests say it should not: the pane is stale and cannot recover
          // from the byte stream, so point at the reconnect that can.
          if (_writeFailures > 0) ...[
            Icon(Icons.warning_amber_rounded, size: 13, color: t.attention),
            const SizedBox(width: 4),
            Text(
              'stale — reconnect',
              style: t.meta(size: 10, color: t.attention),
            ),
            const SizedBox(width: 8),
          ],
          // Dropped when compact: on a landscape phone the width goes to the
          // session name, and the throughput is the one thing here that is
          // curiosity rather than state or action.
          if (!compact)
            ValueListenableBuilder<String>(
              valueListenable: _throughput,
              builder: (context, value, _) =>
                  Text(value, style: t.meta(size: 10, color: t.textFaint)),
            ),
          // Agent attaches only: the server injects the image path into the
          // agent pane, so on a shell attach this would type somewhere the user
          // can't see. Lives here rather than in the modifier bar so desktop
          // layouts — which run without that bar — get it too.
          if (_canAttachImage)
            _barAction(
              context,
              compact: compact,
              onPressed: _imageBusy || _ended ? null : _attachImage,
              icon: _uploading
                  ? SizedBox.square(
                      dimension: 18,
                      child: CircularProgressIndicator(
                        strokeWidth: 2,
                        color: t.textMuted,
                      ),
                    )
                  : const Icon(Icons.image_outlined),
              tooltip: 'Attach image',
            ),
          // Never gated on [_ended]. A half-open socket — the network path gone
          // without a TCP FIN, so no detach frame ever arrives — leaves the UI
          // reading "attached" over a frozen pane, and that is precisely when the
          // user needs this button. Disabling it there turns a recoverable stall
          // into a dead end.
          _barAction(
            context,
            compact: compact,
            onPressed: _reconnect,
            icon: const Icon(Icons.refresh),
            tooltip: 'Reconnect',
          ),
        ],
      ),
    );
  }

  /// One icon action in the status bar.
  ///
  /// Compact drops Material's 40dp minimum tap target to [_shortActionSize],
  /// which is what lets the whole bar come in at 35dp rather than 48. A
  /// deliberate trade: sideways, a 48dp bar of secondary actions is 13% of the
  /// display, and these three are recoverable one-taps rather than destructive
  /// ones.
  Widget _barAction(
    BuildContext context, {
    required bool compact,
    required Widget icon,
    required String tooltip,
    required VoidCallback? onPressed,
    Key? key,
  }) {
    final t = CommanderTokens.of(context);
    return IconButton(
      key: key,
      // Compact drops the density adjustment rather than stacking it on the
      // explicit style below: `VisualDensity.compact` is (-2, -2), scaled ×4
      // into a -8dp base size adjustment (Flutter 3.47.0
      // `material/theme_data.dart:3225,3307-3314`) applied to the button's
      // minimum constraints (`material/button_style_button.dart:456,473-480`),
      // so the two together would land 8dp short of the size being pinned.
      visualDensity: compact ? VisualDensity.standard : VisualDensity.compact,
      // A tighter `constraints` alone will not do it, and not for the reason it
      // looks like: M3 converts that box straight into the style's
      // `minimumSize`/`maximumSize` (Flutter 3.47.0
      // `material/icon_button.dart:720-737`), so it *does* beat the 40dp
      // default (`:1128`). What it cannot touch is `tapTargetSize`, which
      // defaults to the theme's — `MaterialTapTargetSize.padded` — and pads the
      // outer box to 48 regardless (`:1155`), nor the default padding. Hence
      // the explicit style rather than a box.
      //
      // Measured: box-only came out 43dp tall (48 padded target, less 8 for the
      // density below, plus the bar's padding and border); this comes out 35.
      style: compact
          ? IconButton.styleFrom(
              minimumSize: const Size.square(_shortActionSize),
              maximumSize: const Size.square(_shortActionSize),
              padding: EdgeInsets.zero,
              tapTargetSize: MaterialTapTargetSize.shrinkWrap,
            )
          : null,
      iconSize: compact ? 16 : 18,
      onPressed: onPressed,
      icon: icon,
      color: t.textMuted,
      disabledColor: t.textDim,
      tooltip: tooltip,
    );
  }
}

/// The phone (stacked-navigation) terminal screen: a [ChromePage] titled by the
/// session, wrapping a [TerminalBody] with the on-screen modifier bar enabled.
class TerminalPage extends StatelessWidget {
  final CommanderApi api;
  final String handle;
  final SessionInfo session;

  /// Which pane to attach to: the agent pane (default) or the paired shell.
  final AttachKind kind;

  /// Forwarded to [TerminalBody] so tests can inject fake image sources; `null`
  /// means "use the real platform implementation".
  final ImagePickerService? imagePicker;
  final ClipboardImageReader? clipboardImages;

  /// Forwarded to [TerminalBody] so a test can drive the foreground-reconnect
  /// deadline; `null` means [DateTime.now].
  final DateTime Function()? clock;

  /// Forwarded to [TerminalBody] so a test can drive its emulator-failure path;
  /// `null` means the real emulator.
  final Terminal Function()? terminalFactory;

  const TerminalPage({
    super.key,
    required this.api,
    required this.handle,
    required this.session,
    this.kind = AttachKind.agent,
    this.imagePicker,
    this.clipboardImages,
    this.clock,
    this.terminalFactory,
  });

  @override
  Widget build(BuildContext context) {
    final isShell = kind == AttachKind.shell;
    final title = isShell ? '${session.title} · shell' : session.title;
    // Sideways on a phone the page's own title bar is the largest single thing
    // between the user and the pane, and the terminal already draws a bar with
    // room for the name. So a short viewport gets a **titleless** page — no
    // Mission Control app bar, no LCARS title block — and the body carries both
    // the name and, where the chrome no longer offers one, the way back.
    final short = isShortViewport(context);
    final needsOwnBack =
        short &&
        !Chrome.of(context).backSurvivesTitleless &&
        Navigator.of(context).canPop();
    return ChromePage(
      code: '47-T',
      title: short ? null : title,
      // The keyboard must not shrink the body: [TerminalBody] insets its own
      // chrome and pans the pane instead, so the remote PTY never sees a resize.
      // ChromeInsets.pan *is* main's resizeToAvoidBottomInset:false plus
      // SafeArea(maintainBottomViewPadding: true) — see applyChromeInsets. It
      // lives in the chrome so LCARS cannot diverge from it.
      insets: ChromeInsets.pan,
      body: TerminalBody(
        api: api,
        handle: handle,
        session: session,
        kind: kind,
        barTitle: short ? title : null,
        onBack: needsOwnBack ? () => Navigator.of(context).maybePop() : null,
        imagePicker: imagePicker,
        clipboardImages: clipboardImages,
        clock: clock,
        terminalFactory: terminalFactory,
      ),
    );
  }
}

/// A minimal `Sink<String>` that forwards each decoded chunk to [onData] the
/// moment it arrives — so terminal output renders live rather than only when
/// the decoder is closed.
class _ChunkSink implements Sink<String> {
  final void Function(String chunk) onData;
  const _ChunkSink(this.onData);

  @override
  void add(String data) => onData(data);

  @override
  void close() {}
}

/// Where the attach-image action should get its bytes from. Distinct from
/// [ImagePickSource] because the clipboard isn't a picker source.
enum _ImageSource { gallery, camera, clipboard }

/// On-screen keys for touch — the modifiers and arrows a soft keyboard can't
/// easily produce. Each sends the raw byte sequence the PTY expects, except
/// `Ctrl`, which arms [ctrlArmed] so the *next* character typed on the keyboard
/// becomes a chord: the row has room for a handful of presets (^C, ^D, …) and
/// this is how the rest — ^W, ^X, ^O — are reachable at all from a phone.
class _ModifierBar extends StatelessWidget {
  final void Function(List<int> bytes) onSend;

  /// Whether the Ctrl key is currently armed. Owned by the parent, which is
  /// where the arm is spent (on the next character the emulator sends).
  final bool ctrlArmed;
  final VoidCallback onToggleCtrl;

  /// Short-viewport form: the same keys in a 36dp row rather than a 48dp one.
  final bool compact;

  const _ModifierBar({
    required this.onSend,
    required this.ctrlArmed,
    required this.onToggleCtrl,
    this.compact = false,
  });

  static const _esc = [0x1b];
  static const _tab = [0x09];
  // Ctrl-<letter> is the letter's code & 0x1f.
  static const _ctrlC = [0x03];
  static const _ctrlD = [0x04];
  static const _ctrlZ = [0x1a];
  static const _ctrlL = [0x0c];
  static const _ctrlR = [0x12];
  static const _ctrlA = [0x01];
  static const _ctrlE = [0x05];
  static const _ctrlU = [0x15];
  static const _up = [0x1b, 0x5b, 0x41];
  static const _down = [0x1b, 0x5b, 0x42];
  static const _right = [0x1b, 0x5b, 0x43];
  static const _left = [0x1b, 0x5b, 0x44];
  static const _home = [0x1b, 0x5b, 0x48];
  static const _end = [0x1b, 0x5b, 0x46];
  static const _pgUp = [0x1b, 0x5b, 0x35, 0x7e];
  static const _pgDn = [0x1b, 0x5b, 0x36, 0x7e];

  @override
  Widget build(BuildContext context) {
    final t = CommanderTokens.of(context);
    // No SafeArea here: [TerminalBody] already insets the bottom chrome, and a
    // bar whose height changed with the keyboard would change the pane's rows.
    return Container(
      key: terminalModifierBar,
      height: compact ? 36 : 48,
      decoration: BoxDecoration(
        color: t.terminalBg,
        border: Border(top: BorderSide(color: t.borderSubtle)),
      ),
      child: ListView(
        scrollDirection: Axis.horizontal,
        padding: EdgeInsets.symmetric(
          horizontal: compact ? 7 : 10,
          vertical: compact ? 4 : 7,
        ),
        children: [
          _key(context, 'Esc', () => onSend(_esc)),
          _key(context, 'Tab', () => onSend(_tab)),
          _key(context, 'Ctrl', onToggleCtrl, armed: ctrlArmed),
          _key(context, '^C', () => onSend(_ctrlC)),
          _key(context, '^D', () => onSend(_ctrlD)),
          _key(context, '^Z', () => onSend(_ctrlZ)),
          _key(context, '^L', () => onSend(_ctrlL)),
          _key(context, '^R', () => onSend(_ctrlR)),
          _key(context, '^A', () => onSend(_ctrlA)),
          _key(context, '^E', () => onSend(_ctrlE)),
          _key(context, '^U', () => onSend(_ctrlU)),
          _key(context, '↑', () => onSend(_up)),
          _key(context, '↓', () => onSend(_down)),
          _key(context, '←', () => onSend(_left)),
          _key(context, '→', () => onSend(_right)),
          _key(context, 'Home', () => onSend(_home)),
          _key(context, 'End', () => onSend(_end)),
          _key(context, 'PgUp', () => onSend(_pgUp)),
          _key(context, 'PgDn', () => onSend(_pgDn)),
        ],
      ),
    );
  }

  /// A single key pill: a raised mono chip that fires its raw byte sequence on
  /// tap. Deliberately not a Material button so it matches the deck's flat pills
  /// and stays compact in the horizontal strip.
  ///
  /// [armed] is for the sticky Ctrl: a held modifier has to be visible, or the
  /// keystroke after it lands somewhere the user didn't ask for with nothing on
  /// screen to explain why.
  Widget _key(
    BuildContext context,
    String label,
    VoidCallback onTap, {
    bool armed = false,
  }) {
    final t = CommanderTokens.of(context);
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 3),
      child: Material(
        color: armed ? t.surfaceSelected : t.surface,
        borderRadius: BorderRadius.circular(7),
        child: InkWell(
          onTap: onTap,
          borderRadius: BorderRadius.circular(7),
          child: Container(
            alignment: Alignment.center,
            padding: EdgeInsets.symmetric(horizontal: compact ? 10 : 12),
            decoration: BoxDecoration(
              borderRadius: BorderRadius.circular(7),
              border: Border.all(color: armed ? t.primary : t.border),
            ),
            child: Text(
              label,
              style: t.meta(
                size: 10.5,
                weight: FontWeight.w600,
                color: armed ? t.primary : t.textBright,
              ),
            ),
          ),
        ),
      ),
    );
  }
}

/// The status bar's icon-button side in a short viewport. Below Material's 40dp
/// minimum on purpose — see `_barAction` — and matched to the modifier bar's key
/// pills, which are the same height and are the row a thumb actually works in.
const _shortActionSize = 32.0;
