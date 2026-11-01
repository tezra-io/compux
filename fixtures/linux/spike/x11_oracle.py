"""What the private X server says, read past compux (M86 L2): the pixel stage's
ORACLE and its one piece of setup, through libX11 by ctypes.

Runs INSIDE the private-session container only, against the Xvfb that
private-session.sh started; it is imported by pixels.py, which refuses to run
anywhere else. It never posts input: it paints the root window's background once,
and reads the pointer's buttons and the keyboard's keys.
"""

import ctypes
import ctypes.util

# The root background's tile: 2x2 blocks of BLOCK pixels, one colour each, from
# the root's origin. Any point of the root that no window covers shows
# TILE[(y // BLOCK) % 2][(x // BLOCK) % 2].
BLOCK = 32
TILE = ((0xD03020, 0x20B040), (0x2040D0, 0xF0F0F0))

# The core pointer's button masks (X.h).
BUTTON1_MASK = 1 << 8
BUTTON_MASKS = 0x1F << 8


def _library():
    x11 = ctypes.cdll.LoadLibrary(ctypes.util.find_library("X11") or "libX11.so.6")
    x11.XOpenDisplay.restype = ctypes.c_void_p
    x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
    x11.XCloseDisplay.argtypes = [ctypes.c_void_p]
    x11.XDefaultRootWindow.restype = ctypes.c_ulong
    x11.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
    x11.XDefaultScreen.argtypes = [ctypes.c_void_p]
    x11.XDefaultDepth.argtypes = [ctypes.c_void_p, ctypes.c_int]
    x11.XCreatePixmap.restype = ctypes.c_ulong
    x11.XCreatePixmap.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_uint,
                                  ctypes.c_uint, ctypes.c_uint]
    x11.XFreePixmap.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
    x11.XCreateGC.restype = ctypes.c_void_p
    x11.XCreateGC.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_ulong, ctypes.c_void_p]
    x11.XFreeGC.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
    x11.XSetForeground.argtypes = [ctypes.c_void_p, ctypes.c_void_p, ctypes.c_ulong]
    x11.XFillRectangle.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_void_p,
                                   ctypes.c_int, ctypes.c_int, ctypes.c_uint, ctypes.c_uint]
    x11.XSetWindowBackgroundPixmap.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_ulong]
    x11.XClearWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
    x11.XSync.argtypes = [ctypes.c_void_p, ctypes.c_int]
    pointer = ctypes.POINTER
    x11.XQueryPointer.argtypes = [ctypes.c_void_p, ctypes.c_ulong, pointer(ctypes.c_ulong),
                                  pointer(ctypes.c_ulong), pointer(ctypes.c_int),
                                  pointer(ctypes.c_int), pointer(ctypes.c_int),
                                  pointer(ctypes.c_int), pointer(ctypes.c_uint)]
    return x11


class Display:
    """One connection to the session's X server, closed on leaving the block."""

    def __init__(self):
        self.x11 = _library()
        self.display = None

    def __enter__(self):
        self.display = self.x11.XOpenDisplay(None)
        if not self.display:
            raise RuntimeError("the private X server did not answer")
        return self

    def __exit__(self, *_exc):
        self.x11.XCloseDisplay(self.display)

    def paint_root(self):
        """Tile the root's background with TILE, as a desktop wallpaper would be."""
        x11, display = self.x11, self.display
        root = x11.XDefaultRootWindow(display)
        depth = x11.XDefaultDepth(display, x11.XDefaultScreen(display))
        tile = x11.XCreatePixmap(display, root, 2 * BLOCK, 2 * BLOCK, depth)
        gc = x11.XCreateGC(display, tile, 0, None)
        for row, colours in enumerate(TILE):
            for column, colour in enumerate(colours):
                x11.XSetForeground(display, gc, colour)
                x11.XFillRectangle(display, tile, gc, column * BLOCK, row * BLOCK, BLOCK, BLOCK)
        x11.XSetWindowBackgroundPixmap(display, root, tile)
        x11.XClearWindow(display, root)
        x11.XFreeGC(display, gc)
        x11.XFreePixmap(display, tile)
        x11.XSync(display, 0)

    def buttons_down(self):
        """The core pointer's button mask: which buttons the server holds down."""
        x11, display = self.x11, self.display
        root = x11.XDefaultRootWindow(display)
        windows = [ctypes.c_ulong(), ctypes.c_ulong()]
        places = [ctypes.c_int() for _ in range(4)]
        mask = ctypes.c_uint()
        x11.XQueryPointer(display, root, *(ctypes.byref(w) for w in windows),
                          *(ctypes.byref(p) for p in places), ctypes.byref(mask))
        return mask.value & BUTTON_MASKS


def expected_colour(x, y):
    """The root background's colour at root pixel (x, y), as 0xRRGGBB."""
    return TILE[(y // BLOCK) % 2][(x // BLOCK) % 2]
