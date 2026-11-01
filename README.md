# compux

Native **computer use** (screen capture + mouse/keyboard input) for Elixir, backed
by a crash-isolated Rust sidecar spawned over a Port — **not** a NIF.

```elixir
{:ok, cu}   = Compux.start()
{:ok, shot} = Compux.screenshot(cu, region: {0, 0, 800, 600})
image       = shot["observation_id"]
:ok         = Compux.click(cu, {120, 80}, observation_id: image, modifiers: [:cmd])

{:ok, list} = Compux.elements(cu)
save        = Enum.find(list["elements"], &("press" in &1["actions"]))
:ok         = Compux.press(cu, save["element_ref"], observation_id: list["observation_id"])

:ok         = Compux.stop(cu)
```

## Why a Port, not a NIF

A GUI driver segfaults for real reasons — a denied macOS TCC permission, an
`xcap`/`enigo` quirk, a raw Accessibility FFI call. As a **NIF** that crash takes
down the whole BEAM node; as a **separate process** behind a Port it is a
recoverable `:exit_status`. Capture + PNG encode is also tens of ms of blocking
work that has no business on a scheduler thread. So the Rust backend is a spawned
executable, and Elixir owns it over stdin/stdout with a line-framed JSON protocol.

The split:

- **`compux` (the library)** owns the *mechanism* — the wire protocol
  (`Compux.Protocol`), the Port plumbing (`Compux.PortDriver`) and the ergonomic
  API (`Compux`). The coordinate math and the images it applies to live in the
  sidecar, which is the only side that can measure a display.
- **The caller** owns the *policy* — when an action is allowed, confirmation, and
  telemetry. `compux` makes no such decisions; it returns `{:ok, _} | {:error, _}`.

## Actions

`screenshot`, `left_click` / `right_click` / `double_click`, `mouse_move`,
`left_click_drag`, `scroll`, `type`, `paste` (clipboard-based — fast and
unicode-safe for long text), `key` (chords like `"cmd+shift+4"`, incl. `f1`–`f12`),
`wait`, `wait_for_change` (block until the screen changes, then return the new
frame), `inspect` (the accessibility element under a point), `elements` (the
interactive accessibility controls, each with a reference, a click point and what
it can do), `press` / `set_value` (act on a control by name, through the
accessibility API), and `select_target` / `release_target` (bind ONE window and
work inside it).

## One window, bound

`select_target` takes the `id` a `windows` listing gave a window and binds it. Pass
the `target_id` it answers as `:target_id` from then on, and the helper answers from
that window's own stream of frames, in that window's own coordinates:

```elixir
{:ok, list}  = Compux.windows(cu)
[window | _] = list["windows"]
{:ok, bound} = Compux.select_target(cu, window["id"])

{:ok, shot} = Compux.screenshot(cu, target_id: bound["target_id"])
:ok = Compux.release_target(cu)
```

What binding one buys, and what it costs:

- **A covered window is still seen.** The frames come from that window, so another
  window in front of it changes nothing about what a `screenshot` or a `check`
  shows.
- **A pixel action on it is refused rather than misdirected.** A click needs the
  window to be topmost at that exact point (`target_obstructed`, which names what is
  in front). Nothing is raised and nothing is activated to make room: the person
  keeps working where they were.
- **A window that moved is never clicked where it was.** The window server's own
  bounds, its display and its measured scale ride on every target observation, and
  a difference at action time is `stale_observation`.
- **An unchanged window is an answer, not a timeout.** A window nobody is touching
  reports idle frames rather than pixels; a check over one comes back stable and
  unchanged, with the picture it was unchanged from.
- **Its controls are its own.** `elements` walks from the window's accessibility
  element where exactly one of the application's matched it (`ax_binding: "bound"`),
  and answers `ax_binding_unavailable` rather than listing the whole application's.
- **The person can see it happening.** While a target is held the helper shows an
  ownership badge, and its Pause and Stop reach the helper's own gate directly —
  they arrive at the owner as `{:compux_session_event, transport, %{"event" =>
  "operator_pause" | "operator_resume" | "operator_stop"}}`. Without that badge
  running, a mutating request naming a window is refused
  `control_surface_unavailable`.

A paused session refuses `select_target` — binding a window opens a capture of it
and puts a badge on somebody's screen, which is what a pause says "not now" to —
and always allows `release_target`, because giving a window back is safe.

Three refusals are worth knowing by name before the first run:
`screen_recording_not_granted` is the grant and only the grant (System Settings,
then restart the helper); `target_minimized` is a window to ask the person to bring
back; `capture_geometry_mismatch` is a surface whose shape is not the window's, and
it carries the numbers rather than mapping a coordinate through them.

There is no "desktop" target: a call with no `:target_id` is the display-level path
this library has always had, unchanged.

## Coordinates name their image

Every reply that hands you coordinates carries an `observation_id`, and every call
that sends coordinates back names the image they were read in. The sidecar keeps
the transform with the image and uses it as stored, so a click is mapped by the
geometry the picture was taken with — never by a rectangle the caller repeated, and
never by one it forgot.

So the actions split in two. `screenshot`, `elements` and `wait_for_change` PRODUCE
coordinates: they take a `:region`, optionally read in an image you name, and each
answers with an id of its own. `left_click`, `right_click`, `double_click`,
`mouse_move`, `left_click_drag`, `scroll` and `inspect` ADDRESS a point: each takes
an `:observation_id` and no `:region`.

The sidecar keeps the last three images for thirty seconds (the numbers are in the
handshake's `capabilities.observations`). Past that, or on an id it never minted,
or after the display has moved or changed mode, the action is refused with
`expired_observation`, `unknown_observation` or `stale_observation` and nothing is
dispatched — take a fresh screenshot and read the coordinates again. A point
outside the image it names is `point_outside_observation`, never clamped onto an
edge.

## Controls have names, not only places

`elements` answers the controls an application publishes, and gives each one an
`element_ref` — `e1`, `e2`, … scoped to that reply's `observation_id`. A reference
is always sent with its observation; alone it means nothing.

```json
{
  "element_ref": "e3", "role": "button", "native_role": "AXButton", "label": "Save",
  "enabled": true, "verbs": ["press", "focus"],
  "bounds": {"x": 480, "y": 312, "w": 84, "h": 24},
  "path": ["Document", "Toolbar"], "x": 186, "y": 121
}
```

`role` is a normalised name every platform shares (`button`, `checkbox`,
`textfield`, `row`, `dialog`, …) and `native_role` the platform's own. `path` is up
to three ancestor labels, nearest last, which is what tells two buttons both
labelled "Save" apart. `value` carries what a field holds, bounded; a secure field
has none and says `"secure": true`. `verbs` lists exactly what this control has a
route for, read from the control itself, and states ride beside it where they read
(`checked`, `expanded`, `selected`, `focused`). Nothing is inferred from a role
name.

A node is listed when its role is one a person operates (a button, a field, a
checkbox, a link, a cell, …) **or** when its own action list advertises a primary
action — `AXPress`, `AXConfirm`, `AXPick`, `AXIncrement`, `AXDecrement` or `AXOpen`.
The second rule picks up the SwiftUI and web groups and rows that act like buttons;
`AXShowMenu`, `AXScrollToVisible`, `AXRaise` and focus alone list nothing, because
every container answers them.

The walk is bounded: 14 levels deep, 250 controls, 3000 nodes visited, 250 children
read from any one parent (128 to a page) and 1.5 seconds of wall clock. A listing
that stopped at one of those says which in `truncated` — `"depth"`, `"nodes"` or
`"time"` — so a partial tree is never passed off as the whole one. `"time"` also
covers a node that did not answer within its own timeout: the walk skips it, still
reads what is inside it, and says the listing is incomplete. A node that is gone
takes its subtree with it and is not a truncation; a parent that will not count its
children (Xcode's windows), or will not hand over a page of them, has its child list
copied once instead.

`ax_messages` says how many accessibility messages the read of the returned list
sent. On macOS each node is read with one batched message for its role, subrole,
title, description, enabled state and frame and one for its action names; only a
node that is listed is asked which of its attributes may be written (one message
per attribute: macOS has no batched form), what its states read, and — unless it is
secure — its value. A verb on a reference asks only the one attribute it writes. A value is asked for only
after the role and subrole say the field is not secure, so a password field's
contents never leave the application. Every node is given a 250 ms messaging
timeout before it is read; an application's windows and the application in front
keep the one-second timeout they had before.

An application whose tree walks to no controls at all is asked to switch its
accessibility on (`AXManualAccessibility`, then `AXEnhancedUserInterface`), and the
switch counts when the attribute reads back on, whatever the setter answered —
Chrome applies `AXEnhancedUserInterface` and still answers
`kAXErrorNotImplemented`. A switch that is already on — a screen reader turned it
on — is left alone and never recorded, so the helper never turns it off under the
client that needs it. The walk is then retried every 300 ms for up to 2.4 seconds,
which outlasts Chromium's two-second debounce, and every switch the helper turned on
is turned off again when it exits. `ax_activation` says what happened, and
`ax_messages` counts only the last of those walks: the one whose list is returned.

On Linux, `elements` reads the same way through AT-SPI, the desktop's accessibility
bus, and the same walk lists what it finds (M86 L1):

* the application read is the one whose window reads ACTIVE on the accessibility
  bus, and the view decides which of its controls are listed;
* `native_role` is AT-SPI's own role, as the name of its `AtspiRole` constant
  (`push button`, `check box`, `password text`, …), and `role` is the normalised
  role `PROTOCOL.md` maps it to; a control is listed for its role or for one of its
  own actions named `click`, `press`, `activate`, `jump`, `check`, `open`,
  `toggle`, `increase` or `decrease`, in any case (a node whose only such action is
  Chromium's `clickAncestor` is its ancestor's text, not a control);
* every property is read with its own `Get`, the optional interfaces only when the
  node lists them, and action names one index at a time (`NActions`, then
  `GetName(i)`). The helper never sends `Action.GetActions` — ATK bridge 2.38
  (Ubuntu 22.04) aborts the application it is sent to — and never
  `Properties.GetAll`, which kills a Qt 6.4 application. A control some of whose
  actions would not name says `"actions_partial": true`;
* a node's reads go out together, and up to eight siblings are read at once;
* bounds are the X11 screen extents of a node that says it is SHOWING, clipped to
  the scroll pane it sits in; a password field (`password text`) is listed and its
  text is never asked for;
* GTK 4 has no screen positions, on X11 too (it answers every node's screen extents
  at 0, 0), and a Wayland session gives no client a position at all: those
  controls are listed with no `bounds` and no click point, and act by reference
  only. A session that places nothing lists without a display;
* a row with no name of its own takes its label's, a combo box its `LABELLED_BY`
  label, a combo box that is not open is not walked into, and the embedded-object
  character (U+FFFC) is dropped from text values;
* `ax_messages` counts the D-Bus calls of the walk.

The verbs act through AT-SPI too, each only where the control's own read gives
it a route: `press` is `DoAction` at the index its press name was read at (never
`doDefault`), `set_value` is `EditableText.SetTextContents` or
`Value.CurrentValue`, `type` by reference deletes the selection and inserts at
the caret, `select` asks the container's `Selection`, `toggle`, `expand` and
`collapse` act only when the state differs, `focus` is `Component.GrabFocus` and
only inside the active window (never on GTK 4, which refuses it), and
`scroll_to` is `Component.ScrollTo` on Chromium and the scroll bar's value on
GTK. A reference is found again before every action, by its place in the tree and
by the object itself: two candidates are `ambiguous_element`, one that stays
unreadable after two more reads is `identity_unknown`. On Qt an object is also
found where it says it is, and only while its label is the one listed.
`inspect` is macOS-only.

Two Qt routes are Qt's own. A Qt menu title (a menu-bar item, or an item with a
submenu) names only `ShowMenu`, and pressing it opens its menu. Qt's item views
and tab list list no `Selection`: a list or tree item that reads `SELECTABLE` and
not `CHECKABLE` is selected by its own `Toggle`, sent only while it reads not
selected, since the action toggles the selection.

What the toolkits do not offer, measured on the fixture apps (`fixtures/linux`,
`spike.sh` stage `verbs`), and so not listed there: a GTK 4 check box names no
action (no `toggle`) and refuses `GrabFocus` (no `focus`); a GTK 4 menu-bar item
names no action, so its menu does not open by reference; a Qt tab is pressed (no
`select`), a Qt tree cell's one action toggles its selection (no `expand`), and
Qt has no `scroll_to`.

Nothing on Linux switches accessibility on for the person. An application in
front whose toolkit publishes no tree until it is asked, and whose tree stays
empty, is refused `toolkit_accessibility_off` with the switch that would make it
publish one (Qt: `QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1`; Chromium:
`--force-renderer-accessibility` and `ACCESSIBILITY_ENABLED=1`). One that is not
on the accessibility bus at all (those, and a GTK 4 one started before the bus)
leaves no window reading active there, and the note says so.

Two actions address a control rather than a point:

* **`press/3`** performs the control's own press. The pointer does not move and it
  cannot miss.
* **`set_value/4`** writes the value and reads it back: the receipt says
  `effect: "verified"` when the read-back matches, `"not_observed"` when it does
  not (a secure field is never read back, so it never verifies). A field that
  shows a write a moment late is read again every 50 ms for up to 400 ms; a pause
  during that wait ends it, and the receipt reports the write `sent` and its
  effect `not_observed`.

Both are offered only where the control advertises support and are refused
`ax_action_unsupported` everywhere else — never silently replaced by a click,
because which of the two to send is the caller's decision. A pointer action may
also take `{:element, ref}` instead of a point, and the helper re-reads the
control's bounds at the moment it acts, so a control that moved is hit where it is
now. Both forms on one request is `addressing_conflict`.

References die with the observation that listed them: the same three replies for
thirty seconds. They also die with the process they came from — a pid is not an
identity, so the process's start time is checked too — and with the helper. A
reference that no longer names what it named is `stale_element`, a control that
will not act is `element_disabled`, and both are refused with nothing dispatched.

Every receipt says which method carried the action: `input_method` is `"ax"` for
`press` and `set_value`, `"foreground_hid"` for everything else. An accessibility
action also reports `foreground_changed`, read before and after, because an action
that promises not to take the focus should have to say when it did.

## Controls before pixels (protocol 12)

`PROTOCOL.md` is the contract; in short:

* **Where a listing looks.** `scope` is `window`, `app`, `focused`, `menu` or
  `dialog`; `query` lists only what matches a role and/or a name, wherever it is;
  `skeleton` stops three levels down and names what each cut hides; `root` drills
  into one control of a listing, keeping the listing's other references, and takes
  a `query` or a `skeleton` inside it.
* **Verbs on a reference.** `focus/3`, `select/3`, `toggle/4`, `expand/3`,
  `collapse/3`, `scroll_to/3`, and `type/3` with an `:element_ref`, each through
  the accessibility API alone. Each reads its state first and acts only when it
  differs (the receipt says `already` when it held), reads it back
  (`effect: "changed"` when it shows what was asked), and is refused
  `ax_action_unsupported` where the control has no route — never clicked
  instead. `focus` never activates a window: `focus_refused` where the control's
  window is not the active one.
* **Waiting on the tree.** `wait_for/3` waits for a condition (`appears`,
  `disappears`, `enabled`, `value`, `window_title`, `menu_open`, `menu_closed`,
  `text`) and never sends input. Any input action can wait in the same request
  with `check: %{"kind" => "semantic", "until" => condition}`; the condition is
  read before the input, so a receipt says `met`, `not_met` or `held_before`. A
  wait never outlives the request's deadline, and says `deadline_cut` when that
  is what ended it.
* **What opened.** Every receipt of input that went out names the dialogs,
  alerts, menus and popups open after it in `surfaces` (`[]` when none is).

The Linux build answers all of these through AT-SPI (M86 L1).

## Every action says what evidence it wants back

An action that dispatches input carries `:check`, and the reply's `receipt` says
which evidence it really came back with.

* **`:image`** — the view you acted in: the crop of the image or listing the action
  named, or the whole display when that is what it named and when it names none at
  all. The helper waits for the view to stop moving before it takes it, and the
  sample that proved it stable is the one encoded, so nothing is captured after the
  evidence. Stable means two consecutive samples of the same rectangle agree AND
  either a change has already been seen or the view has been quiet for 300 ms since
  the input — two equal samples alone would call an application that starts
  repainting late "unchanged". At most 1.5 seconds and 30 samples, with 50 ms as the
  floor between looks rather than a delay added to each one. The check's own image
  is an observation like any other, with rulers drawn and the executed point marked.
* **`:semantic`** — for an action addressed by `element_ref`: the control is read
  again and returned as `element_after` — `{present, role, native_role, label,
  enabled, value}` and its states, with `value` absent for a secure field (which
  says `"secure": true`) and `{"present": false}` alone for a control that no
  longer answers. No capture, and never visual evidence.
* **`:none`** — the receipt alone.

```json
"check": {"kind": "image", "settle": "stable", "changed": false}
```

`settle` is `"stable"` or `"timeout"`: a caret, a spinner or a playing video
reaches the cap every time, which is an observation state and never a reason to
send the input again. `changed` says whether that view differs from the image you
acted on — evidence about the VIEW, not a verdict on the action, and absent when
the action named a listing, which has no picture to compare against.

Whatever the check says, `dispatch` is decided before it runs: evidence that could
not be obtained never changes what was sent. A pause during a settle answers
`cancelled`, with the input reported as `sent` and no check claimed.

`timings_ms` carries `input`, `settle`, `capture` and `encode` — wall time on the
process's one monotonic clock, with every frame grabbed counted as capture, the
settle's own polls included.

## The version handshake

`Compux.start/1` performs a `hello` handshake and refuses a sidecar whose
`protocol_version` differs from `Compux.Protocol.protocol_version/0`, returning
`{:error, {:protocol_mismatch, _}}`. Because the encoder is compiled in but the
binary is installed separately, this is what keeps them from silently drifting.

The handshake's `actions` and `capabilities.input_methods` list only what that
build can do. The macOS build lists every verb and both input methods
(`foreground_hid`, `ax`). The Linux build lists both input methods too, and every
verb but `inspect`, `select_target` and `release_target`, which it does not
answer, so a caller that offers what `hello` lists never offers a verb whose every
use is a refusal.

## Installation

```elixir
def deps do
  [{:compux, "~> 0.1"}]
end
```

`Compux.Binary.path!/0` resolves the sidecar for the host: a checksum-verified
per-target binary downloaded once from the GitHub release and cached, or — with
`COMPUX_BUILD=1` — the local `cargo build --release` output (the dev loop). An
embedder that manages its own signed install passes an explicit `:binary_path` to
`Compux.start/1` instead.

## Supported platforms

macOS-first. **Apple-Silicon macOS** is the primary, fully-featured target
(capture, input, accessibility `inspect` + `elements`, non-prompting permission
probe, idle detection). **Linux** answers `elements`, the verbs on a reference and
`wait_for` through AT-SPI (see "Controls have names") on any session, and its
pixels by one route per session, decided once at start (PROTOCOL.md, "Linux pixel
routes"):

- **X11:** capture, input, `wait_for_change`, `windows` and `paste` (the paste
  chord is Ctrl+V), in pure Rust over x11rb: XTEST with the server's own keymap
  (never remapped), the core `GetImage` in strips, RandR, EWMH, and the
  `CLIPBOARD` selection compux owns for the helper's life. `type` is refused
  before a key is sent for a character the layout cannot type
  (`text_outside_layout`, use `paste`) or while a modifier, Caps Lock or a second
  layout group is on (`keyboard_not_neutral`). A key or button held across an
  action is journalled under `$XDG_RUNTIME_DIR`, and the next helper releases what
  a killed one left held; without a journal it can write there, a drag, a click
  or chord with modifiers, or a paste sends nothing and fails, naming the
  directory.
- **GNOME Wayland:** capture and input through the XDG desktop portal
  (xdg-desktop-portal 1.18 or later, which registers compux as
  `io.tezra.fermix.ComputerUse`): the portal asks the person once, and a restore
  token kept in `$XDG_STATE_HOME/compux/portal.json` lets later sessions start
  without asking. One monitor per session; no `paste` and no `windows`.
- **KDE Plasma, COSMIC, wlroots compositors, and GNOME with an older portal:**
  controls only; every pixel request is refused `pixels_unavailable` with the
  reason.

On both, keys go only into a window that reads `ACTIVE` on the accessibility bus,
and only where exactly one application's does (`no_active_window` otherwise).
`inspect` is macOS-only; the Linux build's `hello` does not list it, and a request
for it is `unknown_action` (see "The version handshake").
The permission `probe` works on both: on Linux it reports which display server
answered its socket (`x11`, `wayland`, `none`) and whether an X endpoint answers
(`x11_endpoint`, XWayland beside Wayland), whether the accessibility bus answers
and how many applications its registry lists (or why it is unavailable), the
at-spi2-core and ATK bridge versions dpkg records, the toolkit notes known
without reading any application, whether XTEST and the keymap answer
(`x11_pixels`), and whether the portal runs, its version, its Registry and any
stored Screenshot answer for compux (`portal`), all read-only, raising no dialog
and starting neither the portal nor its store (PROTOCOL.md, "`probe`"). A helper
started with no display of its own (a daemon a systemd user unit started before
login) reads `DISPLAY`, `WAYLAND_DISPLAY` and `XAUTHORITY`, and nothing else,
from the user manager over the session bus, and carries them as values: it never
writes its own environment, and reaches that display as its own. **Windows** is
not supported.

`idle_ms` / `wait_for_idle` (operational, not model actions) report how long the
human has been idle — a coexistence signal so a policy layer can yield the seat to
a present human. macOS only (typed error elsewhere). They count synthetic input too,
so a caller that also drives input disambiguates its own actions.

Building the Linux target needs no C library but the C runtime: no X, XKB, xdo
or D-Bus development package and no pkg-config
(`fixtures/linux/spike/no_c_deps.sh` proves it in a plain Ubuntu 24.04
container, and checks the helper links nothing else).

macOS requires the user to grant **Screen Recording** (capture) and **Accessibility**
(input) in System Settings → Privacy & Security. Without Accessibility, synthetic
input is silently dropped while screenshots still work — `Compux.probe/1` reports
both grants without prompting so you can tell the user exactly what's missing.

## Capture mode

Besides the request/response actions, the sidecar has an observe rail the embedder
starts with `observe_start` and stops with `observe_stop`: it attaches the macOS
Accessibility observer to the frontmost app **in the caller's allowlist** and streams
window, focus and field events back as unsolicited JSON frames. For an allowlisted
browser it also reports the page the owner is on — the URL reduced to scheme, host and
path (the query string and fragment are dropped in the sidecar, never sent), the page
title, and the window/tab it belongs to.

Typed text is sent only for a window the sidecar can positively tell is **not** a
private-browsing window, and a private window's URL is never sent at all; anything else
arrives as volume-only metadata, so a caller can always tell withheld content from
content that never existed. A private window needs no explanation — it is working as
designed — so only a browser whose private-browsing state the sidecar cannot read at all
also reports a named gap, once per session, naming that browser. Secure text fields are
never read.

## Status

Alpha (`0.x`). The coordinate math is unit-tested (including the Retina
physical-vs-logical regression, at both the backing scale and 1x); the wire
protocol, handshake, and capture paths are verified on-device. The accessibility
seam is a per-node trait with a recording implementation that scripts whole trees,
and the tree walk sits above it (`tree.rs`), so the walk's budgets, which nodes it
lists, its paging and its paths, the checks before an action and the
retain/release balance of every element reference are tested with no OS call — but whether a `press` really presses, and whether it
leaves the person's keyboard alone while it does, needs a real machine with the
grants. `fixtures/macos/FixtureApp.swift` (built by `scripts/build_fixture_app.sh`,
never shipped) is the application those runs are made against, and `LIVE_CHECK.md`
ends with the support matrix they fill in.

## License

MIT.
