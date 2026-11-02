#!/usr/bin/env python3
"""The X11 pixel route on the GTK 3 fixture, judged by what the fixture recorded
and what the X server says (M86 L2).

Runs INSIDE the private-session container only (`spike.sh` stage `pixels` starts
it), never on a desktop someone uses: it starts the compux helper, which captures
the session's screen and posts input to it through XTEST. It refuses (exit 2)
anywhere without a container's marker file, before it starts anything.

  python3 pixels.py --binary /opt/compux/compux \\
      --state /tmp/compux-fixture-state/gtk3.json --out /tmp/spike-out/gtk3-pixels.json

The root window's background is tiled with a known pattern first (x11_oracle.py),
the one piece of setup. Then, each step judged by the fixture's state file, the X
server, or the reply where the reply is the evidence:

- `hello` lists every pixel verb, `paste` and `windows` among them, and `probe`
  says XTEST and the keymap answer;
- `windows` lists the fixture's window, and a `screenshot`'s pixels outside every
  window are the tiled pattern (the core GetImage, in strips);
- `left_click` on the fixture's button reaches it through XTEST;
- `type` into its text field arrives as typed, and a character the layout has no
  key for is refused `text_outside_layout`, nothing typed;
- `paste` lands through the CLIPBOARD selection compux owns;
- a helper killed with the left button held mid-drag leaves it held, and the next
  helper's start releases it and clears the dead one's journal.

Exit 0 when every step met its expectation, 1 otherwise; the JSON has each step's
evidence and verdict.
"""

import argparse
import base64
import json
import os
import struct
import subprocess
import sys
import time
import zlib

import x11_oracle
from verbs import (CONTAINER_MARKERS, Helper, HelperFailed, events, find, fixture_listed,
                   recorded)

PIXEL_VERBS = ("screenshot", "left_click", "right_click", "double_click", "mouse_move",
               "left_click_drag", "scroll", "key", "wait_for_change", "paste", "windows")
WINDOW_TITLE = "Compux Fixture GTK3"
# A pattern block counts only when no window comes within MARGIN root pixels of it;
# at least MIN_BLOCKS of them must be read, each within TOLERANCE per channel (the
# frame may be resampled to its sent size).
MARGIN = 4
MIN_BLOCKS = 16
TOLERANCE = 24
MAX_MISMATCHES_KEPT = 5
# Bounded waits: tries x step.
HELD_TRIES = 200
HELD_STEP_S = 0.005
QUIET_S = 1.0
JOURNAL_DIR = "compux-held"


def refuse_outside_a_container():
    if not any(os.path.exists(marker) for marker in CONTAINER_MARKERS):
        print("pixels.py: runs only inside the fixture container", file=sys.stderr)
        sys.exit(2)


# --- the frame ----------------------------------------------------------------------


def png_rows(data):
    """An 8-bit RGBA, non-interlaced PNG (what compux sends) as its width, height
    and unfiltered rows."""
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise ValueError("not a PNG")
    position, chunks, header = 8, [], None
    while position < len(data):
        length, kind = struct.unpack(">I4s", data[position:position + 8])
        body = data[position + 8:position + 8 + length]
        position += 12 + length
        if kind == b"IHDR":
            header = struct.unpack(">IIBBBBB", body)
        elif kind == b"IDAT":
            chunks.append(body)
        elif kind == b"IEND":
            break
    width, height, depth, colour, _, _, interlace = header
    if (depth, colour, interlace) != (8, 6, 0):
        raise ValueError(f"a PNG compux does not send: depth {depth}, colour {colour}")
    raw = zlib.decompress(b"".join(chunks))
    return width, height, unfiltered(raw, width * 4, height)


def unfiltered(raw, stride, height):
    rows, previous = [], bytearray(stride)
    for y in range(height):
        start = y * (stride + 1)
        kind, row = raw[start], bytearray(raw[start + 1:start + 1 + stride])
        for i in range(stride):
            left = row[i - 4] if i >= 4 else 0
            up = previous[i]
            corner = previous[i - 4] if i >= 4 else 0
            row[i] = (row[i] + predicted(kind, left, up, corner)) & 0xFF
        rows.append(row)
        previous = row
    return rows


def predicted(kind, left, up, corner):
    if kind == 0:
        return 0
    if kind == 1:
        return left
    if kind == 2:
        return up
    if kind == 3:
        return (left + up) // 2
    estimate = left + up - corner
    nearest = min((abs(estimate - left), 0, left), (abs(estimate - up), 1, up),
                  (abs(estimate - corner), 2, corner))
    return nearest[2]


def covered(block, windows, scale):
    """Whether any listed window comes within MARGIN of this root block."""
    x, y = block
    for window in windows:
        region = window.get("region", {})
        left, top = region.get("x", 0) / scale - MARGIN, region.get("y", 0) / scale - MARGIN
        right = left + region.get("w", 0) / scale + 2 * MARGIN
        bottom = top + region.get("h", 0) / scale + 2 * MARGIN
        if x < right and x + x11_oracle.BLOCK > left and y < bottom and y + x11_oracle.BLOCK > top:
            return True
    return False


def pattern_check(shot, windows):
    """Every uncovered block's centre in the sent frame is the tiled colour."""
    width, height, rows = png_rows(base64.b64decode(shot["data"]))
    scale = width / shot["physical"]["width"]
    block = x11_oracle.BLOCK
    read, mismatches = 0, []
    for top in range(0, shot["physical"]["height"] - block + 1, block):
        for left in range(0, shot["physical"]["width"] - block + 1, block):
            if covered((left, top), windows, scale):
                continue
            cx, cy = left + block // 2, top + block // 2
            sx, sy = min(width - 1, round(cx * scale)), min(height - 1, round(cy * scale))
            got = tuple(rows[sy][sx * 4:sx * 4 + 3])
            want = x11_oracle.expected_colour(cx, cy)
            want = ((want >> 16) & 0xFF, (want >> 8) & 0xFF, want & 0xFF)
            read += 1
            if max(abs(a - b) for a, b in zip(got, want, strict=True)) > TOLERANCE:
                mismatches.append({"root": [cx, cy], "got": got, "want": want})
    passed = read >= MIN_BLOCKS and not mismatches
    return {"flow": "screenshot pattern", "passed": passed,
            "why": f"{read} blocks read in a {width}x{height} frame, {len(mismatches)} differ",
            "mismatches": mismatches[:MAX_MISMATCHES_KEPT]}


# --- the steps on one helper ----------------------------------------------------------


def hello_check(hello):
    actions = hello.get("actions") or []
    methods = hello.get("capabilities", {}).get("input_methods")
    missing = [verb for verb in PIXEL_VERBS if verb not in actions]
    passed = not missing and methods == ["foreground_hid", "ax"]
    return {"flow": "hello", "passed": passed, "why": f"missing {missing}, input_methods {methods}"}


# The image ships neither the portal nor its PermissionStore, so on the private
# session bus both are names nobody owns or can start: probe, which reads them
# with NO_AUTO_START, must report both as not running.
PORTAL_NOT_RUNNING = {"running": False, "registry": None, "screenshot_permission": "not_running"}


def probe_check(helper):
    _, reply = helper.ask("probe")
    pixels = reply.get("x11_pixels")
    portal = reply.get("portal") or {}
    portal_read = {key: portal.get(key) for key in PORTAL_NOT_RUNNING}
    passed = (pixels == {"xtest": True, "keymap": True} and reply.get("screen_capture") is True
              and reply.get("input_control") is True and portal_read == PORTAL_NOT_RUNNING)
    return {"flow": "probe", "passed": passed,
            "why": f"x11_pixels {pixels}, screen_capture {reply.get('screen_capture')}, "
                   f"input_control {reply.get('input_control')}, portal {reply.get('portal')}"}


def frame_checks(helper):
    """`windows` lists the fixture, and the frame shows the pattern around it."""
    _, listed = helper.ask("windows")
    windows = listed.get("windows") or []
    titles = [window.get("title") for window in windows]
    found = {"flow": "windows", "passed": WINDOW_TITLE in titles, "why": f"titles {titles}"}
    _, shot = helper.ask("screenshot")
    if not shot.get("ok"):
        return [found, {"flow": "screenshot pattern", "passed": False,
                        "why": f"refused {shot.get('error')}: {shot.get('detail')}"}]
    return [found, pattern_check(shot, windows)]


def click_check(helper, state):
    button, observation, _ = find(helper, "button", "GTK3 button")
    if button is None:
        return {"flow": "left_click", "passed": False, "why": "GTK3 button is not listed"}
    since = len(events(state))
    _, reply = helper.ask("left_click", mutation=True, observation_id=observation,
                          element_ref=button["element_ref"])
    event = recorded(state, since, "gtk3", "button", "press", None)
    return {"flow": "left_click", "passed": bool(reply.get("ok")) and event is not None,
            "why": f"reply {reply.get('error') or 'ok'}, recorded {event}",
            "input_method": reply.get("receipt", {}).get("input_method")}


def typing_checks(helper, state):
    """Click into the field, type into it, be refused a character off the layout,
    then paste after what was typed."""
    field, observation, _ = find(helper, "textfield", "GTK3 text field")
    if field is None:
        return [{"flow": "type", "passed": False, "why": "GTK3 text field is not listed"}]
    helper.ask("left_click", mutation=True, observation_id=observation,
               element_ref=field["element_ref"])
    since = len(events(state))
    _, typed = helper.ask("type", mutation=True, text="hello")
    event = recorded(state, since, "gtk3", "text_field", "set_value", "hello")
    typing = {"flow": "type", "passed": bool(typed.get("ok")) and event is not None,
              "why": f"reply {typed.get('error') or 'ok'}, recorded {event}"}
    since = len(events(state))
    _, outside = helper.ask("type", mutation=True, text="é")
    time.sleep(QUIET_S)
    arrived = events(state)[since:]
    detail = outside.get("detail") or ""
    refusal = {"flow": "type outside the layout",
               "passed": outside.get("error") == "text_outside_layout" and "paste" in detail
               and not arrived,
               "why": f"error {outside.get('error')}: {detail}; recorded since {arrived}"}
    since = len(events(state))
    _, pasted = helper.ask("paste", mutation=True, text=" and ✓ pasted")
    event = recorded(state, since, "gtk3", "text_field", "set_value", "hello and ✓ pasted")
    paste = {"flow": "paste", "passed": bool(pasted.get("ok")) and event is not None,
             "why": f"reply {pasted.get('error') or 'ok'}, recorded {event}"}
    return [typing, refusal, paste]


# --- a held button across a helper's death ----------------------------------------------


def post(helper, action, **fields):
    """Send one mutation without waiting for its reply: the helper is killed first."""
    helper.next_id += 1
    helper.next_seq += 1
    frame = {"type": "request", "request_id": f"r{helper.next_id}", "action": action,
             **helper.generations, "mutation_seq": helper.next_seq, **fields}
    helper.proc.stdin.write(json.dumps(frame) + "\n")
    helper.proc.stdin.flush()


def held_down(display):
    for _ in range(HELD_TRIES):
        if display.buttons_down() & x11_oracle.BUTTON1_MASK:
            return True
        time.sleep(HELD_STEP_S)
    return False


def journals():
    directory = os.path.join(os.environ.get("XDG_RUNTIME_DIR", "/nonexistent"), JOURNAL_DIR)
    try:
        return sorted(os.listdir(directory))
    except FileNotFoundError:
        return []


def held_check(args, display):
    """Kill a helper with the left button held mid-drag; the next one releases it."""
    killed = Helper(args.binary, args.out + ".killed.stderr")
    try:
        killed.handshake()
        _, shot = killed.ask("screenshot")
        post(killed, "left_click_drag", observation_id=shot.get("observation_id"),
             **{"from": {"x": 40, "y": shot["height"] - 40},
                "to": {"x": shot["width"] - 40, "y": shot["height"] - 40}})
        down = held_down(display)
    finally:
        killed.proc.kill()
        killed.proc.wait()
        killed.stderr.close()
    time.sleep(QUIET_S)
    stuck, left = bool(display.buttons_down() & x11_oracle.BUTTON1_MASK), journals()
    following = Helper(args.binary, args.out + ".next.stderr")
    try:
        following.handshake()
    finally:
        following.close()
    released = not display.buttons_down() & x11_oracle.BUTTON1_MASK
    passed = down and stuck and bool(left) and released and not journals()
    return {"flow": "held button released by the next helper", "passed": passed,
            "why": f"held before the kill {down}, still held after it {stuck}, journals "
                   f"{left}; after the next start released {released}, journals {journals()}"}


# --- the run ------------------------------------------------------------------------------


def on_one_helper(args, results):
    helper = Helper(args.binary, args.out + ".stderr")
    try:
        results.append(hello_check(helper.handshake()))
        ready = fixture_listed(helper, "gtk3")
        results.append(ready)
        if not ready["passed"]:
            return
        results.append(probe_check(helper))
        results.extend(frame_checks(helper))
        results.append(click_check(helper, args.state))
        results.extend(typing_checks(helper, args.state))
    except HelperFailed as why:
        results.append({"flow": "helper", "passed": False, "why": str(why)})
    finally:
        helper.close()


def run(args):
    results = []
    with x11_oracle.Display() as display:
        display.paint_root()
        on_one_helper(args, results)
        try:
            results.append(held_check(args, display))
        except (HelperFailed, BrokenPipeError, subprocess.SubprocessError) as why:
            results.append({"flow": "held button released by the next helper",
                            "passed": False, "why": f"{type(why).__name__}: {why}"})
    return {"steps": results, "events": events(args.state),
            "passed": bool(results) and all(result.get("passed") for result in results)}


def main():
    refuse_outside_a_container()
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--state", required=True)
    parser.add_argument("--out", required=True)
    args = parser.parse_args()
    os.makedirs(os.path.dirname(args.out), exist_ok=True)
    result = run(args)
    with open(args.out, "w") as handle:
        json.dump(result, handle, indent=1)
    for step in result["steps"]:
        print(f"{'PASS' if step.get('passed') else 'FAIL'} gtk3 {step['flow']}: {step.get('why')}")
    print(f"PIXELS gtk3: {'PASS' if result['passed'] else 'FAIL'}")
    sys.exit(0 if result["passed"] else 1)


if __name__ == "__main__":
    main()
