#!/usr/bin/env python3
"""Every verb compux answers on Linux, against one fixture, judged by what the
fixture recorded (M86 L1).

Runs INSIDE the private-session container only (`spike.sh` stage `verbs` starts
it), never on a desktop someone uses: it starts the compux helper, which reads and
writes the session's accessibility bus. It refuses (exit 2) anywhere without a
container's marker file, before it starts anything.

  python3 verbs.py --binary /opt/compux/compux --toolkit gtk3 \\
      --state /tmp/compux-fixture-state/gtk3.json --out /tmp/spike-out/gtk3-verbs.json

Each step lists the controls (`elements` with a `query`), finds its control by
role and name, and sends one verb by reference. A step passes on what the FIXTURE
wrote into its state file: the event its handler records for that change. Where a
toolkit gives a control no route for a verb, the step passes only when the listing
does not offer the verb and a request for it is refused with nothing sent. Steps
whose evidence is the reply itself (a refusal, a condition met, a surface on a
receipt) say so in their expectation.

Nothing is asked until the fixture is listed: the session hands over once the
fixture has written its state file, which it does before its window, and a GTK 4
fixture registers on the accessibility bus about half a second after that. The
first step is then `probe`, read as a doctor reads it, with the fixture on the bus.

Exit 0 when every step met its expectation, 1 otherwise; the JSON has each step's
request, reply and verdict.
"""

import argparse
import json
import os
import queue
import subprocess
import sys
import threading
import time

CONTAINER_MARKERS = ("/.dockerenv", "/run/.containerenv")
REPLY_TIMEOUT_S = 30
# The most lines the helper may have written ahead of the reader.
MAX_PENDING_LINES = 1000
DEADLINE_MS = 10000
# Bounded waits: tries x step.
READY_TRIES = 25
READY_STEP_S = 0.2
EVENT_TRIES = 30
EVENT_STEP_S = 0.1

LABELS = {"gtk3": "GTK3", "gtk4": "GTK4", "qt": "Qt"}


def refuse_outside_a_container():
    if not any(os.path.exists(marker) for marker in CONTAINER_MARKERS):
        print("verbs.py: runs only inside the fixture container", file=sys.stderr)
        sys.exit(2)


class HelperFailed(RuntimeError):
    """The helper itself failed: it exited, answered nothing in time, or wrote what
    is not a reply. The run records it as a FAIL by that name."""


class Helper:
    """The compux helper on a pipe, one request at a time. Its replies are read on
    a thread into a bounded queue, so a helper that writes nothing is waited for
    REPLY_TIMEOUT_S, never for ever."""

    def __init__(self, binary, stderr_path, env=None):
        self.stderr = open(stderr_path, "w")
        self.proc = subprocess.Popen([binary], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=self.stderr, text=True, bufsize=1, env=env)
        self.lines = queue.Queue(maxsize=MAX_PENDING_LINES)
        threading.Thread(target=self._read, daemon=True).start()
        self.next_id = 0
        self.next_seq = 0
        self.generations = {}

    def _read(self):
        """Every line the helper writes, then None when its output ends."""
        for line in self.proc.stdout:
            self.lines.put(line)
        self.lines.put(None)

    def ask(self, action, mutation=False, **fields):
        self.next_id += 1
        frame = {"type": "request", "request_id": f"r{self.next_id}", "action": action,
                 **self.generations, **fields}
        if mutation:
            self.next_seq += 1
            frame["mutation_seq"] = self.next_seq
        deadline = time.monotonic() + REPLY_TIMEOUT_S
        try:
            self.proc.stdin.write(json.dumps(frame) + "\n")
            self.proc.stdin.flush()
        except BrokenPipeError:
            raise HelperFailed(f"the helper exited before {action} was sent") from None
        while True:
            reply = self._next_reply(action, deadline)
            if reply.get("request_id") == frame["request_id"]:
                return frame, reply

    def _next_reply(self, action, deadline):
        """The helper's next line as a reply, waiting until `deadline` at most."""
        left = deadline - time.monotonic()
        if left <= 0:
            raise HelperFailed(f"no reply to {action} in {REPLY_TIMEOUT_S} s")
        try:
            line = self.lines.get(timeout=left)
        except queue.Empty:
            raise HelperFailed(f"no reply to {action} in {REPLY_TIMEOUT_S} s") from None
        if line is None:
            raise HelperFailed(f"the helper exited during {action}")
        try:
            return json.loads(line)
        except ValueError:
            raise HelperFailed(f"the helper wrote no reply during {action}: {line[:200]!r}") from None

    def handshake(self):
        _, reply = self.ask("hello", session_generation=1, authorization_generation=1)
        if not reply.get("ok"):
            raise HelperFailed(f"hello refused: {reply}")
        self.generations = {"sidecar_generation": reply.get("sidecar_generation"),
                            "session_generation": 1, "authorization_generation": 1}
        return reply

    def close(self):
        try:
            self.proc.stdin.close()
        except BrokenPipeError:
            # The helper had already exited: its input has nobody to read it.
            print("the helper exited before its input was closed", file=sys.stderr)
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        self.stderr.close()


# --- the fixture's own record ---------------------------------------------------


def events(state_path):
    try:
        with open(state_path, encoding="utf-8") as handle:
            return json.load(handle).get("events", [])
    except (OSError, ValueError):
        return []


def recorded(state_path, since, toolkit, control, action, value):
    """The first event after `since` events that matches, waiting a bounded time."""
    name = f"{toolkit}.{control}"
    for _ in range(EVENT_TRIES):
        for event in events(state_path)[since:]:
            if event.get("control") != name or event.get("action") != action:
                continue
            if value is None or event.get("value") == value:
                return event
        time.sleep(EVENT_STEP_S)
    return None


# --- finding a control ------------------------------------------------------------


def find(helper, role, name, scope=None):
    """The one listed control of this role whose label is `name`, and the listing's
    observation. Asked again while the fixture is still coming up."""
    query = {"name": name}
    if role:
        query["role"] = role
    fields = {"query": query, "deadline_ms": DEADLINE_MS}
    if scope:
        fields["scope"] = scope
    last = None
    for _ in range(READY_TRIES):
        _, last = helper.ask("elements", **fields)
        matches = [element for element in last.get("elements", [])
                   if element.get("label") == name and element.get("element_ref")]
        if last.get("ok") and matches:
            return matches[0], last.get("observation_id"), last
        time.sleep(READY_STEP_S)
    return None, None, last


# --- the steps --------------------------------------------------------------------
#
# (verb, role, name, fields, expectation). The expectation is one of:
#   ("event", control, action, value)  the fixture recorded it (value None: any)
#   ("refused", code)                  the reply is that refusal, nothing sent
#   ("unrouted",)                      the listing offers no such verb, and asking is
#                                      refused ax_action_unsupported, nothing sent
#   ("effect", effect)                 the receipt's effect (no event to record)


def steps_for(toolkit):
    label = LABELS[toolkit]
    common = [
        ("press", "button", f"{label} button", {}, ("event", "button", "press", None)),
        ("set_value", "textfield", f"{label} text field", {"value": "hello"},
         ("event", "text_field", "set_value", "hello")),
        ("type", "textfield", "Search", {"text": "abc"},
         ("event", "search", "set_value", "abc")),
        ("set_value", "slider", "Volume", {"value": "75"},
         ("event", "slider", "set_value", "75")),
        ("press", "button", f"{label} disabled button", {}, ("refused", "element_disabled")),
    ]
    per_toolkit = {
        "gtk3": [
            ("select", "tab", "Advanced", {}, ("event", "tabs", "select", "Advanced")),
            ("toggle", "checkbox", f"{label} checkbox", {"on": True},
             ("event", "checkbox", "toggle", "on")),
            ("focus", "textfield", "Search", {}, ("event", "search", "focus", None)),
            ("select", "row", "Banana", {}, ("event", "list", "select", "Banana")),
            ("expand", None, "Documents", {}, ("event", "outline", "expand", "Documents")),
            ("collapse", None, "Projects", {}, ("event", "outline", "collapse", "Projects")),
        ],
        "gtk4": [
            ("select", "tab", "Advanced", {}, ("event", "tabs", "select", "Advanced")),
            # A GTK 4 check box names no action, and GrabFocus is NotSupported.
            ("toggle", "checkbox", f"{label} checkbox", {"on": True}, ("unrouted",)),
            ("focus", "textfield", "Search", {}, ("unrouted",)),
            ("select", "row", "Banana", {}, ("event", "list", "select", "Banana")),
            # The row is named by its label; the expander is the button inside it.
            ("expand", "button", "Documents", {}, ("event", "outline", "expand", "Documents")),
            ("collapse", "button", "Projects", {},
             ("event", "outline", "collapse", "Projects")),
            ("scroll_to", None, "Entry 200", {}, ("event", "far_list", "scroll", "bottom")),
        ],
        "qt": [
            # Qt's tab list and item views list no Selection: a tab is pressed, and
            # a list or tree item is selected by its own Toggle.
            ("select", "tab", "Advanced", {}, ("unrouted",)),
            ("press", "tab", "About", {}, ("event", "tabs", "select", "About")),
            ("toggle", "checkbox", f"{label} checkbox", {"on": True},
             ("event", "checkbox", "toggle", "on")),
            ("focus", "textfield", "Search", {}, ("event", "search", "focus", None)),
            ("select", "row", "Banana", {}, ("event", "list", "select", "Banana")),
            ("select", "cell", "Pictures", {}, ("event", "outline", "select", "Pictures")),
            # That Toggle toggles the selection, so it never expands a tree cell.
            ("expand", "cell", "Documents", {}, ("unrouted",)),
            ("collapse", "cell", "Projects", {}, ("unrouted",)),
            ("scroll_to", "row", "Entry 14", {}, ("unrouted",)),
        ],
    }
    return common + per_toolkit[toolkit]


def judge(expectation, element, reply, state_path, since, toolkit):
    kind = expectation[0]
    if kind == "event":
        _, control, action, value = expectation
        if not reply.get("ok"):
            return False, f"refused {reply.get('error')}: {reply.get('detail')}"
        event = recorded(state_path, since, toolkit, control, action, value)
        return event is not None, f"recorded {event}" if event else "the fixture recorded nothing"
    if kind == "refused":
        sent = reply.get("receipt", {}).get("dispatch")
        met = reply.get("error") == expectation[1] and sent in (None, "not_sent")
        return met, f"error {reply.get('error')}, dispatch {sent}"
    if kind == "unrouted":
        offered = element.get("verbs", [])
        sent = reply.get("receipt", {}).get("dispatch")
        met = (reply.get("error") == "ax_action_unsupported" and sent in (None, "not_sent")
               and reply.get("_verb") not in offered)
        return met, f"verbs {offered}, error {reply.get('error')}, dispatch {sent}"
    if kind == "effect":
        effect = reply.get("receipt", {}).get("effect")
        return reply.get("ok") is True and effect == expectation[1], f"effect {effect}"
    return False, f"no such expectation {kind}"


def run_step(helper, toolkit, state_path, step):
    verb, role, name, extra, expectation = step
    element, observation, listing = find(helper, role, name)
    result = {"verb": verb, "role": role, "name": name, "expect": list(expectation)}
    if element is None:
        result.update(passed=False, why="not listed",
                      listing=json.dumps(listing)[:2000] if listing else None)
        return result
    since = len(events(state_path))
    request, reply = helper.ask(verb, mutation=True, observation_id=observation,
                                element_ref=element["element_ref"], **extra)
    reply["_verb"] = verb
    passed, why = judge(expectation, element, reply, state_path, since, toolkit)
    reply.pop("_verb")
    result.update(element=element, request=request, reply=reply, passed=passed, why=why)
    return result


# --- the flows: a menu, a dialog, a wait ---------------------------------------------


# Whether the toolkit's menu-bar item can be pressed open: a GTK 4 menu-bar item
# names no action; Qt's names only ShowMenu, which is its press.
MENU_OPENS = {"gtk3": True, "gtk4": False, "qt": True}


def menu_flow(helper, toolkit, state_path):
    """Open the Actions menu, wait for it with `wait_for`, list it by scope, and
    press Export Report in it. Where the menu-bar item has no press, the press is
    refused with nothing sent."""
    result = {"flow": "menu"}
    actions, observation, _ = find(helper, None, "Actions")
    if actions is None:
        return dict(result, passed=False, why="Actions is not listed")
    _, opened = helper.ask("press", mutation=True, observation_id=observation,
                           element_ref=actions["element_ref"])
    if not MENU_OPENS[toolkit]:
        sent = opened.get("receipt", {}).get("dispatch")
        passed = (opened.get("error") == "ax_action_unsupported" and sent == "not_sent"
                  and "press" not in actions.get("verbs", []))
        return dict(result, opened=opened, element=actions, passed=passed,
                    why=f"verbs {actions.get('verbs')}, error {opened.get('error')}, "
                        f"dispatch {sent}")
    _, waited = helper.ask("wait_for", until={"kind": "menu_open", "timeout_ms": 3000},
                           deadline_ms=DEADLINE_MS)
    since = len(events(state_path))
    item, item_observation, menu = find(helper, None, "Export Report", scope="menu")
    result.update(opened=opened, waited=waited)
    if item is None:
        return dict(result, passed=False, why="Export Report is not listed in the menu",
                    menu=json.dumps(menu)[:2000] if menu else None)
    _, pressed = helper.ask("press", mutation=True, observation_id=item_observation,
                            element_ref=item["element_ref"])
    event = recorded(state_path, since, toolkit, "menu", "activate", "Export Report")
    met = waited.get("met") is True
    result.update(pressed=pressed, event=event, passed=bool(event) and met,
                  why=f"wait_for met {waited.get('met')}, recorded {event}")
    return result


# Whether the application's window still reads ACTIVE once its dialog is open. A GTK 4
# dialog opened by an accessibility press in this session reads ACTIVE nowhere, so
# no application is in front to list a scope of: the receipt and the check still
# name the dialog, from the listing the press named.
DIALOG_IN_FRONT = {"gtk3": True, "gtk4": False, "qt": True}


def dialog_flow(helper, toolkit, state_path):
    """Press Clear All with a semantic check waiting for its Cancel button in a
    dialog; the receipt names the dialog (GTK 3 and Qt make it an alert); list it by
    scope and press Cancel in it."""
    result = {"flow": "dialog"}
    clear, observation, _ = find(helper, "button", "Clear All…")
    if clear is None:
        return dict(result, passed=False, why="Clear All… is not listed")
    since = len(events(state_path))
    _, pressed = helper.ask("press", mutation=True, observation_id=observation,
                            element_ref=clear["element_ref"],
                            check={"kind": "semantic",
                                   "until": {"kind": "appears", "role": "button",
                                             "name": "Cancel", "scope": "dialog",
                                             "timeout_ms": 3000}},
                            deadline_ms=DEADLINE_MS)
    opened = recorded(state_path, since, toolkit, "dialog", "open", None)
    receipt = pressed.get("receipt", {})
    surfaces = [surface.get("kind") for surface in receipt.get("surfaces", [])]
    until = receipt.get("check", {}).get("until")
    shown = bool(opened) and until == "met" and bool({"dialog", "alert"} & set(surfaces))
    since = len(events(state_path))
    cancel, cancel_observation, listing = find(helper, "button", "Cancel", scope="dialog")
    result.update(pressed=pressed, opened=opened, surfaces=surfaces, until=until)
    if not DIALOG_IN_FRONT[toolkit]:
        unlisted = cancel is None and (listing or {}).get("error") == "scope_not_found"
        return dict(result, listing=listing, passed=shown and unlisted,
                    why=f"opened {bool(opened)}, until {until}, surfaces {surfaces}, "
                        f"then {(listing or {}).get('error')}: no window reads ACTIVE")
    if cancel is None:
        return dict(result, passed=False, why="Cancel is not listed in the dialog",
                    listing=json.dumps(listing)[:2000] if listing else None)
    _, cancelled = helper.ask("press", mutation=True, observation_id=cancel_observation,
                              element_ref=cancel["element_ref"])
    event = recorded(state_path, since, toolkit, "dialog", "cancel", None)
    passed = shown and bool(event)
    result.update(cancelled=cancelled, event=event, passed=passed,
                  why=f"opened {bool(opened)}, until {until}, surfaces {surfaces}, "
                      f"cancel recorded {bool(event)}")
    return result


def probe_check(helper):
    """`probe` in this session, as a doctor reads it: the display answered as X11,
    the accessibility bus answers with the fixture on it, the session's status was
    read, dpkg names both packages, and the Qt note is there exactly while the
    status reads off."""
    _, reply = helper.ask("probe")
    bus = reply.get("accessibility_bus") or {}
    packages = reply.get("accessibility_packages") or {}
    notes = [note.get("toolkit") for note in reply.get("toolkit_notes") or []]
    status = bus.get("status")
    off = status is not None and not status.get("is_enabled") \
        and not status.get("screen_reader_enabled")
    checks = {
        "display": reply.get("display_server") == "x11" and reply.get("x11_endpoint") is True,
        "bus": bus.get("state") == "answering" and (bus.get("applications") or 0) >= 1,
        "status": status is not None,
        "packages": packages.get("source") == "dpkg" and bool(packages.get("at_spi2_core"))
        and bool(packages.get("atk_bridge")),
        "notes": notes == (["qt", "chromium"] if off else ["chromium"]),
    }
    failed = [name for name, passed in checks.items() if not passed]
    why = (f"{bus.get('state')} with {bus.get('applications')} applications, status {status}, "
           f"at-spi2-core {packages.get('at_spi2_core')}, ATK bridge "
           f"{packages.get('atk_bridge')}, notes {notes}")
    return {"flow": "probe", "passed": not failed,
            "why": f"failed {failed}: {why}" if failed else why, "probe": reply}


def fixture_listed(helper, toolkit):
    """Wait, bounded, until compux lists the fixture's first control: the fixture
    is then on the accessibility bus. Every step runs after this, never before."""
    label = LABELS[toolkit]
    element, _, listing = find(helper, "button", f"{label} button")
    if element is not None:
        return {"flow": "fixture listed", "passed": True, "why": f"{label} button listed"}
    return {"flow": "fixture listed", "passed": False,
            "why": f"{label} button never listed in {READY_TRIES} tries",
            "listing": json.dumps(listing)[:2000] if listing else None}


def run_all(helper, args, results):
    """Every step, each result appended as it is decided, so a run the helper ends
    keeps what it got to."""
    ready = fixture_listed(helper, args.toolkit)
    results.append(ready)
    if not ready["passed"]:
        return
    results.append(probe_check(helper))
    for step in steps_for(args.toolkit):
        results.append(run_step(helper, args.toolkit, args.state, step))
    results.append(menu_flow(helper, args.toolkit, args.state))
    results.append(dialog_flow(helper, args.toolkit, args.state))


def run(args):
    helper = Helper(args.binary, args.out + ".stderr")
    hello, results = {}, []
    try:
        hello = helper.handshake()
        run_all(helper, args, results)
    except HelperFailed as why:
        results.append({"flow": "helper", "passed": False, "why": str(why)})
    finally:
        helper.close()
    return {"toolkit": args.toolkit, "hello_actions": hello.get("actions"),
            "input_methods": hello.get("capabilities", {}).get("input_methods"),
            "steps": results, "events": events(args.state),
            "passed": all(result.get("passed") for result in results)}


def main():
    refuse_outside_a_container()
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--toolkit", required=True, choices=sorted(LABELS))
    parser.add_argument("--state", required=True)
    parser.add_argument("--out", required=True)
    args = parser.parse_args()
    os.makedirs(os.path.dirname(args.out), exist_ok=True)
    result = run(args)
    with open(args.out, "w") as handle:
        json.dump(result, handle, indent=1)
    for step in result["steps"]:
        what = step.get("flow") or f"{step['verb']} {step['name']}"
        print(f"{'PASS' if step.get('passed') else 'FAIL'} {args.toolkit} {what}: {step.get('why')}")
    print(f"VERBS {args.toolkit}: {'PASS' if result['passed'] else 'FAIL'}")
    sys.exit(0 if result["passed"] else 1)


if __name__ == "__main__":
    main()
