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
it can do), and `press` / `set_value` (act on a control by name, through the
accessibility API).

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
  "element_ref": "e3", "role": "AXButton", "label": "Save",
  "enabled": true, "actions": ["press"], "settable": false,
  "bounds": {"x": 480, "y": 312, "w": 84, "h": 24},
  "path": ["Document", "Toolbar"], "x": 186, "y": 121
}
```

`path` is up to three ancestor labels, nearest last, which is what tells two
buttons both labelled "Save" apart. `value` carries what a field holds, bounded and
absent for a secure field. `actions` lists only what this build can really perform
— `press`, and only where the control's own action list says so — and `settable`
only where the accessibility API says the value may be written. Nothing is
inferred from a role name.

Two actions address a control rather than a point:

* **`press/3`** performs the control's own press. The pointer does not move and it
  cannot miss.
* **`set_value/4`** writes the value and reads it back: the receipt says
  `effect: "verified"` when the read-back matches, `"not_observed"` when it does
  not (a secure field always reads back masked, so it never verifies).

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

## The version handshake

`Compux.start/1` performs a `hello` handshake and refuses a sidecar whose
`protocol_version` differs from `Compux.Protocol.protocol_version/0`, returning
`{:error, {:protocol_mismatch, _}}`. Because the encoder is compiled in but the
binary is installed separately, this is what keeps them from silently drifting.

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
probe, idle detection). **Linux/X11** supports capture + input + `wait_for_change`
+ `paste` (the accessibility actions — `inspect` and `elements` — are macOS-only and
return a typed error on Linux; the paste chord is Ctrl+V). The permission
`probe` works on both (on Linux it reports X11-vs-Wayland capability).
**Wayland**, **Linux accessibility**, and **Windows** are not supported yet. The
accessibility actions `press` and `set_value` are macOS-only for the same reason
`elements` is, and answer a typed error elsewhere rather than an empty success.

`idle_ms` / `wait_for_idle` (operational, not model actions) report how long the
human has been idle — a coexistence signal so a policy layer can yield the seat to
a present human. macOS only (typed error elsewhere). They count synthetic input too,
so a caller that also drives input disambiguates its own actions.

Building the Linux target needs the X11/input system headers the crates link
via pkg-config: `libxcb1-dev libxcb-render0-dev libxcb-shape0-dev
libxcb-xfixes0-dev libxkbcommon-dev libxkbcommon-x11-dev libdbus-1-dev`
(mirrored in `ci.yml`'s `rust-linux` job and the release workflow).

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
seam is a trait with a recording implementation, so the tree walk, the checks
before an action and the retain/release balance of every element reference are
tested with no OS call — but whether a `press` really presses, and whether it
leaves the person's keyboard alone while it does, needs a real machine with the
grants. `fixtures/macos/FixtureApp.swift` (built by `scripts/build_fixture_app.sh`,
never shipped) is the application those runs are made against, and `LIVE_CHECK.md`
ends with the support matrix they fill in.

## License

MIT.
