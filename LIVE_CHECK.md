# Live check — browser capture (owner runs this)

Everything in this branch is proved by unit tests: the private-state classifier per
browser family, the URL reduction to scheme + host + path, "a retitle is not a
navigation", the four browser `field.value` postures (including the unreadable read that
withholds one value without claiming a standing gap), the bounded web-area walk, and the
once-per-app `private_unknown` gap. What no automated test here can prove is what the
**Accessibility API** hands back from a real browser: this session cannot hold the
Accessibility grant, cannot open an incognito window, and no fixture can tell us what
Safari puts in a private window's title. So the live half is below, in the order that
fails fastest.

Expected outcome in one line: `browser.navigated` rows exist for Chrome and Safari with
a query-free `url`, typed text in a normal Chrome window arrives **with** its `host`,
the same text typed in an **incognito** Chrome window arrives withheld with
`private_state = 'private'` and no navigation row at all, and Safari reports
`private_state = 'unknown'` with exactly one `private_unknown` gap naming it.

## 0. Preconditions

* **`dev_local` must be configured.** The sidecar is resolved from
  `[fermix_core.plugins] dev_local` at `<dev_local>/computer_use_sidecar/bin/macos-aarch64/compux`
  (underscores). If `dev_local` is unset, the daemon downloads the pinned release sidecar
  instead and this check silently tests the OLD binary. `COMPUX_BUILD` does not affect it.
* **Safari and Chrome must be in the computer-history app allowlist.** A non-allowlisted
  app is never attached, so it can produce nothing — and consent for browser URLs IS the
  browser's allowlist entry (there is no site allowlist any more). Check `/history
  status`, or `[fermix_core.computer_history]` in `~/.fermix-dev/config.toml`. Keep one
  non-browser app (Notes) in the list too, for step 4.
* `COMPUX_DISCLAIMED=1` tells the sidecar to skip its TCC self-disclaim re-exec, so it
  runs under the **launching process's** Accessibility grant — the terminal you start the
  daemon from. If that terminal has no Accessibility grant, `observe_start` acks
  `ok:false`, capture never starts, and every query below is empty for a reason that has
  nothing to do with this change. Step 3 is what catches that before you browse.
* This is a bare local build (unsigned): macOS may ask for Accessibility again for it.

## 1. Stop the daemon, then install the built sidecar

Stop the dev daemon FIRST — replacing the binary under a running daemon leaves it talking
to the old image, and a copy-in-place can be read half-written.

```sh
SIDE=/Users/sujshe/projects/fermix-plugins/.dev-local/computer_use_sidecar/bin/macos-aarch64
BUILT=/Users/sujshe/projects/compux/native/compux/target/release/compux

# Keep the FIRST known-good binary, and never overwrite that backup on a re-run.
[ -e "$SIDE/compux.bak" ] || cp -p "$SIDE/compux" "$SIDE/compux.bak"

# Atomic replace: copy beside the target, then rename over it.
cp "$BUILT" "$SIDE/compux.new" && mv -f "$SIDE/compux.new" "$SIDE/compux"

# The installed binary must be exactly the one that passed the gates: the two
# lines must print the SAME hash as each other.
shasum -a 256 "$BUILT" "$SIDE/compux"
```

Sanity-check the wire without starting a session (the binary reads stdin, so it must be
given a line — never run it with no input, it will simply wait):

```sh
printf '{"type":"request","request_id":"r1","action":"hello","protocol_version":7,"deadline_ms":10000}\n' \
  | COMPUX_DISCLAIMED=1 "$SIDE/compux"
```

It must report `"protocol_version":7`. **Every line into this sidecar is a tagged frame
now** — a request carries a `type` and a `request_id`, and an untagged protocol-6 line
is refused rather than served. That is why the `printf` above looks nothing like the one
this document carried when it was written against protocol 6.

## 2. Start the dev daemon and watch its console

```sh
cd /Users/sujshe/projects/fermix && COMPUX_DISCLAIMED=1 mix fermix.dev
```

The sidecar's diagnostics go to **stderr**, so they appear in this console (stdout is the
NDJSON frame wire and carries no logs). One line per attach names what happened, and one
new line names a browser whose privacy posture cannot be read:

```
compux: capture: com.google.Chrome: accessibility tree enabled via AXManualAccessibility
compux: capture: com.apple.Safari: AXManualAccessibility unsupported, AXFocusedUIElement Ok — Enabled
compux: capture: com.apple.Safari: private-browsing state unreadable — typed text is withheld for it
compux: capture: <bundle>: title-only coverage — no editable content is observable
compux: capture: <bundle>: ax_refused:AXValueChanged after 3 attempts — no content is observable
```

The Safari line is expected and appears **once per session**, the first time something is
typed in Safari: no window-title marker is pinned for it (§2.2 of the v1.1 design), so its
URLs flow and its typed text does not. A `AXUIElementSetMessagingTimeout refused` line
would be new and worth reporting — the read bound is what keeps a wedged app from
stalling the observer.

## 3. Confirm capture is actually running BEFORE browsing

Ask the assistant `/history status` (the same surface that reports the coverage gaps).

It must say capture is running and list the allowlisted apps. If it says capture is
stopped or refused, fix that first — everything below would be empty and would look like
a browser bug instead of a session that never started.

## 4. The native rail still works (30 seconds, fails fastest)

This change rewired how the focused window is read, so prove the non-browser path before
trusting the browser one. Bring **Notes** to the front, click into a note, type a
sentence, click OUT of the field, wait ~3 s:

```sh
DB=~/.fermix-dev/memory.db
sqlite3 -readonly "$DB" "select type, window_title, char_len, content_withheld, text \
  from computer_history_events where bundle_id='com.apple.Notes' \
  and ts > strftime('%s','now')*1000-600000 order by ts desc limit 20"
```

There must be a `field.value` row carrying your sentence and a `window.focused` /
`window.title_changed` row carrying the note's title. An empty `window_title` on every row
means the focused-window read regressed — stop and report that, the browser steps all
depend on it.

## 5. Browsers

Before reading any of these as a browser bug: a URL is only ever read for a browser whose
**accessibility tree is on**, so the step-2 console line for that browser must say
`accessibility tree enabled via AXManualAccessibility` or end in `— Enabled`. A
`coverage unknown, will re-probe` or `title-only` line means there is no web area to find
and no navigation row can exist; that is the app's tree, not this change.

### 5a. Normal Chrome window — URLs and typed text

In a **normal** (non-incognito) Chrome window visit two pages that are plainly different,
at least one with a query string, e.g. `https://github.com/tezra-io/compux` and then
`https://duckduckgo.com/?q=accessibility+api`. Then click into a text box on a page (a
search field, a comment box) and type a sentence. Switch tabs once. Wait ~3 s.

```sh
DB=~/.fermix-dev/memory.db
sqlite3 -readonly "$DB" "select type, url, host, page_title, window_title, private_state, \
  content_withheld, char_len, gap_reason from computer_history_events \
  where browser_id='com.google.Chrome' and ts > strftime('%s','now')*1000-600000 \
  order by ts"
```

Read it like this:

* A `browser.navigated` row **per page**, with `private_state = 'not_private'`, a
  non-empty `host`, and a `url` that contains **no `?` and no `#`** — the DuckDuckGo row
  must read `https://duckduckgo.com/` with the query gone. A `?` in a stored URL is inv.
  27 broken and the most important thing this step can catch.
* **No second row for the same page.** A spinner retitling a loading page is not a
  navigation; two rows one page title apart, or a row per second while a page loads, is
  the debounce or the change check failing.
* The tab switch produces its own `browser.navigated` row (a different `tab_ref`), which
  is how you confirm the per-window cache does not pin the first tab's URL forever. If the
  tab switch produced **no** row while the window title clearly changed, report that: the
  cached web area is answering for the previous tab.
* The typed sentence arrives as a `field.value` row with `content_withheld = 0`, the text
  present, `private_state = 'not_private'`, and a `host` — **the host of the page it was
  typed on**, carried over from that window's last navigation. A `field.value` with text
  but an empty `host` means the correlation did not bind, which is what §13.2 exists to
  prevent; report it.
* A Chrome window whose **title could not be read** is treated as a window that might be
  incognito with its marker unseen: it sends **no `browser.navigated` row at all**, and a
  value typed in it arrives withheld. So a gap in the navigation rows is not necessarily a
  URL bug — if a page you visited has no row, check whether the neighbouring rows for that
  window have a `window_title` at all.
* **No `private_unknown` gap row for Chrome, ever.** Chrome's marker is pinned, so that
  gap is not a thing its windows can owe. A window whose title the sidecar could not read
  withholds *that one value* (it arrives `private_state = 'unknown'`, `content_withheld =
  1`) and says nothing about the app — a single failed read must never name Chrome
  private-unknown for the rest of the session. If a `private_unknown` row names Chrome,
  that split has regressed; report it.

### 5b. Incognito Chrome window — nothing leaves

Open a **new incognito window** (⇧⌘N), visit one page in it, click into a text field and
type a sentence. Wait ~3 s.

```sh
sqlite3 -readonly "$DB" "select type, url, host, private_state, content_withheld, \
  char_len, window_title from computer_history_events \
  where browser_id='com.google.Chrome' and ts > strftime('%s','now')*1000-300000 \
  order by ts"
```

* There must be **no `browser.navigated` row at all** for the incognito window, and no row
  anywhere carrying that page's URL or host. A private window's URL never crosses the Port
  (inv. 26). This is the single most important assertion in this document.
* The typed sentence must appear as a `field.value` row with `private_state = 'private'`,
  `content_withheld = 1`, a `char_len` matching what you typed, and **no** `text`, **no**
  `host`, **no** `tab_ref`.
* There must be **no** `private_unknown` gap for this: a private window is working as
  designed, not a coverage gap.
* `window_title` on nearby Chrome rows is what the classifier read. If the incognito rows
  are `not_private`, the marker did not appear in the title — copy the `window_title`
  value into the report, it is the evidence the table is wrong.

This step is what pins the whole Chromium family. Chrome, Canary, Beta, Dev and Chromium
share one code base and one `Incognito` window-title string, so verifying stable Chrome
here is what licenses all five entries in `PRIVATE_WINDOW_MARKERS`. Nothing else is
pinned.

### 5c. Safari — URLs yes, typed text no

In Safari visit two pages in a normal window, then type a sentence into a field.

```sh
sqlite3 -readonly "$DB" "select type, url, host, page_title, private_state, \
  content_withheld, char_len, gap_reason, window_title from computer_history_events \
  where bundle_id='com.apple.Safari' and ts > strftime('%s','now')*1000-600000 \
  order by ts"
```

* `browser.navigated` rows with `private_state = 'unknown'`, a real `url` (query-free) and
  `host`. Unknown gates TEXT, not URLs — consent is per browser.
* The typed sentence arrives withheld: `private_state = 'unknown'`, `content_withheld = 1`,
  a `char_len`, no `text`.
* Exactly **one** `observer.gap` row with `gap_reason = 'private_unknown'` for the whole
  session, and it names Safari in `bundle_id`. More than one row per session is the
  once-per-app ledger failing; zero rows means the session never reached a Safari value.
* If Safari produced **no** `browser.navigated` rows while Chrome did, the `AXURL` read is
  the suspect: Safari's web area may answer with a type other than `CFURL` (the read
  refuses anything else rather than reinterpreting it). Report that, with whether
  `page_title` came through on any Safari row.

## 6. Pinning another browser family

Everything except the Chromium family is deliberately **unpinned** — Safari, Brave, Arc,
Opera, Vivaldi, **and Edge and Firefox**, whose candidate markers (`InPrivate`,
`Private Browsing`) are recorded in a comment beside `PRIVATE_WINDOW_MARKERS` but have
never been checked against a live private window of those browsers. An unpinned browser
records where you go and withholds what you type, and says so once per session. Pinning a
marker that turns out to be wrong would leak typed text out of a private window, which is
why none of them is pinned on reputation.

Pinning one is a 5-minute job, per browser:

1. Open a **private** window and a **normal** window of that browser on comparable pages,
   and type a character into a field in each (a value is what makes the classifier read
   the window title).
2. Run the `window_title` query below against that browser's `bundle_id`, find the
   substring the private window's title has and the normal one does not, add
   `("<bundle_id>", "<marker>")` to `PRIVATE_WINDOW_MARKERS` in
   `native/compux/src/capture.rs`, extend
   `private_state_reads_each_pinned_family_from_its_window_title` and drop that bundle id
   from `private_state_is_unknown_without_pinned_evidence`.
3. Rebuild (`cargo build --release`), reinstall per §1, and re-run **§5b** against that
   browser: its private window must produce no navigation row and a withheld value.

Safari is the one worth doing first, and it may not be a title marker at all:

## 6a. Pinning Safari

Safari is deliberately **unpinned**: no window-title marker for it has been checked
against a live private window, and pinning a wrong one would leak typed text out of a
private window, where an unknown one only loses capture. Closing that is a 5-minute job
for the owner, and this is it.

1. Open one **normal** Safari window and one **private** Safari window on comparable
   pages, and type a character into a field in each (a value is what makes the classifier
   read the title).
2. Compare the `window_title` column of the two, which is on every row precisely so this
   comparison needs no extra tooling:

   ```sh
   sqlite3 -readonly "$DB" "select private_state, window_title, type from \
     computer_history_events where bundle_id='com.apple.Safari' \
     and ts > strftime('%s','now')*1000-600000 and window_title is not null \
     order by ts desc limit 30"
   ```

3. If the private window's title carries a distinguishing substring the normal one does
   not (e.g. `Private Browsing`), that is the marker: add
   `("com.apple.Safari", "<marker>")` to `PRIVATE_WINDOW_MARKERS` in
   `native/compux/src/capture.rs`, extend
   `private_state_reads_each_pinned_family_from_its_window_title`, and re-run this
   section — Safari then behaves like Chrome in 5a/5b.
4. If the titles are **identical**, the signal is not in the title. Inspect the private
   window with **Xcode's Accessibility Inspector** (Xcode → Open Developer Tool →
   Accessibility Inspector, target Safari, walk the toolbar) and note the attribute that
   distinguishes a private window — Safari 17+ is reported to expose a `Private Browsing`
   element there. Record the exact attribute and its value in
   `docs/design/MILESTONE_32_COMPUTER_HISTORY_V1_1.md` §2.2 of the fermix repo. Reading a
   toolbar element is a different code path from the title table, so it is its own change;
   until then Safari stays `unknown` and the gap says so, which is the documented posture.

Do the same for any browser you actually use that is not in the table (Brave, Arc, Opera,
Vivaldi, Safari Technology Preview): they are all `unknown` today for the same reason.

## 7. Restore

```sh
# stop the daemon first, then:
SIDE=/Users/sujshe/projects/fermix-plugins/.dev-local/computer_use_sidecar/bin/macos-aarch64
mv -f "$SIDE/compux.bak" "$SIDE/compux"
```

Restart the daemon afterwards. One caveat about the accessibility switch: the sidecar
turns an app's tree back off when it detaches and at process exit, but a **SIGKILLed**
sidecar runs neither path, so a Chromium app it activated keeps `AXManualAccessibility`
set until that app quits. Nothing else needs undoing, and an app whose owner already had
the switch on is never touched.

---

# Live check — held input is released on every path (M42 slice 2, R0)

The unit tests prove the shape of this: for each of `left_click`, `left_click_drag`,
`paste` and `key`, a refusal injected at **every** step of the sequence ends with
nothing recorded as held and the clipboard as the user left it, the release order is
the reverse of the press order, and a panic mid-sequence still releases. What they
cannot prove is the only thing that matters to a person sitting at the machine: that
the release actually reaches the window server, so a failed action does not leave the
Command key down or the left button dragging. This session holds no Accessibility
grant and can post no input at all, so the live half is below, in the order that
fails fastest.

Expected outcome in one line: every action still behaves exactly as it did, and when
one of them fails part way through, the desktop is left the way it was found — no
modifier down, no button held, and the clipboard still holding the owner's own text.

## 0. Preconditions

* **Build the sidecar from this branch**: `cd native/compux && cargo build --release`.
  The binary is `native/compux/target/release/compux` — call it `$CX` below. No
  daemon, no Fermix and no model is needed for any step here.
* **Every step drives the sidecar through the small script below, not through a bare
  `printf`.** Under protocol 7 an action request must carry the boot identity the
  sidecar minted at start-up, and the only way to learn it is to say `hello` first on
  the same process. One `printf` cannot do that, so `cx.py` says hello, then sends
  each action you give it as a tagged frame and prints the reply.
* `COMPUX_DISCLAIMED=1` makes the sidecar skip its TCC self-disclaim re-exec, so it
  runs under the **launching terminal's** Accessibility grant. Without that variable
  it re-execs into its own (unsigned, brand-new) identity, which has no grant — which
  step 3 uses on purpose.
* **Coordinates are sent-screenshot pixels**, not physical ones: the space of a
  capture whose long edge is at most 1366. Pick a point over empty desktop; it does
  not have to be precise for any step here.
* Nothing below writes to `~/.fermix*` or to any database, and nothing needs the
  `dev_local` sidecar to be replaced. Do that only if you also want to check the
  actions through the assistant.

```sh
CX=/Users/sujshe/projects/compux/native/compux/target/release/compux

cat > /tmp/cx.py <<'PYEOF'
import json, os, subprocess, sys

# The environment is passed through UNCHANGED, so `COMPUX_DISCLAIMED=1 python3 …`
# runs the sidecar under this terminal's Accessibility grant and plain `python3 …`
# makes it re-exec into its own ungranted identity. Step 3 needs the second.
side = subprocess.Popen([sys.argv[1]], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                        env=os.environ.copy(), text=True, bufsize=1)

def send(frame):
    side.stdin.write(json.dumps(frame) + "\n")
    side.stdin.flush()

def recv():
    return json.loads(side.stdout.readline())

send({"type": "request", "request_id": "r1", "action": "hello",
      "protocol_version": 7, "deadline_ms": 10000})
hello = recv()
print("hello   ", json.dumps(hello))

envelope = {"sidecar_generation": hello["sidecar_generation"],
            "session_generation": 1, "authorization_generation": 1}

for n, argument in enumerate(sys.argv[2:], start=2):
    frame = {"type": "request", "request_id": "r%d" % n, "deadline_ms": 30000,
             "mutation_seq": n - 1}
    frame.update(envelope)
    frame.update(json.loads(argument))
    send(frame)
    print("action  ", json.dumps(recv()))

side.stdin.close()
PYEOF
```

The first line it prints is always the handshake, and it must say
`"protocol_version": 7`. If it does not, the binary is not the one you just built.

## 1. The success paths still land (30 seconds, fails fastest)

This change rewrote the inside of four input verbs, so prove they still do what they
did before worrying about how they fail. Open TextEdit with an empty document and
bring it to the front.

```sh
# A modified click. The modifiers are now released in the REVERSE of the order they
# were pressed, which is the one ordering difference in the whole change.
COMPUX_DISCLAIMED=1 python3 /tmp/cx.py "$CX" \
  '{"action":"left_click","x":400,"y":300,"modifiers":["cmd","shift"]}'

# A drag. Pick two points over a Finder window with an icon under `from`.
COMPUX_DISCLAIMED=1 python3 /tmp/cx.py "$CX" \
  '{"action":"left_click_drag","from":{"x":400,"y":300},"to":{"x":520,"y":380}}'

# A paste, with the owner's own clipboard put back afterwards.
printf 'the owner clipboard' | pbcopy
COMPUX_DISCLAIMED=1 python3 /tmp/cx.py "$CX" \
  '{"action":"paste","text":"pasted by compux"}'
pbpaste; echo
```

Each action line must answer `"ok": true`, and each carries a `receipt` whose
`dispatch` reads `sent` — that is the sidecar saying the input reached the screen. The click must click where a cmd-shift-click would;
the drag must actually drag the icon (not demote to a click and leave it where it
was — that is the interpolated path and its dwell, unchanged here); `pasted by
compux` must appear in the document and `pbpaste` must print `the owner clipboard`.

Then, with your hand off the keyboard, type a letter into the document. It must type
the letter. A letter that fires a menu shortcut instead means a modifier is still
down after a **successful** action, which would be a new defect in this change, not
the one it fixes — stop and report it.

## 2. The sidecar still starts, and 75 still means one thing

The disclaim re-exec runs on every launch and now reports a failed spawn attribute as
**77** instead of 75, so that a real capture stall is the only thing that ever exits
75. A botched edit there breaks every launch, which step 1 already exercised — but
check the disclaimed path too, since step 1 skipped it:

```sh
printf '{"type":"request","request_id":"r1","action":"hello","protocol_version":7,"deadline_ms":10000}\n' \
  | "$CX"; echo "exit=$?"
```

It must print the hello frame with `"protocol_version":7` and `exit=0`. An exit in
the 70s here is a disclaim failure and the
stderr line above it names which step; report the number. There is no way to provoke
`posix_spawnattr_setflags` failing on a healthy machine, so 77 itself is proved by
the unit gate (`no_disclaim_exit_code_collides_with_the_capture_stall`) and not here.

## 3. A failed paste leaves the clipboard as it was (no patched build)

This is the one failure that can be provoked without touching a line of code, and it
is the most damaging of the three: before this change a paste that could not reach
the keyboard still overwrote the clipboard and never put it back, silently destroying
whatever the owner had copied.

Run the sidecar **without** `COMPUX_DISCLAIMED`, so it re-execs into its own unsigned
identity, which holds no Accessibility grant:

```sh
printf 'the owner clipboard' | pbcopy
python3 /tmp/cx.py "$CX" '{"action":"paste","text":"pasted by compux"}'
pbpaste; echo
```

* The action line must read `"ok": false` with `"error": "init input: ..."` and a
  `receipt` whose `dispatch` is `not_sent` — the keystroke never
  reached the desktop. A stderr line above it reads `compux: Key(Meta) was NOT
  released: init input: ...`: the guard recorded the modifier before posting it (it
  has to — a call can fail after the event is already out) and could not lift it
  through the same dead connection. It is reported there on purpose and kept OUT of
  the reply, so the action's own error names the one thing that actually went wrong.
* `pbpaste` must print **`the owner clipboard`**. This is the assertion. Before this
  change it printed `pasted by compux` and the owner's text was gone for good.
* If the action line reads `"ok": true` instead, this identity happens to already
  hold an Accessibility grant and the step proved nothing. There is a fallback, and
  it takes away a grant you use every day, so do it deliberately:

  1. System Settings → Privacy & Security → Accessibility, switch the entry for your
     **terminal** OFF. Quit and reopen the terminal so the change is observed.
  2. Re-run the two commands above **with** `COMPUX_DISCLAIMED=1` and read the same
     two assertions.
  3. **Switch that entry back ON immediately, before doing anything else**, quit and
     reopen the terminal again, and confirm it is on: the entry must show a filled
     toggle in that same list. A terminal left without its Accessibility grant
     silently breaks every later step of this document and a good deal else besides.

  Step 6 asks you to confirm this a second time at the end, because the moment to
  discover a grant is still off is not three days from now.

## 4. A failed click leaves no modifier down (throwaway build)

A click cannot be made to fail mid-sequence from the outside: everything that can
refuse, refuses before the first modifier goes down. So this one needs a deliberately
broken binary. **The patch below must never be committed** — it is a one-line
throwaway, and step 6 rebuilds over it.

In `native/compux/src/held.rs`, make the button event refuse. Insert the `if` below
as the first statement of `impl Platform for Real`'s `button`, leaving the rest of
the function as it is:

```rust
    fn button(&mut self, button: Button, direction: Direction) -> Result<(), String> {
        if direction == Direction::Click {
            return Err("provoked".to_string()); // TEMPORARY — never commit
        }
```

Only the click verbs go through `Direction::Click`, so a drag is untouched by it.
`cargo build --release`, then, with TextEdit in front and a document focused:

```sh
COMPUX_DISCLAIMED=1 python3 /tmp/cx.py "$CX" \
  '{"action":"left_click","x":400,"y":300,"modifiers":["cmd","shift"]}'
```

* The action line must read `"ok": false` with `"error": "provoked"`.
* **Now type a letter into the document, with your hand off every modifier.** It must
  type the letter. If it fires a shortcut (or selects to the end of the line), Command
  or Shift is still down — that is the defect, and it is what the old code did on
  every failed click. Recover by physically tapping each of Command and Shift once.
* Run it again with `"modifiers":["cmd","shift","alt","ctrl"]` and type again. All
  four must be up.
* The `receipt` on that refusal reads `"dispatch": "partial"`: the modifiers reached
  the screen and the click did not.

## 5. A failed drag leaves no button held (same throwaway build)

Undo the step-4 patch and instead make every drag step refuse — the failure lands
between the press and the release, which is where the left button used to be
stranded. In the same file, replace the BODY of the `#[cfg(target_os = "macos")]`
`drag_step` (the one that calls `crate::pointer::drag_step`), renaming its two
parameters so the build stays quiet:

```rust
    #[cfg(target_os = "macos")]
    fn drag_step(&mut self, _x: i32, _y: i32) -> Result<(), String> {
        Err("provoked".to_string()) // TEMPORARY — never commit
    }
```

`cargo build --release` — it warns that `pointer::drag_step` and its CoreGraphics
externs are now unused, which is the patch doing its job and not a problem. Then,
over a Finder window with an icon under `from`:

```sh
COMPUX_DISCLAIMED=1 python3 /tmp/cx.py "$CX" \
  '{"action":"left_click_drag","from":{"x":400,"y":300},"to":{"x":520,"y":380}}'
```

* The action line must read `"ok": false` with `"error": "provoked"` and a `receipt`
  whose `dispatch` is `partial` — the press landed and the drag did not finish.
* **Then move the pointer around with your hand, touching nothing.** Nothing may be
  dragged and no rubber-band selection may appear. If the desktop is dragging the
  icon or drawing a selection rectangle with you, the left button is still down —
  the old behaviour, and the single worst symptom in this change, because it takes a
  physical click to clear and it drops whatever it is holding wherever you stop.
* Recover, if it does stick, with one physical click.

## 6. Restore and rebuild

```sh
cd /Users/sujshe/projects/compux && git diff --stat native/compux/src/held.rs
```

That must report **no** change once you have undone the step-4 and step-5 patches.
Then `cargo build --release` once more, so no provoked binary is left on disk, and
re-run step 1 to confirm the clean build still works.

Finally, **if step 3 had you switch your terminal's Accessibility grant off, confirm
it is back on**: System Settings → Privacy & Security → Accessibility, the entry for
your terminal, filled toggle. Step 1 passing again is the practical proof — it cannot
click without it.

## 7. What this check still cannot prove

* **A SIGKILLed sidecar releases nothing.** The guard covers a returned error and a
  panic; it cannot run after `kill -9`, and the technical design says as much. If the
  daemon force-kills a sidecar mid-drag, the button stays down. That is a known limit
  of this slice, not a regression — cancellation as a control arrives with the
  protocol work.
* **A release the window server drops.** If a release event is posted and accepted
  but the target application never processes it, compux believes the key is up. The
  sidecar reports what it posted, and what it could not post it names on stderr and
  in the action's own error.
* **The three remaining sequences that hold nothing** (`mouse_move`, `scroll`,
  `type`) were not changed and hold no key or button to leak.

---

# Live check — the protocol-7 wire and Pause (M42 slice 2)

The wire itself is proved here, and more than by unit tests: this session drove the
BUILT sidecar through the REAL `Compux.Transport` end to end — handshake at 7 with a
boot generation, a `pause` acknowledged in 19 ms while a 20-second `wait` was
running and naming it in `in_flight_request_id`, that wait ending `cancelled`, a
click refused `paused` with a `not_sent` receipt, a click at the revoked generation
refused `stale_generation`, `resume`, and a clean stop. So the two halves agree on
the wire, and none of the steps below is about that.

What is left is everything that needs a real desktop and a real daemon: that a
pause stops input the human can SEE stopping, that the button and the modifiers are
up afterwards, and that the computer-history rail still records after the version
moved under it. In the order that fails fastest.

## 0. Preconditions

* Install the built sidecar under `dev_local` exactly as §1 of the browser-capture
  check above describes (stop the daemon first, atomic replace, compare the
  sha256). `COMPUX_BUILD` does not affect it.
* **Fermix must be on the matching branch.** Protocol 7 is an exact-version
  handshake with no legacy mode: a protocol-6 Fermix against this sidecar refuses
  at `hello` and computer history degrades with a protocol mismatch. That is the
  design working, not a fault to report.
* `CX=/Users/sujshe/projects/compux/native/compux/target/release/compux` for the
  two steps that drive the binary directly.

## 1. The handshake, before any daemon (10 seconds)

```sh
printf '{"type":"request","request_id":"r1","action":"hello","protocol_version":7,"deadline_ms":10000}\n' \
  | COMPUX_DISCLAIMED=1 "$CX"
```

One line back, `"type":"response"`, `"request_id":"r1"`, `"protocol_version":7`, a
`"sidecar_generation"` that starts `boot-`, and a `"capabilities"` object listing
`foreground_hid` and the three controls. A different `protocol_version` means the
installed binary is not the one you just built — fix that before anything else.

## 2. The daemon comes up and an action still lands

Start the dev daemon and ask the assistant for something that uses the computer:
a screenshot, then a click on something harmless. Both must work exactly as they
did. A `sidecar_unavailable` or a handshake refusal in the console means the
pinned Fermix and this sidecar disagree on the version — see preconditions.

## 3. `/pause` during a long drag stops it part way and releases the button

This is the step the whole slice exists for, and the one no test here can reach.

Ask the assistant to drag something across a window that takes a moment — a file
from one Finder window to another, a slider, a piece on a board — and while it is
moving, send `/pause`.

* The reply must say it is paused, and **name the action that was still running**
  (that is `in_flight_request_id` reaching the surface).
* The dragged object must stop **part way**, not snap to its destination and not
  return to its origin.
* **Then move the pointer with your hand, touching nothing.** Nothing may be
  dragged and no selection rectangle may appear. If the desktop is still dragging,
  the left button was left down — that is the failure this slice must not have, and
  it is worth reporting with whatever the assistant said.
* Ask for another click while still paused. It must be refused, and the refusal
  must say paused rather than fail as a timeout.
* `/resume`, then click again: it must work. A click that stays refused after a
  resume means the authorization generation did not move.

## 4. `/pause` during a `wait` returns at once

Ask for something that makes the assistant wait (a "wait ten seconds and then look
again" flow), and `/pause` two seconds in. The pause must be acknowledged
immediately — not after the wait finishes — and the waiting action must end
reporting that it was cancelled. A pause that only answers when the wait is over
means the control reader is not on its own thread.

## 5. Computer history still records after the version bump

The capture rail moved to protocol 7 with the rest of the wire, so re-run §4 of the
browser-capture check above (type a sentence into Notes, then read the
`computer_history_events` rows back). There must be a `field.value` row carrying it.

If capture does not start, the console names why. Two refusals mean different
things and only one is a bug:

* `protocol_mismatch ... sidecar: 7` — a protocol-6 Fermix against this sidecar.
  Expected; fix the pairing.
* `observe_start_refused` — the sidecar accepted the frame and declined to start,
  which is the Accessibility grant, not the version.

## 6. What this check still cannot prove

* **A pause cannot retract a call already inside the window server.** The
  acknowledgement names that request rather than claiming it stopped; if the very
  last event of a drag lands after your `/pause`, that is the documented promise,
  not a defect.
* **`type` and `scroll` are not interruptible, but they no longer make the sidecar
  deaf.** Each is a single platform call with no loop of ours inside it, so it is
  checked at the gate before dispatch and not during: a pause sent mid-`type` stops
  the NEXT action, not that one. What it DOES do immediately is answer — the gate's
  mutex is released before the call begins, so the acknowledgement comes back at once
  and names the typing in `in_flight_request_id`. Worth confirming by hand: ask the
  assistant to type a long paragraph somewhere harmless and send `/pause` while it is
  going. The reply must come back straight away and name the action; the typing then
  finishes, and the next action is refused. A `/pause` that only answers once the
  typing has ended is the failure — it is what gets this sidecar killed mid-type.
* **A SIGKILLed sidecar still releases nothing** — unchanged from the held-input
  check above.
