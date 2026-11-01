#!/usr/bin/env python3
"""Dump one application's whole AT-SPI tree, for the coverage half of M86 L0(a).

Runs INSIDE the private-session container only, and refuses (exit 2) anywhere
without a container's marker file, before it opens any bus. Like `a11y_roots.py`
it makes raw D-Bus calls, so the calls it makes are exactly these: `GetAddress` on
the session bus; on the accessibility bus `GetChildren`, `GetRole`, `GetRoleName`,
`GetState`, `GetInterfaces`, single `Properties.Get` reads (`Name`, `Description`,
`ChildCount`, `Action.NActions`), `Component.GetExtents` and `Action.GetName(i)`
only where the interface is listed. Never `Action.GetActions` and never
`Properties.GetAll`.

  python3 a11y_tree.py --app-window "Compux Fixture GTK3" --out /out/gtk3-tree.json
"""

import argparse
import json
import os
import sys
import time

from gi.repository import Gio, GLib

CONTAINER_MARKERS = ("/.dockerenv", "/run/.containerenv")
CALL_TIMEOUT_MS = 2000
WAIT_TRIES = 100
WAIT_STEP_S = 0.2
MAX_NODES = 20000
REGISTRY = ("org.a11y.atspi.Registry", "/org/a11y/atspi/accessible/root")
ACCESSIBLE = "org.a11y.atspi.Accessible"
NULL_PATH = "/org/a11y/atspi/null"


def refuse_outside_a_container():
    if not any(os.path.exists(marker) for marker in CONTAINER_MARKERS):
        print("a11y_tree.py: runs only inside the fixture container", file=sys.stderr)
        sys.exit(2)


def call(bus, name, path, interface, method, args, reply):
    result = bus.call_sync(name, path, interface, method, args,
                           GLib.VariantType.new(reply) if reply else None,
                           Gio.DBusCallFlags.NONE, CALL_TIMEOUT_MS, None)
    return result.unpack() if result is not None else None


def accessibility_bus():
    session = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    (address,) = call(session, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus",
                      "GetAddress", None, "(s)")
    flags = (Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT
             | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION)
    return Gio.DBusConnection.new_for_address_sync(address, flags, None, None)


def prop(bus, ref, interface, name):
    (value,) = call(bus, ref[0], ref[1], "org.freedesktop.DBus.Properties", "Get",
                    GLib.Variant("(ss)", (interface, name)), "(v)")
    return value


def safe(read):
    try:
        return read()
    except GLib.Error as error:
        return {"error": error.message.split(":")[0][:120]}


def bits(words):
    return [word_index * 32 + bit for word_index, word in enumerate(words)
            for bit in range(32) if word & (1 << bit)]


def read_node(bus, ref):
    node = {"bus": ref[0], "path": ref[1]}
    node["role"] = safe(lambda: call(bus, *ref, ACCESSIBLE, "GetRole", None, "(u)")[0])
    node["role_name"] = safe(lambda: call(bus, *ref, ACCESSIBLE, "GetRoleName", None, "(s)")[0])
    node["name"] = safe(lambda: prop(bus, ref, ACCESSIBLE, "Name"))
    node["description"] = safe(lambda: prop(bus, ref, ACCESSIBLE, "Description"))
    node["states"] = safe(lambda: bits(call(bus, *ref, ACCESSIBLE, "GetState", None, "(au)")[0]))
    node["interfaces"] = safe(lambda: call(bus, *ref, ACCESSIBLE, "GetInterfaces", None,
                                           "(as)")[0])
    node["child_count"] = safe(lambda: prop(bus, ref, ACCESSIBLE, "ChildCount"))
    interfaces = node["interfaces"] if isinstance(node["interfaces"], list) else []
    if "org.a11y.atspi.Component" in interfaces:
        node["extents"] = safe(lambda: call(bus, *ref, "org.a11y.atspi.Component", "GetExtents",
                                            GLib.Variant("(u)", (0,)), "((iiii))")[0])
        # The same, relative to the window (ATSPI_COORD_TYPE_WINDOW), to compare.
        node["extents_window"] = safe(lambda: call(
            bus, *ref, "org.a11y.atspi.Component", "GetExtents", GLib.Variant("(u)", (1,)),
            "((iiii))")[0])
    if "org.a11y.atspi.Action" in interfaces:
        count = safe(lambda: prop(bus, ref, "org.a11y.atspi.Action", "NActions"))
        node["actions"] = [safe(lambda i=i: call(bus, *ref, "org.a11y.atspi.Action", "GetName",
                                                 GLib.Variant("(i)", (i,)), "(s)")[0])
                           for i in range(count if isinstance(count, int) else 0)]
    return node


def children(bus, ref):
    listed = safe(lambda: call(bus, *ref, ACCESSIBLE, "GetChildren", None, "(a(so))")[0])
    if isinstance(listed, dict):
        return []
    return [tuple(child) for child in listed if child[0] and child[1] != NULL_PATH]


def find_application(bus, window_name):
    seen = set()
    for _ in range(WAIT_TRIES):
        for app in children(bus, REGISTRY):
            seen.add(f"app {safe(lambda: prop(bus, app, ACCESSIBLE, 'Name'))}")
            for window in children(bus, app):
                name = safe(lambda: prop(bus, window, ACCESSIBLE, "Name"))
                seen.add(f"window {name}")
                # A browser may add its own name after the page's title.
                if isinstance(name, str) and name.startswith(window_name):
                    return app
        time.sleep(WAIT_STEP_S)
    raise SystemExit(f"a11y_tree.py: no application with a window named {window_name!r}; "
                     f"seen: {sorted(seen)[:20]}")


def dump(bus, app):
    nodes, stack = [], [(app, 0, -1)]
    while stack and len(nodes) < MAX_NODES:
        ref, depth, parent = stack.pop()
        node = read_node(bus, ref)
        node.update(depth=depth, parent=parent, index=len(nodes))
        nodes.append(node)
        for child in reversed(children(bus, ref)):
            stack.append((child, depth + 1, node["index"]))
    return nodes


def main():
    refuse_outside_a_container()
    parser = argparse.ArgumentParser()
    parser.add_argument("--app-window", required=True)
    parser.add_argument("--out", required=True)
    args = parser.parse_args()
    bus = accessibility_bus()
    app = find_application(bus, args.app_window)
    nodes = dump(bus, app)
    with open(args.out, "w") as handle:
        json.dump({"window": args.app_window, "nodes": nodes}, handle, indent=1)
    print(f"{args.app_window}: {len(nodes)} nodes")


if __name__ == "__main__":
    main()
