#!/usr/bin/env python3
"""M86 L0(d): the GNOME portal route compux would take on Wayland, probed by hand.

FOR THE OWNER, IN THE GNOME WAYLAND VM, ON A REAL LOGIN. Agents never run this
because `remote` types three letters and moves the pointer of the session it runs
in. Each run registers the app id first (org.freedesktop.host.portal.Registry, needs
xdg-desktop-portal 1.18+), so every grant it gets is that app id's, not the empty
one every host process shares.

  python3 owner_portal_probe.py screenshot
      Screenshot, interactive false: the response code (0 granted, 1 refused,
      2 ended), how long it took, and the image's size in pixels.
  python3 owner_portal_probe.py remote [--token TOKEN] [--type-after 5] [--hold 15]
      RemoteDesktop (keyboard + pointer) with a ScreenCast monitor, persist_mode 2:
      prints the stream's position and size and the restore token; after
      --type-after seconds types "abc" (focus a text editor first: does the "a"
      arrive?), moves the pointer to (100, 100) of the stream, then keeps the
      session --hold seconds (is the shell's indicator showing?) and closes it.
      Run it again in a NEW process with --token: no dialog means the token
      survived a restart.
"""

import argparse
import struct
import sys
import time
from urllib.parse import unquote, urlparse

from gi.repository import Gio, GLib

APP_ID = "io.tezra.fermix.ComputerUse"
PORTAL = ("org.freedesktop.portal.Desktop", "/org/freedesktop/portal/desktop")
REMOTE = "org.freedesktop.portal.RemoteDesktop"
CAST = "org.freedesktop.portal.ScreenCast"
RESPONSE_TIMEOUT_S = 60
CALL_TIMEOUT_MS = 10000
KEYBOARD, POINTER, MONITOR, PERSISTENT = 1, 2, 1, 2
KEYSYMS = (0x61, 0x62, 0x63)  # a b c


def call(bus, interface, method, args, path=PORTAL[1]):
    return bus.call_sync(PORTAL[0], path, interface, method, args, None,
                         Gio.DBusCallFlags.NONE, CALL_TIMEOUT_MS, None)


def register(bus):
    call(bus, "org.freedesktop.host.portal.Registry", "Register",
         GLib.Variant("(sa{sv})", (APP_ID, {})))


def requested(bus, token, start):
    """Run START (which passes TOKEN as handle_token) and wait for its Response."""
    sender = bus.get_unique_name()[1:].replace(".", "_")
    path = f"/org/freedesktop/portal/desktop/request/{sender}/{token}"
    loop, answer = GLib.MainLoop(), {}

    def on_response(_bus, _sender, _path, _iface, _signal, params, _data):
        answer["response"] = params.unpack()
        loop.quit()

    subscription = bus.signal_subscribe(PORTAL[0], "org.freedesktop.portal.Request", "Response",
                                        path, None, Gio.DBusSignalFlags.NONE, on_response, None)
    try:
        start()
        GLib.timeout_add_seconds(RESPONSE_TIMEOUT_S, loop.quit)
        loop.run()
    finally:
        bus.signal_unsubscribe(subscription)
    if "response" not in answer:
        raise SystemExit(f"no Response in {RESPONSE_TIMEOUT_S} s")
    return answer["response"]


def png_size(uri):
    with open(unquote(urlparse(uri).path), "rb") as handle:
        header = handle.read(24)
    return struct.unpack(">II", header[16:24])


def screenshot(bus):
    started = time.monotonic()
    code, results = requested(bus, "l0shot", lambda: call(
        bus, "org.freedesktop.portal.Screenshot", "Screenshot",
        GLib.Variant("(sa{sv})", ("", {"handle_token": GLib.Variant("s", "l0shot"),
                                       "interactive": GLib.Variant("b", False)}))))
    took = time.monotonic() - started
    print(f"Screenshot response {code} after {took:.2f} s")
    if code == 0:
        print(f"image {results['uri']}: {png_size(results['uri'])} pixels")


def open_session(bus, token):
    _, created = requested(bus, "l0create", lambda: call(bus, REMOTE, "CreateSession", GLib.Variant(
        "(a{sv})", ({"handle_token": GLib.Variant("s", "l0create"),
                     "session_handle_token": GLib.Variant("s", "l0session")},))))
    session = created["session_handle"]
    devices = {"handle_token": GLib.Variant("s", "l0devices"),
               "types": GLib.Variant("u", KEYBOARD | POINTER),
               "persist_mode": GLib.Variant("u", PERSISTENT)}
    if token:
        devices["restore_token"] = GLib.Variant("s", token)
    code, _ = requested(bus, "l0devices", lambda: call(
        bus, REMOTE, "SelectDevices", GLib.Variant("(oa{sv})", (session, devices))))
    print(f"SelectDevices response {code}")
    code, _ = requested(bus, "l0sources", lambda: call(bus, CAST, "SelectSources", GLib.Variant(
        "(oa{sv})", (session, {"handle_token": GLib.Variant("s", "l0sources"),
                               "types": GLib.Variant("u", MONITOR)}))))
    print(f"SelectSources response {code}")
    return session


def start(bus, session):
    started = time.monotonic()
    code, results = requested(bus, "l0start", lambda: call(bus, REMOTE, "Start", GLib.Variant(
        "(osa{sv})", (session, "", {"handle_token": GLib.Variant("s", "l0start")}))))
    print(f"Start response {code} after {time.monotonic() - started:.2f} s "
          "(a dialog shown? say so)")
    if code != 0:
        raise SystemExit("not started")
    for node, properties in results.get("streams", []):
        print(f"stream {node}: position {properties.get('position')} size {properties.get('size')}")
    print(f"restore_token {results.get('restore_token')}")
    return results["streams"][0][0]


def act(bus, session, stream, type_after, hold):
    print(f"typing 'abc' in {type_after} s: focus a text editor now")
    time.sleep(type_after)
    for keysym in KEYSYMS:
        for state in (1, 0):
            call(bus, REMOTE, "NotifyKeyboardKeysym",
                 GLib.Variant("(oa{sv}iu)", (session, {}, keysym, state)))
            time.sleep(0.05)
    call(bus, REMOTE, "NotifyPointerMotionAbsolute",
         GLib.Variant("(oa{sv}udd)", (session, {}, stream, 100.0, 100.0)))
    print(f"typed 'abc' (did the 'a' arrive?); pointer at (100, 100) of the stream; "
          f"holding {hold} s (is the shell's indicator showing?)")
    time.sleep(hold)
    call(bus, "org.freedesktop.portal.Session", "Close", None, path=session)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("what", choices=("screenshot", "remote"))
    parser.add_argument("--token")
    parser.add_argument("--type-after", type=float, default=5.0)
    parser.add_argument("--hold", type=float, default=15.0)
    args = parser.parse_args()
    bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    register(bus)
    if args.what == "screenshot":
        screenshot(bus)
        return
    session = open_session(bus, args.token)
    act(bus, session, start(bus, session), args.type_after, args.hold)


if __name__ == "__main__":
    sys.exit(main())
