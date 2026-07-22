# compux roadmap — 10x + continuous vision

**Status:** draft. **Scope:** where compux goes after **v0.3** (bounded-capture
watchdog, `elements`/`wait_for_change`/`paste`, cursor-in-screenshot,
conditional release signing). See `README.md` for what ships today.

The one-line thesis: **turn-based screenshotting is the floor, not the ceiling.**
A consumer's computer-use turn is a round-trip today — ask for a screenshot,
wait ~0.3s for a frame + the model's own vision inference, act, screenshot again.
Two levers make it 10x: **cut the round-trips** (batching, verify-loops) and
**make each frame worth more** (grounding overlays, text layers). Continuous
video is the north star, but it is gated on model capability, so it is sequenced
last and kept architecturally separate.

Throughout, "the consumer" is the embedding application (Fermix is the reference
consumer); compux stays mechanism-only.

---

## 0. Design invariants (do not regress)

Every item below must hold these — they are the lessons v0.1–v0.3 paid for:

1. **Mechanism only.** compux owns capture/input/coordinate-math/transport; the
   consumer owns policy (access gates, telemetry, tool schema). A new capability
   adds a protocol action + a native handler — never policy in the sidecar.
2. **Bounded everything.** Every native call has a hard deadline (the v0.3 capture
   watchdog); a stall fails typed and the process exits so the parent respawns a
   clean sidecar. No new call may be able to hang the request loop.
3. **Coordinates map through one shared transform.** `crop_rect` is the single
   source of truth for capture↔click; any new spatial feature (marks, OCR boxes)
   reuses `to_sent`/`to_logical`, never a parallel math path (the #1 offset-bug
   class).
4. **The protocol is a version-gated contract.** A wire-shape change bumps
   `protocol_version` and the handshake refuses a drifted sidecar. Additive
   response fields (like `cursor`) do not bump it.
5. **Read-only is the deterministic floor.** New actions declare read-only vs
   mutating in `Protocol.@read_only`; the consumer's access gate derives from it.
6. **On-device or bust.** New capture/grounding compute runs on-device (macOS
   Vision, Accessibility) — never a network round-trip in the capture path.

---

## 1. North star — continuous vision (video), gated on models

**The goal:** when a model can accept a live screen *stream* with tool-use, the
consumer should *watch* the screen continuously rather than grabbing discrete
frames — reacting to state changes (a page finishing loading, a dialog appearing,
a progress bar completing) without a poll.

**Why it's last, not first.** No provider today offers affordable, low-latency,
tool-capable *continuous screen video* input. Gemini takes video files; OpenAI
Realtime takes a webcam-style stream but not as a high-fidelity tool surface;
none price continuous 1080p screen frames near a per-action screenshot. Building
the capture side before a model can consume it is speculative work that rots.
**Gate: a provider ships streaming video input priced/latency'd for interactive
screen control.**

**Architecture when it arrives — it rides an event loop, not request/response.**
Turn-based agent loops (request → tool → response) structurally cannot express
"watch until X"; that is an event-driven shape (e.g. a realtime session). So
continuous vision is a **capture source feeding the consumer's realtime session**,
not a new one-shot action:

- compux gains a **stream mode**: `Compux.stream/2` opens an `SCStream`
  (ScreenCaptureKit, macOS) / PipeWire (Linux) and pushes frames over a second
  Port channel, with **server-side frame gating** — adaptive frame rate (drop to
  1–2fps when the screen is static via the v0.3 thumbnail-hash diff, burst to
  10–15fps on change), region-of-interest cropping, and a hard bandwidth cap.
- The consumer multiplexes screen frames + audio + the tool channel; the model's
  "eyes" become the stream, its "hands" stay the existing bounded input actions
  (click/type/etc.), which do NOT change.
- Cost control is the whole game: raw continuous frames are 100–1000x a
  screenshot's tokens. Frame gating + ROI + the diff-hash keep it to "send a frame
  only when something changed." This reuses v0.3's `region_hash` machinery.

**Do NOT build the stream capture until the model gate opens.** This section's
only deliverable today is the documented seam (`Compux.stream`, the second Port
channel, the consumer-side multiplex) so it's a known extension point.

---

## 2. The 10x levers buildable NOW (model-independent)

Ranked by impact/effort. Each is a protocol action or capture enrichment, and
each has a performance guardrail (§3).

### A. Set-of-Marks overlay — the biggest grounding win  *(v0.4)*
Render numbered marks on interactive elements (from the existing `elements`
accessibility tree) directly onto the screenshot, and return the mark→element
map. The model picks **"click mark 7"** instead of guessing pixels. Set-of-Marks
prompting is the single most-proven lever for GUI grounding accuracy — fewer
misreads → fewer retry round-trips → the 10x compounds.
- new `screenshot` option `marks: true` → run `elements`, draw numbered boxes on
  the sent image (reuse `to_sent` for placement), return `marks: [{id, role,
  label, x, y}]`.
- pair it with a `click_mark`-style convenience so the coordinate is resolved
  from the mark **server-side** — it never round-trips through the model, so a
  mark can't be mis-transcribed.

### B. On-device OCR text layer — target text without reading pixels  *(v0.4)*
Attach a structured text map (strings + bounding boxes) from macOS Vision
(`VNRecognizeTextRequest`, on-device, ~50–100ms) alongside the screenshot. The
model targets **"click the text 'Submit'"** without transcribing tiny text off a
downscaled image (unreliable + token-heavy). Huge for dense UIs and web.
- `screenshot` option `ocr: true` → Vision OCR → `text: [{s, x, y, w, h}]` in sent
  coords. Linux: defer (Tesseract is optional + heavier — gate it off).
- a `click_text`-style convenience resolves server-side (nearest/unique match),
  same "coordinate never round-trips" safety as marks.

### C. Action batching — collapse the round-trips  *(v0.4, the efficiency 10x)*
The dominant computer-use cost is the per-action LLM round-trip, not the capture
(~0.3s). Let the model send a **bounded, verified sequence** in one call — e.g.
`[{click, x, y}, {type, "hello"}, {key, "enter"}]` — with a single post-batch
screenshot. A 5-step form goes from 5 vision turns to 1.
- a `batch` action = an ordered list of existing actions, each still individually
  bounded; stops on the first failure and reports which step failed + a screenshot
  at the stop point.
- read-only/mutating classification is per-constituent-step, so the consumer's
  access gate still applies (a batch with a mutating step is gated like that step).
- net *negative* load — fewer LLM calls, fewer captures.

### D. Diff-crop post-action frames — cheaper verification  *(v0.5)*
After a mutating action, return only the changed region (crop to the diff bbox
from `region_hash`'s comparison) when the change is localized. Fewer image tokens,
faster to reason over. Full frame on request.

### E. Auto-verify + self-correct on click  *(v0.5)*
After a click, check via Accessibility whether the intended element actually
received focus/activation; if not, retry once with the AX-reported element center
(no extra model round-trip). Turns "landed 3px off" misses into silent successes.

### F. Window/app-scoped capture  *(v0.5)*
Capture a specific window or app, not the whole desktop: privacy (don't ship the
whole screen), focus (sharper image of the target), security (bound what's seen).
`screenshot` option `window: <id|title>`.

### G. `wait_for` predicate  *(v0.4, small)*
Beyond `wait_for_change`: block until a specific condition — an element with label
X appears, or text Y is on screen (AX/OCR-based) — so the model states its
precondition once instead of polling. Bounded by the same deadline.

**Deferred / low-value:** perceptual-hash frame dedup (byte-identical is rare on a
live screen; perceptual risks eliding real change), media keys, drag-to-scroll
gestures. Revisit only if a concrete task needs them.

---

## 3. Performance guardrails (why "10x better" ≠ "slower")

Net performance-**positive** or neutral, by construction:

| Feature | Added cost | Guardrail |
|---|---|---|
| Set-of-Marks | one AX walk + a draw pass (~ms) | reuses `elements` (bounded MAX_VISITED); off by default |
| OCR layer | Vision OCR ~50–100ms on-device | opt-in per call; inside the capture deadline; macOS-only |
| Batching | none — **removes** N-1 LLM round-trips | each step individually bounded; stops on first failure |
| Diff-crop | a bbox compute (~ms) | reuses `region_hash`; falls back to full frame |
| Auto-verify | one AX read per click | bounded; single retry cap; skip if AX unavailable |
| Window capture | none (smaller frame = faster) | — |

Hard rules: **nothing runs in the capture hot path unless opt-in**; **every new
native call goes through the v0.3 watchdog** (worker thread + deadline);
**on-device only**. The floor stays the ~0.3s bounded capture we have today.

---

## 4. Consumer integration & release checklist

compux is distributed as per-target release tarballs, verified against the
`checksum-compux.exs` map baked into `Compux.Binary` at compile time (or bypassed
by an embedder that passes an explicit `:binary_path`). Any consumer wiring
enable-by-toggle must respect these:

1. **The release must exist BEFORE the consumer version ships.** The download path
   fetches `compux-<version>-<target>.tar.gz` from the GitHub release and verifies
   it against `checksum-compux.exs`. If the release isn't cut or the checksum map
   is empty, the download fails typed with `no_checksum_for_target` — the consumer
   should surface that as "not published for this version yet", not a crash. Order
   of operations: **cut the release → populate `checksum-compux.exs` (the release
   CI prints the map) → pin the consumer to THAT commit → then ship the consumer.**
   The pinned commit and the published release version are coupled through
   `Compux.Binary.version()` (= the loaded app vsn).
2. **Unsigned is functional but re-grants TCC.** An ad-hoc-signed arm64 binary
   downloaded via `:httpc` runs (no LaunchServices quarantine on a programmatic
   write), but its cdhash changes per release, so macOS Screen Recording +
   Accessibility must be re-granted after each upgrade. Fine for early releases;
   Developer-ID sign + notarize (the 7 workflow secrets → stable cdhash) before
   wide GA. Consumers should warn users about the re-grant in their setup UI.
3. **Enable ≠ ready without a restart.** Readiness is `enabled AND installed`, and
   the capability typically registers at the consumer's boot. The consumer's UI
   must tell the user to restart after enabling, or the capability silently won't
   appear until the next restart.
4. **Upgrade across a version bump.** The download cache is version-scoped
   (`…/compux/<version>/<target>/compux`), so a consumer upgrade pinning a new
   compux version downloads fresh rather than running a stale binary, and the
   protocol handshake refuses a mismatched leftover. Consequence: readiness flips
   false until the re-download, so the consumer should surface an "enabled but not
   installed — reinstall" state (a `doctor`-style probe distinguishing
   `not_installed` from `disabled`) rather than appearing broken.
5. **Cover every error shape with human prose.** `Compux.Binary` returns typed
   errors — `no_checksum_for_target`, `checksum_mismatch`, `http_status`,
   `http_error`, `untar`, `binary_not_in_archive`, `unsupported_target` /
   `unsupported_os` / `unsupported_arch`, plus the runtime `capture_stalled` /
   `display_asleep` / `display_disconnected`. Map each to a readable message.

*Reference consumer:* Fermix installs the sidecar via `Compux.Binary` (a
sha256-verified download from the compux release — no separate catalog, no cosign),
gates the capability on `enabled AND installed`, derives its access posture from
its sandbox mode, and only starts a host session from an attended origin.

---

## 5. Phasing

- **v0.4** — Set-of-Marks (A), OCR text layer (B), action batching (C), `wait_for`
  (G). Grounding + efficiency; `protocol_version` → 3 (batch + new screenshot
  options are wire changes). The 10x release.
- **v0.5** — diff-crop (D), auto-verify (E), window-scoped capture (F). Robustness
  + polish; likely `protocol_version` → 4.
- **vNext (video)** — §1, gated on a streaming-video-capable model. New
  `Compux.stream` + consumer realtime multiplex; the input actions are untouched.

Each version ships through its PR → checksum → consumer-ref-bump loop (§4), with
the adversarial-review + on-device-verification discipline v0.2/v0.3 used.

---

_If this is where compux stays for a while, that's a fine place to stand: v0.3 is
correct, bounded, and safe. §2 is upside, not debt._
