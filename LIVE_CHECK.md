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
printf '{"type":"request","request_id":"r1","action":"hello","protocol_version":11,"deadline_ms":10000}\n' \
  | COMPUX_DISCLAIMED=1 "$SIDE/compux"
```

It must report `"protocol_version":11`. **Every line into this sidecar is a tagged frame
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
  `printf`.** An action request must carry the boot identity the sidecar minted at
  start-up, and the only way to learn it is to say `hello` first on the same process.
  Since protocol 8 an action that addresses a point must also name the IMAGE that
  point was read in, and since protocol 9 an action addressed at a CONTROL must name
  the `elements` reply that listed it. Only a screenshot or an `elements` can mint
  one. A single `printf` can do none of that, so `cx.py` says hello, takes whichever
  of the two an action needs before sending it, then sends each action you give it
  as a tagged frame and prints the reply.
* **One sidecar, many actions.** The observation table is PER PROCESS, so an id
  from an earlier run of the script can never resolve in a later one — it answers
  `unknown_observation`, which is a different fact from the `stale_element` some
  steps below are checking for. So `cx.py` also reads actions from stdin, one JSON
  object per line, and keeps the same sidecar for all of them. Any step that spans
  more than one action (listing then pressing, relaunching an app in between) uses
  that form, and every such step below says so.
* `COMPUX_DISCLAIMED=1` makes the sidecar skip its TCC self-disclaim re-exec, so it
  runs under the **launching terminal's** Accessibility grant. Without that variable
  it re-execs into its own (unsigned, brand-new) identity, which has no grant — which
  step 3 uses on purpose. The terminal needs **Screen Recording** too now, because
  every step that clicks takes a screenshot first to have an image to aim at.
* **Coordinates are pixels in the image the action names** — the screenshot the
  script takes for it, whose long edge is at most 1366, not physical pixels. Pick a
  point over empty desktop; it does not have to be precise for any step here.
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

seq = [1]

def send(frame):
    side.stdin.write(json.dumps(frame) + "\n")
    side.stdin.flush()

def recv():
    reply = json.loads(side.stdout.readline())
    if "data" in reply:                      # an image, abbreviated so this is readable
        reply["data"] = "<%d base64 bytes>" % len(reply["data"])
    return reply

send({"type": "request", "request_id": "r1", "action": "hello",
      "protocol_version": 11, "deadline_ms": 10000})
hello = recv()
print("hello   ", json.dumps(hello))

envelope = {"sidecar_generation": hello["sidecar_generation"],
            "session_generation": 1, "authorization_generation": 1}

def act(body):
    seq[0] += 1
    frame = {"type": "request", "request_id": "r%d" % seq[0], "deadline_ms": 30000,
             "mutation_seq": seq[0] - 1}
    frame.update(envelope)
    frame.update(body)
    send(frame)
    return recv()

# A coordinate is pixels in the image you name and a reference is a control in the
# list you name, so every addressed action needs one of the two — and only a
# screenshot or an `elements` mints one. This takes a fresh one before each such
# action rather than reusing it, so no step can fail on an observation that expired
# while you were reading.
ADDRESSED = {"left_click", "right_click", "double_click", "mouse_move",
             "left_click_drag", "scroll", "inspect", "press", "set_value"}

def run(body):
    if body.get("action") in ADDRESSED and "observation_id" not in body:
        # A control is named in an `elements` reply; a point, in a screenshot. Both
        # are taken IN THE SAME WINDOW the action names, when it names one: an image
        # of the display would put the action's coordinates in a space the bound
        # window's transform knows nothing about.
        into = {"target_id": body["target_id"]} if "target_id" in body else {}
        if "element_ref" in body:
            listed = act({"action": "elements", **into})
            print("elements", json.dumps(listed))
            body["observation_id"] = listed.get("observation_id")
        else:
            shot = act({"action": "screenshot", **into})
            print("image   ", json.dumps(shot))
            body["observation_id"] = shot.get("observation_id")
    print("action  ", json.dumps(act(body)))

for argument in sys.argv[2:]:
    run(json.loads(argument))

# Then keep reading from stdin, so a sequence of actions shares ONE sidecar — and
# therefore one observation table. Type a JSON object per line and press return;
# ctrl-D ends the session. Run it with </dev/null to skip this entirely.
if sys.stdin.isatty():
    print("ready  (one JSON action per line, ctrl-D to finish)")
for line in sys.stdin:
    line = line.strip()
    if line:
        run(json.loads(line))

side.stdin.close()
PYEOF
```

`select_target` and `release_target` are ordinary actions to this script: send one
and every later line may carry `"target_id": "t1"`, and the script then takes its
screenshots and listings in that window rather than on the display.

The first line it prints is always the handshake, and it must say
`"protocol_version": 11`, with a `capabilities.observations` of
`{"max": 3, "ttl_ms": 30000}`. If it does not, the binary is not the one you just
built. Every `image` or `elements` line names the reply the action after it is
aimed at. If that line reads `"ok": false`, the sidecar could not capture — the
launching terminal needs **Screen Recording** as well as Accessibility — and the
action after it is refused `observation_required` for that reason and no other.

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

The click and the drag each print an `image` line first — the screenshot their
coordinates are read in, which the script takes for them — and the paste none,
because it addresses no point. Each action line must answer `"ok": true`, and each
carries a `receipt` whose `dispatch` reads `sent` — that is the sidecar saying the
input reached the screen. The click must click where a cmd-shift-click would;
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
printf '{"type":"request","request_id":"r1","action":"hello","protocol_version":11,"deadline_ms":10000}\n' \
  | "$CX"; echo "exit=$?"
```

It must print the hello frame with `"protocol_version":11` and `exit=0`. An exit in
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
BUILT sidecar through the REAL `Compux.Transport` end to end — the handshake with a
boot generation (at 7 then; slice 3 has since moved it to 8, and the commands below
say 8 because that is what this tree speaks), a `pause` acknowledged in 19 ms while a 20-second `wait` was
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
* **Fermix must be on the matching branch.** The handshake is exact-version with
  no legacy mode: a Fermix on any other protocol refuses at `hello` and computer
  history degrades with a protocol mismatch. That is the
  design working, not a fault to report.
* `CX=/Users/sujshe/projects/compux/native/compux/target/release/compux` for the
  two steps that drive the binary directly.

## 1. The handshake, before any daemon (10 seconds)

```sh
printf '{"type":"request","request_id":"r1","action":"hello","protocol_version":11,"deadline_ms":10000}\n' \
  | COMPUX_DISCLAIMED=1 "$CX"
```

One line back, `"type":"response"`, `"request_id":"r1"`, `"protocol_version":11`, a
`"sidecar_generation"` that starts `boot-`, a `"capabilities"` object listing
`["foreground_hid","ax"]`, the three controls and
`"observations":{"max":3,"ttl_ms":30000}`. A different `protocol_version` means the
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

The capture rail moves with the rest of the wire, so re-run §4 of the
browser-capture check above (type a sentence into Notes, then read the
`computer_history_events` rows back). There must be a `field.value` row carrying it.

If capture does not start, the console names why. Two refusals mean different
things and only one is a bug:

* `protocol_mismatch ... sidecar: 8` — a Fermix on an older protocol against this
  sidecar. Expected; fix the pairing.
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

---

# Live check — observation identity and geometry (M42 slice 3)

This slice fixed a coordinate defect nobody here can see. On a display at 2x, the
old constructor read the display's width in POINTS into its PHYSICAL width and then
halved the already-logical bounds and origin: the model saw and could reach only the
top-left quarter of the screen, and every click on a second display landed on the
display next door. At 1x — the only panel this machine has — every one of those is
the identity, which is why it has never shown.

So the geometry is now built from a ratio MEASURED on the frame that was really
captured, not from the display mode, and both answers a capture API can give (the
backing scale, or 1x) map correctly. The unit tests prove both worlds and both
origins. **What only a real Retina panel and a real second display can prove is
which world this machine is in, and that a click lands where the model looked.**

The other half is addressing: every reply that hands out coordinates names its
image, and every action that sends coordinates back names one. That part was proved
in this session against the built sidecar — `observation_required` on a click with
no id, `unknown_observation` on a made-up one, `unknown_field` on a click carrying a
`region`, each with a `not_sent` receipt — so the steps below are only what needs a
screen.

Expected outcome in one line: on a Retina display, in EVERY scaled mode it offers,
the model sees the WHOLE screen and a click lands where it looked, full screen and
zoomed; the same beside a display of a different backing scale, including one placed
left of or above the main one; changing the display between a screenshot and a click
is refused rather than relocated; and an image older than thirty seconds is refused
while a fresh one works.

The refusal to watch for is `capture_geometry_mismatch`. It is deliberately
fail-closed — a wrong click is worse than a refusal — and its `detail` line carries
every number that went into the decision, so it is a complete bug report on its own.
Steps 2 and 3 are the two configurations most likely to produce one.

## 0. Preconditions

* The sidecar built from this branch, installed under `dev_local` exactly as §1 of
  the browser-capture check describes (stop the daemon first, atomic replace,
  compare the sha256), and Fermix on the matching branch — the handshake is
  exact-version.
* **A Retina display.** Any MacBook panel will do; the owner's 3840x1080 desktop
  panel is 1x, where the whole defect is the identity, so §1 and §2 prove nothing on
  it. Run those on the laptop's own screen. §4 to §7 are true on any display.
* For §3, both displays at once, and at least one arrangement with the second one
  placed to the LEFT of or ABOVE the main one, which is what gives it a negative
  origin.
* `CX=/Users/sujshe/projects/compux/native/compux/target/release/compux` and the
  `/tmp/cx.py` from the held-input check above, which now takes a screenshot before
  any action that addresses a point.

## 1. The whole screen, and a click that lands (the defect, fails fastest)

Ask the assistant for a screenshot of the Retina display and look at it.

* **It must show the whole screen.** The old code sent the top-left quarter and said
  nothing was wrong, because the click map agreed with it: the model simply lived in
  a quarter of the desktop. A screenshot that shows a quarter here means this fix
  did not take.
* No `capture_geometry_mismatch` anywhere in the reply or the console — see §2 for
  what to do if there is one.
* Then ask it to click something small and unambiguous near a CORNER of that screen
  (a menu-bar item at the top right, the Dock's last icon). It must land on that
  thing. A click that lands at half its coordinates, or a quarter of the way in, is
  the defect in the other direction and is worth reporting with the screenshot.
* Ask it to zoom into a region and click something inside the zoomed image. That
  must land too: the crop's transform is the one stored with the crop.

## 2. Every scaled mode of that display

This is the configuration most likely to trip the new rule, and the one no test here
can reach. A Retina panel is normally run SCALED — System Settings → Displays offers
"More Space" and "Larger Text" beside the default — and each of those is a different
number of points over the same physical panel. The helper measures the ratio from
the frame it actually captured, so each mode should simply measure differently.

For **each** setting Displays offers on that panel, including the default and both
extremes, take a screenshot and write down three numbers from the reply: the image's
`width`x`height`, the `physical` width and height, and `scale`. Then click something
near a corner.

* Every mode must show the whole screen and land its click.
* **If `capture_geometry_mismatch` fires, that IS the bug report.** Its `detail` line
  carries the frame's size, the display's size in points, the mode's backing scale
  and both measured ratios — copy it verbatim along with the mode you had selected.
  Computer use refusing there is the design working, not a crash: the helper found a
  frame it cannot explain and would rather refuse than click somewhere nobody chose.
  Everything else in Fermix keeps working; only computer use on that display stops.
* A mode that answers `scale` 1 on a Retina panel is not a fault either — it means
  the capture came back at 1x, which this code handles. Write it down; it is the one
  fact nobody has yet observed.

## 3. Two displays with DIFFERENT backing scales

The pairing that breaks assumptions: a 1x external monitor beside a Retina panel.
Each display is measured on its own, so neither's ratio may leak into the other.

Do it in both arrangements, because which display is "main" changes the origins:

1. Retina panel main, 1x external to the RIGHT.
2. 1x external main, Retina panel to the right — and then, once, with the second
   display placed to the LEFT of or ABOVE the main one, so its origin goes negative.

In each arrangement, for **each** display: a full screenshot (whole screen, no
mismatch) and a click near a corner. The click must land on the display it was read
from. This is the case the old code got most wrong: the origin was halved, so a
click meant for the second display landed somewhere on the first.

If `capture_geometry_mismatch` fires on one display and not the other, say which —
the `detail` names the display's size in points, which identifies it.

## 4. The image is named, and the name is what a click carries

Drive the sidecar directly, with TextEdit or Finder in front:

```sh
COMPUX_DISCLAIMED=1 python3 /tmp/cx.py "$CX" '{"action":"left_click","x":400,"y":300}'
```

* The `image` line carries `"observation_id"` (four characters, a dash, a counter,
  e.g. `7c1e-1`), `"observation_kind": "image"`, a `frame_seq` and a
  `captured_at_monotonic_ns`.
* The `action` line is `"ok": true`, and its `receipt` names
  `"observation_id_before"` — the image the click was aimed at.
* A point past the edge of that image is refused rather than clamped onto it:

```sh
COMPUX_DISCLAIMED=1 python3 /tmp/cx.py "$CX" '{"action":"mouse_move","x":99999,"y":10}'
```

  must answer `"error": "point_outside_observation"` with the image's size in the
  detail, and the pointer must not move at all.

## 5. The view does not walk

Ask the assistant to do several mutating actions in a row in one app — click, type,
click, scroll, click — without asking for a fresh screenshot in between. Fermix
re-derives its view from its own last image after each one.

The check images must stay on the same view: no drift left or right, no edge
creeping in, no strip of the screen sliding out. A view that walks a pixel or two
per action is the defect the fixed-point test in `geometry.rs` now pins, and seeing
it here means the test is measuring something narrower than reality.

## 6. Changing the display between a screenshot and a click

Ask for a screenshot. Before the next action, change the display arrangement — drag
one display in System Settings, or change its resolution or its scaled mode. Then
ask for a click on something in that screenshot.

* It must be refused with `stale_observation` and `geometry_changed`, and nothing
  may be clicked. A click that happens anyway has been computed from a geometry that
  no longer exists, which is the failure this refusal is for.
* A fresh screenshot and the same click must then work — and on a display whose
  MODE you just changed, that fresh screenshot is also what re-measures it. Ask for
  `windows` or an element list right after, and its coordinates must agree with the
  new screenshot rather than the old mode: a click on a listed window must land on
  that window.

## 7. Thirty seconds

Ask for a screenshot, then wait more than thirty seconds doing nothing, then ask for
a click on something in it.

* `expired_observation`, nothing clicked, and the sentence tells the model to look
  again. Then a fresh screenshot and the same click must work.
* Worth noting how often this costs a turn in ordinary use. The 30 s bound was
  chosen against 227 recorded actions (p95 20 s, about 1.3% over 30 s); if it bites
  more often than that in practice, the number is the thing to change.

## 8. What this check still cannot prove

* **Which world this machine is in.** The measurement is read from the frame, so the
  code does not care whether the capture answers at the backing scale or at 1x — but
  nobody has yet seen which one macOS gives here. If §1 and §2 pass, both are
  handled; the `scale` field in the screenshot reply says which one it was, and that
  is the number to write down.
* **That a pixel is exactly the right pixel.** `to_logical` rounds to a whole logical
  point because that is all enigo takes, so on a crop magnified past the point grid
  the finest aim is one point, not one image pixel. A click landing a pixel or two
  off the centre of a small target is that bound, not a defect.
* **Three displays, or a display hot-plugged mid-action.** The staleness check reads
  what the OS says before every addressed action, so it should refuse rather than
  misfire, but no test here has a third panel to prove it.

---

# Live check — semantic references and accessibility actions (M42 slice 4)

This slice gives every control a NAME. `elements` now answers what each control
is, what it holds, whether it is enabled, what it can do, whether its value can be
set, where it is, a short path of ancestor labels, and an `element_ref` the next
action can use. Two actions address a control instead of a point — `press` and
`set_value` — and a click may be addressed by reference too, in which case the
helper reads the control's bounds again at the moment it acts.

None of that moves the pointer, and that is the claim only a real machine can
test. Everything about REFUSING is already proved against the built helper in this
session (`observation_required`, `element_required`, `unknown_observation`,
`addressing_conflict`, `unknown_field` on `target_id`), and the revalidation, the
retain/release balance and the receipts are unit-tested against a scripted
application. **What needs a screen is whether a press actually presses, and
whether it leaves the person's keyboard alone while it does.**

Expected outcome in one line: with the fixture app in front and the person typing
continuously into TextEdit, `press` changes the fixture's state file, moves no
pointer, drops and inserts no character in TextEdit, and reports
`foreground_changed: false`; `set_value` verifies on a text field and does not on
a secure one; a disabled button is refused; relaunching the fixture makes every
old reference `stale_element`; and the support matrix at the end has a row per
control family the owner actually ran.

## 0. Preconditions

* The sidecar built from this branch, installed under `dev_local` exactly as §1 of
  the browser-capture check describes (stop the daemon first, atomic replace,
  compare the sha256), and Fermix on the matching branch — the handshake is
  exact-version.
* `CX=/Users/sujshe/projects/compux/native/compux/target/release/compux` and the
  `/tmp/cx.py` above, which now takes an `elements` reply before any action that
  names a control.
* **The fixture application**, which is what gives these steps an answer that does
  not come from a picture:

```sh
cd /Users/sujshe/projects/compux
./scripts/build_fixture_app.sh /tmp/FixtureApp

# The recording path, with no window and no grant: this must exit 0 before you
# trust anything the file says later.
/tmp/FixtureApp --self-test --state-file /tmp/fixture.json ; echo "exit=$?"

# Then the real thing. Leave it running, in front, for §1 to §5.
rm -f /tmp/fixture.json
/tmp/FixtureApp --state-file /tmp/fixture.json &
```

  `--self-test` drives the handlers in process. It shows no window and posts no
  input, so it proves the recording path without a window server or an
  Accessibility grant; the steps below are the half that needs both.
* **TextEdit open with an empty document, and your hands on the keyboard.** Steps
  1 and 2 ask you to keep typing while the helper acts. That is the test: an
  accessibility action must not take the focus, and the only way to see that it
  did is to lose a character.
* `watch -n1 cat /tmp/fixture.json` in a second terminal, or just `cat` it after
  each step. Every step below says which event it should add.

## 1. The list names things (fails fastest)

With the fixture app frontmost:

```sh
COMPUX_DISCLAIMED=1 python3 /tmp/cx.py "$CX" '{"action":"elements"}'
```

* Every element carries `element_ref` (`e1`, `e2`, …), `role`, `label`, `enabled`,
  `actions`, `settable`, `bounds` and the click point `x`,`y`. A reply whose
  `element_ref` and `actions` are missing EVERYWHERE is one that could not read the
  target application's start time, so it offers no references rather than ones that
  would be refused the moment they were used — the controls are still listed with
  their click points. Report that if you see it; it should not happen on a healthy
  machine.
* **All four "Save" buttons are listed, and their `path` differs** — `Document`
  and `Sidebar` in AppKit, `Draft` and `Archive` in SwiftUI. Two identical labels
  that cannot be told apart is the failure this field exists to prevent; if any
  pair has equal or empty paths, say so and paste both entries. The SwiftUI pair
  is the one to watch: a hosting view with no group of its own publishes no
  ancestor label at all, and every control under it collapses to the window title.
* Every SwiftUI control's `path` should name `SwiftUI`, as every AppKit one names
  `AppKit`. If the SwiftUI column's paths are all empty, that box is not
  publishing itself and the comparison between the two halves is not meaningful.
* The buttons list `"actions": ["press"]` and `"settable": false`; the text field
  lists `"actions": []` and `"settable": true`. Nothing is inferred from a role —
  if a button reports `settable: true` here, that is the application saying so.
* The DISABLED button is listed with `"enabled": false`, not omitted.
* The secure field is listed, and it carries **no `value`** while the ordinary
  text field carries the text it holds.
* A `truncated` key means the walk stopped early (`"nodes"`, `"depth"` or
  `"time"`). On this window it should be absent. On a large application it may
  not be, and that is the number worth writing down.

## 2. A press presses, and takes nothing from you

Keep typing into TextEdit — a steady stream of the same letter is easiest to
check. From the other terminal, press the fixture's AppKit button by reference:

```sh
COMPUX_DISCLAIMED=1 python3 /tmp/cx.py "$CX" '{"action":"press","element_ref":"e1"}'
```

The script takes a fresh `elements` for that press and prints it, so the reference
it resolves is one from THAT listing — which is why this single-argument form is
safe even though §1 ran in a different sidecar. Read the `elements` line it prints
to see which control you just named; if it is not the one you meant, run the
interactive form and press a reference you picked yourself:

```sh
COMPUX_DISCLAIMED=1 python3 /tmp/cx.py "$CX"
{"action":"elements"}
{"action":"press","observation_id":"<the id it printed>","element_ref":"e4"}
```

* `"ok": true`, `receipt.input_method` is `"ax"`, `receipt.dispatch` is `"sent"`,
  `receipt.effect` is `"not_observed"` and **`receipt.foreground_changed` is
  `false`**. If that last field is ABSENT, the accessibility API would not say
  which application was in front, either before or after — the helper leaves the
  question unanswered rather than guessing `false`. Note it: your eyes on TextEdit
  are then the only evidence for that row of the matrix.
* `/tmp/fixture.json` gains one event: `{"control":"appkit.button","action":"press"}`.
* **The pointer did not move.** Watch it. A press that warps the cursor is this
  slice failing.
* **TextEdit lost nothing.** No dropped letter, no letter arriving in another
  window, no focus ring moving. If a character went missing, note which control
  it was and mark that family `failed` in the matrix.

Repeat for each control family of the fixture, in BOTH toolkit groups (the AppKit
column and the SwiftUI column), and note what the state file records:

| Control | Ask for | The file should record | Recorded by |
|---|---|---|---|
| button | `press` | `press` | the control's own action |
| checkbox | `press` | `toggle` with `on` | its value, watched |
| pop-up button | `press` | the menu opens (no event until you pick) | its value, watched |
| text field | `set_value` | `set_value` with your text | its value, watched |
| secure field | `set_value` | `set_value` with a `length` and no text | its value, watched |
| disabled button | `press` | **nothing** — see §4 | the action, which must not run |

**Why two mechanisms, and what a silence means.** A press really runs the
control's action: `AXPress` on an AppKit button goes through `performClick:`. A
value set does NOT — `AXUIElementSetAttributeValue(AXValue)` writes the string and
sends nothing — so the AppKit half SAMPLES its controls four times a second and
records a value that changed, however it changed. The SwiftUI half cannot be
sampled from outside, so its controls use bindings whose setter records.

That difference matters when a cell comes back empty. An AppKit value that does
not appear in the file within a second means the value never reached the control.
A SwiftUI value that does not appear may instead mean SwiftUI did not route the
accessibility setter through its binding — a real limitation of that toolkit, and
exactly the kind of thing the matrix exists to record. Say which of the two you
saw; the reply's own `verified` flag settles it, because it is a read-back of the
control rather than of the file.

The pop-up is the one to watch for a foreground change: opening a menu is exactly
the kind of thing that takes the front. If `foreground_changed` comes back `true`
there, that is a truthful report, not a bug — record it in the matrix.

## 3. `set_value` verifies, and a secure field cannot

```sh
COMPUX_DISCLAIMED=1 python3 /tmp/cx.py "$CX" \
  '{"action":"set_value","element_ref":"e3","value":"written by compux"}'
```

(Again, `e3` is whatever the `elements` line the script prints calls the field you
mean. The references are numbered in walk order and that order is the
application's, not this document's.)

* On the TEXT field: `"ok": true`, `"verified": true`, `receipt.effect` is
  `"verified"`, and `"value"` echoes what the field now holds. The text is visible
  in the window, and the state file records it.
* On the SECURE field: `"ok": true`, `"verified": false`, `receipt.effect` is
  `"not_observed"`, and **there is no `value` on the reply at all**. The state
  file records a `length` and no text. A secure field reads back masked, so it can
  never verify — that is the design, not a failure.
* On a BUTTON (not settable): `ax_action_unsupported`, `receipt.dispatch` is
  `"not_sent"`, and the sentence tells you to type or paste instead. Nothing is
  typed, nothing is clicked, and the state file gains no event.
* Every refusal so far carries `"dispatch": "not_sent"`. **On a machine with no
  Accessibility grant at all, so does every attempted press**: the platform answers
  that the API is disabled, which means nothing left the helper, and the receipt
  must say so rather than `sent`. Worth one deliberate run with the grant revoked.

## 4. The refusals, on real controls

Each of these must add NOTHING to the state file. Check it after every one.

* **A disabled button**: `press` on it answers `element_disabled` with
  `dispatch: not_sent`. If the file gains an `appkit.disabled_button` event, a
  press reached a control that said it was disabled — report that; the handler is
  there precisely so it cannot happen silently.
* **A control that cannot be pressed by name**: `press` on the text field answers
  `ax_action_unsupported` and says to click it instead. **The helper must not
  click it.** Nothing in the file, nothing in the window.
* **A stale reference.** This one needs ONE sidecar across the whole sequence: an
  id from a second run of the script was never minted by that process and answers
  `unknown_observation`, which proves nothing about staleness. Start the script
  with no arguments and type the lines:

```sh
COMPUX_DISCLAIMED=1 python3 /tmp/cx.py "$CX"
# then, at the prompt:
{"action":"elements"}
#   ... note the observation_id it prints. Now QUIT AND RELAUNCH the fixture app,
#   ... within thirty seconds (past that it is expired_observation, not stale), then:
{"action":"press","observation_id":"<the id it printed>","element_ref":"e1"}
```

  It must answer `stale_element` with `dispatch: not_sent`. This is the case a pid
  alone would get wrong — the relaunched fixture may well take the same pid — so
  it is worth doing twice. If it answers `unknown_observation` you are in a second
  sidecar; if `expired_observation`, the relaunch took longer than the thirty
  seconds and the step needs redoing, not reporting.
* **Both addressing forms at once**: a click carrying `x`, `y` AND an
  `element_ref` answers `addressing_conflict`, and nothing is clicked.

## 5. A click by reference lands where the control IS

```sh
COMPUX_DISCLAIMED=1 python3 /tmp/cx.py "$CX" \
  '{"action":"left_click","element_ref":"e1"}'
```

* It clicks the control, and the state file records the press.
* `receipt.input_method` is `"foreground_hid"`, not `"ax"`: a click by reference is
  still a click, and the receipt says so.
* Then MOVE the fixture window between the listing and the click — one sidecar
  again, because both halves must be in the same observation table:

```sh
COMPUX_DISCLAIMED=1 python3 /tmp/cx.py "$CX"
{"action":"elements"}
#   ... now DRAG the fixture window a few hundred points across the screen, then:
{"action":"left_click","observation_id":"<the id it printed>","element_ref":"e1"}
```

  The click must land on the control in its NEW place, because the bounds are read
  again at that moment. A click that lands where the control used to be is the
  defect this addressing exists to remove.

## 6. Through Fermix, once

Ask the assistant to do something in the fixture app that is best served by
pressing a named control ("tick the AppKit checkbox in the fixture window"). It
should take an element list and press by reference rather than aiming at pixels.
Then ask it to do the same while you type into TextEdit; the answer must be
correct and your typing must be untouched.

## 7. A control in a browser or an Electron app

The hardest family, and the one the matrix is really for. With Chrome in front,
take `elements`, then press a link or a button by reference.

* Chrome builds its accessibility tree lazily. A first `elements` that comes back
  empty with an `ax_activation` note and a second that does not is the settle poll
  working; a `truncated: "time"` there is worth recording.
* A `press` that answers `ax_action_unsupported` on something that plainly looks
  like a button is a TRUE answer about that control: Chromium publishes many
  elements with no `AXPress`. Record the family as `failed` and click it instead —
  the point of the matrix is to know which families can be named and which cannot.

## 8. The support matrix

Fill one row per combination you actually ran. **A cell nobody ran stays
`unqualified`** — that is the honest value and the reason the column exists.
`qualified` means it did the thing, changed the state the application owns, and
reported `foreground_changed: false`. `failed` means any of those three did not
hold; say which in the note.

| OS version | App / toolkit | Control family | Action | Result | Note |
|---|---|---|---|---|---|
| | FixtureApp / AppKit | button | `press` | unqualified | |
| | FixtureApp / AppKit | checkbox | `press` | unqualified | |
| | FixtureApp / AppKit | text field | `set_value` | unqualified | |
| | FixtureApp / AppKit | secure field | `set_value` | unqualified | |
| | FixtureApp / AppKit | disabled button | `press` (refused) | unqualified | |
| | FixtureApp / AppKit | pop-up button | `press` | unqualified | |
| | FixtureApp / SwiftUI | button | `press` | unqualified | |
| | FixtureApp / SwiftUI | checkbox | `press` | unqualified | |
| | FixtureApp / SwiftUI | text field | `set_value` | unqualified | |
| | FixtureApp / SwiftUI | secure field | `set_value` | unqualified | |
| | FixtureApp / SwiftUI | disabled button | `press` (refused) | unqualified | |
| | FixtureApp / SwiftUI | pop-up button | `press` | unqualified | |
| | FixtureApp / AppKit | two "Save" buttons | `path` tells them apart | unqualified | |
| | FixtureApp / SwiftUI | two "Save" buttons | `path` tells them apart | unqualified | |
| | TextEdit / AppKit | text area | `set_value` | unqualified | |
| | Finder / AppKit | toolbar button | `press` | unqualified | |
| | Safari / WebKit | link | `press` | unqualified | |
| | Chrome / Chromium | link | `press` | unqualified | |
| | Chrome / Chromium | form field | `set_value` | unqualified | |
| | an Electron app | button | `press` | unqualified | |

## 9. What this check still cannot prove

* **That a press is invisible to the application.** An application can tell an
  `AXPress` from a click if it looks, and some will behave differently. The state
  file proves the handler ran; it cannot prove the application could not tell.
* **That the foreground never moves.** `foreground_changed` is read from the
  accessibility API before and after, and a platform that will not answer reports
  `false` rather than inventing a change. So a `false` on an application whose
  accessibility is half-implemented is weaker evidence than a `false` on the
  fixture. Your eyes on TextEdit are the stronger test.
* **A control that moves DURING the action.** The bounds are read at the moment of
  the action, which closes the window between the listing and the click, not the
  one inside the click itself.
* **Anything about a background window.** Every action here still runs under the
  same courtesy wait and the same input seat as a click. Window-scoped traversal
  and acting on a window that is not in front need a bound target, which is the
  next slice.

---

# Live check — one action with its check, settled, and timed (M42 slice 6)

**What this is for.** An action's check image used to be captured the instant the
input left, with no wait for the application to react: a click on anything that
repaints in more than a few milliseconds came back showing the screen BEFORE the
change, which reads as "the click did nothing" and costs a model turn. A zoomed
action then cost a second round trip to get its crop, and `wait_for_change`
returned a third frame that may show something else again.

Protocol 10 answers all three. `check` replaces `screenshot_after` on the wire:
`image` returns the view the action was aimed in — its crop, not the whole screen —
waited on until it has stopped moving, and encoded from the sample that proved it.
"Stopped moving" is two consecutive equal samples AND either a change already seen
or 300 ms of quiet since the input: two equal samples alone would call an
application that starts repainting a poll later "unchanged", which is the same
misleading check by another route. `semantic` re-reads the control an `element_ref` named instead of
photographing it. `none` is the receipt alone. The receipt says which evidence it
carries and what each phase cost.

**Only the owner can prove:** that a slow-repainting control comes back repainted;
that `settle` reads `stable` on an ordinary control and `timeout` over a playing
video, both inside 1.5 s; that `/pause` during a settle returns at once with the
input still reported as sent; and what `timings_ms` actually reads on this machine
— which is the input to the copy-and-encode work the performance specification
leaves deferred until these numbers say it is worth doing.

## 0. Preconditions

* The sidecar built from this branch (`cd native/compux && cargo build --release`),
  `CX` pointing at it, and `/tmp/cx.py` from the held-input check above — which
  now says hello at **protocol 10**. A sidecar that answers 9 is the old one.
* **Every action here names its check explicitly.** `cx.py` sends the body you
  type, and Fermix is the half that fills `check` in by rule, so an action typed
  without one brings nothing back at all. That is the new default and it is
  correct: `{"action":"left_click","x":400,"y":300}` now answers a bare
  `{"ok":true}` with a receipt.
* Steps 1 to 4 drive the sidecar directly and need Screen Recording and
  Accessibility on the launching terminal. Step 6 goes through Fermix.
* **Steps 2 to 5 post REAL input to the machine you are sitting at**, and step 5
  clicks FORTY times. Step 1 is the only one that cannot: every request in it is
  refused before anything is dispatched. So choose a target deliberately — empty
  desktop, or a scratch window — close anything you care about, and **never leave
  one of these running unattended**. That a click dispatches correctly is proved in
  the test suite against a recording platform; these steps exist for the half only a
  real screen can show.

## 1. The field that was replaced is refused (10 seconds, fails fastest)

```sh
COMPUX_DISCLAIMED=1 python3 /tmp/cx.py "$CX" \
  '{"action":"left_click","x":400,"y":300,"screenshot_after":true}'
```

* `"error": "unknown_field"`, and the detail names `check` and its three kinds.
  **Nothing is clicked** — `receipt.dispatch` is `not_sent`. A build that still
  sends the old field gets this rather than a silent action with no evidence, which
  is the whole reason it is refused instead of ignored.
* Two more that must be refused before anything is dispatched:

```sh
COMPUX_DISCLAIMED=1 python3 /tmp/cx.py "$CX" \
  '{"action":"left_click","x":400,"y":300,"check":"semantic"}' \
  '{"action":"left_click","x":400,"y":300,"check":"bogus"}'
```

  The first is `check_unsupported` saying a semantic check re-reads a control and
  this click names a point; the second is `check_unsupported` naming the three
  kinds. Neither clicks anything.

## 2. A control that repaints slowly comes back repainted

This is the defect the slice exists for. There are TWO shapes of it and they fail
differently, so run both if you can find them:

* **slow to FINISH** — the repaint starts at once and goes on for a while. Two
  equal samples catch it, and this is what the old code got wrong by capturing
  immediately.
* **slow to START** — nothing happens for a few hundred milliseconds and then the
  view changes. This is the one the quiet window is for: without it the first two
  looks would both show the view as it was, and the check would say `stable`,
  `changed: false` of a click that worked. A menu that opens after a beat, a button
  whose handler does a round trip, a window that appears late.

Any of these will do; use the one you have:

* a Finder window's sidebar item that loads a large folder;
* a browser tab's Reload button on a heavy page;
* the fixture app from the slice-4 check, with its button pressed by point.

Take a screenshot, pick the control's point out of it, and click it with an image
check — one sidecar, so the click names the image you read the point in:

```sh
COMPUX_DISCLAIMED=1 python3 /tmp/cx.py "$CX"
{"action":"screenshot"}
#   ... read the control's point out of the image, then:
{"action":"left_click","observation_id":"<the id it printed>","x":<x>,"y":<y>,"check":"image"}
```

* The reply carries an image, and **that image shows the screen AFTER the repaint**
  — the folder loaded, the page reloading, the button in its pressed-through state.
  Write the base64 to a file and open it if you cannot tell from the numbers:
  `python3 -c 'import json,sys,base64;d=json.load(sys.stdin);open("/tmp/check.png","wb").write(base64.b64decode(d["data"]))'`.
  An image showing the screen as it was before the click is this step failing, and
  it is the only step here that cannot be argued about.
* `receipt.check.settle` is `"stable"`.
* `receipt.check.changed` is `true`: the view moved on from the image you acted in.
  On a control whose click changes nothing visible it is `false`, and that is
  evidence about the view rather than a verdict on the click.
* **On the slow-to-START control, `changed` must still be `true`.** A `false` there
  is this step failing, and the reply's `timings_ms.settle` says why: a settle that
  finished in well under 300 ms did not wait for the repaint to begin.
* An action that really changes nothing costs that window once — about 300 ms of
  `settle`, plus its looks. That is the price of the line above being trustworthy.
* The image is the CROP of the observation you named. On a full screenshot that is
  the whole display as PNG; do the same thing again from a zoomed screenshot
  (`{"action":"screenshot","region":{...}}`) and the check comes back as that same
  rectangle, as JPEG, at the zoom you were working at — one reply, where the old
  build needed a second request to get it.

## 3. `stable` on an ordinary control, `timeout` over a video

Open a video and start it playing. Full screen is easiest; a window works if you
crop to it.

```sh
COMPUX_DISCLAIMED=1 python3 /tmp/cx.py "$CX"
{"action":"screenshot"}
{"action":"left_click","observation_id":"<id>","x":<a point over the playing video>,"y":<y>,"check":"image"}
```

* `receipt.check.settle` is `"timeout"`. A playing video never agrees with itself,
  so the settle runs to its cap and returns the last sample. **That is an
  observation state, not a failure, and never a reason to send the input again** —
  the image is still a real frame of the screen.
* **The honest bound is 1.5 s plus one poll plus one capture.** The cap is checked
  AFTER a look has been taken and hashed, so the last one always runs past it: add
  `receipt.timings_ms.settle` and `receipt.timings_ms.capture` (the settle's own
  looks are counted in `capture`) and the sum should sit just above 1500, not near
  2500. Much more than that means the cap is not binding, which is the one thing
  here that would eat a caller's deadline.
* The same click over a still part of the screen reads `stable`. Both on one
  machine, minutes apart, is the comparison worth recording.

## 4. A pause during a settle returns at once

The pause has to arrive WHILE the settle is polling, which `cx.py` cannot do —
it writes one line and then blocks reading the reply. This one sends the pause
from a second thread, a tenth of a second after the click:

```sh
cat > /tmp/cxpause.py <<'PYEOF2'
import json, os, subprocess, sys, threading, time

side = subprocess.Popen([sys.argv[1]], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                        env=os.environ.copy(), text=True, bufsize=1)
lock = threading.Lock()

def send(frame):
    with lock:
        side.stdin.write(json.dumps(frame) + "\n")
        side.stdin.flush()

def recv():
    reply = json.loads(side.stdout.readline())
    if "data" in reply:
        reply["data"] = "<%d base64 bytes>" % len(reply["data"])
    return reply

send({"type": "request", "request_id": "r1", "action": "hello",
      "protocol_version": 11, "deadline_ms": 10000})
hello = recv()
envelope = {"sidecar_generation": hello["sidecar_generation"],
            "session_generation": 1, "authorization_generation": 1}

shot = {"type": "request", "request_id": "r2", "action": "screenshot",
        "deadline_ms": 30000}
shot.update(envelope)
send(shot)
image = recv()

click = {"type": "request", "request_id": "r3", "action": "left_click",
         "observation_id": image["observation_id"], "x": int(sys.argv[2]),
         "y": int(sys.argv[3]), "check": "image", "mutation_seq": 1,
         "deadline_ms": 30000}
click.update(envelope)

started = time.monotonic()
threading.Timer(0.1, lambda: send({"type": "control", "request_id": "c1",
                                   "action": "pause"})).start()
send(click)

for _ in range(2):
    reply = recv()
    print("%6.0f ms  %s" % ((time.monotonic() - started) * 1000, json.dumps(reply)))

side.stdin.close()
PYEOF2

COMPUX_DISCLAIMED=1 python3 /tmp/cxpause.py "$CX" 400 300
```

Point it at something that repaints for a while, so the pause really lands inside
the settle rather than after it.

* The `control_ack` comes back at once, and the click's own reply follows it
  **well inside the settle's 1.5 s cap** — the settle sleeps through the gate, so a
  pause ends it rather than running it out.
* The click answers `"error": "cancelled"`.
* **Its receipt still says `dispatch: "sent"`.** The input went out; the evidence
  did not. Those are two facts and the receipt reports them separately, so the model
  must not be told the action failed — this is the rule the whole receipt design
  exists for, and the one a live run is the only proof of.
* There is **no `check` on that receipt at all**. Evidence that could not be
  obtained is claimed by nobody.
* The same thing through Fermix — `/pause` during a slow computer-use action — must
  read the same way to the person: the action stops promptly and nothing says it
  failed to act.

## 5. Twenty clicks and twenty zoomed clicks, timed

This is the measurement the deferred encode work is decided on, so it is worth
running properly: one sidecar, the same target, no other load on the machine.

```sh
cat > /tmp/cxtime.py <<'PYEOF2'
import json, os, subprocess, sys

side = subprocess.Popen([sys.argv[1]], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                        env=os.environ.copy(), text=True, bufsize=1)
seq = [1]

def send(frame):
    side.stdin.write(json.dumps(frame) + "\n")
    side.stdin.flush()

def recv():
    return json.loads(side.stdout.readline())

send({"type": "request", "request_id": "r1", "action": "hello",
      "protocol_version": 11, "deadline_ms": 10000})
hello = recv()
envelope = {"sidecar_generation": hello["sidecar_generation"],
            "session_generation": 1, "authorization_generation": 1}

def act(body):
    seq[0] += 1
    frame = {"type": "request", "request_id": "r%d" % seq[0], "deadline_ms": 30000,
             "mutation_seq": seq[0] - 1}
    frame.update(envelope)
    frame.update(body)
    send(frame)
    return recv()

# argv: CX, x, y, rounds, and an optional region "x,y,w,h" for the zoomed run.
x, y, rounds = int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4])
shot_body = {"action": "screenshot"}
if len(sys.argv) > 5:
    rx, ry, rw, rh = (int(n) for n in sys.argv[5].split(","))
    shot_body["region"] = {"x": rx, "y": ry, "w": rw, "h": rh}

print("| # | input | settle | capture | encode | total | settle | changed | bytes |")
print("|---|---|---|---|---|---|---|---|---|")
for round in range(1, rounds + 1):
    shot = act(shot_body)
    click = act({"action": "left_click", "observation_id": shot["observation_id"],
                 "x": x, "y": y, "check": "image"})
    if not click.get("ok"):
        print("| %d | %s |" % (round, click.get("error")))
        continue
    t = click["receipt"]["timings_ms"]
    check = click["receipt"]["check"]
    total = t["input"] + t["settle"] + t["capture"] + t["encode"]
    print("| %d | %d | %d | %d | %d | %d | %s | %s | %d |" % (
        round, t["input"], t["settle"], t["capture"], t["encode"], total,
        check.get("settle"), check.get("changed"), len(click.get("data", ""))))

side.stdin.close()
PYEOF2

# Twenty full-screen clicks. This CLICKS, twenty times, at the point you give it:
# pick one over empty desktop or a scratch window, and watch it run.
COMPUX_DISCLAIMED=1 python3 /tmp/cxtime.py "$CX" 400 300 20

# Twenty zoomed ones, which click twenty MORE times: the coordinates are pixels in
# the CROP, so pick them from a screenshot of that region first.
COMPUX_DISCLAIMED=1 python3 /tmp/cxtime.py "$CX" 120 90 20 200,150,480,360
```

Paste both tables here, with the machine and the display arrangement beside them:

**Full-screen checks** — machine: , display: 

| # | input | settle | capture | encode | total | settle | changed | bytes |
|---|---|---|---|---|---|---|---|---|
| | | | | | | | | |

**Zoomed checks** — region: 

| # | input | settle | capture | encode | total | settle | changed | bytes |
|---|---|---|---|---|---|---|---|---|
| | | | | | | | | |

What to read out of them, and what each answer would mean:

* **`capture` dominates** — the frame grab is the cost, and a settle needs at least
  two of them. The lever is capturing less, not encoding less.
* **`settle` dominates** — the waiting between looks, and the hashing that decides
  whether to stop. The hash reads the crop's raw pixel rows in place, about 10 ms
  for a whole 3840x1080 frame; the first version downscaled a COPY of the crop to a
  256x256 thumbnail instead and cost around 250 ms a sample, more than the capture
  beside it, which is why it is gone (`cargo test --release the_hash_costs --
  --nocapture` re-takes that measurement). If `settle` still dominates here, it is
  the 50 ms poll interval and the number of looks, not the hash.
* **`encode` dominates** — then the copy-and-encode work the performance
  specification lists is worth doing, and this is the table that says so.
* **A zoomed check is not cheaper than a full one** — the crop is taken from a
  whole frame either way, so only the encode shrinks. If the difference is small,
  cropping is not the latency lever it looks like.

## 6. Through Fermix, once each

* An ordinary click: the result names the check it got, and the image is the view
  the model acted in.
* A `press` by reference: Fermix asks for a `semantic` check, so the result says
  what the control reads NOW — which control it was (`label`), its role, whether it
  is enabled, and its value unless it is a secure field — and there is no image at
  all. A secure field's LABEL is fine; its value must not appear anywhere. A control
  that went away between the action and the read says only that it is not there.
* A click that changes nothing visible: the result says so in one sentence and
  says plainly that it is not a reason to repeat the action. Three of those in a
  row and the session says so and names the ways out.

## 7. What this check still cannot prove

* **That the settle waits long enough for every application.** 1.5 s is a cap, not
  a promise: an application that repaints in two seconds returns `timeout` with a
  frame from the middle of its repaint, and the receipt says so honestly. Whether
  that is often enough to matter is what step 5's tables are for.
* **That the settle's rule holds for every application.** Two equal samples plus
  either a seen change or 300 ms of quiet is a heuristic, and an application that
  repaints in stages can be still between two looks, or start later than the window
  allows. What the hash itself cannot miss is a changed pixel: every pixel of the
  rectangle is read, so a one-pixel change flips it. If a real control fools it,
  the number to change is `SETTLE_QUIET_MS`, and this step is where that is decided.
* **Anything about a display this machine does not have.** Every number above is
  this panel's; a Retina panel at a scaled mode, or two displays of different
  backing scales, are their own rows.

---

# Live check — one window, bound (M42 slice 5)

Everything in this slice is a promise about somebody else's window, and no unit
test can keep any of them: whether a covered window is really still captured,
whether the badge really sits on the right window and really takes no focus,
whether Stop on it really stops a drag when the daemon is suspended. The seams are
proved without a screen (`window_server.rs`, `window_frames.rs`, `target.rs`,
`indicator.rs` — 329 Rust tests); this is the half that needs a person.

Expected outcome in one line: with one window bound, the helper sees it even when
something covers it, refuses to click through whatever is in front, shows the
person a badge that follows the window and never takes their focus, and stops on
that badge's Stop even when the daemon cannot answer.

> **Steps 3, 5, 6, 7 and 8 POST REAL INPUT and every step from 2 on STARTS A REAL
> CAPTURE STREAM of one window.** Nothing here may be run by an agent. Run it
> yourself, at the machine, with the fixture app and TextEdit open and nothing
> confidential on screen.

## 0. Preconditions

* **Build both halves.** `cd native/compux && cargo build --release` for the
  helper, and the indicator from its own repository half
  (`scripts/build_app.sh`); the badge's own steps are in
  [`native/indicator/LIVE_CHECK_INDICATOR.md`](native/indicator/LIVE_CHECK_INDICATOR.md),
  which is where its panel, its status item and its `--self-test` are checked. Run
  that file FIRST: if the badge does not come up on its own, nothing below can.
* **`compux-indicator` must sit beside the helper.** It is resolved from the
  running executable's own directory, so a `cargo build` tree needs it copied into
  `native/compux/target/release/` and a bundle needs it in
  `Fermix.app/Contents/MacOS/`. `hello` says which it found — see step 1.
* **The fixture app** (`scripts/build_fixture_app.sh`, slice 4) is the target for
  every step. **TextEdit** is the canary: it is what you type into to prove the
  badge and a background action take nothing from you.
* **Screen Recording and Accessibility** on the launching terminal, as every
  earlier section needs. A bound window additionally needs Screen Recording for
  the STREAM, which is a different prompt from the one a `screenshot` raises on
  some versions. The helper now says this in its own code: a selection that
  answers **`screen_recording_not_granted`** is the grant and nothing else, and
  the remedy is System Settings, Privacy & Security, Screen Recording, then a
  restart of the helper. `capture_unavailable` is a different fault and step 2
  says what to do about it.
* `cx.py` from the slice-2 section drives everything below; it speaks protocol 11
  and takes `select_target` like any other action. Keep ONE sidecar for a whole
  step: a target is per process, and a second run of the script binds nothing the
  first one bound.

## 1. The handshake says what this build can do (10 seconds, fails fastest)

```sh
CX=/Users/sujshe/projects/compux/native/compux/target/release/compux
printf '{"type":"request","request_id":"r1","action":"hello","protocol_version":11,"deadline_ms":10000}\n' \
  | COMPUX_DISCLAIMED=1 "$CX" | python3 -m json.tool
```

It must say `"protocol_version": 11`, and its `capabilities` must carry
`"targets": true`, `"capture_methods": ["display", "window"]` and
**`"indicator": "present"`**. `"missing"` means the badge is not beside the binary
and every step below will refuse `control_surface_unavailable`; fix that before
going on. `actions` must list `select_target` and `release_target`.

## 2. A window binds, and its first frame arrives

**This starts a capture stream of the fixture window.** Bring the fixture app to
the front, then:

```sh
COMPUX_DISCLAIMED=1 python3 /tmp/cx.py "$CX"
# at the prompt, one line at a time:
{"action":"windows"}
#   ... read the fixture app's `id` out of that list, then:
{"action":"select_target","window_id":<id>}
```

What to look at, in order:

* the reply carries `target_id: "t1"`, the app and title you expect, `methods`,
  and an `ax_binding`. **`"bound"` is what you want**: `ambiguous` means the
  fixture has two windows that look alike (close one), `unavailable` means its
  accessibility window could not be matched (note it — step 5 depends on it);
* it carries a picture — `width`, `height`, `observation_id` — and those
  dimensions are the WINDOW's, not the display's. Decode the image and confirm it
  is the fixture window and nothing around it;
* **write down how long the reply took.** That is the stream's first frame, and it
  is the number that decides whether binding a window is usable at all;
* **the badge is now on screen**, at the window's upper left. Leave it.

**If `select_target` hangs for about five seconds and then refuses**, the first
suspect is not the grant. `shareable_content()` asks `SCShareableContent` what is
capturable and blocks the calling thread on a completion handler, and **it is
unverified which queue ScreenCaptureKit delivers that completion on**. If it is
the main queue, and the request came in on the helper's main thread, the wait and
the delivery are the same thread and the five seconds is a self-deadlock that ends
at `HANDSHAKE_MS`, not a slow window server. What to try, in order: run the same
selection with the helper started fresh and nothing else in flight (if it succeeds
only sometimes, it is a race and not a deadlock); then check whether the refusal
is exactly "the window server did not say what is capturable in time" every time,
which is the deadlock's signature. If it is, the fix is to drive that call from a
worker thread rather than to lengthen the wait — say so and stop rather than
raising the constant.

**A picture the wrong size, or a refusal naming two scales.** A reply of
`capture_geometry_mismatch` means the surface that came back is not the shape this
window is: the refusal carries the frame's pixels, the window's points and both
measured ratios. Copy the numbers into the report — they are what says whether the
surface request (window points x the display's backing scale) was honoured on this
panel.

## 3. The badge takes nothing from you

**Real input, by you, not by the helper.** With the target still bound, click into
TextEdit and type a sentence. Every character must land in TextEdit. The badge
must not come to the front, must not steal the caret, and must not appear in the
Cmd-Tab list.

Then drag the fixture window a few hundred points across the screen. The badge
must follow it within about a quarter of a second, and must not lag behind by more
than that. Move another window over the fixture's upper-left corner: the badge
must **hide**. Move it away: the badge must come back.

## 4. A covered window is still seen, and a click on it is refused

Put a large window (a browser, Finder) fully over the fixture window, so none of
the fixture is visible. Then, in the same `cx.py` session:

```
{"action":"screenshot","target_id":"t1"}
```

The image must still be the fixture window, complete. That is the whole point of
the slice; if it is the covering window, or blank, stop and report it.

Now aim at a control in that image and click it:

```
{"action":"left_click","target_id":"t1","x":<x>,"y":<y>,"check":"image"}
```

It must be refused **`target_obstructed`**, and the detail must name the window
that is in front. Nothing must move: the covering window must not have been
raised, lowered or activated, and the pointer must not have moved.

## 5. A covered window's controls are still pressed, by name

**Real input.** Still fully covered:

```
{"action":"elements","target_id":"t1"}
#   ... find a button of the fixture window, then:
{"action":"press","target_id":"t1","element_ref":"<ref>","check":"semantic"}
```

* the listing must contain the fixture WINDOW's controls and nothing from the
  application's other windows;
* the press must land — the fixture records it — with `input_method: "ax"`, the
  pointer must not move, and the covering window must stay in front;
* if step 2 said `ax_binding: "unavailable"`, `elements` here answers
  `ax_binding_unavailable` instead. That is correct behaviour and a finding: write
  down which application it was.

## 6. A window that moved is not clicked where it was

**Real input.** Uncover the fixture window.

```
{"action":"screenshot","target_id":"t1"}
```

Now **drag the fixture window** a few hundred points, and then send a click at a
coordinate you read in that image:

```
{"action":"left_click","target_id":"t1","x":<x>,"y":<y>,"check":"image"}
```

It must be refused **`stale_observation`** with the detail `geometry_changed`, and
nothing may be clicked. Take a fresh screenshot and click again: it must land.

This is the step that proves the mapping is built from the window server's bounds
rather than from the frame's own content rectangle. That rectangle is
surface-relative — it says the same thing wherever the window sits — so a build
that read it as the window's position would pass every other step in this file and
fail only here, by never refusing. **A click that lands after the drag, with no
refusal, is that bug and not a pass.**

Then the other half: **do nothing at all** to the fixture window and send an
action with `check: "image"` that changes nothing (a click on empty space inside
it). The check must come back `settle: stable` with `changed: false` — NOT
`effect: unknown`. An unchanged window reports idle frames rather than pixels, and
"the window server says nothing changed" is an answer; a timeout here means the
idle path is not being credited.

## 7. Stop on the badge halts a long drag with the daemon suspended

**This is the step the whole slice exists for, and it posts real input.** Start a
long drag inside the fixture window, then suspend the process driving it and press
**Stop** on the badge.

```sh
# in one terminal, with the target bound, start a slow drag:
{"action":"left_click_drag","target_id":"t1","from":{"x":<x1>,"y":<y1>},"to":{"x":<x2>,"y":<y2>}}

# in another, the instant it starts:
kill -STOP $(pgrep -f 'cx.py')
```

With the driver suspended, press **Stop** on the badge. The drag must stop within
a moment, the left button must come back **up** (nothing on the desktop is still
dragging), and `kill -CONT` on the driver must then show the action answered
`cancelled` with a receipt that says what it had dispatched. This proves the
badge's buttons reach the helper's gate with no daemon in the path.

Repeat with **Pause** and then **Resume**: the badge must read `paused` while it
is paused, and `working` again after.

## 8. Minimize, close, relaunch

Each of these, with the target still bound, and each must answer its own typed
error and nothing else:

* **minimize the fixture window** → `{"action":"screenshot","target_id":"t1"}` is
  `target_minimized`, and the badge reads `unavailable` with no bounds;
* **close the window** → `target_unavailable`;
* **quit and relaunch the fixture app**, then act on `t1` → `target_unavailable`,
  *even if the new process has the same pid*. That is the start-time half of the
  binding, and it is the one nothing else can catch.

## 9. Twenty cycles, and nothing left behind

```sh
# Before: note the numbers.
pgrep -f compux-indicator | wc -l
ps -o rss= -p $(pgrep -f 'release/compux' | head -1)
```

Then, in ONE `cx.py` session, twenty times:

```
{"action":"select_target","window_id":<id>}
{"action":"release_target"}
```

Afterwards, with the sidecar still running:

* `pgrep -f compux-indicator | wc -l` must be **0**. One left behind is a zombie
  or an unreaped child and is a defect;
* the sidecar's RSS must be within a few MB of what it was. A bound window holds
  at most four frames' worth — the stream's `queueDepth` of three plus the one
  copy the slot keeps — and twenty cycles must not accumulate;
* `sudo lsof -p <sidecar pid> | grep -ci screencapture` (or Activity Monitor's
  Open Files) must not have grown.

Then quit the sidecar and confirm no `compux-indicator` survives it.

## 10. Three numbers this check decides

Each of these is a constant chosen against a documented behaviour with no promised
cadence, and each has one live symptom. If you see the symptom, change the number
here rather than working around it.

* **`SILENCE_LIMIT_MS` (5 s, `window_frames.rs`).** A stream that says nothing at
  all for this long stops being trusted, because otherwise a stream that quietly
  died answers its last picture as the present forever. The header documents an
  idle sample for an unchanged window but promises no interval, so the symptom of
  a bound that is too tight is **`capture_unavailable` on a window nobody is
  touching** — leave the fixture window alone for ten seconds, then take a
  screenshot of it. It must still answer a picture.
* **`FENCE_WAIT_MS` (400 ms, `target.rs`).** How long an after-action look waits
  for a frame past the fence. Too short shows up as `effect: unknown` with the
  dispatch preserved — a truthful answer, but a useless one if it is common.
* **`LETTERBOX_TOLERANCE_PIXELS` (2 px, `target.rs`).** How far the content may
  fall short of filling the surface before the mapping is refused
  `capture_geometry_mismatch`. A materially inset surface must be refused; a
  surface a pixel short of the window's rounded point size must not. The symptom
  of too tight is a **selection that refuses a perfectly ordinary window** — the
  refusal carries the numbers, so copy them into the report rather than raising
  the constant blind.

## 11. What this check still cannot prove

* **That the fence never returns a frame from before the action.** It is proved at
  the seam and the timing is the machine's; what a live run can show is the
  opposite failure — a check that says `effect: unknown` because no frame arrived
  in time. The fence now compares the frame's `SCStreamFrameInfoDisplayTime` with
  a `mach_absolute_time()` read at the dispatch — the same base, no conversion —
  so what remains unproven is only whether the window server's display times are
  monotonic across a display change.
* **That a frame the reader panicked on is recovered from.** The callback catches
  its own unwind and records the fault, and the next look answers it; nothing here
  can make a real frame panic.
* **What a routed post would do instead.** `cargo run --example
  routed_input_probe -- <pid> click <x> <y>` posts ONE event to one process
  through `CGEventPostToPid`. It is the owner's study (M42 §7.3), not a step of
  this check, and it posts real input.
* **Anything about a window on another display, or a display this machine does
  not have.** A window dragged between two panels of different backing scales is
  its own row.
