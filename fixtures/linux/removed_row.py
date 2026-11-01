#!/usr/bin/env python3
"""What a GTK 3 row says of its place after its list dropped it: the live check's
step 2a (LIVE_CHECK.md, M86 L1), on the GTK 3 fixture only.

  python3 fixtures/linux/removed_row.py --pid PID [--name Alpha] [--seconds 20]

PID is the GTK 3 fixture's own process (`fixture_gtk3.py`); any other process is
refused before a bus is opened. The script finds the row named NAME (Alpha, a
child of the Folders list's Projects, which starts expanded) and prints its
`Parent`, its `GetIndexInParent`, whether that parent still lists it at that
index, and its state bits. Then it asks you to collapse Projects, which drops the
row from the list, and reads the same again once a second for SECONDS, each line
with the time since you pressed Enter.

compux finds a reference by an object's own account of its place on Qt only,
because a destroyed GTK 3 object was measured to keep answering its old parent and
index for about 16 s. This shows whether yours does.

Read-only, and only the fixture's objects are read. The calls are exactly these:
`org.a11y.Bus.GetAddress` on the session bus; on the accessibility bus,
`GetConnectionUnixProcessID` from the bus daemon and the registry's
`GetChildren`; then on the fixture's objects only `GetChildren`, `GetRole`,
`GetState`, `GetIndexInParent`, `GetChildAtIndex` and single `Properties.Get`
reads of `Name` and `Parent`. Never `Action.GetActions`, never
`Properties.GetAll`, no action, no event registration, no setting.
"""

import argparse
import sys
import time
from collections import deque

from gi.repository import Gio, GLib

CALL_TIMEOUT_MS = 2000
# The fixture's whole tree is a few hundred nodes; this bounds the search.
MAX_NODES = 4000
TABLE_CELL = 56
REGISTRY = ("org.a11y.atspi.Registry", "/org/a11y/atspi/accessible/root")
ACCESSIBLE = "org.a11y.atspi.Accessible"
PROPERTIES = "org.freedesktop.DBus.Properties"
FIXTURE = "fixture_gtk3.py"


def call(bus, name, path, interface, method, args, reply):
    result = bus.call_sync(name, path, interface, method, args,
                           GLib.VariantType.new(reply), Gio.DBusCallFlags.NONE,
                           CALL_TIMEOUT_MS, None)
    return result.unpack()


def prop(bus, name, path, prop_name):
    (value,) = call(bus, name, path, PROPERTIES, "Get",
                    GLib.Variant("(ss)", (ACCESSIBLE, prop_name)), "(v)")
    return value


def accessibility_bus():
    session = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    (address,) = call(session, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus",
                      "GetAddress", None, "(s)")
    flags = (Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT
             | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION)
    return Gio.DBusConnection.new_for_address_sync(address, flags, None, None)


def refuse_unless_fixture(pid):
    try:
        with open(f"/proc/{pid}/cmdline", "rb") as handle:
            cmdline = handle.read().replace(b"\0", b" ").decode(errors="replace")
    except OSError as error:
        sys.exit(f"removed_row.py: no process {pid}: {error}")
    if FIXTURE not in cmdline:
        sys.exit(f"removed_row.py: process {pid} is not the GTK 3 fixture ({cmdline.strip()})")


def fixture_root(bus, pid):
    """The registry's entry whose connection belongs to `pid`."""
    (apps,) = call(bus, *REGISTRY, ACCESSIBLE, "GetChildren", None, "(a(so))")
    for name, path in apps:
        (owner,) = call(bus, "org.freedesktop.DBus", "/org/freedesktop/DBus",
                        "org.freedesktop.DBus", "GetConnectionUnixProcessID",
                        GLib.Variant("(s)", (name,)), "(u)")
        if owner == pid:
            return name, path
    sys.exit(f"removed_row.py: process {pid} is not on the accessibility bus")


def find_row(bus, name, root, label):
    """The first table cell named `label` in the fixture's tree, breadth first."""
    queue, seen = deque([root]), 0
    while queue and seen < MAX_NODES:
        path = queue.popleft()
        seen += 1
        if node_named(bus, name, path, label):
            return path
        (children,) = call(bus, name, path, ACCESSIBLE, "GetChildren", None, "(a(so))")
        queue.extend(child for child_bus, child in children if child_bus == name)
    sys.exit(f"removed_row.py: no table cell named {label} in {seen} nodes")


def node_named(bus, name, path, label):
    (role,) = call(bus, name, path, ACCESSIBLE, "GetRole", None, "(u)")
    return role == TABLE_CELL and prop(bus, name, path, "Name") == label


def account(bus, name, path):
    """What the row says of its place now, each read on its own: an error is
    printed by its D-Bus name, never taken for an answer."""
    def read(what):
        try:
            return what()
        except GLib.Error as error:
            return f"error {Gio.DBusError.get_remote_error(error) or error.message}"

    parent = read(lambda: prop(bus, name, path, "Parent"))
    index = read(lambda: call(bus, name, path, ACCESSIBLE, "GetIndexInParent", None, "(i)")[0])
    states = read(lambda: call(bus, name, path, ACCESSIBLE, "GetState", None, "(au)")[0])
    listed = "unknown"
    # Only the fixture's own objects are asked: a parent on another bus is not read.
    on_fixture = isinstance(parent, tuple) and parent[0] == name
    if on_fixture and isinstance(index, int) and index >= 0:
        there = read(lambda: call(bus, parent[0], parent[1], ACCESSIBLE, "GetChildAtIndex",
                                  GLib.Variant("(i)", (index,)), "((so))")[0])
        listed = "yes" if there == (name, path) else f"no (it lists {there})"
    return f"parent={parent} index={index} listed_there={listed} states={states}"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--pid", type=int, required=True)
    parser.add_argument("--name", default="Alpha")
    parser.add_argument("--seconds", type=int, default=20)
    args = parser.parse_args()
    refuse_unless_fixture(args.pid)
    bus = accessibility_bus()
    name, root = fixture_root(bus, args.pid)
    row = find_row(bus, name, root, args.name)
    print(f"row {args.name}: {name} {row}")
    print(f"before: {account(bus, name, row)}")
    input("Collapse Projects in the Folders list now (click its triangle), then press Enter. ")
    start = time.monotonic()
    for _ in range(args.seconds):
        print(f"t=+{time.monotonic() - start:4.1f}s {account(bus, name, row)}", flush=True)
        time.sleep(1)


if __name__ == "__main__":
    main()
