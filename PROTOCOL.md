# The compux wire, protocol 12

This is the contract between a caller (`Compux.Protocol`, and the Fermix engine above it) and the `compux`
sidecar, for what protocol 12 adds and changes. Everything protocol 11 had and this page does not mention is
unchanged: the tagged frames, the generations, `mutation_seq`, `deadline_ms`, observations, bound targets and the
`check` kinds `image` and `none`. The README describes the protocol-11 surface in prose.

Protocol 12 is M86 stage P2: the model's surface, defined once for macOS and Linux. The macOS build answers all of
it. The Linux build answers the same wire through AT-SPI from M86 L1, all but `inspect`, and binds a window on X11
only (M86 L3, §1.2). The refusals only Linux answers are named here with the rest.

Every example is one JSON line on the wire, shown wrapped. Envelope fields (`sidecar_generation`,
`session_generation`, `authorization_generation`, `mutation_seq`, `deadline_ms`) are left out of the examples where
they are as protocol 11 had them.

## 1. `hello`

```json
{"type":"response","request_id":"h1","ok":true,"protocol_version":12,"compux_version":"0.10.1",
 "actions":["screenshot","left_click","right_click","double_click","mouse_move","left_click_drag","scroll",
            "type","key","wait","inspect","wait_for_change","paste","elements","windows","press","set_value",
            "focus","select","toggle","expand","collapse","scroll_to","wait_for","select_target","release_target"],
 "capabilities":{"input_methods":["foreground_hid","ax"],"controls":["pause","resume","release"],
                 "observations":{"max":3,"ttl_ms":30000},"targets":true,
                 "capture_methods":["display","window"],"indicator":"present"},
 "sidecar_generation":"7c1e…","session_generation":1}
```

- `protocol_version` is 12. A library at 12 refuses any other sidecar, as before.
- `actions` lists only what this build can do (protocol 11 rule, M86 F1).
  - **macOS:** the list above.
  - **Linux:** the list above without `inspect`. A request for `inspect` is `unknown_action`: a Linux session does
    not place every control (GTK 4, Wayland), so no control is read at a point. `select_target` and
    `release_target` are listed only on X11, the one session that can bind a window (§1.2); there `targets` is
    true and `capture_methods` is `["display","window"]`. Elsewhere `targets` is false, `capture_methods` is
    `["display"]` at most, and a `select_target` sent anyway is `target_binding_unavailable`. `indicator` says
    whether the X11 badge, `compux-indicator-x11`, is beside the helper.
- A Linux helper started with no display of its own takes the session's `DISPLAY`, `WAYLAND_DISPLAY` and
  `XAUTHORITY` from the user manager, as values; it never writes them into its environment. Its X connection is made
  to that display with that authority, so an adopted display is reached as the helper's own.
- **Linux pixels** go one route per session, decided once at start and never retried through another (§1.1). The
  pixel verbs are `screenshot`, the clicks, `mouse_move`, `left_click_drag`, `scroll`, `key`, `wait_for_change`,
  `paste`, `windows`, and `type` without `element_ref`. `actions` lists the ones this session's route serves, and
  only those: a session whose route is refused leaves them all out, `input_methods` is `["ax"]`, `capture_methods`
  is `[]`, and a pixel request (or any action asking for an image check) is refused `pixels_unavailable` with the
  route's reason before anything is sent.
- `input_methods` is `["foreground_hid","ax"]` on both builds where the session's pixel route serves input. `ax` is the wire's name for acting through the
  platform's accessibility API, whichever it is: macOS's AX API, or AT-SPI on Linux. A receipt's `input_method` is
  `"ax"` for every verb on a reference on both.
- `type` is listed on both builds because it types at the focus. Its `element_ref` form (§4.7) is offered where
  `capabilities.input_methods` contains `"ax"`.
- The `semantic` check with `until` (§5.2) is offered where `wait_for` is listed.

### 1.1 Linux pixel routes

| Session | Route | Pixel verbs served |
|---|---|---|
| X11 (`display_server: "x11"`) | x11rb, pure Rust: XTEST for the pointer and the keys, the core `GetImage` on the root in strips for the frame, RandR for the monitors, EWMH for `windows`, the `CLIPBOARD` selection owned in-process for `paste` | all |
| GNOME Wayland, XWayland beside it or not | the XDG desktop portal, under the app id `io.tezra.fermix.ComputerUse` registered through the host `Registry` (xdg-desktop-portal 1.18 or later): `Screenshot` for the frame, one RemoteDesktop session with a ScreenCast monitor source for the pointer and the keys | all but `paste` (no portal clipboard compux can own) and `windows` (no portal lists them) |
| GNOME Wayland whose portal has no `Registry` (xdg-desktop-portal before 1.18) | refused, naming the portal's version: without its own app id compux would share the empty one with every host process | none |
| GNOME Wayland whose portal is not running and does not start when asked, or answers an error that does not say whether it runs | refused, saying which | none |
| KDE Plasma, COSMIC, wlroots compositors on Wayland | refused, naming the compositor: no route is qualified there | none |
| no display answers | refused | none |

- **X11.** A monitor is in root pixels at a scale of 1, which is what XTEST takes too. Keys are the server's own
  keycodes from `GetKeyboardMapping`, read again when `MappingNotify` says the map changed; compux never changes the
  map. `type` is refused before a key is sent when a character is not on the layout's first group
  (`text_outside_layout`, naming `paste`) or the keyboard is not neutral: a group other than the first, a modifier
  held, Caps Lock on (`keyboard_not_neutral`). Keys go only into a window that reads `ACTIVE` on the accessibility
  bus (`no_active_window`). A key or button held across an action is written to a journal under
  `$XDG_RUNTIME_DIR/compux-held` before it is pressed and cleared after its release, so the next helper on that
  display releases what a killed one left held. A drag, a click or chord with modifiers, and `paste` hold input, so
  on X11 they need a journal they can write under `$XDG_RUNTIME_DIR`: without one they are refused before their
  first input, with an error that names the directory. Nothing is sent, not even the pointer's move to a drag's
  start. The check writes the journal and clears it again before each hold; a helper killed between the two leaves
  an empty journal, which the next start removes. `scroll` turns at most 50 notches, one press and release of a
  wheel button each. `paste` puts the text on the `CLIPBOARD`, which compux owns for the helper's life (answering
  `TARGETS` and `UTF8_STRING`, INCR for text past one property, 4 MiB at most) and releases at exit. The person's
  own text is read before a paste and put back after it; a text of theirs past 4 MiB is not read at all, never cut,
  so it is not put back.
- **GNOME Wayland.** Nothing is asked of the portal before the first pixel action. The first one starts the session
  (keyboard and pointer, `persist_mode` 2, one monitor), which shows the portal's dialog unless the restore token
  kept in `$XDG_STATE_HOME/compux/portal.json` still serves; each token is offered once and replaced by the one the
  start hands back. The session lives as long as the helper: closed by `release`, at exit, or by the person from the
  shell (the next pixel action then asks again, without a token). The display is the one monitor picked for the
  session, in the compositor's logical points; the frame is the portal's non-interactive screenshot, accepted only
  when the desktop is that one monitor (`capture_unavailable` otherwise). A portal answer is taken only from the
  process that owns the portal when the call is made; another process's is ignored. The portal saves each screenshot
  as a file and leaves it; compux reads the file the answer named only if it is a regular file (a link is refused,
  `capture_unavailable`, and neither it nor what it points at is touched), and removes that file once it has
  decoded, and no other. Whether the desktop is one monitor is judged by size alone, since the portal says nothing
  of the others: a desktop of four 1280×800 monitors in a 2×2 grid at 100% gives the same 2560×1600 screenshot as
  one 1280×800 monitor at 200%, and is taken as that one monitor. Keys go as keysyms, the session's first one after
  a Shift tap that types nothing; the pointer goes as absolute motion in the monitor stream's coordinates. The
  session's set-up steps (`CreateSession`, `SelectDevices`, `SelectSources`) show no dialog and are each waited on
  for 5 s, past which the portal is not answering (`capture_unavailable`, naming the step); only `Start` and
  `Screenshot` wait on the person. A dialog answered no is `consent_refused`; one ended by the portal, or not
  answered within 25 s, is `consent_timeout`. Keys go only into a window that reads `ACTIVE` (`no_active_window`);
  the compositor drops a character its layout cannot type without saying so, so `type` is not refused for one here.

### 1.2 Bound windows on X11

`select_target` and `release_target` work on X11 as on macOS (the README's "One window, bound"). What differs:

- **The windows.** `windows` reads the window manager's `_NET_CLIENT_LIST_STACKING`, front to back, at most 512
  clients (a longer list is refused, never cut). A window's `bounds` are what the person sees: the client's
  rectangle less the shadow GTK draws inside it (`_GTK_FRAME_EXTENTS`), plus the frame the window manager draws
  around it (`_NET_FRAME_EXTENTS`). Its process is its `_NET_WM_PID`.
- **The binding** is the window's X id, the X connection that watches it from the moment it is bound, and its
  process with that process's start time. A window that names no process (no `_NET_WM_PID`, as a Qt popup) is
  refused `target_binding_unavailable`, and so is one whose process, read again once the watch holds it, is not
  what `windows` listed (its id was given to another window in between; take `windows` again). Its title is not
  compared, since windows retitle themselves. A window destroyed after it was bound is `target_unavailable` for
  the rest of the binding, even when the X server has given its id to a new window of the same process: each
  request first has the watch read everything the server sent it, so a window closed before the request is never
  answered with its last picture.
- **Frames** come from Composite: the window is redirected (Automatic) unless a compositing manager (GNOME Shell's
  Mutter) already redirects it, and its pixmap is named and read, so a window another one covers is still seen. A
  redirect fills the new pixmap with what the screen showed there, another window's pixels where one covered it, so
  no pixel is sent until it is proved the window's own, whoever redirected it. Every window the picture is drawn
  from (the window manager's frame and its decorations, the client and the client's own windows) is cleared to its
  background and sent an Expose (`ClearArea` with exposures), and the drawing that follows must reach every pixel
  of the picture, frame included, within 1 s. Clearing and that Expose are the one side effect a selection has: the
  application repaints, and under a window manager that composites nothing the window flashes its background once.
  A window that does not repaint in time (a hung application, or one that draws only once) is `capture_unavailable`
  and nothing is held, and so is a selection the X server does not let finish (the window watched, proved and its
  first frame read) within 3 s. The proof does not depend on which windows are over it, so a window moving
  meanwhile changes nothing; one resized, hidden or shown during it is refused at selection, and proved again
  later. A window mapped again, resized or given a new border width is proved the same way before its next frame,
  and nothing of it is read until that holds; a proof that falls short then is tried again at each of the next
  three intervals, and the binding ends (`capture_unavailable`) if none holds. While it waits, a request for its
  picture is `capture_unavailable` (retry in a moment), never the last frame: that one has the window's old size
  and contents. Any part of a picture the proof did not reach (the frame extents moved it) is sent black. The
  picture is the window's listed bounds, frame included, in root pixels at a scale of 1. A damaged window is read
  again at most ten times a second; an undamaged one reports idle frames.
- **Pixel actions** on a bound window need it topmost at the point, as on macOS (`target_obstructed`, naming the
  window in front). In front means a window the window manager lists in front of it, or any window the X server
  stacks above it that takes input at that point: menus, tooltips, notifications and a desktop shell's own
  windows are override-redirect and in no list, and are read from the root's children in the server's stacking
  order, each with its SHAPE input region (none is the whole window and its border). One of those is named by
  its class or title, or as "a menu or popup". Nothing moves the pointer or grabs anything to tell. A program
  holding a pointer grab with no window at the point takes the click all the same, and cannot be seen this
  way.
- **Controls.** `ax_binding` is `bound` when exactly one of the application's AT-SPI windows has the X window's
  title and place (with or without the window manager's frame). GTK 4 places no window on X11 (its screen extents
  are 0, 0), so a GTK 4 window binds with `ax_binding: "unavailable"`, and `elements` in it is
  `ax_binding_unavailable`.
- **The badge** is `compux-indicator-x11`, a second binary beside the helper, which starts it when a window is
  bound and ends it at release (its pipe closes) or with the helper. It speaks the macOS badge's line wire, and its
  Pause and Stop reach the same gate. It is an override-redirect window, so the window manager never manages or
  focuses it: 28 pixels high and at most 440 wide, at the window's left above its top edge, or just inside that
  edge where there is no room above, kept on the window's monitor and out of its panels. Only its two buttons take
  clicks (a Shape input region); a click anywhere else on it reaches the window under it. A button event another
  program sends to it (`SendEvent`) is dropped and logged, so no program can pause, resume or stop the session that
  way; input made through XTEST is the server's own, as the person's is, and nothing on X11 tells the two apart,
  for the badge or any other window. It hides while the window is minimized or gone, or while another window covers
  the area it would take. It goes back, at most ten times a second, just over a window the window manager manages
  that moves, restacks or maps onto it (the bound window raised over it among them), never higher, and never over
  an override-redirect one: the person's own menus and tooltips stay over it. While one of those is open over it
  (above it, and taking input where the badge is), it does not raise itself at all, for a new state either, and
  goes back on top when that window is unmapped, or once it no longer takes input there (it looks again every half
  second while it waits). "Never higher" is about a window that comes over it: on a new state from the helper, and
  when the window that held it under closes or stops taking input there, it goes back over every window. Its text
  is Noto Sans Regular, built in, at 13 pixels, in root pixels at a scale of 1 on any screen. It draws glyphs in
  the order the characters come, with kerning and no shaping: right-to-left text reads in the wrong order, and a
  character the font lacks (CJK, among others) is its missing-glyph box. It is a real window, so it shows in
  display screenshots, never in the bound window's frames. A badge that cannot show itself (no SHAPE 1.1, or a
  display other than 24-bit TrueColor with 32-bit pixels) exits, and a mutation on the window is then
  `control_surface_unavailable`.

## 2. `elements`

### 2.1 Request

```json
{"type":"request","request_id":"r4","action":"elements",
 "scope":"window","query":{"role":"button","name":"save"},"skeleton":false,
 "root":"e12","observation_id":"7c1e-3","region":{"x":0,"y":0,"w":800,"h":600},"target_id":"t1"}
```

Every parameter is optional. A protocol-11 request (none of the new ones) reads exactly what it read before.

| Param | Value | Meaning |
|---|---|---|
| `scope` | `window`, `app`, `focused`, `menu`, `dialog` | Where the walk starts (below). Absent: `window` when the request names a target, else `app`, which is what protocol 11 read. Refused beside `root`. |
| `query` | `{"role": R, "name": N}`, at least one of the two | A find: only the nodes that match are listed, each with a reference. `role` is a normalised role (§6.1), compared exactly. `name` is compared case-insensitively as a substring of the node's text: its label, or for a `text` node (static text) its value. A match is listed whether or not it is a control. |
| `skeleton` | `true` | Cut the walk at depth 3 below the scope's root, or below `root` on a drill (§2.3). Refused beside `query`. |
| `root` | a reference, e.g. `"e12"` | Drill into that control's subtree. `query` or `skeleton` beside it are honoured inside the subtree. Needs `observation_id` naming the reply that listed it. Refused beside `scope`. |
| `observation_id`, `region`, `target_id`, `display` | as protocol 11 | With `root` and no `region`, the view is the named listing's own view, so the new click points are in the same pixels as the listing's. |

The scopes, on macOS:

| Scope | Root of the walk | Root listed |
|---|---|---|
| `app` | The application element of the application in view (as protocol 11), or of the bound window's application. Refused with a target: a bound window's controls are the window's (release the target to list the application). | no |
| `window` | The bound window; without a target, the application's focused window (`AXFocusedWindow`). | no |
| `focused` | The application's focused element (`AXFocusedUIElement`), and what is inside it. With a target, only one whose `AXWindow` is the bound window. | yes |
| `menu` | Every menu open in the application (§6.2): a context menu, a pop-up's menu, a menu-bar menu. With a target, only a menu inside the bound window: the menu bar's menus and a menu in a window of its own (a context menu, a pop-up's menu) are the application's, not the window's. | no |
| `dialog` | Every dialog or alert open in the application (§6.2), sheets included. With a target, the bound window itself when it is one and those inside it (a sheet): a dialog in a window of its own is the application's, not the window's. | no |

The scopes, on Linux (AT-SPI):

| Scope | Root of the walk | Root listed |
|---|---|---|
| `app` | The application's root on the accessibility bus: the application whose window reads `ACTIVE`, or the bound window's. | no |
| `window` | The bound window; without a target, the application's window (`frame`) that reads `ACTIVE`. | no |
| `focused` | The node that reads `FOCUSED` inside that window, found breadth first among at most 512 nodes (AT-SPI has no focused-element query). With a target, inside the bound window while it reads `ACTIVE`: focus in another window of the application, a dialog of the bound window included, is not the bound window's. | yes |
| `menu` | Every menu open in the application (§6.2). With a target, the bound window's: one inside it (a menu-bar menu, a GTK 4 popover's menu), and one in a window of its own whose X window is `WM_TRANSIENT_FOR` the bound window, directly or through at most 8 such links (a GTK 3 pop-up or combo box menu). A menu whose window names no owner (Chromium's) is the application's, not the window's. | no |
| `dialog` | Every dialog or alert open in the application (§6.2). With a target, the bound window itself when it is one, those inside it, and one in a window of its own that is `WM_TRANSIENT_FOR` the bound window as above (a GTK 3 or Qt dialog). | no |

A scope whose root does not exist (no focused window, nothing focused, no menu or dialog open) is refused
`scope_not_found`, with a detail that names what is missing. A `menu` or `dialog` scope whose scan for surfaces
(§6.2) stopped before it finished and found none is refused the same way, and its detail says the look stopped
short, not that nothing is open: look again, or list the window and find the controls there.

A target confines `focused`, `menu` and `dialog` to the bound window, as the tables say. Focus in another window of
the application is refused `scope_not_found`, and the detail says it is there: list the bound window, a dialog or
menu of it, or release the target to read the application's focus. A `menu` or `dialog` scope that finds none of the
bound window's is refused `scope_not_found` saying so: one open in another window of the application is not this
window's, and releasing the target lists the application's. A window is tied to its accessibility element as the
binding ties the bound one (§1.2): exactly one of the application's windows on screen sits at the element's frame. On
Linux these are the window manager's clients and the root's own children, where an override-redirect menu sits; a
window server that will not list them is `target_unavailable`. Without a target nothing here changes.

### 2.2 Reply

```json
{"type":"response","request_id":"r4","ok":true,
 "observation_id":"7c1e-4","observation_kind":"semantic","captured_at_monotonic_ns":81234500000,
 "scope":"window",
 "elements":[
   {"element_ref":"e1","role":"button","native_role":"AXButton","label":"Save","enabled":true,
    "verbs":["press","focus","scroll_to"],"path":["Document","Toolbar"],
    "bounds":{"x":480,"y":312,"w":84,"h":24},"x":522,"y":324},
   {"element_ref":"e2","role":"textfield","native_role":"AXTextField","label":"Name","value":"ada",
    "enabled":true,"focused":true,"verbs":["set_value","type","focus"],
    "bounds":{"x":480,"y":350,"w":200,"h":22},"x":580,"y":361},
   {"element_ref":"e3","role":"checkbox","native_role":"AXCheckBox","label":"Bold","enabled":true,
    "checked":false,"verbs":["press","toggle","focus"],
    "bounds":{"x":480,"y":380,"w":60,"h":18},"x":510,"y":389},
   {"element_ref":"e5","role":"textfield","native_role":"AXTextField","label":"Password","enabled":true,
    "secure":true,"verbs":["set_value","type","focus"],
    "bounds":{"x":480,"y":410,"w":200,"h":22},"x":580,"y":421},
   {"element_ref":"e4","role":"group","native_role":"AXGroup","label":"Sidebar","enabled":true,
    "verbs":[],"hidden":42,"bounds":{"x":0,"y":40,"w":220,"h":700},"x":110,"y":390}
 ],
 "skeleton":{"depth":3,"hidden":0},
 "ax_messages":57}
```

One entry per listed node:

| Field | Always | Meaning |
|---|---|---|
| `element_ref` | when the reply can resolve references | `e1`, `e2`, … scoped to this reply's `observation_id`, as protocol 11. |
| `role` | yes | The normalised role (§6.1). |
| `native_role` | yes | The platform's own role, untouched (`AXButton`; on Linux the AT-SPI role name). |
| `label` | when it has one | Title, else description; one line, at most 80 characters. |
| `value` | when it has one | One line, at most 80 characters. **Never present for a secure field**, whose value is never read. |
| `secure` | secure fields only | `true`: a password field. It says why there is no `value`. |
| `enabled` | yes | |
| `verbs` | when it has an `element_ref` | Exactly the verbs this control has a route for (§4, §6.3). Replaces protocol 11's `actions` and `settable`. |
| `checked` | where the control has one | `true`/`false`, for `checkbox`, `switch`, `radio`. |
| `expanded` | where the control has one | `true`/`false`, for a control with an expanded state (a row, a disclosure, a combobox). |
| `selected`, `focused` | only when `true` | |
| `path` | when it has ancestors with labels | Up to three ancestor labels, nearest last. |
| `hidden` | skeleton cut nodes only | How many children this node hides at the cut. |
| `bounds`, `x`, `y` | **optional** | The control's rectangle and click point in this reply's pixels. Absent where the platform reports no position: every GTK 4 control (its screen extents are all 0, 0, on X11 too) and every control on a Wayland session. Then nothing may be clicked by point, and no caller may synthesise one. On a session that places nothing, a listing needs no display, and its observation has no pixels: every point named against it is `point_outside_observation`. The macOS build always sends them. |

Reply fields beside `elements`:

| Field | Meaning |
|---|---|
| `scope` | The scope the walk read. Absent on a `root` drill, and on a request with none of protocol 12's parameters, which reads what protocol 11 read. |
| `root` | The reference drilled into, on a `root` request. |
| `skeleton` | On a skeleton listing: `{"depth": 3, "hidden": N}`, where `N` counts children behind cut nodes that could not be listed (no position, or outside the view). |
| `truncated` | `"depth"`, `"nodes"` or `"time"`, as protocol 11. |
| `ax_messages` | As protocol 11 (M7). |
| `ax_activation` | Only on a request with none of protocol 12's parameters, as protocol 11 (M7). Only that request asks an application whose tree walked to nothing to expose one (Chromium, Electron) and waits for it to settle; a `scope`, `query`, `skeleton` or `root` listing reads the tree as it stands. List once without them first when an application lists nothing. |

### 2.3 Skeleton and drill

- **Skeleton.** The walk reads the scope's root, its children (depth 1), theirs (depth 2) and theirs (depth 3), and
  reads no further. A node at depth 3 that has children is a *cut node*: it is listed with a reference whether or
  not it is a control, carries `hidden` (its child count), and is what the caller drills into. Controls above the
  cut are listed as usual.
- **Protocol 11's settling.** A request with none of the new parameters reads exactly what protocol 11 read,
  including its wait for an application whose tree is still empty. A request with any of them reads the tree
  as it is, without that wait.
- **Drill (`root`).** The root is re-validated first, like an action's target (process, role); a root that no longer
  names what it named is `stale_element`, and so is one a request naming a target drills into when it was listed
  without that window bound (§4.8). The walk reads the root's subtree, without listing the root again, as
  `query` or `skeleton` ask: a find lists only the matches inside it, and a skeleton cuts three levels below the
  root and replies with `skeleton`, as a scope's does.
- **References after a drill.** The reply's new observation holds the named listing's references **minus those
  inside the root's subtree**, plus the new ones, which are numbered after the named listing's last (a listing that
  went to `e40` drills to `e41`, `e42`, …). So `e12` and every reference outside it stay valid with the new
  `observation_id`, and only the drilled subtree's are replaced. The named listing stays valid too, until it expires.

### 2.4 Refusals of `elements`

`scope_not_found`, `stale_element` (a `root` that is gone or changed), `invalid_argument` (a combination this table
refuses), `ax_binding_unavailable` (a bound window with no accessibility element, as protocol 11),
`accessibility_unavailable` (a build with no accessibility API), and the observation and display refusals of
protocol 11.

## 3. Element after, and surfaces

### 3.1 `element_after`

A `semantic` check re-reads the control the action named, as protocol 10 did, now with the normalised role and the
states:

```json
"element_after":{"present":true,"role":"checkbox","native_role":"AXCheckBox","label":"Bold","enabled":true,
                 "checked":true}
```

`{"present": false}` alone for a control that no longer answers. `value` never for a secure field, which carries
`"secure": true` instead. A field is secure when the listing said so **or** when it says so now: a field can
turn secure after the walk that listed it, and the value is withheld at the source either way.

### 3.2 `surfaces` on receipts

Every receipt of an action that **dispatched input** (`dispatch` is `sent` or `partial`) carries `surfaces`: the
dialogs, alerts, menus and popups open in the action's application once it has run (§6.2), and for an action naming a
target only the bound window's, as its `menu` and `dialog` scopes list them (§2.1). `[]` means none is open.
A receipt with nothing dispatched carries none, and neither does a build with no accessibility API, or a bound
window whose window server would not list its windows: absent means "not looked", never "nothing open". A scan that stopped before it finished says why beside the list:
`"surfaces_truncated": "time"` (a node did not answer in time) or `"nodes"` (it read as many nodes as it may). Then
`surfaces` is what it saw, and a surface it did not reach may be open.

```json
"receipt":{"dispatch":"sent","effect":"not_observed","input_method":"ax",
           "timings_ms":{"input":4,"settle":0,"capture":0,"encode":0},
           "observation_id_before":"7c1e-4",
           "surfaces":[{"kind":"dialog","native_kind":"AXSheet","label":"Clear all items?"}]}
```

- `kind` is `dialog`, `alert`, `menu` or `popup`; `native_kind` the platform's own name (§6.2); `label` when it
  has one.
- A surface carries **no reference** in this build. To act inside one, list it: `elements` with `scope: "dialog"`
  or `scope: "menu"`, which answers references in a new observation.
- The application is the one the action addressed: the listing's when it named a control or a listing, the bound
  window's, else the one in front.

## 4. Verbs on a reference

Each is addressed like `press`: `observation_id` naming the listing, `element_ref` naming the control, no point
(`addressing_conflict` beside one), optional `target_id`, `check` as for any input action. Each is input: a pause
refuses it, it carries `mutation_seq` and earns a receipt with `input_method: "ax"` and `foreground_changed`.

Before any write the control is re-validated (the process that listed it, its role, enabled), then its route is read
again: a control that no longer has the route is refused `ax_action_unsupported` and **never clicked instead**.
Every write is read back. The receipt's `effect`:

| `effect` | Meaning |
|---|---|
| `changed` | The read-back shows what was asked, and it did not before. |
| `not_observed` | The read-back is readable and unchanged (or changed to something other than asked). |
| `unknown` | The read-back could not be read (or the field is secure). |
| `verified` | `set_value` only, as protocol 9: the value read back equals the value written. |

`toggle`, `expand`, `collapse`, `select` and `focus` read the state first and act only when it differs. When it
already holds, nothing is sent, the reply is `ok`, and the receipt says so:

```json
"receipt":{"dispatch":"not_sent","effect":"not_observed","input_method":"ax","already":true,
           "state_before":"on","state_after":"on", …}
```

Every state-reading verb reports `state_before` and `state_after` (`on`/`off`, `expanded`/`collapsed`,
`selected`/`unselected`, `focused`/`unfocused`; absent when unreadable). A state that cannot be read before the write
is refused `state_unreadable`, nothing sent.

### 4.1 `focus`

```json
{"type":"request","request_id":"r7","action":"focus","observation_id":"7c1e-4","element_ref":"e2","mutation_seq":8}
```

Moves keyboard focus to the control, inside a window that is already active, and **never activates a window**.
The control's application must be the one in front and the control's window its focused window, else
`focus_refused`, nothing sent. macOS route: `AXFocused` settable; write `AXFocused = true`; read back `AXFocused`.
Linux route: `Component.GrabFocus` on a node that lists `Component` and reads `FOCUSABLE`, once the control's window
reads `ACTIVE`; read back `FOCUSED`. Never on GTK 4, which answers `GrabFocus` `NotSupported`: there `focus` is not
listed.

### 4.2 `select`

```json
{"type":"request","request_id":"r8","action":"select","observation_id":"7c1e-4","element_ref":"e9","mutation_seq":9}
```

Selects this item in its container. macOS route: `AXSelected` settable on the item (write `true`), else the
container's `AXSelectedChildren` settable (write `[item]`). Read back `AXSelected`. Linux route: the container's
`Selection.SelectChild(i)`, `i` the item's own `GetIndexInParent`, where the container lists `Selection`; read back
`SELECTED`. A Qt item, whose views and tab lists list no `Selection`, selects itself with its own `Toggle` action
where it reads `SELECTABLE` and not `CHECKABLE`: the action toggles the selection, so it is sent only while the item
reads not selected, and a read-back that is still not selected is `not_observed`. That action is never a `toggle`
route. No other AT-SPI item selects itself.

### 4.3 `toggle`

```json
{"type":"request","request_id":"r9","action":"toggle","observation_id":"7c1e-4","element_ref":"e3","on":true,
 "mutation_seq":10}
```

`on` (required, boolean) is the state the control should end in. macOS route: `AXPress` with a readable check state
(`AXValue` 0/1 on a `checkbox`, `switch` or `radio`). Read the state; press only if it differs; read it back. Linux
route: the control's press (§4.6), its state the `CHECKED` bit; Qt's `Toggle` action is its press on a check box,
radio or toggle button. A GTK 4 check box names no action, so it lists no `toggle`.

### 4.4 `expand` and `collapse`

```json
{"type":"request","request_id":"r10","action":"expand","observation_id":"7c1e-4","element_ref":"e14",
 "mutation_seq":11}
```

macOS route: `AXExpanded` or `AXDisclosing` settable (write it), or a `disclosure` with `AXPress` and a readable
`AXValue`. Read the expanded state; act only if it differs; read it back. Linux route: a node that reads `EXPANDABLE`
does its own expanding action (`expand or contract` on GTK 3, `listitem.toggle-expand` on GTK 4), else its press; the
state is the `EXPANDED` bit. AT-SPI has no expanded attribute to write.

### 4.5 `scroll_to`

```json
{"type":"request","request_id":"r11","action":"scroll_to","observation_id":"7c1e-4","element_ref":"e30",
 "mutation_seq":12}
```

Brings the control into view. macOS route: the control's own `AXScrollToVisible` action. The read-back is the
control's frame: moved is `changed`, the same is `not_observed` (it was already in view), unreadable is `unknown`.
It reports no `state_before`/`state_after`. Linux route: Chromium's `Component.ScrollTo(ANYWHERE)`; on GTK 3 and
GTK 4, the vertical scroll bar of the nearest scroll pane above the node (at most eight parents up) is set through its
`Value` by how far the node lies outside the pane, both read in window coordinates, held to the bar's range. A bar
whose range is no range (a minimum above its maximum, or a number that is not finite) is never set: `scroll_to` is
refused `state_unreadable` with nothing sent. Qt has no route, so `scroll_to` is not listed there.

### 4.6 `set_value` and `press`

As protocol 9 to 11, advertised in `verbs` (`set_value` where protocol 11 said `settable`, `press` where it listed
`press`). `set_value`'s effect stays `verified` or `not_observed`. Linux routes: `press` is `Action.DoAction(i)`, `i`
the index the first action named `click`, `press`, `activate`, `jump`, `check` or `open` (any case) was read at by
`GetName(i)`, never `doDefault` and never by `GetActions`; `set_value` is `EditableText.SetTextContents` on a node
that reads `EDITABLE`, else `Value.CurrentValue` on a slider, dial, spin button or scroll bar (text that is no
finite number, `NaN` and `inf` among it, is refused `invalid_argument` before anything is sent).

### 4.7 `type` with `element_ref`

```json
{"type":"request","request_id":"r12","action":"type","observation_id":"7c1e-4","element_ref":"e2",
 "text":"hello","mutation_seq":13}
```

Replaces the control's current selection with `text`, or inserts it at the caret when nothing is selected, with
**no keyboard** (macOS route: `AXSelectedText` settable; write it. Linux route: `EditableText.DeleteText` over the
selection `Text.GetSelection(0)` names, then `InsertText` at `CaretOffset`, on a node that reads `EDITABLE`; the
offsets count characters and the inserted text's length its UTF-8 bytes, as ATK and GTK read it).
`observation_id` is required with `element_ref` and refused without it. The value is read before and after: different is `changed`, the same is `not_observed`,
a secure or unreadable field is `unknown`. The reply carries `value` (the value after, never for a secure field).
Without `element_ref`, `type` types at the focus as before.

### 4.8 Refusals of the verbs

`stale_element`, `element_disabled`, `ax_action_unsupported` (no route for this verb on this control),
`state_unreadable`, `focus_refused`, `ax_action_failed` and `ax_timed_out` (the write itself; the second means it
went out), `invalid_argument` (a missing or wrong `on`/`text`), plus the gate's and the observation's refusals.
`ambiguous_element` and `identity_unknown` are Linux's (§7): there a reference is the object a listing read and its
child-index path from the application, and before any write both are read again. The object at that path and the
object itself, wherever its own parent still lists it, are each a candidate when they answer the listed role. One is
the control; two (it moved, and another of its role took its place) are `ambiguous_element`; none is
`stale_element`; a candidate that does not answer in time is read again at most twice, then `identity_unknown`. On
Qt the object is at its place too when it says so (its `Parent` is the node at its place's parent, and its
`GetIndexInParent` the index its place ends in): Qt answers every `GetChildAtIndex` of a table with a new object for
the same cell. Such an object is the control only while its label is the one listed; another (a sort put another item
there) is `stale_element`. On every other toolkit its own account proves nothing: a destroyed GTK 3 or Chromium object
keeps answering its old parent and index for a while. A `ServiceUnknown` error means the application quit.

A request naming a target uses only references listed while that window was bound (by any binding of it): one
listed without a target, or while another window was bound, may name a control of another window, and is
`stale_element` with nothing sent; the detail says to list the bound window and use a reference from that list. A
request naming no target uses any listing, as before.

## 5. Waiting on the tree

### 5.1 The condition

One object, used by `wait_for` (§5.3) and by an action's check (§5.2):

```json
{"kind":"appears","role":"button","name":"Done","scope":"app","timeout_ms":5000}
```

| `kind` | Other fields | Holds when |
|---|---|---|
| `appears` | `role` and/or `name` (as `query`) | A node in the scope matches. |
| `disappears` | `element_ref`, or `role` and/or `name` | That control no longer answers / no node matches. |
| `enabled` | `element_ref` | That control reads enabled. |
| `value` | `element_ref`, `value` (string) | That control's value (bounded to 80 characters) equals `value`. Refused on a secure field. |
| `window_title` | `contains` (string) | The scope's window title contains it (case-insensitive): the bound window, else the application's focused window. |
| `menu_open`, `menu_closed` | none | A menu is open / no menu is open in the application (§6.2), or with a target in the bound window (§2.1): a menu of another window neither opens nor keeps open the bound window's; `menu_closed` only by a scan that saw everything it looks at. |
| `text` | `contains` (string) | Some node in the scope has that text (case-insensitive) in its label, or for a static text in its value. |

- `scope` (optional): as `elements`' scope; default `window` with a target, else `app`. Not accepted on the
  `element_ref` kinds, which read their control directly. A scope that is not open is read as **empty**, never
  refused: waiting for a dialog's button before the dialog exists is the ordinary case. A `menu` or `dialog` scope
  whose scan stopped before it finished is not proven empty (below). `window_title` reads the
  bound window, else the application's focused window, whatever its scope says.
- The application is the bound window's when the request names one, else the listing's the request names, else
  the one in front.
- `timeout_ms` (optional): 1 to 25 000, default 5 000.
- `element_ref` names a control of the listing the request's own `observation_id` names. A request with no
  `observation_id` (`key`, `type` at the focus, `paste`) cannot use the `element_ref` kinds: `invalid_argument`.
  An `observation_id` that listed no controls (a screenshot's) names none of them: `stale_element`.

How it is read: a look every 100 ms, each look at most 1.5 s (the walk's own budget), until the condition holds or
`timeout_ms` has passed; at most `timeout_ms / 100 + 1` looks. It never sends input. A pause ends it at once
(`cancelled`).

**The request's deadline.** A wait never outlives the request that asked for it. The wait is the smaller of
`timeout_ms` and what is left of the request's `deadline_ms` (less 250 ms for the reply to travel), and each look's
walk is held to that deadline too, as is the look an action's check takes before its input (§5.2). When the
deadline is what ended a wait that was not met, the reply says `"deadline_cut": true`: on `wait_for`'s reply, and
in the receipt's semantic `check`. A wait its own `timeout_ms` ended says nothing extra.

**An absence a look cannot prove.** Each look's walk has the bounds an `elements` walk has, and one that stops at
them says why (`truncated`, §2.2), and so does the bounded scan for surfaces (§6.2). `disappears` by `role`/`name`
holds only when a look saw the whole scope and listed no match, or when the scope is not open and the scan that
says so saw everything; `menu_closed` only when that scan saw everything and found no menu. A look that stopped
short and found nothing does not hold: the control or the menu may be past where it stopped. When a wait for an
absence ends not met and its last look was cut short, the reply says why: `"truncated": "nodes"`, `"depth"` or
`"time"` (a scan for surfaces stops for `"nodes"` or `"time"`), on `wait_for`'s reply and in the receipt's semantic
`check` beside `not_met`. Then wait on the control itself (`disappears` with `element_ref`, which reads that one
control and is exact), or on a narrower scope. The look before an action's input follows the same rule, so a look
cut short never reads as `held_before`.

Two reads run after the wait and are **not** held to the deadline: an action's `element_after` (one read of its
control, §3.1) and the surfaces scan on its receipt (at most 64 nodes, §3.2; a node that does not answer within its
own 250 ms ends the scan). The 250 ms margin is for the reply, not for them, so a slow application can answer an
action after its `deadline_ms`.

### 5.2 `check: {"kind": "semantic", "until": …}` on an action

```json
{"type":"request","request_id":"r13","action":"key","chord":"cmd+s","mutation_seq":14,
 "check":{"kind":"semantic","until":{"kind":"appears","role":"dialog"}}}
```

- The object form of `check`. `kind` must be `"semantic"`; `until` is required in it. The string form (`"image"`,
  `"semantic"`, `"none"`) is unchanged.
- Offered on every action that takes a check, not only one addressed at a control. With an `element_ref` the reply
  also carries `element_after`.
- **The baseline is taken before the input**, in the same request: the condition is read once before anything is
  dispatched, then the action runs, then the wait. So the action and its wait are one request and one model call.
- The receipt's check:

```json
"check":{"kind":"semantic","until":"met","looks":3,"waited_ms":212}
```

| `until` | Meaning |
|---|---|
| `met` | The condition did not hold before the input and held after it, within `timeout_ms`. |
| `not_met` | It did not hold before and still did not at the deadline. |
| `held_before` | It already held before the input, so it cannot show what the action did. Nothing is waited for: `looks` and `waited_ms` are 0. |

`looks` counts the looks after the input (the baseline is not one of them). `deadline_cut: true` rides beside
`not_met` when the request's deadline ended the wait, and `truncated` when a wait for an absence ended unproven
(§5.1). The `until` outcome is evidence, not a verdict; `effect`
still comes from the action's own read-back.

### 5.3 `wait_for`

```json
{"type":"request","request_id":"r14","action":"wait_for","until":{"kind":"enabled","element_ref":"e7"},
 "observation_id":"7c1e-4"}
```

Read-only: no `mutation_seq`, no receipt, no `check`. Takes `observation_id` (needed by the `element_ref` kinds),
`region`, `target_id`, `display` like `elements`.

Met:

```json
{"type":"response","request_id":"r14","ok":true,"met":true,"looks":4,"waited_ms":318}
```

For `appears`, `text` and `menu_open`, a met reply also lists what matched, with references, exactly as an
`elements` reply lists controls, in a new semantic observation:

```json
{"type":"response","request_id":"r15","ok":true,"met":true,"looks":2,"waited_ms":104,
 "observation_id":"7c1e-6","observation_kind":"semantic","captured_at_monotonic_ns":81240100000,
 "elements":[{"element_ref":"e1","role":"button","native_role":"AXButton","label":"Done","enabled":true,
              "verbs":["press","focus"],"bounds":{"x":610,"y":420,"w":70,"h":24},"x":645,"y":432}]}
```

Not met by the deadline is an answer, not a failure:

```json
{"type":"response","request_id":"r14","ok":true,"met":false,"looks":51,"waited_ms":5004}
```

A wait for an absence whose last look was cut short says so (§5.1):

```json
{"type":"response","request_id":"r16","ok":true,"met":false,"looks":4,"waited_ms":5120,"truncated":"time"}
```

## 6. Names

### 6.1 Normalised roles

`role` is one of these, on every platform. `native_role` carries the platform's own: on Linux the name of the
`AtspiRole` constant its role number is, in words (`push button`, `check box`), never the toolkit's own
`GetRoleName`, which GTK 4 and Chromium spell differently.

| `role` | macOS role (subrole) | AT-SPI role |
|---|---|---|
| `button` | `AXButton` | `push button` |
| `menubutton` | `AXMenuButton` | `push button menu` |
| `checkbox` | `AXCheckBox` | `check box` |
| `switch` | `AXCheckBox` (`AXSwitch`, `AXToggle`) | `toggle button`, `switch` |
| `radio` | `AXRadioButton` | `radio button` |
| `tab` | `AXRadioButton` (`AXTabButton`), `AXTabButton` | `page tab` |
| `textfield` | `AXTextField` (incl. `AXSecureTextField`, `AXSearchField`) | `entry`, `password text`, `text` (one line) |
| `textarea` | `AXTextArea` | `text`, `entry` that reads `MULTI_LINE` |
| `combobox` | `AXComboBox`, `AXPopUpButton` | `combo box` |
| `menuitem` | `AXMenuItem`, `AXMenuBarItem` | `menu item`, `check menu item`, `radio menu item`, `tearoff menu item` |
| `link` | `AXLink` | `link` |
| `slider` | `AXSlider` | `slider`, `dial` |
| `stepper` | `AXIncrementor`, `AXStepper` | `spin button` |
| `disclosure` | `AXDisclosureTriangle` | — |
| `row` | `AXRow` (incl. `AXOutlineRow`, `AXTableRow`) | `list item`, `tree item`, `table row` |
| `cell` | `AXCell` | `table cell` |
| `column` | `AXColumn` | — |
| `list` | `AXList` | `list`, `list box` |
| `table` | `AXTable` | `table` |
| `tree` | `AXOutline` | `tree`, `tree table` |
| `scrollarea` | `AXScrollArea` | `scroll pane` |
| `scrollbar` | `AXScrollBar` | `scroll bar` |
| `splitter` | `AXSplitter` | `split pane` |
| `toolbar` | `AXToolbar` | `tool bar` |
| `tablist` | `AXTabGroup` | `page tab list` |
| `group` | `AXGroup`, `AXSplitGroup`, `AXRadioGroup` | `panel`, `filler`, `grouping`, `section`, `form` |
| `menu` | `AXMenu` | `menu`, `popup menu` |
| `menubar` | `AXMenuBar` | `menu bar` |
| `window` | `AXWindow` | `frame`, `window` |
| `dialog` | `AXSheet`; `AXWindow` (`AXDialog`, `AXSystemDialog`) | `dialog`, `file chooser`, `color chooser`, `font chooser` |
| `alert` | any role (`AXApplicationAlert`, `AXApplicationAlertDialog`) | `alert`, `notification` |
| `popup` | `AXPopover` | — |
| `text` | `AXStaticText` | `label`, `static` |
| `image` | `AXImage` | `image`, `icon` |
| `heading` | `AXHeading` | `heading` |
| `progress` | `AXProgressIndicator`, `AXBusyIndicator` | `progress bar`, `level bar` |
| `datefield` | `AXDateField`, `AXTimeField` | `date editor` |
| `document` | `AXWebArea` | `document frame`, `document web`, `document text`, `document email`, `document spreadsheet`, `document presentation` |
| `application` | `AXApplication` | `application` |
| `unknown` | any other role; `native_role` says which | any other role |

The check state of a `checkbox`, `switch` or `radio` is its `AXValue` (0 or 1); the expanded state of a
`disclosure` is its `AXValue`; every other control's expanded state is `AXExpanded`, else `AXDisclosing`. On Linux
each state is its AT-SPI state bit: `CHECKED`, `EXPANDED` (on a node that reads `EXPANDABLE`), `SELECTED`,
`FOCUSED`; `enabled` is `SENSITIVE` (GTK 4 sets `ENABLED` on nothing).

### 6.2 Surface kinds

| `kind` | macOS `native_kind` | Where the macOS build finds it | Linux `native_kind` |
|---|---|---|---|
| `dialog` | `AXSheet`, `AXDialog`, `AXSystemDialog` | A window's sheet; a window whose subrole is a dialog | `dialog`, `file chooser`, `color chooser`, `font chooser` |
| `alert` | `AXApplicationAlertDialog`, `AXApplicationAlert` | A window or a window's child with that subrole | `alert`, `notification` |
| `menu` | `AXMenu` | A menu that is a child of the application or of a window (a context menu, a pop-up's menu), or the menu of a menu-bar item that reads selected | `menu`, `popup menu` |
| `popup` | `AXPopover` | A window's child | none |

The macOS build looks two levels down from the application (its windows and their children) and into the menu
bar, and reads at most 64 nodes doing so. A dialog drawn inside web content is not a surface here. The scan says
whether it saw everything it looks at: one that met a node that did not answer within its 250 ms, or read its 64
nodes with more left to read, stopped short, and proves nothing closed (§3.2, §5.1).

The Linux build looks the same two levels down, the same 64 nodes at most, and for the menu bar inside each window
rather than on the application: directly among the window's children (Qt), or inside a box among them (GTK 3). A
menu-bar item is open while it reads `SELECTED`; a GTK 3 one is the menu itself. A dialog or alert is a child of the
application, and a context menu the menu inside the window a toolkit puts up for it. A node that does not read
`SHOWING` is no surface, and nothing below it is looked at: GTK 3 keeps a menu's window after it closes, and Qt every
menu it has made.

With a target, the scan reads the bound window as it reads a child of the application, so the window itself when it
is a surface and the surfaces inside it, and then, on X11, each child of the application whose frame ties it to a
window `WM_TRANSIENT_FOR` the bound one (§2.1). It reads the same 64 nodes at most. What the toolkits do on X11:

| Toolkit | Dialog | Menu |
|---|---|---|
| GTK 3 | Its own managed window, `WM_TRANSIENT_FOR` its parent; a child of the application at that window's frame. | A pop-up or combo box menu: an override-redirect window `WM_TRANSIENT_FOR` its parent, and a child of the application (`window`) at its frame holding the `menu`. A menu-bar menu is inside the window. A context menu attached to a widget is not in the tree at all. |
| GTK 4 | Its own managed window, `WM_TRANSIENT_FOR` its parent; GTK 4 places nothing on X11 (§2.2), so none is tied to one. | A popover's menu, inside the window. |
| Qt | Its own managed window, `WM_TRANSIENT_FOR` its parent; a child of the application at that window's own area. | A menu-bar menu, inside the window under its item. A context menu's window is override-redirect and `WM_TRANSIENT_FOR` its parent, and is not in the tree. |
| Chromium | A page's `alert()` is a `dialog` inside the window. | A context or `<select>` menu: an override-redirect window that names no owner, and a child of the application (`frame`) at its frame: the application's, never a bound window's. |

### 6.3 Which verbs a control lists

| Verb | Listed when (macOS) | Listed when (Linux) |
|---|---|---|
| `press` | its own actions include `AXPress` | one of its actions is named `click`, `press`, `activate`, `jump`, `check` or `open` (any case), `Toggle` on a check box, radio or toggle button, or `ShowMenu` on a Qt menu item that holds its menu (a menu-bar title, or an item with a submenu: pressing a menu title opens it) |
| `set_value` | `AXValue` is settable | it lists `EditableText` and reads `EDITABLE`, or it lists `Value` and is a slider, dial, spin button or scroll bar |
| `type` | `AXSelectedText` is settable | it lists `EditableText` and `Text` and reads `EDITABLE` |
| `focus` | `AXFocused` is settable | it lists `Component` and reads `FOCUSABLE`, and is not GTK 4 |
| `select` | `AXSelected` is settable, or its container's `AXSelectedChildren` is | its container lists `Selection`; on Qt, it reads `SELECTABLE` and not `CHECKABLE` and has a `Toggle` action |
| `toggle` | a `checkbox`, `switch` or `radio` with `AXPress` and a readable check state | a `checkbox`, `switch` or `radio` with a press and a readable `CHECKED` |
| `expand`, `collapse` | `AXExpanded` or `AXDisclosing` is settable, or a `disclosure` with `AXPress` and a readable value | it reads `EXPANDABLE` and has an expanding action or a press |
| `scroll_to` | its own actions include `AXScrollToVisible` | Chromium: it lists `Component`; GTK: it sits inside a scroll pane; Qt: never |

`focus` is listed by route; whether the control's window is active is decided at the action (`focus_refused`).
A pointer action by `element_ref` (`left_click` and the rest) needs only `bounds` and is not a verb here.

## 7. Refusals

Every `error` code protocol 12 answers. "Linux" marks the ones only the Linux build answers.

**Frames and arguments**

| Code | When |
|---|---|
| `malformed_frame` | Not a JSON object, no usable `request_id`, or no `action`. |
| `untagged_frame` | A line with no `type` (protocol 6); answered in the `ack` family. |
| `unknown_frame_type` | A `type` this wire does not have. |
| `request_too_large` | A line over 65 536 bytes; never correlated. |
| `unknown_field` | A field this action does not take, or a retired one (`screenshot_after`). |
| `unknown_action` | An action this build does not have. |
| `observation_required` | An addressed action, or `type`/`elements` with `element_ref`/`root`, without `observation_id`. |
| `element_required` | A verb on a reference without `element_ref`. |
| `addressing_conflict` | A point and `element_ref` on one request. |
| `check_unsupported` | A check on an action that dispatches no input, a string `semantic` check on an action that named no control, or a malformed check object. |
| `invalid_argument` | A wrong value: an unknown scope, an empty query, a refused combination, a bad `until`, `toggle` without a boolean `on`, `type` without `text`, a `scroll` `amount` that is not a whole number from 1 to 50 (the most notches one scroll turns; nothing sent, never turned down to the cap); on Linux, `set_value` on a control that takes a number with text that is no finite number (nothing sent). |

**The gate**

| Code | When |
|---|---|
| `stale_generation` | The request belongs to a revoked authority. |
| `stale_mutation` | Its `mutation_seq` is not above the last one admitted. |
| `paused` | Input is paused. |
| `cancelled` | A pause landed while it ran; the receipt says what had gone out. |
| `busy` | Another request is in flight. |

**Observations and displays**

| Code | When |
|---|---|
| `unknown_observation` | No such id is held. |
| `expired_observation` | The id is older than `ttl_ms`. |
| `stale_observation` | The display (or bound window) moved or changed since the id was minted. |
| `point_outside_observation` | A point outside the image it names. |
| `no_active_display`, `display_asleep`, `display_disconnected` | The display named cannot be captured. |
| `capture_stalled` | The OS capture wedged; the helper exits 75 after this reply. |
| `capture_geometry_mismatch` | A frame whose shape is not the display's or window's. |
| `capture_unavailable`, `capture_budget_exceeded`, `screen_recording_not_granted` | A bound window's stream could not deliver. |

**Bound windows**

| Code | When |
|---|---|
| `target_unavailable` | The bound window is gone, or the `target_id` is not the one bound. On Linux (X11) also a window destroyed after it was bound, whatever window has its id now, and a window whose process would not say when it started. |
| `target_minimized` | The bound window is minimized. |
| `target_obstructed` | A pixel action on the bound window where something covers that point. |
| `control_surface_unavailable` | No ownership badge is running for a mutation on a bound window: on Linux, `compux-indicator-x11` is not beside the helper, or it exited (§1.2). |
| `ax_binding_unavailable` | The bound window has no accessibility element to list from. |
| `target_binding_unavailable` | Linux: `select_target` on a session that can bind no window (any but X11's), on an X11 window that names no process (`_NET_WM_PID`), or on one whose process or title is no longer what `windows` listed. |

**Controls**

| Code | When |
|---|---|
| `stale_element` | The reference names nothing now: its process quit or its pid was reused, it is gone, its role changed, it did not answer in time, or the reply listed no such reference; on Qt, the control at that place no longer has the label it was listed with; or the request names a target and the reference was listed without that window bound (§4.8). Nothing sent. |
| `element_disabled` | The control reads disabled. Nothing sent. |
| `ax_action_unsupported` | The control has no route for this verb. Nothing sent, nothing clicked instead. |
| `state_unreadable` | A verb could not read the state it acts from, so it could not decide whether or how to act. Nothing sent. That state is a toggle's or an expander's; on Linux also a scroll bar's range (or one that reads as no range), a field's selection or caret before `type`, and an item's place in its container before `select`. On Linux the reads before a write are held to the verb's own time: reads that use it up end the verb here too. |
| `focus_refused` | `focus` on a control whose window is not the active one. Nothing sent. |
| `ax_action_failed` | The application refused the message. Nothing went out. |
| `ax_timed_out` | The message went out and no answer came; never repeated. |
| `scope_not_found` | An `elements` scope has no root: no focused window, nothing focused, no menu or dialog open, focus or the only menu or dialog in another window than the bound one (§2.1), or the scan for a menu or dialog stopped before it finished (the detail says which). A condition reads such a scope as empty instead (§5.1), and one a scan stopped short of as unproven. |
| `accessibility_unavailable` | A build with no accessibility API (neither macOS nor Linux) was asked to read or act on controls. |
| `ambiguous_element` | Linux: re-validation found several candidates for one reference. |
| `identity_unknown` | Linux: a candidate stayed unreadable after two re-reads. |
| `toolkit_accessibility_off` | Linux: the application in front publishes no tree until it is asked, and its tree stayed empty after `elements` waited for it (Qt without `QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1`; Chromium without `--force-renderer-accessibility` and `ACCESSIBILITY_ENABLED=1`); the detail names the switch. An application that is not on the accessibility bus at all leaves no window reading `ACTIVE` there, and `elements`' note names the switches instead. |

**Input on Linux**

| Code | When |
|---|---|
| `text_outside_layout` | Linux (X11): `type` text the active keymap cannot produce; the detail names the characters and `paste`. Nothing sent. |
| `keyboard_not_neutral` | Linux (X11): `type` while the keyboard is in a group other than the first, a modifier is held, or Caps Lock is on; the detail says which. Nothing sent. |
| `no_active_window` | Linux: `type` without `element_ref`, `key` or `paste` while no window, or more than one, reads `ACTIVE` on the accessibility bus, so the keys would go where nobody chose. The detail says which: no window, or how many applications claim one. compux never activates a window to make room. Nothing sent. |
| `pixels_unavailable` | Linux: a pixel action (or `type` without `element_ref`, or an image check) where the session's pixel route is refused (§1.1: a compositor with no qualified route, a portal without its `Registry`, a portal that is not running and does not start or whose error does not say whether it runs, no display), or one the route does not serve (`paste` and `windows` on GNOME Wayland). The detail is the route's reason. Nothing sent; work by control. |
| `consent_refused` | Linux (GNOME Wayland): the portal answered no, now or by a stored answer. A stored no stays until the person resets it; `fermix doctor` prints how. compux never writes the PermissionStore. Nothing sent. |
| `consent_timeout` | Linux (GNOME Wayland): the portal ended its dialog unanswered, or nobody answered it within 25 s; the dialog is closed. Nothing sent. |
| `capture_unavailable` | Also Linux (GNOME Wayland): the portal's call failed or was answered by another process than the portal's owner, a session set-up step (`CreateSession`, `SelectDevices`, `SelectSources`, which show no dialog) went unanswered for 5 s (the detail names the step), its screenshot is not a regular file or would not decode (the file is left where it is), or it holds more than the session's one monitor. Nothing sent. |

Legacy argument errors that the library refuses before they reach the sidecar (a missing `text`, an unknown key in a
chord, a bad drag point) still answer their sentence as the code from the sidecar.

## 8. `probe`

`probe` is operational, never a model verb, and unchanged on macOS: `platform`, `display_server` (`quartz`),
`screen_capture` and `input_control`, the grants read without a prompt. On Linux it reports what a doctor shows,
read-only and raising no prompt:

```json
{"type":"response","request_id":"p1","ok":true,"platform":"linux","display_server":"x11","x11_endpoint":true,
 "display_adopted":false,"screen_capture":true,"input_control":true,
 "x11_pixels":{"xtest":true,"keymap":true},
 "portal":{"running":true,"version":"1.14.4-1ubuntu2~22.04.2","registry":false,"screenshot_permission":"unset"},
 "accessibility_bus":{"state":"answering","applications":7,
                      "status":{"is_enabled":false,"screen_reader_enabled":false}},
 "accessibility_packages":{"source":"dpkg","at_spi2_core":"2.44.0-3","atk_bridge":"2.38.0-3"},
 "toolkit_notes":[{"toolkit":"qt","note":"the session's accessibility status reads off, so a Qt application publishes no tree: start it with QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1 in its environment"},
                  {"toolkit":"chromium","note":"a Chromium-based application publishes no page tree by itself: start it with --force-renderer-accessibility, and with ACCESSIBILITY_ENABLED=1 in its environment"}]}
```

- `display_server` is decided by connecting, never by a variable: `wayland` when a Wayland socket answers, `x11`
  when only an X socket answers, `none` when neither does. `x11_endpoint` says whether an X endpoint answers: the
  session's own on X11, XWayland beside Wayland. `display_adopted` is true where the helper had no display of its
  own and took the session's from the user manager (§1), which it reaches as its own. `screen_capture` and
  `input_control` say whether the session's pixel route (§1.1) serves `screenshot` and the pointer: true on X11 and
  on GNOME Wayland with a `Registry`, false where the route is refused. They are read without a prompt, so on GNOME
  Wayland they do not say the person will allow it.
- `x11_pixels` is the X11 route's two needs, read on a connection of its own where an X endpoint answers (`null`
  where none does): `xtest`, the XTEST extension; `keymap`, the server's keyboard map answered.
- `portal` is read where a session bus answers (`null` where none does), without raising a dialog and without
  starting the portal or its store: `running`, whether the portal answered (false where the bus says nobody runs it;
  `null` where it answered an error that says neither, which the helper prints); `version`, the installed
  xdg-desktop-portal as the package database records it (`null` where it does not say); `registry`, whether the
  portal offers the host `Registry` (1.18 and later; `null` unless the portal answered);
  `screenshot_permission`, the PermissionStore's stored Screenshot answer for `io.tezra.fermix.ComputerUse`: `yes`,
  `no` (every screenshot is refused `consent_refused` until the person resets it), `unset` (no answer stored, or no
  table at all), or `not_running` (the store is not running, so nothing was read). `null` where the store would not
  answer.
- `accessibility_bus.state`:
  - `answering`: the registry answers and lists `applications` applications, one or more;
  - `empty`: the registry answers and lists none (`applications` is 0);
  - `unavailable`: no registry answered. `cause` names why and `detail` says what failed: `no_session_bus` (this
    process has no session bus), `not_installed` (nothing on the session bus is or can start `org.a11y.Bus`:
    at-spi2-core is not installed for this session), `no_address` (the launcher gave no usable address),
    `unreachable` (the address would not connect), `not_answering` (the registry did not answer its child count).
- `accessibility_bus.status` is the session's `org.a11y.Status`, `is_enabled` and `screen_reader_enabled`, or
  `null` where it was not read. Qt publishes its tree only while one of them is true, or with its own switch.
- `accessibility_packages` holds the versions the package database records for at-spi2-core and the ATK bridge
  (`libatk-bridge2.0-0`, `libatk-bridge2.0-0t64` from Ubuntu 24.04), each `null` where it is not installed. `source`
  is `dpkg`, or `null` with every version `null` where this build reads no package database (only dpkg's is read).
  An ATK bridge older than 2.52 aborts a GTK 3 application asked `Action.GetActions`; compux never asks it.
- `toolkit_notes` holds the notes known without reading any application, each `toolkit` and `note`: Qt while the
  status reads off, and Chromium's switches. None where the bus is `unavailable`.

What it sends: one `GetAddress` to the session bus's `org.a11y.Bus`, which may start the accessibility bus as any
accessible application's start does (a check for the name first would report a healthy session where nothing has
asked yet as one without accessibility); two property `Get`s of `org.a11y.Status`; and the registry's
`ChildCount`. It registers for no event, reads no application, never sends `GetAll`, and sets nothing. It reads
`/var/lib/dpkg/status` as a file. For the pixel facts it asks the X server for its XTEST extension and keyboard map,
and the session bus `NameHasOwner` for `org.gnome.Shell` and `org.kde.KWin`, the portal object's `Introspect`, and
the PermissionStore's `Lookup("screenshot", "screenshot")`, the last two with `NO_AUTO_START`, so neither the portal
nor the store is started to answer: one that is not running reads as such. It never calls the portal itself and
never writes the store. Outside `probe`, the helper's start on GNOME Wayland decides its pixel route (§1.1) by the
same `Introspect` without that flag, and so may start xdg-desktop-portal, as any portal client's first call does;
on X11 the route is decided without asking the portal anything.
