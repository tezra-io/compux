"""A window made and remade under one X id, through libxcb by ctypes (M86 L3).

Xlib picks a new id for every window; xcb lets the client name it. So a window can
be destroyed and a new one created under the SAME id, by the same process, with
the same title: everything the window list says about it is what it said before.
bound.py binds the first, closes it, makes the second, and asks the helper to act
on the binding: a reused id must never bring the binding back.

The window is an ordinary top-level the window manager frames, with `_NET_WM_PID`
set to this process, and it repaints itself in one colour on every expose from a
thread of its own, as an application would. A LAZY one has no background and
paints only on its first expose, as a hung application would: whatever is later
copied into it stays until something else draws there. A BORDERED one, on a
display with no window manager, keeps the X border its client gives it, in a
colour of its own. An OVERRIDE one is override-redirect, as a menu or a popup
is: no window manager frames, lists or moves it. Runs only inside the
private-session container; bound.py refuses to start anywhere else.
"""

import ctypes
import ctypes.util
import os
import threading

# X protocol numbers (xproto.h).
EXPOSE = 12
INPUT_OUTPUT = 1
CW_BACK_PIXEL = 1 << 1
CW_BORDER_PIXEL = 1 << 3
CW_OVERRIDE_REDIRECT = 1 << 9
CW_EVENT_MASK = 1 << 11
CONFIG_WINDOW_BORDER_WIDTH = 1 << 4
CONFIG_WINDOW_STACK_MODE = 1 << 6
STACK_MODE_ABOVE = 0
EXPOSURE_MASK = 1 << 15
GC_FOREGROUND = 1 << 2
PROP_MODE_REPLACE = 0
ATOM_STRING = 31
ATOM_WM_NAME = 39
ATOM_CARDINAL = 6
ATOM_WINDOW = 33
SIZE = (520, 320)
PLACE = (640, 560)


class _Iterator(ctypes.Structure):
    _fields_ = [("data", ctypes.c_void_p), ("rem", ctypes.c_int), ("index", ctypes.c_int)]


class _Screen(ctypes.Structure):
    _fields_ = [("root", ctypes.c_uint32), ("default_colormap", ctypes.c_uint32),
                ("white_pixel", ctypes.c_uint32), ("black_pixel", ctypes.c_uint32),
                ("current_input_masks", ctypes.c_uint32),
                ("width_in_pixels", ctypes.c_uint16), ("height_in_pixels", ctypes.c_uint16),
                ("width_in_millimeters", ctypes.c_uint16),
                ("height_in_millimeters", ctypes.c_uint16),
                ("min_installed_maps", ctypes.c_uint16), ("max_installed_maps", ctypes.c_uint16),
                ("root_visual", ctypes.c_uint32), ("backing_stores", ctypes.c_uint8),
                ("save_unders", ctypes.c_uint8), ("root_depth", ctypes.c_uint8),
                ("allowed_depths_len", ctypes.c_uint8)]


class _Rectangle(ctypes.Structure):
    _fields_ = [("x", ctypes.c_int16), ("y", ctypes.c_int16),
                ("width", ctypes.c_uint16), ("height", ctypes.c_uint16)]


def _library():
    xcb = ctypes.cdll.LoadLibrary(ctypes.util.find_library("xcb") or "libxcb.so.1")
    void, u8, u16, u32, i16 = (ctypes.c_void_p, ctypes.c_uint8, ctypes.c_uint16,
                               ctypes.c_uint32, ctypes.c_int16)
    signatures = {
        "xcb_connect": (void, [ctypes.c_char_p, ctypes.POINTER(ctypes.c_int)]),
        "xcb_connection_has_error": (ctypes.c_int, [void]),
        "xcb_get_setup": (void, [void]),
        "xcb_setup_roots_iterator": (_Iterator, [void]),
        "xcb_generate_id": (u32, [void]),
        "xcb_create_window": (ctypes.c_uint, [void, u8, u32, u32, i16, i16, u16, u16, u16, u16,
                                              u32, u32, ctypes.POINTER(u32)]),
        "xcb_destroy_window": (ctypes.c_uint, [void, u32]),
        "xcb_map_window": (ctypes.c_uint, [void, u32]),
        "xcb_configure_window": (ctypes.c_uint, [void, u32, u16, void]),
        "xcb_change_property": (ctypes.c_uint, [void, u8, u32, u32, u32, u8, u32, void]),
        "xcb_intern_atom": (ctypes.c_uint, [void, u8, u16, ctypes.c_char_p]),
        "xcb_intern_atom_reply": (void, [void, ctypes.c_uint, void]),
        "xcb_create_gc": (ctypes.c_uint, [void, u32, u32, u32, ctypes.POINTER(u32)]),
        "xcb_change_gc": (ctypes.c_uint, [void, u32, u32, void]),
        "xcb_change_window_attributes": (ctypes.c_uint, [void, u32, u32, void]),
        "xcb_poly_fill_rectangle": (ctypes.c_uint, [void, u32, u32, u32,
                                                    ctypes.POINTER(_Rectangle)]),
        "xcb_wait_for_event": (void, [void]),
        "xcb_flush": (ctypes.c_int, [void]),
    }
    for name, (result, arguments) in signatures.items():
        function = getattr(xcb, name)
        function.restype, function.argtypes = result, arguments
    return xcb


_libc = ctypes.CDLL(None)
_libc.free.argtypes = [ctypes.c_void_p]


class Victim:
    """One window id, made and remade. The connection is never closed by hand: the
    repaint thread is blocked on it, and it closes, with any window still standing,
    when this process exits."""

    def __init__(self, title, colour, lazy=False, place=PLACE, display=None, border=(0, 0),
                 override=False):
        self.xcb = _library()
        self.title = title.encode()
        self.colour = colour
        self.lazy, self.place, self.painted = lazy, place, False
        self.override = override
        self.border, self.border_colour = border
        self.conn = self.xcb.xcb_connect(display.encode() if display else None, None)
        if not self.conn or self.xcb.xcb_connection_has_error(self.conn):
            raise RuntimeError("the private X server did not answer xcb")
        screen = _Screen.from_address(self.xcb.xcb_setup_roots_iterator(
            self.xcb.xcb_get_setup(self.conn)).data)
        self.root, self.visual, self.depth = screen.root, screen.root_visual, screen.root_depth
        self.id = self.xcb.xcb_generate_id(self.conn)
        self.gc = self.xcb.xcb_generate_id(self.conn)
        self.xcb.xcb_create_gc(self.conn, self.gc, self.root, GC_FOREGROUND,
                               (ctypes.c_uint32 * 1)(colour))
        self.atoms = {name: self._atom(name) for name in
                      (b"_NET_WM_PID", b"_NET_WM_NAME", b"UTF8_STRING",
                       b"_NET_CLIENT_LIST_STACKING", b"_NET_CLIENT_LIST")}
        threading.Thread(target=self._repaint, daemon=True).start()

    def _atom(self, name):
        cookie = self.xcb.xcb_intern_atom(self.conn, 0, len(name), name)
        reply = self.xcb.xcb_intern_atom_reply(self.conn, cookie, None)
        if not reply:
            raise RuntimeError(f"no atom for {name!r}")
        atom = ctypes.c_uint32.from_address(reply + 8).value
        _libc.free(reply)
        return atom

    def _repaint(self):
        """Fill the window on every expose. Bounded by the connection: it ends when
        `xcb_wait_for_event` returns nothing, which it does once disconnected."""
        everything = _Rectangle(0, 0, SIZE[0], SIZE[1])
        while True:
            event = self.xcb.xcb_wait_for_event(self.conn)
            if not event:
                return
            kind = ctypes.c_uint8.from_address(event).value & 0x7F
            window = ctypes.c_uint32.from_address(event + 4).value
            _libc.free(event)
            if kind == EXPOSE and window == self.id and not (self.lazy and self.painted):
                self.xcb.xcb_poly_fill_rectangle(self.conn, self.id, self.gc, 1,
                                                 ctypes.byref(everything))
                self.xcb.xcb_flush(self.conn)
                self.painted = True

    def create(self):
        """The window, under this victim's one id, mapped and titled."""
        # A lazy window has no background: the server paints nothing for it.
        # Values go in the order of their mask bits.
        if self.lazy:
            mask, values = CW_EVENT_MASK, (ctypes.c_uint32 * 1)(EXPOSURE_MASK)
        else:
            mask = CW_BACK_PIXEL | CW_BORDER_PIXEL | CW_OVERRIDE_REDIRECT | CW_EVENT_MASK
            values = (ctypes.c_uint32 * 4)(self.colour, self.border_colour, int(self.override),
                                           EXPOSURE_MASK)
        self.xcb.xcb_create_window(self.conn, self.depth, self.id, self.root, self.place[0],
                                   self.place[1], SIZE[0], SIZE[1], self.border, INPUT_OUTPUT,
                                   self.visual, mask, values)
        self._property(ATOM_WM_NAME, ATOM_STRING, 8, self.title)
        self._property(self.atoms[b"_NET_WM_NAME"], self.atoms[b"UTF8_STRING"], 8, self.title)
        pid = (ctypes.c_uint32 * 1)(os.getpid())
        self._property(self.atoms[b"_NET_WM_PID"], ATOM_CARDINAL, 32, pid)
        self.xcb.xcb_map_window(self.conn, self.id)
        self.xcb.xcb_flush(self.conn)

    def listed_on_root(self):
        """On a display with no window manager, the root's client lists name this
        window alone, as a window manager's would."""
        ids = (ctypes.c_uint32 * 1)(self.id)
        for name in (b"_NET_CLIENT_LIST_STACKING", b"_NET_CLIENT_LIST"):
            self._property(self.atoms[name], ATOM_WINDOW, 32, ids, window=self.root)
        self.xcb.xcb_flush(self.conn)

    def set_colour(self, colour):
        """Paint in `colour` from the next expose on, its background included.
        Nothing is drawn now."""
        self.colour = colour
        value = ctypes.cast((ctypes.c_uint32 * 1)(colour), ctypes.c_void_p)
        self.xcb.xcb_change_gc(self.conn, self.gc, GC_FOREGROUND, value)
        self.xcb.xcb_change_window_attributes(self.conn, self.id, CW_BACK_PIXEL, value)
        self.xcb.xcb_flush(self.conn)

    def set_border(self, width):
        """A new X border width, the window's size unchanged."""
        self.xcb.xcb_configure_window(self.conn, self.id, CONFIG_WINDOW_BORDER_WIDTH,
                                      ctypes.cast((ctypes.c_uint32 * 1)(width), ctypes.c_void_p))
        self.xcb.xcb_flush(self.conn)

    def raise_to_top(self):
        """Above every other window, as a window raised with no window manager
        to say otherwise goes."""
        self.xcb.xcb_configure_window(self.conn, self.id, CONFIG_WINDOW_STACK_MODE,
                                      ctypes.cast((ctypes.c_uint32 * 1)(STACK_MODE_ABOVE),
                                                  ctypes.c_void_p))
        self.xcb.xcb_flush(self.conn)

    def _property(self, name, kind, bits, data, window=None):
        count = len(data)
        buffer = data if isinstance(data, ctypes.Array) else ctypes.create_string_buffer(data, count)
        self.xcb.xcb_change_property(self.conn, PROP_MODE_REPLACE, window or self.id, name, kind,
                                     bits, count, ctypes.cast(buffer, ctypes.c_void_p))

    def destroy(self):
        self.xcb.xcb_destroy_window(self.conn, self.id)
        self.xcb.xcb_flush(self.conn)
