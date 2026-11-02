#!/usr/bin/env python3
"""A plain window of one colour that writes down every click it gets (M86 L3).

The bound-window stage (bound.py) needs windows that are not the fixture: one in
front of it, so a covered window is really covered, and one behind the badge, so a
click that passes through the badge is seen to arrive somewhere. Each is this
script with its own title, colour and record file. It is a GTK 3 window, so it has
a pid and a title like any application's, and the window manager frames it.

  python3 panes.py --title "Compux Cover" --colour ff00ff --record /tmp/cover.json

The record is `{"presses": [{"x": root x, "y": root y, "button": n}, ...]}`,
rewritten whole through a rename on every press. Runs only inside the
private-session container.
"""

import argparse
import json
import os
import sys

import gi

gi.require_version("Gtk", "3.0")
gi.require_version("Gdk", "3.0")
from gi.repository import Gdk, Gtk  # noqa: E402

CONTAINER_MARKERS = ("/.dockerenv", "/run/.containerenv")
SIZE = (400, 300)


class Pane(Gtk.Window):
    def __init__(self, title, colour, record):
        super().__init__(title=title)
        self.record_path = record
        self.presses = []
        self.set_default_size(*SIZE)
        # The colour by CSS, which needs no cairo binding in the image.
        style = Gtk.CssProvider()
        style.load_from_data(f"window, eventbox {{ background-color: #{colour}; }}".encode())
        Gtk.StyleContext.add_provider_for_screen(
            Gdk.Screen.get_default(), style, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION)
        area = Gtk.EventBox()
        area.add_events(Gdk.EventMask.BUTTON_PRESS_MASK)
        area.connect("button-press-event", self.pressed)
        self.add(area)
        self.connect("destroy", Gtk.main_quit)
        self.write()

    def pressed(self, _area, event):
        self.presses.append({"x": round(event.x_root), "y": round(event.y_root),
                             "button": event.button})
        self.write()
        return True

    def write(self):
        partial = self.record_path + ".partial"
        with open(partial, "w", encoding="utf-8") as handle:
            json.dump({"presses": self.presses}, handle)
        os.replace(partial, self.record_path)


def main():
    if not any(os.path.exists(marker) for marker in CONTAINER_MARKERS):
        print("panes.py: runs only inside the fixture container", file=sys.stderr)
        sys.exit(2)
    parser = argparse.ArgumentParser()
    parser.add_argument("--title", required=True)
    parser.add_argument("--colour", required=True, help="rrggbb")
    parser.add_argument("--record", required=True)
    args = parser.parse_args()
    Pane(args.title, args.colour, args.record).show_all()
    Gtk.main()


if __name__ == "__main__":
    main()
