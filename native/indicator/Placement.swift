// Where the badge goes, as a function of the target's rectangle and the displays.
//
// Two coordinate spaces meet here, and getting them the wrong way round puts the badge
// on the wrong screen rather than nowhere, which is the failure nobody notices:
//
//   * the helper sends GLOBAL POINTS with a TOP-LEFT origin (the window server's
//     space): y grows downwards from the top of the PRIMARY display;
//   * AppKit places windows in a BOTTOM-LEFT space: y grows upwards from the bottom of
//     the primary display.
//
// So the only number that converts one to the other is the primary display's height,
// and a display above or to the left of the primary one has negative coordinates in
// both spaces. `ScreenLayout` carries exactly what the conversion needs and nothing
// AppKit-shaped, so every rule below is proved against a synthetic list of displays
// rather than against whatever is plugged into the build machine.

import CoreGraphics

/// One display, in AppKit's space. `visibleFrame` is the part the badge may use: it
/// excludes the menu bar and the Dock.
struct ScreenBox: Equatable {
    let frame: CGRect
    let visibleFrame: CGRect
}

/// The whole arrangement, plus the height that converts the two spaces.
struct ScreenLayout: Equatable {
    let screens: [ScreenBox]
    let primaryHeight: CGFloat
}

enum Placement {
    /// Between the badge and the window's top edge when it sits outside.
    static let gap: CGFloat = 6
    /// From the window's left edge when it has to sit inside.
    static let inset: CGFloat = 8
    /// The strip the traffic lights live in. Sitting below it is what "inside" means:
    /// the badge must never cover the buttons that close the person's window.
    static let titleBarHeight: CGFloat = 28

    // --- the size contract ---------------------------------------------------
    //
    // These two numbers, with `gap` and `inset` above, are a CONTRACT with the helper.
    // It decides `occluded` by hit-testing ONE rectangle covering both placements
    // (`native/compux/src/indicator.rs`, `badge_area`): a panel reaching outside that
    // rectangle is a panel the helper believes is clear while something is over it —
    // which is a badge drawn on a stranger's window, the one thing it must never do.
    // In the window server's top-left space, for a window at (x, y):
    //
    //   above the top edge   gap + badgeHeight             = 34 pt   (helper: 34)
    //   below the top edge   titleBarHeight + badgeHeight  = 56 pt   (helper: 80)
    //   across, from x       inset + maxBadgeWidth         = 448 pt
    //
    // The vertical half has room to spare; the horizontal half has none, so it is
    // enforced by a constraint rather than trusted to a layout that grows with a label.
    // The CLAMP is bounded by the same rectangle, so keeping the badge on a display can
    // never walk it out of the contract: where it will not fit, it is not drawn.

    /// The badge's height, always. `BadgeContent` pins its view to this and centres the
    /// row inside it, so no control's intrinsic height can quietly grow the panel.
    static let badgeHeight: CGFloat = 28

    /// The widest the badge may ever be — measured, not chosen.
    ///
    /// The real row was laid out off screen and asked for its fitting width: 283 pt for
    /// everything that is not the target's label (the glyph, "Fermix", the longest state
    /// word, the wider of Pause/Resume, the spacing and the insets), and 1131 pt for the
    /// worst case the label bounding can produce. 440 sits between them: about 155 pt of
    /// title, a readable twenty characters, and a contract that stays under 450 pt from
    /// the window's left edge. `BadgeContent` enforces it with a `<=` constraint on the
    /// view and a compressible label, so a long title costs characters and never costs
    /// the person a Stop button; the panel is narrower than this whenever the row is.
    static let maxBadgeWidth: CGFloat = 440

    /// How far above the window's top edge the badge may reach — the helper's
    /// `BADGE_ABOVE`, and exactly what the outside placement uses.
    static let contractAbove: CGFloat = gap + badgeHeight

    /// How far below it the badge may reach — the helper's `BADGE_INSIDE_DROP` (52)
    /// plus `BADGE_HEIGHT`. It allows the inside placement to sit up to 52 pt down;
    /// ours sits at `titleBarHeight`, well inside, which leaves the clamp its room.
    static let contractBelow: CGFloat = 52 + badgeHeight

    /// Every point the badge may be drawn on, in AppKit's space: the helper's
    /// `badge_area` seen from this side.
    static func contractRegion(_ target: CGRect) -> CGRect {
        CGRect(
            x: target.minX, y: target.maxY - contractBelow,
            width: inset + maxBadgeWidth, height: contractBelow + contractAbove)
    }

    /// The target's rectangle in AppKit's space.
    static func appKitRect(_ bounds: TargetState.Bounds, primaryHeight: CGFloat) -> CGRect {
        CGRect(
            x: bounds.x, y: primaryHeight - bounds.y - bounds.h,
            width: bounds.w, height: bounds.h)
    }

    /// Where the badge of this size belongs, or `nil` when it must not be shown at all.
    ///
    /// Hidden, and each of these is a rule rather than a fallback: the state is
    /// `unavailable` (there is nothing to point at), the target is occluded (the badge
    /// would sit on an unrelated window), the helper sent no bounds, the target is on no
    /// display this process can see, or the part of the contract's rectangle that is on
    /// that display is too small to hold the badge — a badge drawn outside the rectangle
    /// the helper hit-tests is a badge whose occlusion nobody is checking.
    static func badgeFrame(for state: TargetState, size: CGSize, layout: ScreenLayout) -> CGRect? {
        guard state.activity != .unavailable else { return nil }
        guard !state.occluded else { return nil }
        guard let bounds = state.bounds else { return nil }

        let target = appKitRect(bounds, primaryHeight: layout.primaryHeight)
        guard let screen = screen(showing: target, in: layout) else { return nil }

        // Where it may go at all: the contract's rectangle, and only the part of it this
        // display is showing.
        let allowed = contractRegion(target).intersection(screen.visibleFrame)
        guard allowed.width >= size.width, allowed.height >= size.height else { return nil }

        // Above the title bar when this screen has room for it, else just inside the
        // window's top-left, below the traffic lights.
        let above = CGRect(
            x: target.minX, y: target.maxY + gap, width: size.width, height: size.height)
        let inside = CGRect(
            x: target.minX + inset, y: target.maxY - titleBarHeight - size.height,
            width: size.width, height: size.height)
        let chosen = above.maxY <= screen.visibleFrame.maxY ? above : inside
        return clamp(chosen, into: allowed)
    }

    /// The display showing most of the target. A window straddling two screens gets the
    /// one it is mostly on, which is the one the person is looking at.
    static func screen(showing target: CGRect, in layout: ScreenLayout) -> ScreenBox? {
        var best: ScreenBox?
        var bestArea: CGFloat = 0
        for screen in layout.screens {
            let overlap = screen.frame.intersection(target)
            guard !overlap.isNull else { continue }
            let area = overlap.width * overlap.height
            guard area > bestArea else { continue }
            best = screen
            bestArea = area
        }
        return best
    }

    /// Keep the badge inside the part of the contract this display is showing, whatever
    /// the window did.
    static func clamp(_ rect: CGRect, into bounds: CGRect) -> CGRect {
        // `max(bounds.minX, ...)` matters when the badge is wider than the display:
        // the left edge wins, so it is never pushed off the screen entirely.
        let x = min(max(rect.minX, bounds.minX), max(bounds.minX, bounds.maxX - rect.width))
        let y = min(max(rect.minY, bounds.minY), max(bounds.minY, bounds.maxY - rect.height))
        return CGRect(x: x, y: y, width: rect.width, height: rect.height)
    }
}
