#!/usr/bin/env python3
"""One window bound on X11, and its badge, judged by what the fixture, the X server
and the windows around it recorded (M86 L3).

Runs INSIDE the private-session container only (`spike.sh` stage `bound` starts
it), never on a desktop someone uses: it starts the compux helper, which binds a
window, reads its frames through Composite and posts input through XTEST, and it
clicks the badge as the person would. It refuses (exit 2) anywhere without a
container's marker file, before it starts anything.

  python3 bound.py --binary /opt/compux/compux \\
      --state /tmp/compux-fixture-state/gtk3.json --out /tmp/spike-out/gtk3-bound.json

Two plain windows (panes.py) go beside the GTK 3 fixture: a magenta COVER in front
of its right side, title bar and frame as well as client, and a FLOOR behind it,
under where the badge floats. Then:

- `hello` offers `select_target` and `release_target`, `targets: true`, and window
  capture;
- the fixture is bound under the cover; its first frame holds not one pixel of the
  cover's colour, title bar and frame included, while the display shows the cover
  over them; a later frame shows the fixture where the cover hides it;
- the badge is up: one override-redirect window of the helper's own child, inside
  the rectangle the helper checks for anything covering it, taking clicks on two
  rectangles only;
- a click on the bound window's button lands (the fixture recorded it); one at a
  point the cover hides is `target_obstructed`, and nothing recorded it;
- an override-redirect popup, which no window manager lists, over the button
  refuses the click `target_obstructed`, naming it, and nothing is recorded; with
  its input region set clear of that point, the same click lands;
- the badge's Pause, clicked as the person clicks, reaches the gate (an
  `operator_pause`, and the next click is refused `paused`); Resume undoes it; a
  click on the badge outside its buttons reaches the floor under it; a press and
  release on Stop that another program sends (SendEvent) is dropped, and the gate
  goes on; Stop clicked as the person clicks reaches the gate; none of these moved
  the keyboard focus;
- a window bound, closed, and made again under the SAME id by the same process
  (xcb_victim.py) does not bring the binding back: `target_unavailable`;
- a window that paints once and never repaints, bound with the cover over part of
  it, is refused `capture_unavailable`, and no frame of it is sent;
- on a second X server with no window manager, a top-level window with an X
  border of its own is read from inside the border: every pixel of its frames is
  the window's colour, at the border it was bound with and after it is given
  another; and a bound window at the top of the screen, with the badge inside its
  top edge, leaves a popup mapped later over the badge, and when it is raised to
  the very top the badge goes back over it;
- twenty select and release cycles leave no badge process, no zombie and no badge
  window, and the helper's resident memory flat.

Exit 0 when every step met its expectation, 1 otherwise; the JSON has each step's
evidence and verdict.
"""

import argparse
import base64
import ctypes
import ctypes.util
import json
import os
import queue
import subprocess
import sys
import time

import x11_oracle
import xcb_victim
from pixels import png_rows
from verbs import CONTAINER_MARKERS, Helper, HelperFailed, events, fixture_listed, recorded
from xcb_victim import Victim

FIXTURE = "Compux Fixture GTK3"
FLOOR, FLOOR_COLOUR = "Compux Floor", 0x406080
COVER, COVER_COLOUR = "Compux Cover", 0xFF00FF
VICTIM, VICTIM_COLOUR = "Compux Victim", 0x30A030
LAZY, LAZY_COLOUR, LAZY_AT = "Compux Lazy", 0x2050C0, (400, 450)
BORDERED, BORDERED_COLOUR, BORDERED_AT = "Compux Bordered", 0x30A0A0, (200, 300)
BORDER_COLOUR, BORDER_WIDTHS = 0xE0E020, (8, 3)
# The bordered window's colour from its new border on: only a frame read after
# the change can show it.
RECOLOURED = 0xA03050
POPUP, POPUP_COLOUR, POPUP_SIZE = "Compux Popup", 0xC08020, (160, 80)
# Clear of the bare server's badge, bottom right.
POPUP_AWAY = (900, 700)
# At the top of the screen, so the badge has no room above it and sits inside.
RAISED, RAISED_COLOUR, RAISED_AT = "Compux Raised", 0x8040A0, (300, 4)
# Also at the top, for the badge held under a popup; nudged right so the
# helper sends the badge a new place.
HELD, HELD_COLOUR, HELD_AT, NUDGE = "Compux Held", 0x40A080, (300, 4), 16
# How far a popup opened over the badge reaches past it on every side.
POPUP_MARGIN = 20
# A second X server, with no window manager on it.
BARE_DISPLAY = 77
BADGE_CLASS = "^compux-indicator-x11$"
# Where the windows go, in root pixels: the fixture with room above it for the
# badge, the floor under that room, the cover over the fixture's right side from
# above its title bar down, and clear of the badge's rectangle.
FIXTURE_AT = (60, 160)
FLOOR_GEOMETRY = (0, 0, 1280, 440)
COVER_OFFSET = (520, -40)
COVER_SIZE = (560, 480)
# The helper's badge rectangle around a window at (x, y): its `badge_area`.
BADGE_AREA = (34, 448, 114)
CYCLES = 20
RSS_WARM_CYCLE = 5
RSS_GROWTH_LIMIT_KB = 4096
COLOUR_TOLERANCE = 24
INSET = 6
# Bounded waits: tries x step.
WAIT_TRIES = 100
WAIT_STEP_S = 0.1
EVENT_WAIT_S = 3.0
QUIET_S = 1.0
STATE_SETTLE_S = 0.8
COMMAND_TIMEOUT_S = 10
# Shape's kind for the input region (shape.h).
SHAPE_INPUT = 2


def refuse_outside_a_container():
    if not any(os.path.exists(marker) for marker in CONTAINER_MARKERS):
        print("bound.py: runs only inside the fixture container", file=sys.stderr)
        sys.exit(2)


# --- the helper, with the frames it writes unasked -------------------------------------


class Session(Helper):
    """The helper on its pipe, keeping every `session_event` it writes between
    replies, so a badge press can be waited for."""

    def __init__(self, binary, stderr_path, env=None):
        super().__init__(binary, stderr_path, env)
        self.session_events = []

    def _next_reply(self, action, deadline):
        while True:
            frame = super()._next_reply(action, deadline)
            if frame.get("type") != "session_event":
                return frame
            self.session_events.append(frame)

    def wait_event(self, name, wait_s=EVENT_WAIT_S):
        """The first unclaimed `session_event` named `name`, waiting `wait_s` at most."""
        deadline = time.monotonic() + wait_s
        while True:
            for frame in self.session_events:
                if frame.get("event") == name and not frame.get("claimed"):
                    frame["claimed"] = True
                    return frame
            left = deadline - time.monotonic()
            if left <= 0:
                return None
            try:
                line = self.lines.get(timeout=left)
            except queue.Empty:
                return None
            if line is None:
                raise HelperFailed(f"the helper exited waiting for {name}")
            frame = json.loads(line)
            if frame.get("type") == "session_event":
                self.session_events.append(frame)

    def take_authority(self, frame):
        """A pause, a resume and a stop each mint a new authorization generation."""
        self.generations["authorization_generation"] = frame["authorization_generation"]


# --- the windows, through xdotool and the helper's own listing ---------------------------


def xdotool(*arguments, display=None):
    env = {**os.environ, "DISPLAY": display} if display else None
    done = subprocess.run(["xdotool", *map(str, arguments)], capture_output=True, text=True,
                          timeout=COMMAND_TIMEOUT_S, check=False, env=env)
    return done.stdout.split()


def window_id(title):
    found = xdotool("search", "--name", f"^{title}$")
    return found[0] if found else None


def listed(helper):
    _, reply = helper.ask("windows")
    return reply.get("windows") or []


def by_title(windows, title):
    return next((window for window in windows if window.get("title") == title), None)


def wait_listed(helper, title, present=True):
    """The window once the helper lists it (or `None` once it does not)."""
    for _ in range(WAIT_TRIES):
        window = by_title(listed(helper), title)
        if (window is not None) == present:
            return window
        time.sleep(WAIT_STEP_S)
    return None if present else by_title(listed(helper), title)


def region(window):
    r = window["region"]
    return (r["x"], r["y"], r["w"], r["h"])


def overlap(a, b):
    left, top = max(a[0], b[0]), max(a[1], b[1])
    right, bottom = min(a[0] + a[2], b[0] + b[2]), min(a[1] + a[3], b[1] + b[3])
    return (left, top, right - left, bottom - top) if right > left and bottom > top else None


def inside(outer, inner):
    return (inner[0] >= outer[0] and inner[1] >= outer[1]
            and inner[0] + inner[2] <= outer[0] + outer[2]
            and inner[1] + inner[3] <= outer[1] + outer[3])


def arrange(helper):
    """Floor at the back, the fixture over it, the cover in front; then where each
    is and the front-to-back order the helper lists them in."""
    fixture, floor, cover = window_id(FIXTURE), window_id(FLOOR), window_id(COVER)
    xdotool("windowsize", floor, FLOOR_GEOMETRY[2], FLOOR_GEOMETRY[3])
    xdotool("windowmove", "--sync", floor, FLOOR_GEOMETRY[0], FLOOR_GEOMETRY[1])
    xdotool("windowmove", "--sync", fixture, *FIXTURE_AT)
    xdotool("windowsize", cover, *COVER_SIZE)
    xdotool("windowmove", "--sync", cover, FIXTURE_AT[0] + COVER_OFFSET[0],
            FIXTURE_AT[1] + COVER_OFFSET[1])
    restack()
    time.sleep(QUIET_S)
    windows = listed(helper)
    order = [window.get("title") for window in windows]
    found = {title: by_title(windows, title) for title in (FIXTURE, FLOOR, COVER)}
    ranked = all(title in order for title in found)
    passed = ranked and order.index(COVER) < order.index(FIXTURE) < order.index(FLOOR)
    return found, {"flow": "arranged", "passed": passed,
                   "why": f"front to back {order}; "
                          f"{ {t: w and region(w) for t, w in found.items()} }"}


def restack():
    """The fixture over the floor and the cover over the fixture."""
    xdotool("windowraise", window_id(FIXTURE))
    xdotool("windowraise", window_id(COVER))
    time.sleep(QUIET_S)


# --- pictures -----------------------------------------------------------------------------


def colour_share(shot, area, colour):
    """The share of pixels in `area` (the sent image's own pixels) within tolerance
    of `colour`."""
    width, height, rows = png_rows(base64.b64decode(shot["data"]))
    left, top, w, h = area
    hits = total = 0
    want = ((colour >> 16) & 0xFF, (colour >> 8) & 0xFF, colour & 0xFF)
    for y in range(max(0, top), min(height, top + h)):
        for x in range(max(0, left), min(width, left + w), 2):
            got = rows[y][x * 4:x * 4 + 3]
            total += 1
            hits += max(abs(a - b) for a, b in zip(got, want, strict=True)) <= COLOUR_TOLERANCE
    return hits / total if total else 0.0, total


def scaled(area, origin, scale):
    """A root rectangle in an image's pixels, `INSET` in from each side."""
    x, y, w, h = area
    return (round((x - origin[0] + INSET) * scale), round((y - origin[1] + INSET) * scale),
            round((w - 2 * INSET) * scale), round((h - 2 * INSET) * scale))


def keep(out, name, shot):
    """The picture a step judged, beside the stage's report."""
    if shot.get("data"):
        with open(os.path.join(out, name), "wb") as handle:
            handle.write(base64.b64decode(shot["data"]))


def covered_capture_check(helper, target, fixture, cover, out):
    """The bound window's frame shows the window where the cover hides it; the
    display shows the cover there."""
    hidden = overlap(region(fixture), region(cover))
    if hidden is None:
        return {"flow": "covered window captured", "passed": False, "why": "nothing is covered"}
    order = [window.get("title") for window in listed(helper)]
    _, frame = helper.ask("screenshot", target_id=target)
    _, display = helper.ask("screenshot")
    keep(out, "covered-frame.png", frame)
    keep(out, "covered-display.png", display)
    if not frame.get("ok") or not display.get("ok"):
        return {"flow": "covered window captured", "passed": False,
                "why": f"frame {frame.get('error')}, display {display.get('error')}"}
    fx, fy, fw, _ = region(fixture)
    in_frame, read = colour_share(frame, scaled(hidden, (fx, fy), frame["width"] / fw), COVER_COLOUR)
    on_display, seen = colour_share(display, scaled(hidden, (0, 0),
                                    display["width"] / display["physical"]["width"]), COVER_COLOUR)
    passed = read > 1000 and in_frame == 0.0 and on_display > 0.95
    return {"flow": "covered window captured", "passed": passed,
            "why": f"cover over {hidden}: {in_frame:.3f} of {read} frame pixels are the cover's, "
                   f"{on_display:.3f} of {seen} display pixels are; front to back {order}; "
                   f"frame {frame['width']}x{frame['height']} {frame.get('target_id')}"}


def cover_pixels(shot, colour=COVER_COLOUR):
    """How many of an image's pixels are `colour`, the cover's unless named, every
    pixel looked at."""
    width, height, rows = png_rows(base64.b64decode(shot["data"]))
    want = ((colour >> 16) & 0xFF, (colour >> 8) & 0xFF, colour & 0xFF)
    found = 0
    for y in range(height):
        row = rows[y]
        for x in range(width):
            got = row[x * 4:x * 4 + 3]
            found += max(abs(a - b) for a, b in zip(got, want, strict=True)) <= COLOUR_TOLERANCE
    return found


def frame_border(display, client):
    """The window manager's frame around `client` (the root's child holding it)
    and that frame's border width."""
    x11, pointer = display.x11, ctypes.POINTER
    x11.XQueryTree.argtypes = [ctypes.c_void_p, ctypes.c_ulong, pointer(ctypes.c_ulong),
                               pointer(ctypes.c_ulong), pointer(ctypes.c_void_p),
                               pointer(ctypes.c_uint)]
    x11.XGetGeometry.argtypes = [ctypes.c_void_p, ctypes.c_ulong, pointer(ctypes.c_ulong),
                                 pointer(ctypes.c_int), pointer(ctypes.c_int),
                                 pointer(ctypes.c_uint), pointer(ctypes.c_uint),
                                 pointer(ctypes.c_uint), pointer(ctypes.c_uint)]
    window, root, parent = client, ctypes.c_ulong(), ctypes.c_ulong()
    for _ in range(WAIT_TRIES):
        kids, count = ctypes.c_void_p(), ctypes.c_uint()
        x11.XQueryTree(display.display, window, ctypes.byref(root), ctypes.byref(parent),
                       ctypes.byref(kids), ctypes.byref(count))
        if kids:
            x11.XFree(kids)
        if parent.value in (0, root.value):
            break
        window = parent.value
    at, size = (ctypes.c_int(), ctypes.c_int()), (ctypes.c_uint(), ctypes.c_uint())
    border, depth = ctypes.c_uint(), ctypes.c_uint()
    x11.XGetGeometry(display.display, window, ctypes.byref(root), ctypes.byref(at[0]),
                     ctypes.byref(at[1]), ctypes.byref(size[0]), ctypes.byref(size[1]),
                     ctypes.byref(border), ctypes.byref(depth))
    return window, border.value


def first_frame_check(helper, display, bound, fixture, cover, out):
    """The cover was over the fixture's title bar and frame, and its client, when
    the fixture was bound: the frame `select_target` answered holds none of the
    cover's colour, while the display shows the cover over the title bar."""
    flow = "a window over the target at the bind is not in its first frame"
    keep(out, "first-frame.png", bound)
    client = geometry(window_id(FIXTURE))
    fx, fy, fw, _ = region(fixture)
    title = overlap(region(cover), (fx, fy, fw, client[1] - fy))
    if title is None or not bound.get("data"):
        return {"flow": flow, "passed": False,
                "why": f"the cover {region(cover)} is not over the title bar above {client}"}
    tx, ty, tw, th = title
    band = (tx + INSET, ty + INSET, tw - 2 * INSET, th - 2 * INSET)
    # The cover on screen is read from the X server's root, past compux.
    on_display, seen = display.colour_share(band, COVER_COLOUR, COLOUR_TOLERANCE), band[2] * band[3]
    stray = cover_pixels(bound)
    frame, border = frame_border(display, int(window_id(FIXTURE)))
    passed = stray == 0 and seen > 0 and on_display > 0.95
    return {"flow": flow, "passed": passed,
            "why": f"{stray} of {bound['width']}x{bound['height']} first-frame pixels are the "
                   f"cover's; it hides the title bar at {title}, where {on_display:.3f} of {seen} "
                   f"display pixels are the cover's; frame {frame:#x} border {border}"}


# --- the badge, as the X server and /proc see it ---------------------------------------------


def badges(visible=True, display=None):
    """The badge windows there are: mapped ones only, or every one."""
    if visible:
        return xdotool("search", "--onlyvisible", "--classname", BADGE_CLASS, display=display)
    return xdotool("search", "--classname", BADGE_CLASS, display=display)


def geometry(window, display=None):
    fields = dict(line.split("=", 1)
                  for line in xdotool("getwindowgeometry", "--shell", window, display=display))
    return tuple(int(fields[key]) for key in ("X", "Y", "WIDTH", "HEIGHT"))


def children(pid):
    found = []
    for task in os.listdir(f"/proc/{pid}/task"):
        with open(f"/proc/{pid}/task/{task}/children", encoding="ascii") as handle:
            found.extend(int(child) for child in handle.read().split())
    return found


def badge_processes():
    """Every process whose executable is the badge, with its state letter."""
    found = []
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/cmdline", "rb") as handle:
                program = handle.read().split(b"\0")[0]
            with open(f"/proc/{entry}/stat", encoding="ascii") as handle:
                state = handle.read().rsplit(")", 1)[1].split()[0]
        except OSError:
            continue
        if program.endswith(b"compux-indicator-x11"):
            found.append((int(entry), state))
    return found


def resident_kb(pid):
    with open(f"/proc/{pid}/status", encoding="ascii") as handle:
        for line in handle:
            if line.startswith("VmRSS:"):
                return int(line.split()[1])
    return -1


def wait_for(predicate):
    for _ in range(WAIT_TRIES):
        if predicate():
            return True
        time.sleep(WAIT_STEP_S)
    return predicate()


def badge_check(display, helper, fixture):
    """One visible badge, the helper's child, inside the helper's rectangle, taking
    clicks on two rectangles of its own and nowhere else."""
    if not wait_for(lambda: len(badges()) == 1):
        return None, {"flow": "badge shown", "passed": False, "why": f"visible badges {badges()}"}
    window = int(badges()[0])
    place = geometry(window)
    owner = xdotool("getwindowpid", window)
    x, y, _, _ = region(fixture)
    area = (x, y - BADGE_AREA[0], BADGE_AREA[1], BADGE_AREA[2])
    rectangles = input_region(display, window)
    own = (0, 0, place[2], place[3])
    passed = (bool(owner) and int(owner[0]) in children(helper.proc.pid) and inside(area, place)
              and len(rectangles) == 2 and all(inside(own, r) for r in rectangles)
              and overlap(*rectangles) is None)
    badge = {"window": window, "place": place, "buttons": rectangles}
    return badge, {"flow": "badge shown", "passed": passed,
                   "why": f"badge {window} at {place} inside {area}: {inside(area, place)}; "
                          f"pid {owner} of children {children(helper.proc.pid)}; input {rectangles}"}


def person_click(x, y):
    """A click where the person would click: the core pointer, moved and pressed,
    in that order on one connection. (`mousemove --sync` waits for a motion that
    never comes when the pointer is already there.)"""
    xdotool("mousemove", x, y, "click", "1")


def button_centre(badge, index):
    bx, by, _, _ = badge["place"]
    x, y, w, h = badge["buttons"][index]
    return bx + x + w // 2, by + y + h // 2


def input_region(display, window):
    """The window's input region (Shape), as rectangles in its own pixels."""
    xext = ctypes.cdll.LoadLibrary(ctypes.util.find_library("Xext") or "libXext.so.6")

    class Rectangle(ctypes.Structure):
        _fields_ = [("x", ctypes.c_short), ("y", ctypes.c_short),
                    ("width", ctypes.c_ushort), ("height", ctypes.c_ushort)]

    xext.XShapeGetRectangles.restype = ctypes.POINTER(Rectangle)
    xext.XShapeGetRectangles.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int,
                                         ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_int)]
    count, ordering = ctypes.c_int(), ctypes.c_int()
    found = xext.XShapeGetRectangles(display.display, window, SHAPE_INPUT, ctypes.byref(count),
                                     ctypes.byref(ordering))
    rectangles = [(found[i].x, found[i].y, found[i].width, found[i].height)
                  for i in range(count.value)]
    if found:
        display.x11.XFree(ctypes.cast(found, ctypes.c_void_p))
    return rectangles


# --- acting in the bound window ------------------------------------------------------------


def presses(path):
    try:
        with open(path, encoding="utf-8") as handle:
            return json.load(handle).get("presses", [])
    except (OSError, ValueError):
        return []


def button_in(helper, target):
    """The fixture's button in the bound window's own view, and that listing."""
    listing = {}
    for _ in range(WAIT_TRIES // 4):
        _, listing = helper.ask("elements", target_id=target, deadline_ms=10000,
                                query={"role": "button", "name": "GTK3 button"})
        matches = [element for element in listing.get("elements", [])
                   if element.get("label") == "GTK3 button"]
        if listing.get("ok") and matches:
            return matches[0], listing
        time.sleep(WAIT_STEP_S * 4)
    return None, listing


def click_button(helper, target):
    """A click on the fixture's button through the binding: the reply, or the
    listing's refusal when the button could not be found."""
    element, listing = button_in(helper, target)
    if element is None:
        return {"ok": False, "error": listing.get("error"), "detail": listing.get("detail")}
    _, reply = helper.ask("left_click", mutation=True, observation_id=listing["observation_id"],
                          x=element["x"], y=element["y"], target_id=target)
    return reply


def target_click_check(helper, target, state):
    since = len(events(state))
    reply = click_button(helper, target)
    event = recorded(state, since, "gtk3", "button", "press", None)
    return {"flow": "click in the bound window", "passed": bool(reply.get("ok")) and event is not None,
            "why": f"reply {reply.get('error') or 'ok'} {reply.get('detail') or ''}, recorded {event}",
            "input_method": reply.get("receipt", {}).get("input_method")}


def popup_check(helper, target, state, display):
    """An override-redirect popup over the fixture's button: no window manager lists
    it, and a click there is refused `target_obstructed`, naming it, with nothing
    recorded. With its input region set to a corner clear of the point, the same
    click lands: the server's own input regions decide, not the popup's place."""
    flow = "a popup over the point refuses the click, and one taking no input there does not"
    element, _ = button_in(helper, target)
    fixture = by_title(listed(helper), FIXTURE)
    if element is None or fixture is None:
        return {"flow": flow, "passed": False, "why": f"button {element}, fixture {fixture}"}
    _, shot = helper.ask("screenshot", target_id=target)
    fx, fy, fw, _ = region(fixture)
    scale = shot["width"] / fw
    px, py = fx + round(element["x"] / scale), fy + round(element["y"] / scale)
    popup = Victim(POPUP, POPUP_COLOUR, place=(px - POPUP_SIZE[0] // 2, py - POPUP_SIZE[1] // 2),
                   override=True)
    popup.create()
    shown = wait_for(lambda: bool(xdotool("search", "--onlyvisible", "--name", f"^{POPUP}$")))
    time.sleep(QUIET_S)
    since = len(events(state))
    refused = click_button(helper, target)
    time.sleep(QUIET_S)
    arrived = events(state)[since:]
    display.shape_input(popup.id, [(0, 0, 10, 10)])
    since = len(events(state))
    landed = click_button(helper, target)
    event = recorded(state, since, "gtk3", "button", "press", None)
    popup.destroy()
    passed = (shown and refused.get("error") == "target_obstructed"
              and POPUP in (refused.get("detail") or "") and not arrived
              and bool(landed.get("ok")) and event is not None)
    return {"flow": flow, "passed": passed,
            "why": f"popup over ({px}, {py}) shown {shown}; refused {refused.get('error') or 'ok'}: "
                   f"{refused.get('detail')}; recorded under it {arrived}; with its input clear of "
                   f"the point {landed.get('error') or 'ok'}, recorded {event}"}


def obstructed_check(helper, target, state, fixture, cover, record):
    """A click at a point of the bound window that the cover hides is refused, and
    neither the fixture nor the cover was clicked."""
    # The click on the fixture's button before this raised it over the cover.
    restack()
    hx, hy, hw, hh = overlap(region(fixture), region(cover))
    fx, fy, fw, _ = region(fixture)
    order = [window.get("title") for window in listed(helper)]
    _, shot = helper.ask("screenshot", target_id=target)
    scale = shot["width"] / fw
    since, before = len(events(state)), len(presses(record))
    _, reply = helper.ask("left_click", mutation=True, observation_id=shot.get("observation_id"),
                          x=round((hx + hw / 2 - fx) * scale), y=round((hy + hh / 2 - fy) * scale),
                          target_id=target)
    time.sleep(QUIET_S)
    arrived, clicked = events(state)[since:], presses(record)[before:]
    passed = (order.index(COVER) < order.index(FIXTURE) and reply.get("error") == "target_obstructed"
              and not arrived and not clicked)
    return {"flow": "covered point refused", "passed": passed,
            "why": f"front to back {order}; error {reply.get('error')}: {reply.get('detail')}; "
                   f"fixture recorded {arrived}, cover recorded {clicked}",
            "reply": {key: value for key, value in reply.items() if key != "data"},
            "shot": {key: value for key, value in shot.items() if key != "data"}}


# --- the badge's buttons, pressed as the person presses them ----------------------------------


def focus():
    return xdotool("getwindowfocus")


def press_badge(helper, badge, index, expected):
    """Click badge button `index` as the person would; the session event it should
    raise, with its authority taken; and whether the keyboard focus stayed put."""
    before = focus()
    person_click(*button_centre(badge, index))
    event = helper.wait_event(expected)
    if event is not None:
        helper.take_authority(event)
    # The helper tells the badge the new state on its next look (four a second).
    time.sleep(STATE_SETTLE_S)
    return event, before == focus() and bool(before)


def pause_check(helper, target, state, badge):
    event, kept = press_badge(helper, badge, 0, "operator_pause")
    since = len(events(state))
    reply = click_button(helper, target)
    time.sleep(QUIET_S)
    arrived = events(state)[since:]
    passed = event is not None and kept and reply.get("error") == "paused" and not arrived
    return {"flow": "badge Pause reaches the gate", "passed": passed,
            "why": f"event {event and event.get('event')}, focus kept {kept}, next click "
                   f"{reply.get('error') or 'ok'}, recorded {arrived}"}


def resume_check(helper, target, state, badge):
    event, kept = press_badge(helper, badge, 0, "operator_resume")
    since = len(events(state))
    reply = click_button(helper, target)
    arrived = recorded(state, since, "gtk3", "button", "press", None)
    passed = event is not None and kept and bool(reply.get("ok")) and arrived is not None
    return {"flow": "badge Resume reaches the gate", "passed": passed,
            "why": f"event {event and event.get('event')}, focus kept {kept}, next click "
                   f"{reply.get('error') or 'ok'}, recorded {arrived}"}


def pass_through_check(helper, badge, record):
    """A click on the badge away from its buttons reaches the floor under it and
    raises nothing; the floor is then put back behind the fixture."""
    bx, by, _, bh = badge["place"]
    point = (bx + 30, by + bh // 2)
    on_button = any(inside((bx + x, by + y, w, h), (point[0], point[1], 1, 1))
                    for x, y, w, h in badge["buttons"])
    before = len(presses(record))
    person_click(*point)
    arrived = wait_for(lambda: len(presses(record)) > before)
    landed = presses(record)[before:]
    raised = [helper.wait_event(name, QUIET_S / 3)
              for name in ("operator_pause", "operator_resume", "operator_stop")]
    restack()
    back = wait_for(lambda: len(badges()) == 1)
    exact = bool(landed) and (landed[0]["x"], landed[0]["y"]) == point
    passed = not on_button and arrived and exact and not any(raised) and back
    return {"flow": "a click off the badge's buttons passes through", "passed": passed,
            "why": f"clicked {point} (on a button: {on_button}); floor recorded {landed}; "
                   f"events {[e and e.get('event') for e in raised]}; badge back {back}"}


def dropped(stderr_path):
    """How many sent button events the badge has said it dropped."""
    with open(stderr_path, encoding="utf-8", errors="replace") as handle:
        return sum("another program sent" in line for line in handle)


def forged_stop_check(helper, target, state, badge, stderr_path):
    """A press and release on Stop sent by another program (`xdotool click
    --window` sends them with SendEvent, the pointer resting on Stop) stop
    nothing: no event, the next click lands, and the badge says it dropped both.
    The person's click on Stop after this is the control that the same point
    does reach the gate."""
    xdotool("mousemove", *button_centre(badge, 1))
    before = dropped(stderr_path)
    xdotool("click", "--window", badge["window"], "1")
    event = helper.wait_event("operator_stop", EVENT_WAIT_S)
    if event is not None:
        helper.take_authority(event)
    since = len(events(state))
    reply = click_button(helper, target)
    arrived = recorded(state, since, "gtk3", "button", "press", None)
    count = dropped(stderr_path) - before
    passed = event is None and bool(reply.get("ok")) and arrived is not None and count == 2
    return {"flow": "a click on Stop another program sends is dropped", "passed": passed,
            "why": f"event {event and event.get('event')}; next click {reply.get('error') or 'ok'}, "
                   f"recorded {arrived}; the badge dropped {count} sent event(s)"}


def stop_check(helper, target, state, badge):
    """Stop reaches the gate and holds the next click; Resume then reopens it, so
    the stages after this one can act."""
    event, kept = press_badge(helper, badge, 1, "operator_stop")
    since = len(events(state))
    reply = click_button(helper, target)
    time.sleep(QUIET_S)
    arrived = events(state)[since:]
    resumed, _ = press_badge(helper, badge, 0, "operator_resume")
    passed = (event is not None and kept and reply.get("error") == "paused" and not arrived
              and resumed is not None)
    return {"flow": "badge Stop reaches the gate", "passed": passed,
            "why": f"event {event and event.get('event')}, focus kept {kept}, next click "
                   f"{reply.get('error') or 'ok'}, recorded {arrived}; resumed after "
                   f"{resumed and resumed.get('event')}"}


def release_check(helper):
    _, reply = helper.ask("release_target")
    gone = wait_for(lambda: not badge_processes() and not badges(visible=False))
    passed = bool(reply.get("ok")) and reply.get("released") is True and gone
    return {"flow": "release takes the badge down", "passed": passed,
            "why": f"reply {reply}, badge processes {badge_processes()}, "
                   f"badge windows {badges(visible=False)}"}


# --- a reused window id ---------------------------------------------------------------------


def xid_reuse_check(helper):
    """Bind a window, close it, make another under the same id: the binding stays
    gone, for a capture and for a click alike."""
    victim = Victim(VICTIM, VICTIM_COLOUR)
    victim.create()
    first = wait_listed(helper, VICTIM)
    shown = wait_for(lambda: bool(xdotool("search", "--onlyvisible", "--name", f"^{VICTIM}$")))
    if first is None or not shown:
        return {"flow": "a reused window id never revives a binding", "passed": False,
                "why": f"the victim window was listed {first is not None}, shown {shown}"}
    _, bound = helper.ask("select_target", window_id=first["id"])
    _, before = helper.ask("screenshot", target_id=bound.get("target_id"))
    victim.destroy()
    closed = wait_listed(helper, VICTIM, present=False) is None
    victim.create()
    again = wait_listed(helper, VICTIM)
    wait_for(lambda: bool(xdotool("search", "--onlyvisible", "--name", f"^{VICTIM}$")))
    same = again is not None and again["id"] == first["id"] == victim.id
    owner = xdotool("getwindowpid", victim.id)
    _, shot = helper.ask("screenshot", target_id=bound.get("target_id"))
    # The newest picture of the binding is the one before the close.
    _, click = helper.ask("left_click", mutation=True, observation_id=before.get("observation_id"),
                          x=20, y=20, target_id=bound.get("target_id"))
    helper.ask("release_target")
    victim.destroy()
    passed = (bool(bound.get("ok")) and bool(before.get("ok")) and closed and same
              and owner == [str(os.getpid())]
              and shot.get("error") == "target_unavailable"
              and click.get("error") == "target_unavailable")
    shot = {key: value for key, value in shot.items() if key != "data"}
    return {"flow": "a reused window id never revives a binding", "passed": passed,
            "why": f"bound {bound.get('ok')} {bound.get('error') or ''}; captured before the close "
                   f"{before.get('ok')}; closed {closed}; same id "
                   f"{same} ({victim.id:#x}, pid {owner}); capture {shot.get('error')}: "
                   f"{shot.get('detail')}; click {click.get('error')}: {click.get('detail')}",
            "capture_after": shot}


# --- a window that never repaints -------------------------------------------------------------


def lazy_window_check(helper):
    """A window that paints once and never again, bound while the cover hides part
    of it: nothing after the bind ever draws its picture, so the binding is refused
    `capture_unavailable` and no frame of it, the cover's pixels and all, is sent."""
    flow = "a window that never repaints is refused, never sent"
    lazy = Victim(LAZY, LAZY_COLOUR, lazy=True, place=LAZY_AT)
    lazy.create()
    shown = wait_for(lambda: bool(xdotool("search", "--onlyvisible", "--name", f"^{LAZY}$")))
    # The window manager places a window that asks for no place where it likes.
    if shown:
        xdotool("windowmove", "--sync", window_id(LAZY), *LAZY_AT)
    xdotool("windowraise", window_id(COVER))
    time.sleep(QUIET_S)
    listed_lazy = wait_listed(helper, LAZY)
    under = overlap(region(listed_lazy), region(by_title(listed(helper), COVER))) if listed_lazy else None
    if listed_lazy is None or not shown or under is None:
        lazy.destroy()
        return {"flow": flow, "passed": False,
                "why": f"the lazy window was listed {listed_lazy is not None}, shown {shown}, "
                       f"under the cover {under}"}
    _, bound = helper.ask("select_target", window_id=listed_lazy["id"])
    stray = cover_pixels(bound) if bound.get("data") else 0
    helper.ask("release_target")
    lazy.destroy()
    passed = (bound.get("error") == "capture_unavailable" and "not drawn" in (bound.get("detail") or "")
              and "data" not in bound)
    return {"flow": flow, "passed": passed,
            "why": f"cover over {under}; select {bound.get('error') or 'ok'}: {bound.get('detail')}; "
                   f"{stray} cover pixels in what it sent"}


# --- a window with an X border ---------------------------------------------------------------


def bare_display():
    """A second private X server with no window manager on it: a top-level window
    there keeps the X border its client gave it, which openbox takes off."""
    socket = f"/tmp/.X11-unix/X{BARE_DISPLAY}"
    server = subprocess.Popen(["Xvfb", f":{BARE_DISPLAY}", "-screen", "0", "1280x1024x24",
                               "-nolisten", "tcp"],
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if not wait_for(lambda: os.path.exists(socket)):
        stop(server)
        raise OSError(f"the bare X server never opened {socket}")
    return server


def stop(process):
    process.terminate()
    try:
        process.wait(timeout=COMMAND_TIMEOUT_S)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait()


def all_window(shot, colour):
    """Whether a frame is the bordered window's own picture: its size, and every
    pixel `colour`, none the border's."""
    if not shot.get("data"):
        return False, f"{shot.get('error')}: {shot.get('detail')}"
    size = (shot["width"], shot["height"])
    own, border = cover_pixels(shot, colour), cover_pixels(shot, BORDER_COLOUR)
    whole = size == xcb_victim.SIZE and own == size[0] * size[1] and border == 0
    return whole, f"{size[0]}x{size[1]}, {own} pixels the window's, {border} the border's"


def bare_steps(binary, out):
    """The steps on a second X server with no window manager, with a helper of
    their own."""
    server = bare_display()
    helper = None
    display = f":{BARE_DISPLAY}"
    try:
        helper = Session(binary, out + ".bare.stderr", env={**os.environ, "DISPLAY": display})
        helper.handshake()
        with x11_oracle.Display(display) as oracle:
            return [bordered_window_check(helper, display, out),
                    badge_restack_check(helper, display, oracle),
                    badge_held_check(helper, display, oracle)]
    finally:
        if helper is not None:
            helper.close()
        stop(server)


def bordered_window_check(helper, display, out):
    """A top-level window with an X border of its own, bound on a display with no
    window manager: its frames hold the window's pixels and none of its border's,
    at the border it was bound with, and after it is given another. It takes a new
    colour with the new border, so the frame after is one read since: the binding's
    first frame served again would still be the old colour. An observation id
    cannot show that, since every reply mints a new one."""
    flow = "a window with an X border is read from inside it"
    victim = Victim(BORDERED, BORDERED_COLOUR, place=BORDERED_AT, display=display,
                    border=(BORDER_WIDTHS[0], BORDER_COLOUR))
    victim.create()
    victim.listed_on_root()
    try:
        window = wait_listed(helper, BORDERED)
        if window is None:
            return {"flow": flow, "passed": False, "why": "the bordered window was never listed"}
        _, bound = helper.ask("select_target", window_id=window["id"])
        first, first_why = all_window(bound, BORDERED_COLOUR)
        victim.set_colour(RECOLOURED)
        victim.set_border(BORDER_WIDTHS[1])
        time.sleep(QUIET_S)
        _, after = helper.ask("screenshot", target_id=bound.get("target_id"))
        again, again_why = all_window(after, RECOLOURED)
        helper.ask("release_target")
        keep(os.path.dirname(out), "bordered-first.png", bound)
        keep(os.path.dirname(out), "bordered-after.png", after)
        return {"flow": flow, "passed": first and again,
                "why": f"border {BORDER_WIDTHS[0]}: {first_why}; border {BORDER_WIDTHS[1]}, "
                       f"recoloured: {again_why}; observations {bound.get('observation_id')} "
                       f"then {after.get('observation_id')}"}
    finally:
        victim.destroy()


def badge_restack_check(helper, display, oracle):
    """A bound window at the top of the screen, the badge inside its top edge. A
    popup mapped afterwards (the person's menu, as far as the server knows) stays
    over the badge. The bound window raised to the very top, over the badge, has the
    badge back over it. Judged by the server's stacking order."""
    flow = "the badge stays under a popup, and goes back over its window raised onto it"
    raised = Victim(RAISED, RAISED_COLOUR, place=RAISED_AT, display=display)
    raised.create()
    raised.listed_on_root()
    popup = Victim(POPUP, POPUP_COLOUR, place=POPUP_AWAY, display=display, override=True)
    try:
        window = wait_listed(helper, RAISED)
        _, bound = helper.ask("select_target", window_id=window["id"]) if window else (None, {})
        shown = wait_for(lambda: len(badges(display=display)) == 1)
        if not bound.get("ok") or not shown:
            return {"flow": flow, "passed": False,
                    "why": f"select {bound.get('error') or 'ok'} {bound.get('detail')}; badge {shown}"}
        badge = int(badges(display=display)[0])
        meets = overlap(geometry(badge, display), region(window))
        popup.create()
        time.sleep(QUIET_S)
        before = oracle.stacking()
        raised.raise_to_top()
        time.sleep(QUIET_S)
        after = oracle.stacking()
        helper.ask("release_target")
        under = before.index(popup.id) > before.index(badge)
        over = after.index(badge) > after.index(raised.id)
        return {"flow": flow, "passed": meets is not None and under and over,
                "why": f"badge {badge:#x} meets the window at {meets}; popup over the badge {under} "
                       f"(bottom to top {[hex(w) for w in before]}); badge over the raised window "
                       f"{over} ({[hex(w) for w in after]})"}
    finally:
        popup.destroy()
        raised.destroy()


def badge_held_check(helper, display, oracle):
    """A bound window at the top of the screen, the badge inside its top edge, and
    a popup (the person's menu, as far as the server knows) opened over the badge.
    The window nudged right has the helper send the badge a new place, and the
    badge goes there under the popup; the window raised to the very top, over the
    badge, does not bring the badge over the popup either. Once the popup closes,
    the badge is back over the window. Judged by the server's stacking order and
    the badge's place."""
    flow = "the badge stays under a popup open over it through a new state, and is back on top once it closes"
    held = Victim(HELD, HELD_COLOUR, place=HELD_AT, display=display)
    held.create()
    held.listed_on_root()
    popup, closed = None, False
    try:
        window = wait_listed(helper, HELD)
        _, bound = helper.ask("select_target", window_id=window["id"]) if window else (None, {})
        shown = wait_for(lambda: len(badges(display=display)) == 1)
        if not bound.get("ok") or not shown:
            return {"flow": flow, "passed": False,
                    "why": f"select {bound.get('error') or 'ok'} {bound.get('detail')}; badge {shown}"}
        badge = int(badges(display=display)[0])
        x, y, _, _ = geometry(badge, display)
        popup = Victim(POPUP, POPUP_COLOUR, place=(x - POPUP_MARGIN, y - POPUP_MARGIN),
                       display=display, override=True)
        popup.create()
        time.sleep(QUIET_S)
        held.move_to(HELD_AT[0] + NUDGE, HELD_AT[1])
        moved = wait_for(lambda: geometry(badge, display)[0] == x + NUDGE)
        time.sleep(QUIET_S)
        after_state = oracle.stacking()
        held.raise_to_top()
        time.sleep(QUIET_S)
        after_raise = oracle.stacking()
        popup.destroy()
        closed = True
        time.sleep(QUIET_S)
        after_close = oracle.stacking()
        helper.ask("release_target")
        under_state = after_state.index(popup.id) > after_state.index(badge)
        under_raise = after_raise.index(popup.id) > after_raise.index(badge)
        back = after_close.index(badge) > after_close.index(held.id)
        return {"flow": flow, "passed": moved and under_state and under_raise and back,
                "why": f"badge {badge:#x} moved to the nudged window {moved}; under the popup after "
                       f"the new state {under_state} ({[hex(w) for w in after_state]}), after the "
                       f"window was raised {under_raise} ({[hex(w) for w in after_raise]}); over the "
                       f"window once the popup closed {back} ({[hex(w) for w in after_close]})"}
    finally:
        if popup is not None and not closed:
            popup.destroy()
        held.destroy()


# --- twenty cycles ----------------------------------------------------------------------------


def cycles_check(helper, fixture):
    """Select and release, CYCLES times: each badge comes up and goes, nothing is
    left, and the helper's resident memory stays flat after the first few."""
    series, failed = [], []
    for cycle in range(1, CYCLES + 1):
        _, bound = helper.ask("select_target", window_id=fixture["id"])
        shown = bool(bound.get("ok")) and wait_for(lambda: len(badges()) == 1)
        _, released = helper.ask("release_target")
        gone = wait_for(lambda: not badge_processes() and not badges(visible=False))
        series.append(resident_kb(helper.proc.pid))
        if not (shown and released.get("ok") and gone):
            failed.append({"cycle": cycle, "bound": bound.get("error") or "ok", "shown": shown,
                           "gone": gone})
    left = children(helper.proc.pid)
    growth = series[-1] - series[RSS_WARM_CYCLE - 1]
    passed = not failed and not left and not badge_processes() and growth <= RSS_GROWTH_LIMIT_KB
    return {"flow": f"{CYCLES} select and release cycles", "passed": passed,
            "why": f"failed {failed}; children left {left}; badge processes {badge_processes()}; "
                   f"resident kB {series} (growth after cycle {RSS_WARM_CYCLE}: {growth})"}


# --- the run ----------------------------------------------------------------------------------


def hello_check(hello):
    actions = hello.get("actions") or []
    capabilities = hello.get("capabilities") or {}
    passed = ("select_target" in actions and "release_target" in actions
              and capabilities.get("targets") is True
              and "window" in (capabilities.get("capture_methods") or []))
    return {"flow": "hello offers bound windows", "passed": passed,
            "why": f"targets {capabilities.get('targets')}, capture_methods "
                   f"{capabilities.get('capture_methods')}, indicator {capabilities.get('indicator')}"}


def unscaled_check(helper):
    """Every rectangle here is read as root pixels: the display is sent unscaled."""
    _, shot = helper.ask("screenshot")
    passed = bool(shot.get("ok")) and shot.get("width") == shot.get("physical", {}).get("width")
    return {"flow": "display sent unscaled", "passed": passed,
            "why": f"sent {shot.get('width')}x{shot.get('height')} of {shot.get('physical')}"}


def start_panes(records):
    started = []
    for title, colour in ((FLOOR, FLOOR_COLOUR), (COVER, COVER_COLOUR)):
        record = records[title]
        started.append(subprocess.Popen(
            [sys.executable, os.path.join(os.path.dirname(__file__), "panes.py"),
             "--title", title, "--colour", f"{colour:06x}", "--record", record],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
    return started


def bound_steps(helper, display, args, records):
    """Every step on the fixture's binding, appended as decided."""
    found, arranged = arrange(helper)
    yield arranged
    if not arranged["passed"]:
        return
    fixture, cover = found[FIXTURE], found[COVER]
    _, bound = helper.ask("select_target", window_id=fixture["id"])
    yield {"flow": "select the fixture", "passed": bool(bound.get("ok")),
           "why": f"{bound.get('error') or 'ok'} {bound.get('detail') or ''}; ax_binding "
                  f"{bound.get('ax_binding')}, methods {bound.get('methods')}"}
    if not bound.get("ok"):
        return
    target = bound["target_id"]
    yield first_frame_check(helper, display, bound, fixture, cover, os.path.dirname(args.out))
    yield covered_capture_check(helper, target, fixture, cover, os.path.dirname(args.out))
    badge, shown = badge_check(display, helper, fixture)
    yield shown
    yield target_click_check(helper, target, args.state)
    yield popup_check(helper, target, args.state, display)
    yield obstructed_check(helper, target, args.state, fixture, cover, records[COVER])
    if badge is None:
        return
    yield pause_check(helper, target, args.state, badge)
    yield resume_check(helper, target, args.state, badge)
    yield pass_through_check(helper, badge, records[FLOOR])
    yield forged_stop_check(helper, target, args.state, badge, args.out + ".stderr")
    yield stop_check(helper, target, args.state, badge)
    yield release_check(helper)


def run(args):
    out = os.path.dirname(args.out)
    records = {FLOOR: os.path.join(out, "floor-presses.json"),
               COVER: os.path.join(out, "cover-presses.json")}
    panes, results = [], []
    helper = Session(args.binary, args.out + ".stderr")
    try:
        with x11_oracle.Display() as display:
            results.append(hello_check(helper.handshake()))
            results.append(unscaled_check(helper))
            # Listed while it is the only window, and so the active one: a pane
            # mapped before it is listed takes the focus, and the active
            # application's tree is then the pane's.
            results.append(fixture_listed(helper, "gtk3"))
            panes = start_panes(records)
            for title in (FLOOR, COVER):
                results.append({"flow": f"{title} listed", "passed": wait_listed(helper, title) is not None,
                                "why": title})
            if all(result.get("passed") for result in results):
                results.extend(bound_steps(helper, display, args, records))
                results.append(xid_reuse_check(helper))
                results.append(lazy_window_check(helper))
                results.append(cycles_check(helper, by_title(listed(helper), FIXTURE)))
                results.extend(bare_steps(args.binary, args.out))
    except (HelperFailed, subprocess.SubprocessError, OSError, KeyError, ValueError) as why:
        results.append({"flow": "the run", "passed": False, "why": f"{type(why).__name__}: {why}"})
    finally:
        helper.close()
        for pane in panes:
            pane.terminate()
        for pane in panes:
            try:
                pane.wait(timeout=COMMAND_TIMEOUT_S)
            except subprocess.TimeoutExpired:
                pane.kill()
                pane.wait()
    return {"steps": results, "events": events(args.state),
            "session_events": helper.session_events,
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
    print(f"BOUND gtk3: {'PASS' if result['passed'] else 'FAIL'}")
    sys.exit(0 if result["passed"] else 1)


if __name__ == "__main__":
    main()
