# Live check — the ownership indicator (owner runs this)

The indicator's logic is proved by `--self-test-headless`: the wire (every malformed
line it must ignore rather than die on), the placement maths against a synthetic list of
displays including one above and to the left of the main one, the bounding of a hostile
window title, and the four event lines. What no test on a build machine can prove is the
half that only a real display session has: that a panel really appears where the window
is, that showing it and **pressing its buttons takes no focus from the person typing**,
that VoiceOver reads it, and that it is gone the moment the helper is.

This document is the indicator alone, driven by hand over a pipe. That it follows a
**real** window — that the bounds and the occlusion flag are true — is the helper's half,
in `LIVE_CHECK.md`: here you are the helper.

Expected outcome in one line: the badge sits just above the title bar of the rectangle
you name, follows it across displays, disappears when you say it is covered, reports
every press on stdout, and you can type a paragraph into TextEdit throughout without
losing or gaining a single character.

## 0. Preconditions

* Command Line Tools (`swiftc`). Nothing else: no Xcode project, no package manager, no
  dependency.
* **No grant of any kind is needed.** The indicator captures nothing, reads nothing from
  the screen and posts no input. If anything below raises a permission prompt, stop:
  something is wrong, not something to approve.
* Every `y` you type below is a **global top-left** coordinate — pixels down from the top
  of the main display, which is the window server's space, not AppKit's. That conversion
  is the one thing most likely to be wrong on a multi-display desk, which is why §4 has
  you put a display above or to the left of the main one.

## 1. Build it, and run the half that needs no screen (about 15 seconds)

```sh
cd /Users/sujshe/projects/compux
scripts/build_indicator.sh                      # arm64 for this machine -> /tmp/compux-indicator
scripts/build_indicator.sh x86_64 /tmp/compux-indicator-x86_64
/tmp/compux-indicator --self-test-headless
```

Both builds must be silent (they compile with `-warnings-as-errors`), and the self-test
must print `ok: wire, placement, label and event lines (no window)` and exit 0. A failure
names what it expected and what it got, one line per failure, on stderr.

## 2. The self-test that shows a panel (about one second)

```sh
/tmp/compux-indicator --self-test ; echo "exit=$?"
```

Watch the screen: a small dark badge reading **Fermix — Working**, with Pause and Stop,
appears near the top-left for a second and goes. Keep your hands off the keyboard while
it runs; the test asserts that the front application is the same before and after, and
that the panel never became key or main.

It must print `ok: the panel showed, took no focus, and reported every press` and exit 0.
Nothing must be left on screen afterwards, neither the badge nor a menu-bar item.

## 3. The bundle: two executables, one identity

```sh
cd /Users/sujshe/projects/compux
cargo build --release --manifest-path native/compux/Cargo.toml
scripts/build_app.sh native/compux/target/release/compux /tmp/Fermix.app 0.0.0-dev
ls -l /tmp/Fermix.app/Contents/MacOS/
codesign -dv --verbose=4 /tmp/Fermix.app/Contents/MacOS/compux-indicator 2>&1 | grep -E 'flags|Team'
codesign --verify --deep --strict --verbose=2 /tmp/Fermix.app
```

What to look at:

* `Contents/MacOS` holds **`compux` and `compux-indicator`**, both executable. The
  indicator's architectures are the helper's: `lipo -archs` on the two must agree.
* `flags=...(runtime)` on the indicator — the hardened runtime, which the bundle's own
  signature does not confer on a nested binary by itself. On this local build it also
  says `adhoc` and `TeamIdentifier=not set`; that is what an unsigned-for-release build
  looks like. **One team on both executables** is a check against a real release:
  `ditto -x -k compux-<ver>-macos-aarch64.zip /tmp/rel` and run the same two commands
  there.
* `--verify --deep --strict` says `valid on disk` and `satisfies its Designated
  Requirement`.

After installing that release: System Settings ▸ Privacy & Security ▸ Screen Recording
(and Accessibility) must still show **exactly one** row for the sidecar, named
*Fermix Computer Use*, and selecting a target must raise **no new prompt**. A second row,
or a prompt naming the indicator, means the two executables no longer share one identity.

## 4. Drive it by hand: placement, following, hiding

Put a window (TextEdit will do) roughly where the first rectangle below says, then run
this and watch. Each step holds for four seconds; the comment says what to look at.

```sh
{
  # 1. Upper-left, OUTSIDE the frame, just above the title bar.
  printf '%s\n' '{"state":"working","app":"TextEdit","title":"Notes","bounds":{"x":300,"y":200,"w":800,"h":500},"occluded":false}'; sleep 4
  # 2. It FOLLOWS: same badge, moved with the rectangle. No animation, no trail.
  printf '%s\n' '{"state":"working","app":"TextEdit","title":"Notes","bounds":{"x":700,"y":420,"w":800,"h":500},"occluded":false}'; sleep 4
  # 3. Covered: the badge is GONE (it must never sit over an unrelated window), and the
  #    menu-bar item is the only surface left. The helper sends this whenever a window in
  #    front of the target touches ANY PART of where the badge may be drawn — a rectangle
  #    at the target's top-left corner, 448 pt wide and reaching from 34 pt above the
  #    window's top edge to 80 pt below it — not the corner alone. A window overlapping
  #    only the badge's right-hand end hides it just the same.
  printf '%s\n' '{"state":"working","app":"TextEdit","title":"Notes","bounds":{"x":700,"y":420,"w":800,"h":500},"occluded":true}'; sleep 4
  # 4. Back, and PAUSED: the glyph and the word change, and the button says Resume.
  printf '%s\n' '{"state":"paused","app":"TextEdit","title":"Notes","bounds":{"x":700,"y":420,"w":800,"h":500},"occluded":false}'; sleep 4
  # 5. A window pushed up against the menu bar: no room above it, so the badge sits
  #    INSIDE the window, below the traffic lights — never over them.
  printf '%s\n' '{"state":"working","app":"TextEdit","title":"Notes","bounds":{"x":300,"y":40,"w":800,"h":860},"occluded":false}'; sleep 4
  # 6. No bounds at all, then unavailable: hidden both times; the menu-bar item says
  #    Working, then Unavailable.
  printf '%s\n' '{"state":"working","app":"TextEdit","title":"Notes","bounds":null,"occluded":false}'; sleep 3
  printf '%s\n' '{"state":"unavailable","app":"TextEdit","title":"Notes","bounds":null,"occluded":false}'; sleep 3
  # 7. A hostile title from another application: it must render as ONE line of plain
  #    text, cut, with no blank lines and nothing reversed after it.
  printf '%s\n' '{"state":"working","app":"Ev\nil","title":"one\ntwo‮three and a very long tail that has to be cut somewhere sensible","bounds":{"x":300,"y":200,"w":800,"h":500},"occluded":false}'; sleep 5
} | /tmp/compux-indicator
```

Then the multi-display half, which is where a coordinate-space mistake shows. With a
second display **above or to the left** of the main one (System Settings ▸ Displays ▸
Arrange), read its position off the arrangement diagram and send a rectangle on it — for
a display to the LEFT, `x` is negative; for one ABOVE, `y` is negative:

```sh
{
  printf '%s\n' '{"state":"working","app":"TextEdit","title":"Notes","bounds":{"x":-1700,"y":-900,"w":800,"h":500},"occluded":false}'; sleep 6
} | /tmp/compux-indicator
```

The badge must appear **on that display**, at that window's upper-left, and never at the
mirror-image position on the main one.

One more, which looks like a defect and is not: name a rectangle whose LEFT edge is close
to the right edge of a display (say `x` = display width − 200). The badge does not appear
at all. It may only be drawn inside the rectangle the helper watches for occlusion, that
rectangle runs 448 pt to the right of the window's left edge, and what is left on that
display cannot hold it — so it hides, and the menu-bar item carries the session. A badge
drawn anywhere else would be one whose occlusion nobody is checking.

The badge itself is at most 440 × 28 points and shrinks to its content, so a short title
gives a narrower badge; what never changes is that it stays inside that rectangle.

## 5. It never takes focus (the one that matters)

Open TextEdit, click into the document, and start typing a paragraph — do not stop. While
you type, from a second terminal:

```sh
{
  printf '%s\n' '{"state":"working","app":"TextEdit","title":"Untitled","bounds":{"x":300,"y":200,"w":800,"h":500},"occluded":false}'; sleep 5
  printf '%s\n' '{"state":"working","app":"TextEdit","title":"Untitled","bounds":{"x":420,"y":300,"w":800,"h":500},"occluded":false}'; sleep 5
  printf '%s\n' '{"state":"working","app":"TextEdit","title":"Untitled","bounds":{"x":300,"y":200,"w":800,"h":500},"occluded":false}'; sleep 20
} | /tmp/compux-indicator
```

While the badge appears and moves, and then **while you click Pause and Stop on the badge
itself**, TextEdit must stay the front application: the menu bar keeps saying TextEdit,
the insertion point keeps blinking in the document, and the paragraph you typed has no
dropped, doubled or inserted characters. Read it back before believing it.

The two presses print `{"event":"pause"}` and `{"event":"stop"}` in the terminal, one
line each, and **the badge does not change**: it still says Pause, because the indicator
renders what the helper says and the helper is you, silent. Send the paused line by hand
and it changes then.

## 6. The menu-bar item, and the same three commands

While any of the above is running, look at the right-hand end of the menu bar: a small
glyph (● working, ‖ paused, ○ unavailable). Click it. The menu reads
`Fermix — Working`, then the target's label, then **Pause** and **Stop**. Choose one: it
writes the same event line the button does. This is the surface that survives §4 step 3 —
when the badge is hidden, this is how you still see that Fermix holds a window, and still
stop it.

## 7. VoiceOver

Turn VoiceOver on (⌘F5) with the badge shown, and move the VoiceOver cursor onto it. It
must read the panel as *Fermix computer use*, the target as *Target window* with the
label as its value, the state, and the two buttons as *Pause Fermix* / *Stop Fermix* with
their help text. The glyph must be silent — the word beside it says the same thing, and
hearing "black circle" would only be noise. Nothing here animates, so Reduce Motion has
nothing to turn off.

## 8. Leaving: nothing outlives the helper

```sh
# stdin closes -> everything goes at once, exit 0.
printf '%s\n' '{"state":"working","app":"TextEdit","title":"Notes","bounds":{"x":300,"y":200,"w":800,"h":500},"occluded":false}' | /tmp/compux-indicator ; echo "exit=$?"

# SIGTERM does the same. In one terminal, hold it open with the badge shown (the
# trailing `sleep` keeps the pipe open, so this prompt stays busy until it ends; ⌃C is
# fine once the badge has gone):
{ printf '%s\n' '{"state":"working","app":"TextEdit","title":"Notes","bounds":{"x":300,"y":200,"w":800,"h":500},"occluded":false}'; sleep 30; } | /tmp/compux-indicator ; echo "exit=$?"
# and in another, while it is up:
pkill -TERM -f compux-indicator ; pgrep -fl compux-indicator || echo "gone"
```

The badge and the menu-bar item must disappear together, immediately, and `pgrep` must
print nothing: a badge outliving its helper would tell you Fermix owns a window nothing
is acting on. Finish this document by running that `pgrep` once more.
