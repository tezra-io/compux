#!/usr/bin/env python3
"""Computer history's recorder on Linux, against one fixture, judged by the frames
it emits (M97 stage 4).

Runs INSIDE the private-session container only (`spike.sh` stages `observe`,
`observe-jammy` and `observe-chromium` start it), never on a desktop someone uses:
it starts compux helpers that read and write the session's accessibility bus. It
refuses (exit 2) anywhere without a container's marker file, before it starts
anything.

  python3 observe.py --binary /opt/compux/compux --toolkit gtk3 \\
      --out /tmp/spike-out/gtk3-observe.json

Two helpers. The RECORDER is started with `observe_start` and only listened to.
The DRIVER acts on the fixture through compux's own verbs (`focus`, `set_value`,
`key`), so compux is all that drives the fixture and the recorder drives nothing.

1. Not listed: the recorder observes with an allowlist that does not name the
   fixture, the driver focuses and fills its text field, and the recorder must
   emit no frame at all.
2. Listed, by the fixture's own name on the accessibility bus (the Name of its
   root, read here with raw D-Bus calls, never through compux):
   a. the fixture's active window is announced (`app.activated`, then
      `window.focused` with its title);
   b. where the toolkit takes a focus by reference (GTK 3, Qt), the text field is
      focused and set: `focus.changed` names it and `field.value` carries the text;
      GTK 4 refuses `GrabFocus`, so there Tabs move the focus, from the button to a
      field whose text is hidden and then to libadwaita's password row (where the
      image's libadwaita has one), and a secret is typed into each: GTK 4 before
      4.20 marks neither as a password, so each must say its count alone, withheld,
      and neither secret may be in any frame;
   c. the secure field is focused and set: `gap{secure_input}` and a withheld
      `field.value`, and the secret is in no frame;
   d. in Chromium the text field is set and left: its `field.value` is withheld
      (`private_state` unknown, its count only, the browser named) with one
      `gap{private_unknown}`, and its text is in no frame;
   e. `observe_stop` is acked.

Exit 0 when every step met its expectation, 1 otherwise; the JSON has each step's
frames and verdict.
"""

import argparse
import json
import os
import queue
import subprocess
import sys
import threading
import time

from gi.repository import Gio, GLib

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from verbs import Helper, HelperFailed, find  # noqa: E402

CONTAINER_MARKERS = ("/.dockerenv", "/run/.containerenv")
ACK_TIMEOUT_S = 15
# Bounded waits for a frame: tries x step.
FRAME_TRIES = 60
FRAME_STEP_S = 0.1
# Long enough for the recorder's value debounce (0.6 s) to settle.
SETTLE_S = 1.5
MAX_PENDING_LINES = 10000
CALL_TIMEOUT_MS = 2000
NAME_TRIES = 50
NAME_STEP_S = 0.2

LABELS = {"gtk3": "GTK3", "gtk4": "GTK4", "qt": "Qt", "chromium": "Chromium"}
SECRET = "s3cret-value"
# What is typed into GTK 4's hidden field and its password row.
HIDDEN_SECRET = "hidden-s3cret"
ROW_SECRET = "row-s3cret"
TYPED = "groceries list"
# What the not-listed phase types: never TYPED, since a toolkit sends no text
# change for a field set to the text it already holds (GTK 3 does not).
UNLISTED_TYPED = "nothing to record"
NOT_LISTED = "Nothing Running By This Name"
REGISTRY = ("org.a11y.atspi.Registry", "/org/a11y/atspi/accessible/root")
ACCESSIBLE = "org.a11y.atspi.Accessible"


def refuse_outside_a_container():
    if not any(os.path.exists(marker) for marker in CONTAINER_MARKERS):
        print("observe.py: runs only inside the fixture container", file=sys.stderr)
        sys.exit(2)


# --- the fixture's name on the bus, read with raw calls --------------------------


def call(bus, name, path, interface, method, args, reply):
    result = bus.call_sync(name, path, interface, method, args,
                           GLib.VariantType.new(reply), Gio.DBusCallFlags.NONE,
                           CALL_TIMEOUT_MS, None)
    return result.unpack()


def accessibility_bus():
    session = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    (address,) = call(session, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus",
                      "GetAddress", None, "(s)")
    flags = (Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT
             | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION)
    return Gio.DBusConnection.new_for_address_sync(address, flags, None, None)


def name_of(bus, name, path):
    (value,) = call(bus, name, path, "org.freedesktop.DBus.Properties", "Get",
                    GLib.Variant("(ss)", (ACCESSIBLE, "Name")), "(v)")
    return value


def application_named_by_window(window):
    """The Name of the root of the application one of whose windows is named
    `window` (or starts with it), waited for a bounded time."""
    bus = accessibility_bus()
    for _ in range(NAME_TRIES):
        (applications,) = call(bus, *REGISTRY, ACCESSIBLE, "GetChildren", None, "(a(so))")
        for name, path in applications:
            try:
                (windows,) = call(bus, name, path, ACCESSIBLE, "GetChildren", None, "(a(so))")
                titles = [name_of(bus, name, child) for _bus, child in windows]
            except GLib.Error:
                continue
            if any(title.startswith(window) for title in titles):
                return name_of(bus, name, path)
        time.sleep(NAME_STEP_S)
    return None


# --- the recorder ------------------------------------------------------------------


class Recorder:
    """A compux helper in capture mode: `observe_start`, then every frame it pushes,
    read on a thread into a bounded queue."""

    def __init__(self, binary, stderr_path):
        self.stderr = open(stderr_path, "w")
        self.proc = subprocess.Popen([binary], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=self.stderr, text=True, bufsize=1)
        self.lines = queue.Queue(maxsize=MAX_PENDING_LINES)
        self.frames = []
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.proc.stdout:
            self.lines.put(line)
        self.lines.put(None)

    def _drain(self, wait_s):
        deadline = time.monotonic() + wait_s
        while True:
            left = deadline - time.monotonic()
            try:
                line = self.lines.get(timeout=max(left, 0.01)) if left > 0 else self.lines.get_nowait()
            except queue.Empty:
                return
            if line is None:
                raise HelperFailed("the recorder exited")
            try:
                self.frames.append(json.loads(line))
            except ValueError:
                self.frames.append({"unreadable": line[:200]})

    def control(self, action, params=None):
        request = {"type": "request", "request_id": f"o-{action}", "action": action}
        if params is not None:
            request["params"] = params
        self.proc.stdin.write(json.dumps(request) + "\n")
        self.proc.stdin.flush()
        for _ in range(int(ACK_TIMEOUT_S / FRAME_STEP_S)):
            self._drain(FRAME_STEP_S)
            for frame in self.frames:
                if frame.get("type") == "ack" and frame.get("action") == action:
                    self.frames.remove(frame)
                    return frame
        raise HelperFailed(f"no ack to {action} in {ACK_TIMEOUT_S} s")

    def events(self):
        self._drain(0)
        return [frame for frame in self.frames if frame.get("type") == "event"]

    def wait_for(self, matches):
        """The first event frame `matches`, waiting a bounded time."""
        for _ in range(FRAME_TRIES):
            for frame in self.events():
                if matches(frame):
                    return frame
            time.sleep(FRAME_STEP_S)
        return None

    def close(self):
        try:
            self.proc.stdin.close()
        except BrokenPipeError:
            print("the recorder exited before its input was closed", file=sys.stderr)
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        self.stderr.close()


def kind(name, **fields):
    return lambda frame: frame.get("kind") == name and all(
        frame.get(key) == value for key, value in fields.items())


def gap(reason):
    return lambda frame: frame.get("kind") == "observer.gap" and frame.get("gap_reason") == reason


def nowhere(recorder, *secrets):
    """No frame the recorder wrote holds any of `secrets`."""
    text = json.dumps(recorder.frames)
    return not any(secret in text for secret in secrets)


# --- driving the fixture through compux -----------------------------------------------


def focus_and_set(driver, name, value, takes_focus, by_keys=False):
    """Focus the text field named `name` (by reference, where the toolkit takes it)
    and set its value, or, `by_keys`, type it at the focus through the session's
    X11 route (Chromium's inputs say their value is not settable). The replies, for
    the record. Looked up as a text field, so a label of the same name (Chromium's)
    is never taken for it."""
    element, observation, listing = find(driver, "textfield", name)
    if element is None:
        return {"listed": False, "listing": json.dumps(listing)[:1500] if listing else None}
    replies = {}
    if takes_focus:
        _, replies["focus"] = driver.ask("focus", mutation=True, observation_id=observation,
                                         element_ref=element["element_ref"])
    if by_keys:
        _, replies["type"] = driver.ask("type", mutation=True, text=value)
        return {"listed": True, "replies": replies}
    element, observation, _ = find(driver, "textfield", name)
    _, replies["set_value"] = driver.ask("set_value", mutation=True, observation_id=observation,
                                         element_ref=element["element_ref"], value=value)
    return {"listed": True, "replies": replies}


def focus_away(driver, name):
    element, observation, _ = find(driver, None, name)
    if element is None:
        return {"listed": False}
    _, reply = driver.ask("focus", mutation=True, observation_id=observation,
                          element_ref=element["element_ref"])
    return {"listed": True, "focus": reply}


# --- the steps -----------------------------------------------------------------------


def not_listed_step(args, driver, label):
    recorder = Recorder(args.binary, args.out + ".not-listed.stderr")
    try:
        ack = recorder.control("observe_start", {"apps": [NOT_LISTED]})
        acted = focus_and_set(driver, f"{label} text field", UNLISTED_TYPED,
                              args.toolkit != "gtk4")
        time.sleep(SETTLE_S)
        events = recorder.events()
        stop = recorder.control("observe_stop")
    finally:
        recorder.close()
    passed = ack.get("ok") is True and events == [] and stop.get("ok") is True
    return {"step": "not listed: nothing at all", "ack": ack, "acted": acted,
            "frames": events, "passed": passed,
            "why": f"ack {ack.get('ok')}, {len(events)} frames"}


def listed_steps(args, driver, label, name):
    recorder = Recorder(args.binary, args.out + ".listed.stderr")
    results = []
    try:
        ack = recorder.control("observe_start", {"apps": [name]})
        results.append({"step": "observe_start acked", "ack": ack, "passed": ack.get("ok") is True,
                        "why": f"ack {ack}"})
        results.append(announced_step(recorder, name))
        if args.toolkit == "chromium":
            results.append(browser_step(recorder, driver, label, name))
        elif args.toolkit == "gtk4":
            results.append(gtk4_hidden_step(recorder, driver, label))
        else:
            results.append(typed_step(recorder, driver, label))
        if args.toolkit != "gtk4":
            results.append(secure_step(recorder, driver, label, args.toolkit))
        stop = recorder.control("observe_stop")
        results.append({"step": "observe_stop acked", "passed": stop.get("ok") is True,
                        "why": f"ack {stop}"})
    finally:
        recorder.close()
    results.append({"step": "every frame names the fixture or is a gap",
                    "passed": all(frame.get("app", {}).get("bundle_id") in (None, name)
                                  for frame in recorder.frames if frame.get("type") == "event"),
                    "frames": recorder.frames})
    return results


def announced_step(recorder, name):
    activated = recorder.wait_for(kind("app.activated"))
    focused = recorder.wait_for(kind("window.focused"))
    passed = (activated is not None and activated["app"]["bundle_id"] == name
              and focused is not None and bool(focused.get("window_title")))
    return {"step": "the active window is announced", "passed": passed,
            "why": f"app.activated {activated}, window.focused {focused}"}


def typed_step(recorder, driver, label):
    field = f"{label} text field"
    acted = focus_and_set(driver, field, TYPED, True)
    focused = recorder.wait_for(kind("focus.changed", field_label=field))
    value = recorder.wait_for(kind("field.value", text=TYPED))
    passed = focused is not None and value is not None and value.get("content_withheld") is False
    return {"step": "a typed value is recorded", "acted": acted, "passed": passed,
            "why": f"focus.changed {focused}, field.value {value}"}


def has_password_row():
    """Whether the image's libadwaita, the fixture's, has a PasswordEntryRow."""
    import gi
    gi.require_version("Adw", "1")
    from gi.repository import Adw
    return hasattr(Adw, "PasswordEntryRow")


def labelled(name, label):
    return lambda frame: kind(name)(frame) and frame.get("field_label") == label


def gtk4_hidden_step(recorder, driver, label):
    """GTK 4 refuses GrabFocus: Tabs sent through the driver move the focus from the
    button to the hidden field, then to the password row, and a secret is typed at
    each through the session's X11 route. Before 4.20 GTK 4 marks neither as a
    password, so the recorder must count them and read neither."""
    field = f"{label} hidden field"
    replies = {}
    _, replies["tab"] = driver.ask("key", mutation=True, chord="tab")
    focused = recorder.wait_for(labelled("focus.changed", field))
    _, replies["type"] = driver.ask("type", mutation=True, text=HIDDEN_SECRET)
    _, replies["tab_to_row"] = driver.ask("key", mutation=True, chord="tab")
    value = recorder.wait_for(labelled("field.value", field))
    rowed = has_password_row()
    row_value = None
    if rowed:
        _, replies["type_row"] = driver.ask("type", mutation=True, text=ROW_SECRET)
        _, replies["tab_from_row"] = driver.ask("key", mutation=True, chord="tab")
        row_value = recorder.wait_for(
            lambda f: kind("field.value")(f) and f.get("field_label") != field)
    time.sleep(SETTLE_S)
    counted = (value is not None and value.get("content_withheld") is True
               and "text" not in value and value.get("char_len") == len(HIDDEN_SECRET))
    passed = focused is not None and counted and nowhere(recorder, HIDDEN_SECRET, ROW_SECRET)
    return {"step": "GTK 4's unmarked password fields are never read", "keys": replies,
            "passed": passed, "password_row": rowed,
            "why": f"focus.changed {focused}, field.value {value}, row's {row_value}, "
                   f"secrets absent {nowhere(recorder, HIDDEN_SECRET, ROW_SECRET)}"}


def secure_step(recorder, driver, label, toolkit):
    field = f"{label} secure field"
    acted = focus_and_set(driver, field, SECRET, True, by_keys=toolkit == "chromium")
    if toolkit == "chromium":
        focus_away(driver, f"{label} button")
    secure = recorder.wait_for(gap("secure_input"))
    value = recorder.wait_for(lambda f: kind("field.value")(f)
                              and f.get("role") == "password text")
    time.sleep(SETTLE_S)
    passed = (secure is not None and value is not None
              and value.get("content_withheld") is True and "text" not in value
              and nowhere(recorder, SECRET))
    return {"step": "a password field is never read", "acted": acted, "passed": passed,
            "why": f"gap {secure}, field.value {value}, secret absent {nowhere(recorder, SECRET)}"}


def browser_step(recorder, driver, label, name):
    """A browser sends no text change the recorder subscribes to, so a field is
    counted from the focus event it gets on entry: the focus is moved off the page's
    first field first, which has it when the page opens."""
    field = f"{label} text field"
    focus_away(driver, f"{label} button")
    acted = focus_and_set(driver, field, TYPED, True, by_keys=True)
    left = focus_away(driver, f"{label} button")
    value = recorder.wait_for(kind("field.value", browser_id=name))
    unknown = recorder.wait_for(gap("private_unknown"))
    time.sleep(SETTLE_S)
    gaps = [f for f in recorder.events() if gap("private_unknown")(f)]
    passed = (value is not None and value.get("content_withheld") is True
              and value.get("private_state") == "unknown" and "text" not in value
              and isinstance(value.get("char_len"), int) and value["char_len"] >= len(TYPED)
              and unknown is not None and len(gaps) == 1 and nowhere(recorder, TYPED))
    return {"step": "a browser's typed text is withheld", "acted": acted, "left": left,
            "passed": passed,
            "why": f"field.value {value}, gaps {len(gaps)}, text absent {nowhere(recorder, TYPED)}"}


def run(args):
    label = LABELS[args.toolkit]
    driver = Helper(args.binary, args.out + ".driver.stderr")
    results = []
    try:
        driver.handshake()
        name = application_named_by_window(f"Compux Fixture {label}")
        results.append({"step": "the fixture's name on the bus", "name": name,
                        "passed": name is not None, "why": f"name {name!r}"})
        if name is not None:
            results.append(not_listed_step(args, driver, label))
            results.extend(listed_steps(args, driver, label, name))
    except (HelperFailed, GLib.Error) as why:
        results.append({"step": "helper", "passed": False, "why": str(why)})
    finally:
        driver.close()
    return {"toolkit": args.toolkit, "steps": results,
            "passed": bool(results) and all(r.get("passed") for r in results)}


def main():
    refuse_outside_a_container()
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--toolkit", required=True, choices=sorted(LABELS))
    parser.add_argument("--out", required=True)
    args = parser.parse_args()
    os.makedirs(os.path.dirname(args.out), exist_ok=True)
    result = run(args)
    with open(args.out, "w") as handle:
        json.dump(result, handle, indent=1)
    for step in result["steps"]:
        print(f"{'PASS' if step.get('passed') else 'FAIL'} {args.toolkit} {step['step']}: "
              f"{str(step.get('why'))[:600]}")
    print(f"OBSERVE {args.toolkit}: {'PASS' if result['passed'] else 'FAIL'}")
    sys.exit(0 if result["passed"] else 1)


if __name__ == "__main__":
    main()
