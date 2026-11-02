#!/usr/bin/env python3
"""Time compux's `elements` on Linux against one fixture (M86 L0(a)).

Runs INSIDE the private-session container only (`spike.sh` starts it), never on a
desktop someone uses: it starts the compux helper, which reads the session's
accessibility bus and measures the private Xvfb display. It refuses (exit 2)
anywhere without a container's marker file, before it starts anything.

  python3 elements_timing.py --binary /opt/compux/compux --runs 20 \\
      --expect "Compux Fixture GTK3" --out /out/gtk3-elements.json

It speaks the helper's own wire: `hello`, then `elements` again and again, each
timed from the request line written to the reply line read. The first listing is
kept whole (role, label, path, actions_partial) for the coverage comparison; every
run records its wall time, `ax_messages`, how many controls it listed and whether
the walk was truncated. A helper that answers nothing in time, exits, or writes no
reply ends the run as `FAIL <fixture>: helper: ...`, with the JSON written.
"""

import argparse
import json
import os
import queue
import statistics
import subprocess
import sys
import threading
import time

CONTAINER_MARKERS = ("/.dockerenv", "/run/.containerenv")
REPLY_TIMEOUT_S = 30
# The most lines the helper may have written ahead of the reader.
MAX_PENDING_LINES = 1000
# Bounded wait for the fixture to become the application in front with controls.
READY_TRIES = 15
READY_STEP_S = 0.2


def refuse_outside_a_container():
    if not any(os.path.exists(marker) for marker in CONTAINER_MARKERS):
        print("elements_timing.py: runs only inside the fixture container", file=sys.stderr)
        sys.exit(2)


class HelperFailed(RuntimeError):
    """The helper itself failed: it exited, answered nothing in time, or wrote what
    is not a reply. The run records it as a FAIL by that name."""


class NeverListed(RuntimeError):
    """The helper answered, but the fixture was never listed in the tries a run
    has. The run records it as a FAIL by that name."""


class Helper:
    """The compux helper on a pipe, one request at a time. Its replies are read on
    a thread into a bounded queue, each with the time it was read, so a helper that
    writes nothing is waited for REPLY_TIMEOUT_S, never for ever, and a reply's
    time is still from the request written to the reply read."""

    def __init__(self, binary, stderr_path):
        self.stderr = open(stderr_path, "w")
        self.proc = subprocess.Popen([binary], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=self.stderr, text=True, bufsize=1)
        self.lines = queue.Queue(maxsize=MAX_PENDING_LINES)
        threading.Thread(target=self._read, daemon=True).start()
        self.next_id = 0
        self.generations = {}

    def _read(self):
        """Every line the helper writes with when it was read, then None when its
        output ends."""
        for line in self.proc.stdout:
            self.lines.put((line, time.monotonic()))
        self.lines.put(None)

    def ask(self, action, **fields):
        self.next_id += 1
        frame = {"type": "request", "request_id": f"r{self.next_id}", "action": action,
                 **self.generations, **fields}
        started = time.monotonic()
        deadline = started + REPLY_TIMEOUT_S
        try:
            self.proc.stdin.write(json.dumps(frame) + "\n")
            self.proc.stdin.flush()
        except BrokenPipeError:
            raise HelperFailed(f"the helper exited before {action} was sent") from None
        while True:
            reply, read_at = self._next_reply(action, deadline)
            if reply.get("request_id") == frame["request_id"]:
                return reply, (read_at - started) * 1000.0

    def _next_reply(self, action, deadline):
        """The helper's next line as a reply, and when it was read, waiting until
        `deadline` at most."""
        left = deadline - time.monotonic()
        if left <= 0:
            raise HelperFailed(f"no reply to {action} in {REPLY_TIMEOUT_S} s")
        try:
            read = self.lines.get(timeout=left)
        except queue.Empty:
            raise HelperFailed(f"no reply to {action} in {REPLY_TIMEOUT_S} s") from None
        if read is None:
            raise HelperFailed(f"the helper exited during {action}")
        line, read_at = read
        try:
            return json.loads(line), read_at
        except ValueError:
            raise HelperFailed(f"the helper wrote no reply during {action}: {line[:200]!r}") from None

    def handshake(self):
        reply, _ = self.ask("hello", session_generation=1, authorization_generation=1)
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


def listing(reply):
    return [{key: element.get(key) for key in
             ("role", "native_role", "label", "path", "enabled", "verbs", "value",
              "actions_partial")}
            for element in reply.get("elements", [])]


def wait_until_listed(helper):
    """The fixture registers a moment after its window maps; ask until it is read.
    A reader that answers but lists nothing (GTK 4, which has no screen positions)
    is timed as it is: the last answer is kept once the tries run out."""
    last = None
    for _ in range(READY_TRIES):
        last, _ = helper.ask("elements", deadline_ms=10000)
        if last.get("ok") and last.get("elements"):
            return last
        time.sleep(READY_STEP_S)
    if last and last.get("ok"):
        return last
    raise NeverListed(f"the fixture was never listed; last reply: {json.dumps(last)[:400]}")


def percentile(values, share):
    ordered = sorted(values)
    index = min(len(ordered) - 1, max(0, round(share * (len(ordered) - 1))))
    return ordered[index]


def timed(helper, count, runs):
    """`elements` `count` times, each run appended as it is read."""
    for _ in range(count):
        reply, wall_ms = helper.ask("elements", deadline_ms=10000)
        runs.append({"ok": reply.get("ok"), "wall_ms": round(wall_ms, 2),
                     "ax_messages": reply.get("ax_messages"),
                     "listed": len(reply.get("elements", [])),
                     "truncated": reply.get("truncated"),
                     "error": reply.get("error")})


def run(args):
    helper = Helper(args.binary, args.out + ".stderr")
    hello, first, runs, failed = {}, {}, [], None
    try:
        hello = helper.handshake()
        first = wait_until_listed(helper)
        timed(helper, args.runs, runs)
    except HelperFailed as why:
        failed = f"helper: {why}"
    except NeverListed as why:
        failed = f"fixture: {why}"
    finally:
        helper.close()
    walls = [run["wall_ms"] for run in runs if run["ok"]]
    return {
        "fixture": args.expect,
        "failed": failed,
        "hello_actions": hello.get("actions"),
        "note": first.get("ax_activation"),
        "first": listing(first),
        "runs": runs,
        "p50_ms": statistics.median(walls) if walls else None,
        "p95_ms": percentile(walls, 0.95) if walls else None,
        "ax_messages_median": statistics.median([r["ax_messages"] for r in runs if r["ok"]])
        if walls else None,
    }


def main():
    refuse_outside_a_container()
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--runs", type=int, default=20)
    parser.add_argument("--expect", required=True)
    parser.add_argument("--out", required=True)
    args = parser.parse_args()
    result = run(args)
    with open(args.out, "w") as handle:
        json.dump(result, handle, indent=1)
    if result["failed"]:
        print(f"FAIL {args.expect}: {result['failed']}")
        sys.exit(1)
    print(f"{args.expect}: p50 {result['p50_ms']} ms, p95 {result['p95_ms']} ms, "
          f"ax_messages {result['ax_messages_median']}, listed {len(result['first'])}")


if __name__ == "__main__":
    main()
