# Live check — capture coverage fixes (owner runs this)

Everything in this branch is proved by unit tests, including the real detach/gap emit
paths and a real-CFRunLoop timer test. What no automated test here can prove is what the
**Accessibility API** does to live apps: this session cannot hold the Accessibility grant,
cannot type into Notes or VS Code, and a stub app cannot refuse `AXManualAccessibility`
the way Electron does. So the live half is below, in the order that fails fastest.

Expected outcome in one line: `field.value` rows exist for a native app **and** for an
Electron app, `window.title_changed` is a handful of rows instead of thousands, and any
app whose tree genuinely could not be switched on has said so once, with its bundle id on
the frame.

## 0. Preconditions

* **`dev_local` must be configured.** The sidecar is resolved from
  `[fermix_core.plugins] dev_local` at `<dev_local>/computer_use_sidecar/bin/macos-aarch64/compux`
  (underscores). If `dev_local` is unset, the daemon downloads the pinned release sidecar
  instead and this check silently tests the OLD binary. `COMPUX_BUILD` does not affect it.
* The app you type into must be in the computer-history **app allowlist** — a
  non-allowlisted app is never attached, so it can produce nothing (check `/history
  status`, or `[fermix_core.computer_history]` in `~/.fermix-dev/config.toml`). Add Notes
  if it is not there; keep VS Code.
* `COMPUX_DISCLAIMED=1` tells the sidecar to skip its TCC self-disclaim re-exec, so it
  runs under the **launching process's** Accessibility grant — the terminal you start the
  daemon from. If that terminal has no Accessibility grant, `observe_start` acks
  `ok:false`, capture never starts, and every query below is empty for a reason that has
  nothing to do with this change. Step 3 is what catches that before you type.
* This is a bare local build (unsigned): macOS may ask for Accessibility again for it.

## 1. Stop the daemon, then install the built sidecar

Stop the dev daemon FIRST — replacing the binary under a running daemon leaves it talking
to the old image, and a copy-in-place can be read half-written.

```sh
SIDE=/Users/sujshe/projects/fermix-plugins/.dev-local/computer_use_sidecar/bin/macos-aarch64
BUILT=/private/tmp/claude-501/-Users-sujshe-projects-fermix/666930c4-2ed9-4663-93f8-142cee19f495/scratchpad/compux-capture/native/compux/target/release/compux

# Keep the FIRST known-good binary, and never overwrite that backup on a re-run.
[ -e "$SIDE/compux.bak" ] || cp -p "$SIDE/compux" "$SIDE/compux.bak"

# Atomic replace: copy beside the target, then rename over it.
cp "$BUILT" "$SIDE/compux.new" && mv -f "$SIDE/compux.new" "$SIDE/compux"

# The installed binary must be exactly the one that passed the gates.
shasum -a 256 "$BUILT" "$SIDE/compux"
# both lines must read:
# 02deb60fa95d8ef0c507d2bd17dafbaa44e059e4ab5b46072c69f99b637d7255
```

Sanity-check the wire without starting a session (the binary reads stdin, so it must be
given a line — never run it with no input, it will simply wait):

```sh
printf '{"action":"hello"}\n' | COMPUX_DISCLAIMED=1 "$SIDE/compux"
```

It must report `"protocol_version":6` — this change adds no action and no frame shape, so
the pairing with the pinned Fermix side is unchanged.

## 2. Start the dev daemon and watch its console

```sh
cd /Users/sujshe/projects/fermix && COMPUX_DISCLAIMED=1 mix fermix.dev
```

The sidecar's diagnostics go to **stderr**, so they appear in this console (stdout is the
NDJSON frame wire and carries no logs). One line per attach names what happened:

```
compux: capture: com.microsoft.VSCode: accessibility tree enabled via AXManualAccessibility
compux: capture: com.apple.Notes: AXManualAccessibility unsupported, AXFocusedUIElement Ok — Enabled
compux: capture: <bundle>: AXManualAccessibility unanswered — coverage unknown, will re-probe
compux: capture: <bundle>: title-only coverage — no editable content is observable
compux: capture: <bundle>: ax_refused:AXValueChanged after 3 attempts — no content is observable
```

The first two are the healthy cases (Electron switched on; a native app that needs no
switch). The third is an app that did not answer — it is re-probed when its window
changes, up to three times, and publishes nothing meanwhile. The last two are the named
degradations, which is the answer the 48h session could not give at all.

## 3. Confirm capture is actually running BEFORE typing

Ask the assistant `/history status` (the same surface that will later have to report the
coverage gaps).

It must say capture is running (and list the allowlisted apps). If it says capture is
stopped or refused, fix that first — everything below would be empty and would look like
a coverage bug instead of a session that never started.

## 4. Type, then read the store

Bring **Notes** to the front, click into a note, type a sentence, then click OUT of the
field (the blur is now a flush point — a quick edit followed by leaving the field must
still be recorded). Then **VS Code**, click into an editor, type a sentence. Switch
between the two once. Wait ~3 s (0.6 s value debounce, 1.0 s title debounce), then:

```sh
DB=~/.fermix-dev/memory.db
# a) kinds seen in the last 10 minutes — field.value MUST be > 0
sqlite3 -readonly "$DB" "select * from computer_history_events \
  where ts > strftime('%s','now')*1000-600000 order by ts desc limit 50"

# b) the coverage gaps, which now carry the app they are about
sqlite3 -readonly "$DB" "select * from computer_history_events where type='observer.gap' \
  and ts > strftime('%s','now')*1000-600000 order by ts desc limit 20" \
  | grep -E 'title_only|ax_refused|secure_input' || echo "no coverage gaps in the window"

# c) the title flood
sqlite3 -readonly "$DB" "select * from computer_history_events \
  where type='window.title_changed' and ts > strftime('%s','now')*1000-600000 order by ts" \
  | tail -40
```

Read it like this:

* **(a)** `field.value` present for both apps → the tree is on and the focused element's
  value reaches us. Present for Notes but not VS Code → the Electron tree did not come on;
  (b) must then show a `title_only` or `ax_refused:…` row naming that bundle id, and the
  console line from step 2 says which AXError refused it. Absent for both, with no gap and
  no `unknown`/`re-probe` line → stop and report: the notifications registered and
  nothing is being delivered, which is neither of the two diagnosed causes.
* **(b)** A `title_only` / `ax_refused:…` row appears **once per app** (once per refused
  set), not per app switch, and carries the app's `bundle_id` in the same column every
  other event uses. `ax_refused:AXValueChanged` vs
  `ax_refused:AXValueChanged,AXFocusedUIElementChanged` tells you how wide the refusal
  was. A `grant_revoked` / `secure_input` row is app-less by design.
* **(c)** One row per settled burst. Run a long build in VS Code so its title spins: while
  it spins you should see at most one row every 5 s (the debounce ceiling — a burst that
  never goes quiet still has to report *something*, and nothing at all would be a
  different bug), and never two consecutive rows with the same title. The incident's shape
  — thousands of rows one braille glyph apart — is what this fails on. The last title
  before each app switch must still be present (detach flushes it).

## 5. Restore

```sh
# stop the daemon first, then:
SIDE=/Users/sujshe/projects/fermix-plugins/.dev-local/computer_use_sidecar/bin/macos-aarch64
mv -f "$SIDE/compux.bak" "$SIDE/compux"
```

Restart the daemon afterwards. One caveat about the accessibility switch: the sidecar
turns an app's tree back off when it detaches and at process exit, but a **SIGKILLed**
sidecar runs neither path, so an Electron app it activated keeps `AXManualAccessibility`
set until that app quits. Nothing else needs undoing, and an app whose owner already had
the switch on is never touched.
