defmodule Compux do
  @moduledoc """
  Native screen-capture + input-injection (computer use) for Elixir, backed by a
  crash-isolated Rust sidecar (`native/compux`) spawned over a Port — **not** a NIF.

  This is the ergonomic facade. It owns a `Compux.Driver` (the production
  `Compux.PortDriver`, or a stub in tests), performs the `hello` version handshake
  on `start/1`, and translates typed calls into validated `Compux.Protocol`
  requests.

  It is deliberately **policy-free**: it makes no decision about whether an action
  is allowed — no confirmation gates, no sandboxing, no telemetry. It returns
  `{:ok, response} | {:error, reason}` and lets the caller decide.

      {:ok, cu}   = Compux.start()
      {:ok, shot} = Compux.screenshot(cu, region: {0, 0, 400, 300})
      :ok         = Compux.click(cu, {120, 80}, observation_id: shot["observation_id"])
      {:ok, el}   = Compux.inspect(cu, {120, 80}, observation_id: shot["observation_id"])
      :ok         = Compux.stop(cu)

  ## Coordinates name their image

  Every reply that hands you coordinates carries an `observation_id`, and every
  call that sends coordinates back names the image they were read from. The
  sidecar keeps the transform with the image, so a click is mapped by the geometry
  the picture was taken with — never by a rectangle the caller repeated. The
  actions that address a point therefore take `:observation_id` and no `:region`;
  the actions that produce an image or a list of coordinates take both.

  ## Controls have names, not only places

  `elements/2` gives each control an `element_ref` (`e1`, `e2`, … scoped to that
  reply's observation) beside its normalised `role` and `native_role`, label,
  value, whether it is enabled, the `verbs` it has a route for and, where the
  platform reports one, where it is. A reference is always sent with its
  `observation_id`; on its own it means nothing.

      {:ok, list} = Compux.elements(cu, query: %{"role" => "button", "name" => "Save"})
      image       = list["observation_id"]
      [save | _]  = Enum.filter(list["elements"], &("press" in &1["verbs"]))

      :ok = Compux.press(cu, save["element_ref"], observation_id: image)
      :ok = Compux.click(cu, {:element, save["element_ref"]}, observation_id: image)

  The verbs on a reference (`press/3`, `set_value/4`, `focus/3`, `select/3`,
  `toggle/4`, `expand/3`, `collapse/3`, `scroll_to/3`, and `type/3` with
  `:element_ref`) do not touch the pointer at all, and are offered only where the
  control has a route for them; anywhere else they are refused and nothing is
  sent, never replaced by a click. `click/3`, `move/3` and `scroll/5` may take
  `{:element, ref}` instead of a point, and the sidecar re-reads that control's
  bounds before it clicks its centre. `PROTOCOL.md` is the wire contract.

  ## One window, bound

  `select_target/3` binds a window a `windows/2` listing named and answers a
  `target_id`. Pass that id to a later call as `:target_id` and the helper answers
  from that window's own frames, in that window's own coordinates — so a window
  another one covers is still seen, and a click is refused rather than landing on
  whatever is in front of it. While a target is held the helper shows an ownership
  indicator on screen whose Pause and Stop reach it directly; those arrive as
  session events (see `Compux.Transport`).

      {:ok, list} = Compux.windows(cu)
      [window | _] = list["windows"]
      {:ok, bound} = Compux.select_target(cu, window["id"])

      {:ok, shot} = Compux.screenshot(cu, target_id: bound["target_id"])
      :ok = Compux.release_target(cu)

  There is no "desktop" target: a call with no `:target_id` is the display-level
  path this library has always had, unchanged.

  ## Every action says what evidence it wants back

  An action that dispatches input carries `:check`, and the reply's `receipt` says
  which evidence it really carries (`check: %{kind:, settle:, changed:}`).

    * `:image` — the view you acted in: the crop of the image (or listing) the
      action named, or the whole display when that is what it named and when the
      action names none at all. The helper waits for the view to stop moving
      first, and `settle` says whether it did (`"stable"`) or was still moving at
      the cap (`"timeout"`). `changed` says whether that view differs from the one
      you acted on — evidence about the view, never a verdict on the action.
    * `:semantic` — for an action addressed by `element_ref`: the control is read
      again and returned as `element_after` (role, whether it is enabled, its
      states, and its value unless it is a secure field, which says
      `"secure" => true` instead). No capture, so it is not visual evidence.
    * `:none` — the receipt alone.
    * `%{"kind" => "semantic", "until" => condition}` — wait on the tree for a
      condition (`wait_for/3` describes it). The condition is read once BEFORE the
      input, so the receipt's `check.until` says `met`, `not_met`, or `held_before`
      when it already held and so shows nothing about the action. A wait the
      request's own deadline cut short also says `deadline_cut: true`.

  Left out, the action brings nothing back. Whatever the check says, the receipt's
  `dispatch` is about the INPUT and is decided before the check runs: evidence that
  could not be obtained never changes what was sent.

  ## The version handshake

  `start/1` reads the sidecar's `hello` identity and refuses to run a binary whose
  `protocol_version` differs from `Compux.Protocol.protocol_version/0`, returning
  `{:error, {:protocol_mismatch, %{library: m, sidecar: n}}}`. This closes the
  otherwise-silent drift between a compiled-in encoder and a separately-installed
  binary (a NIF would share versions; a spawned binary does not).
  """

  # `inspect/2` (a screen-space accessibility probe) intentionally shadows
  # `Kernel.inspect/2`; the library never uses the Kernel arity-2 form.
  import Kernel, except: [inspect: 2]

  alias Compux.{Binary, Protocol}

  @enforce_keys [:driver, :state, :info]
  defstruct [:driver, :state, :info]

  @type t :: %__MODULE__{driver: module(), state: term(), info: map()}
  @type coord :: {integer(), integer()}
  @typedoc """
  What a pointer action aims at: a pixel of the named image, or a control the
  named `elements` reply listed. Never both — the sidecar refuses a request that
  carries the two, because nothing may guess which one was meant.
  """
  @type target :: coord() | {:element, String.t()}
  @type response :: {:ok, map()} | {:error, term()}

  @doc "The wire-compatibility version this build speaks."
  @spec protocol_version() :: pos_integer()
  def protocol_version, do: Protocol.protocol_version()

  @doc """
  Open the sidecar and perform the version handshake.

  Options are passed through to the driver's `start/1`; the notable ones:
    * `:driver` — a `Compux.Driver` module (default `Compux.PortDriver`).
    * `:binary_path` — the sidecar executable (defaults, for `PortDriver`, to
      `Compux.Binary.path!/0`).
    * `:timeout` — per-action deadline in ms.
  """
  @spec start(keyword()) :: {:ok, t()} | {:error, term()}
  def start(opts \\ []) when is_list(opts) do
    driver = Keyword.get(opts, :driver, Compux.PortDriver)
    opts = maybe_default_binary_path(driver, opts)

    with {:ok, state} <- driver.start(opts),
         {:ok, identity} <- driver.execute(state, %{"action" => "hello"}),
         :ok <- check_protocol(identity, driver, state) do
      {:ok, %__MODULE__{driver: driver, state: state, info: identity_info(identity)}}
    end
  end

  @doc """
  The sidecar's reported identity: `:protocol_version`, `:compux_version`,
  `:actions`, the `:sidecar_generation` this boot minted, and the `:capabilities`
  it advertises (`input_methods`, `controls`). A capability is listed only if the
  build really has it, so this is what a caller reads before offering one.
  """
  @spec info(t()) :: map()
  def info(%__MODULE__{info: info}), do: info

  @doc "Tear the sidecar down. Idempotent; releases any held keys/buttons."
  @spec stop(t()) :: :ok
  def stop(%__MODULE__{driver: driver, state: state}), do: driver.stop(state)

  @doc """
  Run a raw action params map (string keys) through validation and the driver.
  `opts` may carry `:check` (see the module doc), which is an action argument like
  any other and is validated with them.
  """
  @spec execute(t(), map(), keyword()) :: response()
  def execute(%__MODULE__{} = cu, params, opts \\ []) when is_map(params),
    do: run(cu, params, opts)

  @doc """
  Capture the target display (optionally a `:region` and `:display`).

  Returns lossless PNG by default. `:jpeg_quality` (1-100) switches to JPEG, which a
  BULK periodic caller wants: a full-desktop PNG runs to hundreds of KB and
  saturates an uplink at any real frame cadence, while the same frame at quality 60
  is roughly an order of magnitude smaller. It changes only the encoding — the sent
  dimensions, and therefore every coordinate, are identical either way.
  """
  @spec screenshot(t(), keyword()) :: response()
  def screenshot(%__MODULE__{} = cu, opts \\ []),
    do: run(cu, put_opts(%{"action" => "screenshot"}, opts), opts)

  @doc """
  Click a pixel of the image named by `:observation_id`, or the control that
  image's `elements` reply listed (`{:element, "e3"}`) — the sidecar re-reads that
  control's bounds and clicks its centre, so a control that moved since the
  listing is hit where it is now. `:button` is `:left` (default), `:right`, or
  `:double`; other opts: `:modifiers`, `:display`, `:check`.
  """
  @spec click(t(), target(), keyword()) :: response()
  def click(%__MODULE__{} = cu, target, opts \\ []) do
    params =
      %{"action" => click_action(Keyword.get(opts, :button, :left))}
      |> put_target(target)
      |> maybe_put("modifiers", modifiers(opts))

    run(cu, put_addressed(params, opts), opts)
  end

  @doc "Move the pointer to a pixel, or to a control's centre (read-only, no post-shot)."
  @spec move(t(), target(), keyword()) :: response()
  def move(%__MODULE__{} = cu, target, opts \\ []) do
    params =
      %{"action" => "mouse_move"}
      |> put_target(target)
      |> maybe_put("modifiers", modifiers(opts))

    run(cu, put_addressed(params, opts), opts)
  end

  @doc "Scroll `amount` steps in `:up`/`:down`/`:left`/`:right` at a point or a control."
  @spec scroll(t(), target(), atom() | String.t(), pos_integer(), keyword()) :: response()
  def scroll(%__MODULE__{} = cu, target, direction, amount, opts \\ []) do
    params =
      %{"action" => "scroll", "direction" => to_string(direction), "amount" => amount}
      |> put_target(target)

    run(cu, put_addressed(params, opts), opts)
  end

  @doc """
  Press the control `element_ref` names in the `elements` reply named by
  `:observation_id`, through the accessibility API: the pointer does not move and
  the press cannot miss.

  Offered only where the control's own action list advertises it — `elements`
  says so per element in `verbs`. Anywhere else this is refused
  `ax_action_unsupported` and nothing is dispatched; the sidecar never falls back
  to a click, because which one to send is the caller's decision.
  """
  @spec press(t(), String.t(), keyword()) :: response()
  def press(%__MODULE__{} = cu, element_ref, opts \\ []) when is_binary(element_ref) do
    params = %{"action" => "press", "element_ref" => element_ref}
    run(cu, put_addressed(params, opts), opts)
  end

  @doc """
  Set the value of the control `element_ref` names, then read it back: the receipt
  says `effect: "verified"` when the read-back matches, and `not_observed` when it
  does not (a secure field reads back masked, so it never verifies).

  Offered only where the control reports its value as settable — `elements` says
  so per element in `verbs`. Anywhere else this is refused
  `ax_action_unsupported`; `type` and `paste` remain for a field that is not.
  """
  @spec set_value(t(), String.t(), String.t(), keyword()) :: response()
  def set_value(%__MODULE__{} = cu, element_ref, value, opts \\ [])
      when is_binary(element_ref) and is_binary(value) do
    params = %{"action" => "set_value", "element_ref" => element_ref, "value" => value}
    run(cu, put_addressed(params, opts), opts)
  end

  @doc """
  Move keyboard focus to the control `element_ref` names. It never brings a window
  forward: when the control's window is not the active one the sidecar refuses
  `focus_refused` and sends nothing.
  """
  @spec focus(t(), String.t(), keyword()) :: response()
  def focus(%__MODULE__{} = cu, element_ref, opts \\ []) when is_binary(element_ref),
    do: run_verb(cu, "focus", element_ref, opts)

  @doc "Select the row, tab, cell or item `element_ref` names, then read it back."
  @spec select(t(), String.t(), keyword()) :: response()
  def select(%__MODULE__{} = cu, element_ref, opts \\ []) when is_binary(element_ref),
    do: run_verb(cu, "select", element_ref, opts)

  @doc """
  Bring the checkbox, switch or radio `element_ref` names to `on`. The state is read
  first: a control already there sends nothing and its receipt says `already: true`.
  """
  @spec toggle(t(), String.t(), boolean(), keyword()) :: response()
  def toggle(%__MODULE__{} = cu, element_ref, on, opts \\ [])
      when is_binary(element_ref) and is_boolean(on) do
    params = %{"action" => "toggle", "element_ref" => element_ref, "on" => on}
    run(cu, put_addressed(params, opts), opts)
  end

  @doc "Expand the disclosure, tree row or combo box `element_ref` names (state read first)."
  @spec expand(t(), String.t(), keyword()) :: response()
  def expand(%__MODULE__{} = cu, element_ref, opts \\ []) when is_binary(element_ref),
    do: run_verb(cu, "expand", element_ref, opts)

  @doc "Collapse the control `element_ref` names (state read first)."
  @spec collapse(t(), String.t(), keyword()) :: response()
  def collapse(%__MODULE__{} = cu, element_ref, opts \\ []) when is_binary(element_ref),
    do: run_verb(cu, "collapse", element_ref, opts)

  @doc "Scroll the control `element_ref` names into view, by its own scroll area."
  @spec scroll_to(t(), String.t(), keyword()) :: response()
  def scroll_to(%__MODULE__{} = cu, element_ref, opts \\ []) when is_binary(element_ref),
    do: run_verb(cu, "scroll_to", element_ref, opts)

  @doc """
  Wait on the accessibility tree until `until` holds or its `timeout_ms` (default
  5000, at most 25 000) passes. Read-only: it sends no input, ever.

  `until` is the wire's condition object, string keys: `"kind"` is one of
  `appears`, `disappears`, `enabled`, `value`, `window_title`, `menu_open`,
  `menu_closed` or `text`, beside the fields that kind needs (`role`/`name`,
  `element_ref`, `value`, `contains`, `scope`). The `element_ref` kinds need the
  `:observation_id` that listed the control.

  A condition that never held is still `{:ok, %{"met" => false}}`: the wait ran.
  The wait never outlives the request's own deadline; when that deadline is what
  ended it, the reply also says `"deadline_cut" => true`.
  """
  @spec wait_for(t(), map(), keyword()) :: response()
  def wait_for(%__MODULE__{} = cu, until, opts \\ []) when is_map(until) do
    params =
      %{"action" => "wait_for", "until" => until}
      |> put_display(opts)
      |> put_region(opts)
      |> put_observation(opts)

    run(cu, params, opts)
  end

  @doc "Press-drag from one pixel of the named image to another."
  @spec drag(t(), coord(), coord(), keyword()) :: response()
  def drag(%__MODULE__{} = cu, {fx, fy}, {tx, ty}, opts \\ []) do
    params = %{
      "action" => "left_click_drag",
      "from" => %{"x" => fx, "y" => fy},
      "to" => %{"x" => tx, "y" => ty}
    }

    run(cu, put_addressed(params, opts), opts)
  end

  @doc """
  Type a unicode string at the current focus — or, with `:element_ref` and the
  `:observation_id` that listed it, into that control through the accessibility
  API, replacing its selection.
  """
  @spec type(t(), String.t(), keyword()) :: response()
  def type(%__MODULE__{} = cu, text, opts \\ []) when is_binary(text) do
    params =
      %{"action" => "type", "text" => text}
      |> maybe_put("element_ref", Keyword.get(opts, :element_ref))
      |> put_observation(opts)

    run(cu, params, opts)
  end

  @doc ~S(Send a key chord, e.g. `"ctrl+s"` or `"cmd+shift+4"`.)
  @spec key(t(), String.t(), keyword()) :: response()
  def key(%__MODULE__{} = cu, chord, opts \\ []) when is_binary(chord),
    do: run(cu, %{"action" => "key", "chord" => chord}, opts)

  @doc "Sleep in the sidecar for `ms` (bounded by the protocol)."
  @spec wait(t(), pos_integer()) :: response()
  def wait(%__MODULE__{} = cu, ms) when is_integer(ms),
    do: run(cu, %{"action" => "wait", "ms" => ms}, [])

  @doc """
  Report the accessibility element under a pixel of the named image (role, title,
  description, value). Read-only; macOS only. Shadows `Kernel.inspect/2`.
  """
  @spec inspect(t(), coord(), keyword()) :: response()
  def inspect(%__MODULE__{} = cu, {x, y}, opts \\ []) do
    run(cu, put_addressed(%{"action" => "inspect", "x" => x, "y" => y}, opts), opts)
  end

  @doc """
  Block until the screen (or `:region`) changes, or `:timeout_ms` elapses; returns
  the resulting screenshot. Read-only. `:poll_ms` sets the check interval.
  """
  @spec wait_for_change(t(), keyword()) :: response()
  def wait_for_change(%__MODULE__{} = cu, opts \\ []) do
    params =
      %{"action" => "wait_for_change"}
      |> put_display(opts)
      |> put_region(opts)
      |> put_observation(opts)
      |> maybe_put("timeout_ms", Keyword.get(opts, :timeout_ms))
      |> maybe_put("poll_ms", Keyword.get(opts, :poll_ms))

    run(cu, params, opts)
  end

  @doc """
  Enumerate the accessibility elements under the focused window or a `:region`, so
  the caller can target by control rather than by raw pixels. Read-only; macOS
  only until the Linux build lists controls.

  Each element carries an `element_ref` scoped to this reply's `observation_id`,
  its normalised `role` and the platform's own `native_role`, its `label` (the
  control's title, else its description), its `value` (bounded, and absent for a
  secure field, which says `secure: true` instead), whether it is `enabled`, the `verbs` it has a route for, its
  states where known (`checked`, `expanded`, `selected`, `focused`), a `path` of up
  to three ancestor labels (nearest last, so two "Save" buttons are tellable
  apart) and, where the platform reports them, its `bounds` and the click point
  `x`, `y` in this reply's pixels.

  Options beside `:region`, `:display` and `:observation_id`:
    * `:scope` — where the walk starts: `:window`, `:app`, `:focused`, `:menu`,
      `:dialog`.
    * `:query` — `%{"role" => role, "name" => text}` (either or both): only the
      matching nodes, wherever they are in the scope.
    * `:skeleton` — `true` cuts the walk at depth 3; a cut container says how many
      children it `hidden` and carries a reference to drill into.
    * `:root` — drill into that reference of the listing `:observation_id` names;
      a `:query` or `:skeleton` beside it applies inside that subtree.

  A walk that stopped at one of its bounds says so in `truncated` (`"nodes"`,
  `"depth"` or `"time"`); a reply without that key listed everything it found.
  """
  @spec elements(t(), keyword()) :: response()
  def elements(%__MODULE__{} = cu, opts \\ []) do
    params =
      %{"action" => "elements"}
      |> put_display(opts)
      |> put_region(opts)
      |> put_observation(opts)
      |> maybe_put("scope", wire_name(Keyword.get(opts, :scope)))
      |> maybe_put("query", Keyword.get(opts, :query))
      |> maybe_put("skeleton", Keyword.get(opts, :skeleton))
      |> maybe_put("root", Keyword.get(opts, :root))

    run(cu, params, opts)
  end

  @doc """
  List a display's on-screen windows — app, title, focus, and each window's bounds
  ALREADY EXPRESSED AS A `region` in that display's screenshot space.

  Use it for precision. A full screenshot is downscaled so its long edge fits the
  sidecar's size budget, so on a large or ultrawide display the window you care
  about arrives at a fraction of its real size; a `region` crop is rescaled to that
  same budget on its own, so cropping to one window recovers most of it. Take a
  window's `region` from here and pass it both to `screenshot/2` and to the click
  that follows — the coordinates you read are then in the magnified crop.

  Read-only. Needs the same screen-capture permission as `screenshot/2`: window
  titles are withheld from an unpermitted process, so an empty list on a desktop
  that plainly has windows means the grant is missing, not that nothing is open.
  """
  @spec windows(t(), keyword()) :: response()
  def windows(%__MODULE__{} = cu, opts \\ []) do
    run(cu, put_display(%{"action" => "windows"}, opts), opts)
  end

  @doc """
  Bind ONE window, by the `id` a `windows/2` listing gave it.

  The reply carries the `target_id` to pass as `:target_id` from then on, the
  window's `app` and `title`, the `methods` this binding really offers
  (`foreground_hid`, plus `ax` where exactly one of the application's accessibility
  windows matched this one), `ax_binding`, and a first observation of the window.

  One target per helper: selecting again replaces it. A bound window is watched, so
  an action that names one is refused rather than run when the window has gone,
  been minimized, moved since the image was taken, or — for a click — been covered
  by something else.
  """
  @spec select_target(t(), non_neg_integer(), keyword()) :: response()
  def select_target(%__MODULE__{} = cu, window_id, opts \\ []) when is_integer(window_id) do
    run(cu, %{"action" => "select_target", "window_id" => window_id}, opts)
  end

  @doc "Release the bound window, with its capture stream and its indicator. Idempotent."
  @spec release_target(t(), keyword()) :: response()
  def release_target(%__MODULE__{} = cu, opts \\ []),
    do: run(cu, %{"action" => "release_target"}, opts)

  @doc "Paste `text` via the clipboard — fast and unicode-safe for long strings."
  @spec paste(t(), String.t(), keyword()) :: response()
  def paste(%__MODULE__{} = cu, text, opts \\ []) when is_binary(text),
    do: run(cu, %{"action" => "paste", "text" => text}, opts)

  @doc """
  Non-prompting OS-permission probe: whether screen capture and input control are
  actually available, plus the platform and display server. Not a model action.

  On Linux it also carries, read-only, what a doctor shows of accessibility (each
  `nil` on a build that reports none, as macOS):

    * `:x11_endpoint` — whether an X endpoint answers, as the session or beside
      Wayland (XWayland).
    * `:display_adopted` — the helper had no display of its own and took the
      session's from the user manager, with its X authority, and reaches it as its
      own.
    * `:accessibility_bus` — `%{state: "answering" | "empty" | "unavailable",
      applications:, cause:, detail:, status:}`: the registry's application count,
      or why no registry answered (`cause`: `"no_session_bus"`, `"not_installed"`,
      `"no_address"`, `"unreachable"`, `"not_answering"`), and the session's
      `org.a11y.Status` as `%{is_enabled:, screen_reader_enabled:}` (`nil` unread).
    * `:accessibility_packages` — `%{source:, at_spi2_core:, atk_bridge:}`, the
      package database's versions (`source: "dpkg"`; all `nil` where none is read).
    * `:toolkit_notes` — `[%{toolkit:, note:}]`, what is known without reading any
      application (Qt while the status reads off; Chromium's switches).
    * `:x11_pixels` — `%{xtest:, keymap:}`: whether the X endpoint has the XTEST
      extension and answers its keyboard map, the X11 pixel route's two needs
      (`nil` where no X endpoint answers).
    * `:portal` — `%{running:, version:, registry:, screenshot_permission:}`,
      read where a session bus answers (`nil` where none does), raising no dialog
      and starting neither the portal nor its store: whether the portal answered
      (`false` where it is not running), the installed xdg-desktop-portal's
      version (`nil` where the package database does not say), whether its host
      Registry is there (1.18 or later; without it GNOME Wayland's pixels are
      refused; `nil` where the portal is not running), and the PermissionStore's
      stored Screenshot answer for `io.tezra.fermix.ComputerUse`: `"yes"`, `"no"`
      (every screenshot is refused `consent_refused` until the person resets it),
      `"unset"`, or `"not_running"` (the store is not running, so nothing was
      read).
  """
  @spec probe(t()) :: {:ok, map()} | {:error, term()}
  def probe(%__MODULE__{driver: driver, state: state}) do
    case driver.execute(state, %{"action" => "probe"}) do
      {:ok, response} -> {:ok, normalize_probe(response)}
      {:error, reason} -> {:error, reason}
    end
  end

  @doc """
  Milliseconds since the last input event the OS saw — a coexistence signal that
  lets a policy layer yield the seat to a present human. Operational (not a model
  action), macOS only.

  It counts ANY input the OS saw, INCLUDING synthetic events this library posts, so
  a caller that also drives input must disambiguate "the human vs my own last action"
  itself — compux only reports the raw number.
  """
  @spec idle_ms(t()) :: {:ok, non_neg_integer()} | {:error, term()}
  def idle_ms(%__MODULE__{driver: driver, state: state}) do
    case driver.execute(state, %{"action" => "idle_ms"}) do
      {:ok, %{"idle_ms" => ms}} when is_integer(ms) and ms >= 0 -> {:ok, ms}
      {:ok, other} -> {:error, {:malformed_idle_response, other}}
      {:error, reason} -> {:error, reason}
    end
  end

  @doc """
  Block in the sidecar until the human has been idle for `:idle_ms` (default 1000),
  bounded by `:timeout_ms` (default 3000). Returns `{:ok, %{"idle" => boolean,
  "idle_ms" => n}}` — `idle: true` if the quiet window was reached, `false` if it
  timed out with the human still active. Operational, macOS only; the coexistence
  micro-defer primitive.
  """
  @spec wait_for_idle(t(), keyword()) :: response()
  def wait_for_idle(%__MODULE__{driver: driver, state: state}, opts \\ []) do
    request =
      %{"action" => "wait_for_idle"}
      |> maybe_put("idle_ms", Keyword.get(opts, :idle_ms))
      |> maybe_put("timeout_ms", Keyword.get(opts, :timeout_ms))
      |> maybe_put("poll_ms", Keyword.get(opts, :poll_ms))

    driver.execute(state, request)
  end

  # --- internals ------------------------------------------------------------

  defp run(%__MODULE__{driver: driver, state: state}, params, opts) do
    # The bound window is added here rather than at each verb, so every action that
    # can name one takes `:target_id` the same way and the ones that cannot are
    # refused by name in `Protocol.validate/1` instead of silently ignoring it.
    params =
      params
      |> maybe_put("check", wire_name(Keyword.get(opts, :check)))
      |> maybe_put("target_id", Keyword.get(opts, :target_id))

    with {:ok, request} <- Protocol.validate(params) do
      driver.execute(state, request)
    end
  end

  # A check kind or a scope as the wire spells it. An atom is the ergonomic form and
  # a string is what a caller reading it off a config already has; anything else
  # (a check's object form among them) goes through unchanged so
  # `Protocol.validate/1` judges it by name rather than here.
  defp wire_name(nil), do: nil
  defp wire_name(name) when is_atom(name), do: Atom.to_string(name)
  defp wire_name(name), do: name

  # A verb on a reference that takes nothing but the control.
  defp run_verb(cu, action, element_ref, opts) do
    params = %{"action" => action, "element_ref" => element_ref}
    run(cu, put_addressed(params, opts), opts)
  end

  defp maybe_default_binary_path(Compux.PortDriver, opts) do
    if Keyword.has_key?(opts, :binary_path),
      do: opts,
      else: Keyword.put(opts, :binary_path, Binary.path!())
  end

  defp maybe_default_binary_path(_other_driver, opts), do: opts

  defp check_protocol(identity, driver, state) do
    ours = Protocol.protocol_version()
    theirs = Map.get(identity, "protocol_version")

    if theirs == ours do
      :ok
    else
      driver.stop(state)
      {:error, {:protocol_mismatch, %{library: ours, sidecar: theirs}}}
    end
  end

  defp identity_info(identity) do
    %{
      protocol_version: Map.get(identity, "protocol_version"),
      compux_version: Map.get(identity, "compux_version"),
      actions: Map.get(identity, "actions", []),
      sidecar_generation: Map.get(identity, "sidecar_generation"),
      capabilities: Map.get(identity, "capabilities", %{})
    }
  end

  defp click_action(:left), do: "left_click"
  defp click_action(:right), do: "right_click"
  defp click_action(:double), do: "double_click"

  defp modifiers(opts) do
    case Keyword.get(opts, :modifiers) do
      nil -> nil
      list when is_list(list) -> Enum.map(list, &to_string/1)
    end
  end

  defp put_opts(params, opts),
    do:
      params
      |> put_display(opts)
      |> put_region(opts)
      |> put_observation(opts)
      |> put_jpeg_quality(opts)

  # An addressed action names the reply its target was read from and takes no
  # rectangle: the sidecar holds the transform that image was made with, and the
  # native reference behind each element.
  defp put_addressed(params, opts),
    do: params |> put_display(opts) |> put_observation(opts)

  # A point or a control, never both — so the two forms cannot be sent together
  # from here at all.
  defp put_target(params, {x, y}) when is_integer(x) and is_integer(y),
    do: params |> Map.put("x", x) |> Map.put("y", y)

  defp put_target(params, {:element, reference}) when is_binary(reference),
    do: Map.put(params, "element_ref", reference)

  defp put_observation(params, opts),
    do: maybe_put(params, "observation_id", Keyword.get(opts, :observation_id))

  defp put_jpeg_quality(params, opts),
    do: maybe_put(params, "jpeg_quality", Keyword.get(opts, :jpeg_quality))

  defp put_display(params, opts), do: maybe_put(params, "display", Keyword.get(opts, :display))

  defp put_region(params, opts),
    do: maybe_put(params, "region", region_map(Keyword.get(opts, :region)))

  defp region_map(nil), do: nil
  defp region_map({x, y, w, h}), do: %{"x" => x, "y" => y, "w" => w, "h" => h}
  defp region_map(%{} = map), do: map

  defp maybe_put(map, _key, nil), do: map
  defp maybe_put(map, key, value), do: Map.put(map, key, value)

  defp normalize_probe(response) do
    %{
      platform: probe_string(response, "platform"),
      display_server: probe_string(response, "display_server"),
      screen_capture: Map.get(response, "screen_capture") == true,
      input_control: Map.get(response, "input_control") == true,
      x11_endpoint: probe_boolean(Map.get(response, "x11_endpoint")),
      display_adopted: probe_boolean(Map.get(response, "display_adopted")),
      accessibility_bus: probe_bus(Map.get(response, "accessibility_bus")),
      accessibility_packages: probe_packages(Map.get(response, "accessibility_packages")),
      toolkit_notes: probe_notes(Map.get(response, "toolkit_notes")),
      x11_pixels: probe_x11_pixels(Map.get(response, "x11_pixels")),
      portal: probe_portal(Map.get(response, "portal"))
    }
  end

  defp probe_string(response, key) do
    case Map.get(response, key) do
      value when is_binary(value) -> value
      _other -> "unknown"
    end
  end

  # The Linux facts. A build that reports none (macOS) leaves each nil.
  defp probe_boolean(value) when is_boolean(value), do: value
  defp probe_boolean(_absent), do: nil

  @bus_states ~w(answering empty unavailable)

  defp probe_bus(%{} = bus) do
    %{
      state: if(bus["state"] in @bus_states, do: bus["state"], else: "unknown"),
      applications: probe_count(bus["applications"]),
      cause: probe_text(bus["cause"]),
      detail: probe_text(bus["detail"]),
      status: probe_status(bus["status"])
    }
  end

  defp probe_bus(_absent), do: nil

  defp probe_status(%{} = status) do
    %{
      is_enabled: probe_boolean(status["is_enabled"]),
      screen_reader_enabled: probe_boolean(status["screen_reader_enabled"])
    }
  end

  defp probe_status(_unread), do: nil

  defp probe_packages(%{} = packages) do
    %{
      source: probe_text(packages["source"]),
      at_spi2_core: probe_text(packages["at_spi2_core"]),
      atk_bridge: probe_text(packages["atk_bridge"])
    }
  end

  defp probe_packages(_absent), do: nil

  defp probe_notes(notes) when is_list(notes) do
    for %{"toolkit" => toolkit, "note" => note} <- notes,
        is_binary(toolkit) and is_binary(note),
        do: %{toolkit: toolkit, note: note}
  end

  defp probe_notes(_absent), do: nil

  defp probe_x11_pixels(%{} = pixels) do
    %{xtest: probe_boolean(pixels["xtest"]), keymap: probe_boolean(pixels["keymap"])}
  end

  defp probe_x11_pixels(_absent), do: nil

  @stored_answers ~w(yes no unset not_running)

  defp probe_portal(%{} = portal) do
    stored = portal["screenshot_permission"]

    %{
      running: probe_boolean(portal["running"]),
      version: probe_text(portal["version"]),
      registry: probe_boolean(portal["registry"]),
      screenshot_permission: if(stored in @stored_answers, do: stored)
    }
  end

  defp probe_portal(_absent), do: nil

  defp probe_count(count) when is_integer(count) and count >= 0, do: count
  defp probe_count(_absent), do: nil

  defp probe_text(text) when is_binary(text), do: text
  defp probe_text(_absent), do: nil
end
