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
BUILT=/private/tmp/claude-501/-Users-sujshe-projects-fermix/666930c4-2ed9-4663-93f8-142cee19f495/scratchpad/compux-v11/native/compux/target/release/compux

# Keep the FIRST known-good binary, and never overwrite that backup on a re-run.
[ -e "$SIDE/compux.bak" ] || cp -p "$SIDE/compux" "$SIDE/compux.bak"

# Atomic replace: copy beside the target, then rename over it.
cp "$BUILT" "$SIDE/compux.new" && mv -f "$SIDE/compux.new" "$SIDE/compux"

# The installed binary must be exactly the one that passed the gates.
shasum -a 256 "$BUILT" "$SIDE/compux"
# both lines must read:
# f063e0fbaac89b7b3de75f2d2eb7737d545f111f7f1db50d2fb51134c2d97db7
```

Sanity-check the wire without starting a session (the binary reads stdin, so it must be
given a line — never run it with no input, it will simply wait):

```sh
printf '{"action":"hello"}\n' | COMPUX_DISCLAIMED=1 "$SIDE/compux"
```

It must report `"protocol_version":6`. 0.9.0 adds no action: `browser.navigated` and the
browser context on `field.value` are additive event fields on the existing push wire, so
the pairing with the pinned Fermix side is unchanged.

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
