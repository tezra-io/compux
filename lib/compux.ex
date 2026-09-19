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
  reply's observation) beside its role, label, value, whether it is enabled, what
  it can do and where it is. A reference is always sent with its
  `observation_id`; on its own it means nothing.

      {:ok, list} = Compux.elements(cu)
      image       = list["observation_id"]
      [save | _]  = Enum.filter(list["elements"], &("press" in &1["actions"]))

      :ok = Compux.press(cu, save["element_ref"], observation_id: image)
      :ok = Compux.click(cu, {:element, save["element_ref"]}, observation_id: image)

  `press/3` and `set_value/4` do not touch the pointer at all, and are offered
  only where the control itself advertises support. `click/3`, `move/3` and
  `scroll/5` may take `{:element, ref}` instead of a point, and the sidecar
  re-reads that control's bounds before it clicks its centre.

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
  `opts` may carry `:screenshot_after` (a transport flag, not an action argument).
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
  `:double`; other opts: `:modifiers`, `:display`, `:screenshot_after`.
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
  says so per element in `actions`. Anywhere else this is refused
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
  so per element in `settable`. Anywhere else this is refused
  `ax_action_unsupported`; `type` and `paste` remain for a field that is not.
  """
  @spec set_value(t(), String.t(), String.t(), keyword()) :: response()
  def set_value(%__MODULE__{} = cu, element_ref, value, opts \\ [])
      when is_binary(element_ref) and is_binary(value) do
    params = %{"action" => "set_value", "element_ref" => element_ref, "value" => value}
    run(cu, put_addressed(params, opts), opts)
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

  @doc "Type a unicode string at the current focus."
  @spec type(t(), String.t(), keyword()) :: response()
  def type(%__MODULE__{} = cu, text, opts \\ []) when is_binary(text),
    do: run(cu, %{"action" => "type", "text" => text}, opts)

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
  only.

  Each element carries an `element_ref` scoped to this reply's `observation_id`,
  its `role`, its `label` (the control's title, else its description), its `value`
  (bounded, and absent for a secure field), whether it is `enabled`, the `actions`
  this build can perform on it, whether its value is `settable`, its `bounds`, a
  `path` of up to three ancestor labels (nearest last, so two "Save" buttons are
  tellable apart) and the click point `x`, `y` in this reply's pixels.

  A walk that stopped at one of its bounds says so in `truncated` (`"nodes"`,
  `"depth"` or `"time"`); a reply without that key listed everything it found.
  """
  @spec elements(t(), keyword()) :: response()
  def elements(%__MODULE__{} = cu, opts \\ []) do
    params =
      %{"action" => "elements"} |> put_display(opts) |> put_region(opts) |> put_observation(opts)

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

  @doc "Paste `text` via the clipboard — fast and unicode-safe for long strings."
  @spec paste(t(), String.t(), keyword()) :: response()
  def paste(%__MODULE__{} = cu, text, opts \\ []) when is_binary(text),
    do: run(cu, %{"action" => "paste", "text" => text}, opts)

  @doc """
  Non-prompting OS-permission probe: whether screen capture and input control are
  actually available, plus the platform and display server. Not a model action.
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
    with {:ok, request} <- Protocol.validate(params) do
      request =
        if Keyword.get(opts, :screenshot_after, false),
          do: Map.put(request, "screenshot_after", true),
          else: request

      driver.execute(state, request)
    end
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
      input_control: Map.get(response, "input_control") == true
    }
  end

  defp probe_string(response, key) do
    case Map.get(response, key) do
      value when is_binary(value) -> value
      _other -> "unknown"
    end
  end
end
