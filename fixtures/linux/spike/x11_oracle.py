"""What the private X server says, read past compux (M86 L2): the pixel stage's
ORACLE and its one piece of setup, through libX11 by ctypes.

Runs INSIDE the private-session container only, against the Xvfb that
private-session.sh started (or, named, a second one bound.py started); it is
imported by pixels.py and bound.py, which refuse to run anywhere else. It never
posts input: it paints the root window's background once, reads the pointer's
buttons, the screen's pixels and the root's stacking order, and sets the input
region of a window the stage made itself.
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
# XGetImage's format, and all planes (X.h); SHAPE's input kind and its set operation (shape.h).
Z_PIXMAP = 2
ALL_PLANES = 0xFFFFFFFF
SHAPE_INPUT = 2
SHAPE_SET = 0
UNSORTED = 0


class _Image(ctypes.Structure):
    """The leading fields of Xlib's XImage, as far as reading its pixels needs."""
    _fields_ = [("width", ctypes.c_int), ("height", ctypes.c_int), ("xoffset", ctypes.c_int),
                ("format", ctypes.c_int), ("data", ctypes.c_void_p),
                ("byte_order", ctypes.c_int), ("bitmap_unit", ctypes.c_int),
                ("bitmap_bit_order", ctypes.c_int), ("bitmap_pad", ctypes.c_int),
                ("depth", ctypes.c_int), ("bytes_per_line", ctypes.c_int),
                ("bits_per_pixel", ctypes.c_int)]


class _Rectangle(ctypes.Structure):
    _fields_ = [("x", ctypes.c_short), ("y", ctypes.c_short),
                ("width", ctypes.c_ushort), ("height", ctypes.c_ushort)]


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
    x11.XQueryTree.argtypes = [ctypes.c_void_p, ctypes.c_ulong, pointer(ctypes.c_ulong),
                               pointer(ctypes.c_ulong), pointer(ctypes.c_void_p),
                               pointer(ctypes.c_uint)]
    x11.XGetImage.restype = pointer(_Image)
    x11.XGetImage.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_int,
                              ctypes.c_uint, ctypes.c_uint, ctypes.c_ulong, ctypes.c_int]
    x11.XFree.argtypes = [ctypes.c_void_p]
    return x11


def _shape_library():
    xext = ctypes.cdll.LoadLibrary(ctypes.util.find_library("Xext") or "libXext.so.6")
    xext.XShapeCombineRectangles.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int,
                                             ctypes.c_int, ctypes.c_int,
                                             ctypes.POINTER(_Rectangle), ctypes.c_int,
                                             ctypes.c_int, ctypes.c_int]
    return xext


class Display:
    """One connection to the session's X server, closed on leaving the block."""

    def __init__(self, name=None):
        self.x11 = _library()
        self.name = name
        self.display = None

    def __enter__(self):
        self.display = self.x11.XOpenDisplay(self.name.encode() if self.name else None)
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


    def stacking(self):
        """The root's children, bottom to top, as the server stacks them."""
        x11, display = self.x11, self.display
        root, parent = ctypes.c_ulong(), ctypes.c_ulong()
        kids, count = ctypes.c_void_p(), ctypes.c_uint()
        x11.XQueryTree(display, x11.XDefaultRootWindow(display), ctypes.byref(root),
                       ctypes.byref(parent), ctypes.byref(kids), ctypes.byref(count))
        if not kids:
            return []
        found = list((ctypes.c_ulong * count.value).from_address(kids.value))
        x11.XFree(kids)
        return found

    def colour_share(self, area, colour, tolerance):
        """The share of the screen's pixels in root `area` (x, y, w, h) within
        `tolerance` of `colour`, read from the root, past compux."""
        x, y, w, h = area
        x11, display = self.x11, self.display
        image = x11.XGetImage(display, x11.XDefaultRootWindow(display), x, y, w, h, ALL_PLANES,
                              Z_PIXMAP)
        if not image:
            raise RuntimeError(f"the X server would not give the root's pixels at {area}")
        try:
            facts = image.contents
            if facts.bits_per_pixel != 32:
                raise RuntimeError(f"the root reads {facts.bits_per_pixel} bits a pixel, not 32")
            rows = ctypes.string_at(facts.data, facts.bytes_per_line * h)
        finally:
            x11.XFree(image.contents.data)
            x11.XFree(image)
        want = ((colour >> 16) & 0xFF, (colour >> 8) & 0xFF, colour & 0xFF)
        hits = 0
        for row in range(h):
            line = rows[row * facts.bytes_per_line:]
            for column in range(w):
                blue, green, red = line[column * 4:column * 4 + 3]
                hits += max(abs(a - b) for a, b in zip((red, green, blue), want)) <= tolerance
        return hits / (w * h) if w * h else 0.0

    def shape_input(self, window, rectangles):
        """Set `window`'s input region to `rectangles` (x, y, w, h), in its own
        pixels: it takes clicks there and nowhere else."""
        rects = (_Rectangle * len(rectangles))(*[_Rectangle(*r) for r in rectangles])
        _shape_library().XShapeCombineRectangles(self.display, window, SHAPE_INPUT, 0, 0, rects,
                                                 len(rectangles), SHAPE_SET, UNSORTED)
        self.x11.XSync(self.display, 0)


def expected_colour(x, y):
    """The root background's colour at root pixel (x, y), as 0xRRGGBB."""
    return TILE[(y // BLOCK) % 2][(x // BLOCK) % 2]
