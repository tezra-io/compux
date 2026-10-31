#!/usr/bin/env python3
"""List the accessibility tree's roots: every application registered on the
AT-SPI registry, and each one's top-level windows. Read-only, and tiny on
purpose.

  python3 a11y_roots.py                                  # print what is registered
  python3 a11y_roots.py --expect "Compux Fixture GTK3"   # wait until it is

Run inside `private-session.sh`, never on a desktop someone is using: it talks to
the session's accessibility bus. Like `private-session.sh`, it refuses (exit 2)
anywhere without a container's marker file, before it opens any bus.

It makes raw D-Bus calls rather than going through libatspi, so the calls it makes
are exactly the ones written here: `org.a11y.Bus.GetAddress` on the session bus,
then on the accessibility bus only `Accessible.GetChildren`, `Accessible.GetRole`
and single `Properties.Get` reads of `Name`. Never `Action.GetActions`, which ATK
bridge 2.38 answers by aborting the application, and never `Properties.GetAll`,
which kills a Qt 6.4 application.
"""

import argparse
import os
import sys
import time

from gi.repository import Gio, GLib

CALL_TIMEOUT_MS = 2000
WAIT_TRIES = 100
WAIT_STEP_S = 0.2
REGISTRY = ("org.a11y.atspi.Registry", "/org/a11y/atspi/accessible/root")
ACCESSIBLE = "org.a11y.atspi.Accessible"
# Docker and Podman each leave one of these in every container they start.
CONTAINER_MARKERS = ("/.dockerenv", "/run/.containerenv")


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


def children(bus, name, path):
    (refs,) = call(bus, name, path, ACCESSIBLE, "GetChildren", None, "(a(so))")
    return refs


def node_name(bus, name, path):
    (value,) = call(bus, name, path, "org.freedesktop.DBus.Properties", "Get",
                    GLib.Variant("(ss)", (ACCESSIBLE, "Name")), "(v)")
    return value


def node_role(bus, name, path):
    (role,) = call(bus, name, path, ACCESSIBLE, "GetRole", None, "(u)")
    return role


def roots(bus):
    """[(application name, [(window name, role), ...]), ...]"""
    listing = []
    for name, path in children(bus, *REGISTRY):
        windows = [(node_name(bus, name, child), node_role(bus, name, child))
                   for child_bus, child in children(bus, name, path) if child_bus == name]
        listing.append((node_name(bus, name, path), windows))
    return listing


def missing(listing, expected):
    seen = {app for app, _windows in listing}
    seen.update(window for _app, windows in listing for window, _role in windows)
    return [name for name in expected if name not in seen]


def main(argv):
    parser = argparse.ArgumentParser(description="list the AT-SPI registry's roots")
    parser.add_argument("--expect", action="append", default=[],
                        help="an application or window name that must be registered")
    args = parser.parse_args(argv)
    if not any(os.path.exists(marker) for marker in CONTAINER_MARKERS):
        print("a11y_roots.py runs only inside the fixture container", file=sys.stderr)
        return 2
    bus = accessibility_bus()
    listing, absent = [], args.expect
    for _ in range(WAIT_TRIES):
        listing = roots(bus)
        absent = missing(listing, args.expect)
        if not absent:
            break
        time.sleep(WAIT_STEP_S)
    for app, windows in listing:
        print(f"application {app!r}")
        for window, role in windows:
            print(f"  window {window!r} (role {role})")
    if absent:
        print(f"MISSING after {WAIT_TRIES * WAIT_STEP_S:.0f} s: {', '.join(absent)}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
