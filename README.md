# compux

Native **computer use** (screen capture + mouse/keyboard input) for Elixir, backed
by a crash-isolated Rust sidecar spawned over a Port — **not** a NIF.

```elixir
{:ok, cu}   = Compux.start()
{:ok, shot} = Compux.screenshot(cu, region: {0, 0, 800, 600})
:ok         = Compux.click(cu, {120, 80}, button: :left, modifiers: [:cmd])
{:ok, el}   = Compux.inspect(cu, {120, 80})   # accessibility element under a point (macOS)
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
  (`Compux.Protocol`), the Port plumbing (`Compux.PortDriver`), the coordinate math
  (Retina physical-vs-logical, region zoom), and the ergonomic API (`Compux`).
- **The caller** owns the *policy* — when an action is allowed, confirmation, and
  telemetry. `compux` makes no such decisions; it returns `{:ok, _} | {:error, _}`.

## Actions

`screenshot`, `left_click` / `right_click` / `double_click`, `mouse_move`,
`left_click_drag`, `scroll`, `type`, `paste` (clipboard-based — fast and
unicode-safe for long text), `key` (chords like `"cmd+shift+4"`, incl. `f1`–`f12`),
`wait`, `wait_for_change` (block until the screen changes, then return the new
frame), `inspect` (the accessibility element under a point), and `elements` (the
interactive accessibility elements with a click point each — target by element, not
raw pixels). Every coordinate is in the screenshot's pixel space; a `:region` zooms
capture *and* the click mapping through one shared crop, so clicks can't land offset.

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
**Wayland**, **Linux accessibility**, and **Windows** are not supported yet.

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

## Status

Alpha (`0.x`). The coordinate math is unit-tested (including the Retina
physical-vs-logical regression); the wire protocol, handshake, and capture paths are
verified on-device. Input-injection landing and `inspect` roles need per-machine
verification with the grants in place.

Where it's headed — set-of-marks + OCR grounding, action batching, and eventual
continuous-video capture — is in [ROADMAP.md](ROADMAP.md).

## License

MIT.
