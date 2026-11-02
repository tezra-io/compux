#!/usr/bin/env python3
"""M86 L0(c), Q2: is a root-window grab honest on a GPU-composited GNOME X11 desktop?

FOR THE OWNER, ON THEIR OWN DESKTOP, BY HAND. It starts the compux helper and asks it
for ONE `screenshot` of the whole display: nothing moves, types or clicks, and no
accessibility call is made. Agents never run this: it acts on a real desktop.

  python3 owner_capture.py --binary native/compux/target/release/compux --out ~/l0c

It writes OUT/compux.png (or .jpg: the helper's own choice of encoding) and
OUT/compux.json (the reply without its image). Take GNOME's own picture of the same
moment beside it (`gnome-screenshot -f OUT/gnome.png`) and compare the two.
"""

import argparse
import base64
import json
import os
import subprocess
import sys
import time

REPLY_TIMEOUT_S = 30


def ask(proc, frame):
    """One request, its reply by request_id; other lines (events) are skipped."""
    proc.stdin.write(json.dumps(frame) + "\n")
    proc.stdin.flush()
    started = time.monotonic()
    while time.monotonic() - started < REPLY_TIMEOUT_S:
        line = proc.stdout.readline()
        if not line:
            raise RuntimeError("the helper exited")
        reply = json.loads(line)
        if reply.get("request_id") == frame["request_id"]:
            return reply
    raise RuntimeError(f"no reply to {frame['action']} in {REPLY_TIMEOUT_S} s")


def screenshot(binary):
    proc = subprocess.Popen([binary], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    try:
        hello = ask(proc, {"type": "request", "request_id": "h", "action": "hello",
                           "session_generation": 1, "authorization_generation": 1})
        if not hello.get("ok"):
            raise RuntimeError(f"hello refused: {hello}")
        return ask(proc, {"type": "request", "request_id": "s", "action": "screenshot",
                          "sidecar_generation": hello.get("sidecar_generation"),
                          "session_generation": 1, "authorization_generation": 1})
    finally:
        proc.stdin.close()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--out", required=True)
    args = parser.parse_args()
    if not os.environ.get("DISPLAY"):
        sys.exit("owner_capture.py: no DISPLAY; run it in the desktop session")
    os.makedirs(args.out, exist_ok=True)
    reply = screenshot(args.binary)
    if not reply.get("ok"):
        sys.exit(f"screenshot refused: {json.dumps(reply)[:400]}")
    data = base64.b64decode(reply.pop("data"))
    suffix = "jpg" if reply.get("mime") == "image/jpeg" else "png"
    with open(os.path.join(args.out, f"compux.{suffix}"), "wb") as handle:
        handle.write(data)
    with open(os.path.join(args.out, "compux.json"), "w") as handle:
        json.dump(reply, handle, indent=1)
    print(f"wrote {args.out}/compux.{suffix}: {reply.get('width')}x{reply.get('height')} "
          f"of physical {reply.get('physical')}")


if __name__ == "__main__":
    main()
