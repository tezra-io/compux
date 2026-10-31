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
the walk was truncated.
"""

import argparse
import json
import os
import statistics
import subprocess
import sys
import time

CONTAINER_MARKERS = ("/.dockerenv", "/run/.containerenv")
REPLY_TIMEOUT_S = 30
# Bounded wait for the fixture to become the application in front with controls.
READY_TRIES = 15
READY_STEP_S = 0.2


def refuse_outside_a_container():
    if not any(os.path.exists(marker) for marker in CONTAINER_MARKERS):
        print("elements_timing.py: runs only inside the fixture container", file=sys.stderr)
        sys.exit(2)


class Helper:
    """The compux helper on a pipe, one request at a time."""

    def __init__(self, binary, stderr_path):
        self.stderr = open(stderr_path, "w")
        self.proc = subprocess.Popen([binary], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=self.stderr, text=True, bufsize=1)
        self.next_id = 0
        self.generations = {}

    def ask(self, action, **fields):
        self.next_id += 1
        frame = {"type": "request", "request_id": f"r{self.next_id}", "action": action,
                 **self.generations, **fields}
        started = time.monotonic()
        self.proc.stdin.write(json.dumps(frame) + "\n")
        self.proc.stdin.flush()
        while True:
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError(f"the helper exited during {action}")
            reply = json.loads(line)
            if reply.get("request_id") == frame["request_id"]:
                return reply, (time.monotonic() - started) * 1000.0
            if time.monotonic() - started > REPLY_TIMEOUT_S:
                raise RuntimeError(f"no reply to {action} in {REPLY_TIMEOUT_S} s")

    def handshake(self):
        reply, _ = self.ask("hello", session_generation=1, authorization_generation=1)
        if not reply.get("ok"):
            raise RuntimeError(f"hello refused: {reply}")
        self.generations = {"sidecar_generation": reply.get("sidecar_generation"),
                            "session_generation": 1, "authorization_generation": 1}
        return reply

    def close(self):
        self.proc.stdin.close()
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        self.stderr.close()


def listing(reply):
    return [{key: element.get(key) for key in
             ("role", "label", "path", "enabled", "settable", "value", "actions_partial")}
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
    raise RuntimeError(f"the fixture was never listed; last reply: {json.dumps(last)[:400]}")


def percentile(values, share):
    ordered = sorted(values)
    index = min(len(ordered) - 1, max(0, round(share * (len(ordered) - 1))))
    return ordered[index]


def run(args):
    helper = Helper(args.binary, args.out + ".stderr")
    try:
        hello = helper.handshake()
        first = wait_until_listed(helper)
        runs = []
        for _ in range(args.runs):
            reply, wall_ms = helper.ask("elements", deadline_ms=10000)
            runs.append({"ok": reply.get("ok"), "wall_ms": round(wall_ms, 2),
                         "ax_messages": reply.get("ax_messages"),
                         "listed": len(reply.get("elements", [])),
                         "truncated": reply.get("truncated"),
                         "error": reply.get("error")})
    finally:
        helper.close()
    walls = [run["wall_ms"] for run in runs if run["ok"]]
    return {
        "fixture": args.expect,
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
    print(f"{args.expect}: p50 {result['p50_ms']} ms, p95 {result['p95_ms']} ms, "
          f"ax_messages {result['ax_messages_median']}, listed {len(result['first'])}")


if __name__ == "__main__":
    main()
