# The compux wire, protocol 12

This is the contract between a caller (`Compux.Protocol`, and the Fermix engine above it) and the `compux`
sidecar, for what protocol 12 adds and changes. Everything protocol 11 had and this page does not mention is
unchanged: the tagged frames, the generations, `mutation_seq`, `deadline_ms`, observations, bound targets and the
`check` kinds `image` and `none`. The README describes the protocol-11 surface in prose.

Protocol 12 is M86 stage P2: the model's surface, defined once for macOS and Linux. The macOS build answers all of
it. The Linux build answers the same wire from M86 L1; until then its `hello` lists only the pixel actions, and the
refusals only Linux will answer are already named here, so L1 adds a backend and no second break.

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
  - **Linux, until L1:** the pixel actions only: `screenshot`, `left_click`, `right_click`, `double_click`,
    `mouse_move`, `left_click_drag`, `scroll`, `type`, `key`, `wait`, `wait_for_change`, `paste`, `windows`.
    `input_methods` is `["foreground_hid"]`.
- `type` is listed on both builds because it types at the focus. Its `element_ref` form (§4.7) is offered where
  `capabilities.input_methods` contains `"ax"`.
- The `semantic` check with `until` (§5.2) is offered where `wait_for` is listed.

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
| `focused` | The application's focused element (`AXFocusedUIElement`), and what is inside it. | yes |
| `menu` | Every menu open in the application (§6.2): a context menu, a pop-up's menu, a menu-bar menu. | no |
| `dialog` | Every dialog or alert open in the application (§6.2), sheets included. | no |

A scope whose root does not exist (no focused window, nothing focused, no menu or dialog open) is refused
`scope_not_found`, with a detail that names what is missing.

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
| `bounds`, `x`, `y` | **optional** | The control's rectangle and click point in this reply's pixels. Absent where the platform reports no position (GNOME Wayland); then nothing may be clicked by point, and no caller may synthesise one. The macOS build always sends them. |

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
  names what it named is `stale_element`. The walk reads the root's subtree, without listing the root again, as
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
dialogs, alerts, menus and popups open in the action's application once it has run (§6.2). `[]` means none is open.
A receipt with nothing dispatched carries none, and neither does a build with no accessibility API: absent means
"not looked", never "nothing open".

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

### 4.2 `select`

```json
{"type":"request","request_id":"r8","action":"select","observation_id":"7c1e-4","element_ref":"e9","mutation_seq":9}
```

Selects this item in its container. macOS route: `AXSelected` settable on the item (write `true`), else the
container's `AXSelectedChildren` settable (write `[item]`). Read back `AXSelected`.

### 4.3 `toggle`

```json
{"type":"request","request_id":"r9","action":"toggle","observation_id":"7c1e-4","element_ref":"e3","on":true,
 "mutation_seq":10}
```

`on` (required, boolean) is the state the control should end in. macOS route: `AXPress` with a readable check state
(`AXValue` 0/1 on a `checkbox`, `switch` or `radio`). Read the state; press only if it differs; read it back.

### 4.4 `expand` and `collapse`

```json
{"type":"request","request_id":"r10","action":"expand","observation_id":"7c1e-4","element_ref":"e14",
 "mutation_seq":11}
```

macOS route: `AXExpanded` or `AXDisclosing` settable (write it), or a `disclosure` with `AXPress` and a readable
`AXValue`. Read the expanded state; act only if it differs; read it back.

### 4.5 `scroll_to`

```json
{"type":"request","request_id":"r11","action":"scroll_to","observation_id":"7c1e-4","element_ref":"e30",
 "mutation_seq":12}
```

Brings the control into view. macOS route: the control's own `AXScrollToVisible` action. The read-back is the
control's frame: moved is `changed`, the same is `not_observed` (it was already in view), unreadable is `unknown`.
It reports no `state_before`/`state_after`.

### 4.6 `set_value` and `press`

As protocol 9 to 11, advertised in `verbs` (`set_value` where protocol 11 said `settable`, `press` where it listed
`press`). `set_value`'s effect stays `verified` or `not_observed`.

### 4.7 `type` with `element_ref`

```json
{"type":"request","request_id":"r12","action":"type","observation_id":"7c1e-4","element_ref":"e2",
 "text":"hello","mutation_seq":13}
```

Replaces the control's current selection with `text`, or inserts it at the caret when nothing is selected, with
**no keyboard** (macOS route: `AXSelectedText` settable; write it). `observation_id` is required with `element_ref`
and refused without it. The value is read before and after: different is `changed`, the same is `not_observed`,
a secure or unreadable field is `unknown`. The reply carries `value` (the value after, never for a secure field).
Without `element_ref`, `type` types at the focus as before.

### 4.8 Refusals of the verbs

`stale_element`, `element_disabled`, `ax_action_unsupported` (no route for this verb on this control),
`state_unreadable`, `focus_refused`, `ax_action_failed` and `ax_timed_out` (the write itself; the second means it
went out), `invalid_argument` (a missing or wrong `on`/`text`), plus the gate's and the observation's refusals.
`ambiguous_element` and `identity_unknown` are named for Linux (§7).

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
| `menu_open`, `menu_closed` | none | A menu is open / no menu is open in the application (§6.2). |
| `text` | `contains` (string) | Some node in the scope has that text (case-insensitive) in its label, or for a static text in its value. |

- `scope` (optional): as `elements`' scope; default `window` with a target, else `app`. Not accepted on the
  `element_ref` kinds, which read their control directly. A scope that is not open is read as **empty**, never
  refused: waiting for a dialog's button before the dialog exists is the ordinary case. `window_title` reads the
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
`not_met` when the request's deadline ended the wait (§5.1). The `until` outcome is evidence, not a verdict; `effect`
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

## 6. Names

### 6.1 Normalised roles

`role` is one of these, on every platform. `native_role` carries the platform's own. The macOS column is what the
macOS build maps; L1 fills the AT-SPI column the same way.

| `role` | macOS role (subrole) |
|---|---|
| `button` | `AXButton` |
| `menubutton` | `AXMenuButton` |
| `checkbox` | `AXCheckBox` |
| `switch` | `AXCheckBox` (`AXSwitch`, `AXToggle`) |
| `radio` | `AXRadioButton` |
| `tab` | `AXRadioButton` (`AXTabButton`), `AXTabButton` |
| `textfield` | `AXTextField` (incl. `AXSecureTextField`, `AXSearchField`) |
| `textarea` | `AXTextArea` |
| `combobox` | `AXComboBox`, `AXPopUpButton` |
| `menuitem` | `AXMenuItem`, `AXMenuBarItem` |
| `link` | `AXLink` |
| `slider` | `AXSlider` |
| `stepper` | `AXIncrementor`, `AXStepper` |
| `disclosure` | `AXDisclosureTriangle` |
| `row` | `AXRow` (incl. `AXOutlineRow`, `AXTableRow`) |
| `cell` | `AXCell` |
| `column` | `AXColumn` |
| `list` | `AXList` |
| `table` | `AXTable` |
| `tree` | `AXOutline` |
| `scrollarea` | `AXScrollArea` |
| `scrollbar` | `AXScrollBar` |
| `splitter` | `AXSplitter` |
| `toolbar` | `AXToolbar` |
| `tablist` | `AXTabGroup` |
| `group` | `AXGroup`, `AXSplitGroup`, `AXRadioGroup` |
| `menu` | `AXMenu` |
| `menubar` | `AXMenuBar` |
| `window` | `AXWindow` |
| `dialog` | `AXSheet`; `AXWindow` (`AXDialog`, `AXSystemDialog`) |
| `alert` | any role (`AXApplicationAlert`, `AXApplicationAlertDialog`) |
| `popup` | `AXPopover` |
| `text` | `AXStaticText` |
| `image` | `AXImage` |
| `heading` | `AXHeading` |
| `progress` | `AXProgressIndicator`, `AXBusyIndicator` |
| `datefield` | `AXDateField`, `AXTimeField` |
| `document` | `AXWebArea` |
| `application` | `AXApplication` |
| `unknown` | any other role; `native_role` says which |

The check state of a `checkbox`, `switch` or `radio` is its `AXValue` (0 or 1); the expanded state of a
`disclosure` is its `AXValue`; every other control's expanded state is `AXExpanded`, else `AXDisclosing`.

### 6.2 Surface kinds

| `kind` | macOS `native_kind` | Where the macOS build finds it |
|---|---|---|
| `dialog` | `AXSheet`, `AXDialog`, `AXSystemDialog` | A window's sheet; a window whose subrole is a dialog |
| `alert` | `AXApplicationAlertDialog`, `AXApplicationAlert` | A window or a window's child with that subrole |
| `menu` | `AXMenu` | A menu that is a child of the application or of a window (a context menu, a pop-up's menu), or the menu of a menu-bar item that reads selected |
| `popup` | `AXPopover` | A window's child |

The macOS build looks two levels down from the application (its windows and their children) and into the menu
bar, and reads at most 64 nodes doing so. A dialog drawn inside web content is not a surface here.

### 6.3 Which verbs a control lists

| Verb | Listed when (macOS) |
|---|---|
| `press` | its own actions include `AXPress` |
| `set_value` | `AXValue` is settable |
| `type` | `AXSelectedText` is settable |
| `focus` | `AXFocused` is settable |
| `select` | `AXSelected` is settable, or its container's `AXSelectedChildren` is |
| `toggle` | a `checkbox`, `switch` or `radio` with `AXPress` and a readable check state |
| `expand`, `collapse` | `AXExpanded` or `AXDisclosing` is settable, or a `disclosure` with `AXPress` and a readable value |
| `scroll_to` | its own actions include `AXScrollToVisible` |

`focus` is listed by route; whether the control's window is active is decided at the action (`focus_refused`).
A pointer action by `element_ref` (`left_click` and the rest) needs only `bounds` and is not a verb here.

## 7. Refusals

Every `error` code protocol 12 answers. "Linux" marks the ones only the Linux build answers at first; they are named
now so L1 changes no wire.

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
| `invalid_argument` | A wrong value: an unknown scope, an empty query, a refused combination, a bad `until`, `toggle` without a boolean `on`, `type` without `text`. |

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
| `target_unavailable` | The bound window is gone, or the `target_id` is not the one bound. |
| `target_minimized` | The bound window is minimized. |
| `target_obstructed` | A pixel action on the bound window where something covers that point. |
| `control_surface_unavailable` | No ownership badge is running for a mutation on a bound window. |
| `ax_binding_unavailable` | The bound window has no accessibility element to list from. |
| `target_binding_unavailable` | Linux: a window that cannot be proven to be the frame it claims (X11 EWMH match), or any binding on Wayland. |

**Controls**

| Code | When |
|---|---|
| `stale_element` | The reference names nothing now: its process quit or its pid was reused, it is gone, its role changed, it did not answer in time, or the reply listed no such reference. Nothing sent. |
| `element_disabled` | The control reads disabled. Nothing sent. |
| `ax_action_unsupported` | The control has no route for this verb. Nothing sent, nothing clicked instead. |
| `state_unreadable` | A state-reading verb could not read the state, so it could not decide whether to act. Nothing sent. |
| `focus_refused` | `focus` on a control whose window is not the active one. Nothing sent. |
| `ax_action_failed` | The application refused the message. Nothing went out. |
| `ax_timed_out` | The message went out and no answer came; never repeated. |
| `scope_not_found` | An `elements` scope has no root: no focused window, nothing focused, no menu or dialog open. A condition reads such a scope as empty instead (§5.1). |
| `accessibility_unavailable` | A build with no accessibility API (the Linux build until L1) was asked to read or act on controls. |
| `ambiguous_element` | Linux: re-validation found several candidates for one reference. |
| `identity_unknown` | Linux: a candidate stayed unreadable after two re-reads. |
| `toolkit_accessibility_off` | Linux: the application's toolkit publishes no tree (Qt without `QT_ACCESSIBILITY`, Chromium without an assistive client); the detail names the switch. |

**Input on Linux**

| Code | When |
|---|---|
| `text_outside_layout` | Linux: `type` text the active keymap cannot produce; use `paste`. |
| `consent_refused` | Linux: the person refused the portal's screen-capture or remote-desktop request. |
| `consent_timeout` | Linux: the portal's request was not answered in time. |

Legacy argument errors that the library refuses before they reach the sidecar (a missing `text`, an unknown key in a
chord, a bad drag point) still answer their sentence as the code from the sidecar.
