#!/usr/bin/env python3
"""M86 L3: one window bound on an X11 desktop, and the badge over it.

FOR THE OWNER, ON THEIR OWN DESKTOP, BY HAND. Binding a window reads it through
Composite and puts the badge on the screen, and the actions typed in a session are
real input. Agents never run this: it acts on a real desktop.

  python3 owner_bound.py --binary native/compux/target/release/compux \\
      --title "Compux Fixture GTK3" --save ~/l3-images
  python3 owner_bound.py --binary ... --title "Compux Fixture GTK3" --cycles 20

Without --cycles it binds the window titled exactly TITLE, prints the reply, then
reads one JSON action per line from stdin (ctrl-D ends it), as cx.py does. Each
action is sent in the bound window: it carries the binding's `target_id`, and one
that names a point or a control first takes a fresh screenshot or listing of that
window. Every `session_event` (a press on the badge) is printed the moment it
arrives, and the session's authorization follows it. With --save DIR every picture
a reply carries is written to DIR, named by its `observation_id`.

With --cycles N it binds and releases the window N times and prints, after each
release, how many badge processes the helper still has and its resident memory.
"""

import argparse
import base64
import json
import os
import queue
import subprocess
import sys
import threading
import time

REPLY_TIMEOUT_S = 30
# How long a badge is given to come up after a selection, or to go after a release.
BADGE_WAIT_S = 5
BADGE_POLL_S = 0.05
BADGE_COMM = "compux-indicato"  # the kernel keeps 15 characters of a name
# Memory is judged from this cycle on, once the first selections have warmed up.
WARM_CYCLE = 5
GROWTH_LIMIT_KB = 4096
ADDRESSED = {"left_click", "right_click", "double_click", "mouse_move", "left_click_drag",
             "scroll", "press", "set_value"}
# Actions about bindings and the display, which never carry the binding's id.
UNTARGETED = {"select_target", "release_target", "windows"}


class Helper:
    """The helper on a pipe. A reader thread prints each `session_event` as it
    arrives and queues every other line as a reply."""

    def __init__(self, binary):
        self.proc = subprocess.Popen([binary], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     text=True, bufsize=1)
        self.replies = queue.Queue()
        self.generations = {"session_generation": 1, "authorization_generation": 1}
        self.next_id = 0
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.proc.stdout:
            frame = json.loads(line)
            if frame.get("type") != "session_event":
                self.replies.put(frame)
                continue
            print("event   ", json.dumps(frame), flush=True)
            if "authorization_generation" in frame:
                self.generations["authorization_generation"] = frame["authorization_generation"]
        self.replies.put(None)

    def ask(self, action, **fields):
        self.next_id += 1
        frame = {"type": "request", "request_id": f"r{self.next_id}", "action": action,
                 "mutation_seq": self.next_id, "deadline_ms": REPLY_TIMEOUT_S * 1000,
                 **self.generations, **fields}
        self.proc.stdin.write(json.dumps(frame) + "\n")
        self.proc.stdin.flush()
        deadline = time.monotonic() + REPLY_TIMEOUT_S
        while True:
            left = deadline - time.monotonic()
            if left <= 0:
                raise RuntimeError(f"no reply to {action} in {REPLY_TIMEOUT_S} s")
            reply = self.replies.get(timeout=left)
            if reply is None:
                raise RuntimeError(f"the helper exited during {action}")
            if reply.get("request_id") == frame["request_id"]:
                return reply

    def close(self):
        self.proc.stdin.close()
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()


def shown(reply, save):
    """A reply as printed: an image's bytes are left out, and written to `save`
    when one is given."""
    if save and "data" in reply:
        suffix = "jpg" if reply.get("mime") == "image/jpeg" else "png"
        path = os.path.join(save, f"{reply.get('observation_id')}.{suffix}")
        with open(path, "wb") as handle:
            handle.write(base64.b64decode(reply["data"]))
        print("saved   ", path, flush=True)
    return json.dumps({key: value for key, value in reply.items() if key != "data"})


def badges(helper_pid):
    """The helper's child processes that are the badge."""
    found = []
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/stat", encoding="utf-8") as handle:
                stat = handle.read()
        except OSError:
            continue  # it exited while the list was read
        comm = stat[stat.index("(") + 1:stat.rindex(")")]
        parent = int(stat[stat.rindex(")") + 2:].split()[1])
        if parent == helper_pid and comm == BADGE_COMM:
            found.append(int(entry))
    return found


def resident_kb(pid):
    with open(f"/proc/{pid}/status", encoding="utf-8") as handle:
        for line in handle:
            if line.startswith("VmRSS:"):
                return int(line.split()[1])
    raise RuntimeError(f"no VmRSS for {pid}")


def wait_badges(helper_pid, count):
    deadline = time.monotonic() + BADGE_WAIT_S
    while time.monotonic() < deadline:
        if len(badges(helper_pid)) == count:
            return True
        time.sleep(BADGE_POLL_S)
    return False


def window_id(helper, title):
    reply = helper.ask("windows")
    matches = [window for window in reply.get("windows", []) if window.get("title") == title]
    if len(matches) != 1:
        titles = [window.get("title") for window in reply.get("windows", [])]
        sys.exit(f"{len(matches)} windows are titled {title!r}; listed: {titles}")
    return matches[0]["id"]


def cycles(helper, window, count):
    series, failed = [], []
    for cycle in range(1, count + 1):
        bound = helper.ask("select_target", window_id=window)
        up = bool(bound.get("ok")) and wait_badges(helper.proc.pid, 1)
        released = helper.ask("release_target")
        gone = wait_badges(helper.proc.pid, 0)
        series.append(resident_kb(helper.proc.pid))
        print(f"cycle {cycle}: bound {bound.get('ok')} {bound.get('error') or ''} badge up {up}, "
              f"released {released.get('ok')}, badge gone {gone}, resident {series[-1]} kB",
              flush=True)
        if not (up and released.get("ok") and gone):
            failed.append(cycle)
    growth = series[-1] - series[min(WARM_CYCLE, count) - 1]
    left = badges(helper.proc.pid)
    passed = not failed and not left and growth <= GROWTH_LIMIT_KB
    print(f"{'PASS' if passed else 'FAIL'}: failed cycles {failed}, badges left {left}, "
          f"growth after cycle {WARM_CYCLE} {growth} kB (limit {GROWTH_LIMIT_KB})")
    return passed


def session(helper, window, save):
    bound = helper.ask("select_target", window_id=window)
    print("bound   ", shown(bound, save), flush=True)
    target = {"target_id": bound["target_id"]} if bound.get("ok") else {}
    if sys.stdin.isatty():
        print("ready  (one JSON action per line, ctrl-D to finish)", flush=True)
    for line in sys.stdin:
        if not line.strip():
            continue
        body = json.loads(line)
        action = body.pop("action")
        if action not in UNTARGETED:
            body = {**target, **body}
        if action in ADDRESSED and "observation_id" not in body:
            looked = "elements" if "element_ref" in body else "screenshot"
            fresh = helper.ask(looked, **target)
            print(f"{looked:8}", shown(fresh, save), flush=True)
            body["observation_id"] = fresh.get("observation_id")
        reply = helper.ask(action, **body)
        print("action  ", shown(reply, save), flush=True)
        if action == "select_target" and reply.get("ok"):
            target = {"target_id": reply["target_id"]}
        if action == "release_target":
            target = {}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--title", required=True, help="the window's exact title")
    parser.add_argument("--cycles", type=int, default=0)
    parser.add_argument("--save", help="a directory for the pictures replies carry")
    args = parser.parse_args()
    if not os.environ.get("DISPLAY"):
        sys.exit("owner_bound.py: no DISPLAY; run it in the desktop session")
    if args.save:
        os.makedirs(args.save, exist_ok=True)
    helper = Helper(args.binary)
    try:
        hello = helper.ask("hello", protocol_version=12)
        print("hello   ", shown(hello, None), flush=True)
        helper.generations["sidecar_generation"] = hello.get("sidecar_generation")
        window = window_id(helper, args.title)
        if args.cycles:
            passed = cycles(helper, window, args.cycles)
        else:
            passed = session(helper, window, args.save)
    finally:
        helper.close()
    sys.exit(0 if passed in (None, True) else 1)


if __name__ == "__main__":
    main()
