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
      :ok         = Compux.click(cu, {120, 80}, button: :left, modifiers: [:cmd])
      {:ok, el}   = Compux.inspect(cu, {120, 80})
      :ok         = Compux.stop(cu)

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

  @doc "The sidecar's reported identity (`:protocol_version`, `:compux_version`, `:actions`)."
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

  @doc "Capture the target display (optionally a `:region` and `:display`)."
  @spec screenshot(t(), keyword()) :: response()
  def screenshot(%__MODULE__{} = cu, opts \\ []),
    do: run(cu, put_opts(%{"action" => "screenshot"}, opts), opts)

  @doc """
  Click at a screenshot-space coordinate. `:button` is `:left` (default), `:right`,
  or `:double`; other opts: `:modifiers`, `:display`, `:region`, `:screenshot_after`.
  """
  @spec click(t(), coord(), keyword()) :: response()
  def click(%__MODULE__{} = cu, {x, y}, opts \\ []) do
    params =
      %{"action" => click_action(Keyword.get(opts, :button, :left)), "x" => x, "y" => y}
      |> maybe_put("modifiers", modifiers(opts))

    run(cu, put_opts(params, opts), opts)
  end

  @doc "Move the pointer to a screenshot-space coordinate (read-only, no post-shot)."
  @spec move(t(), coord(), keyword()) :: response()
  def move(%__MODULE__{} = cu, {x, y}, opts \\ []) do
    params =
      maybe_put(%{"action" => "mouse_move", "x" => x, "y" => y}, "modifiers", modifiers(opts))

    run(cu, put_opts(params, opts), opts)
  end

  @doc "Scroll `amount` steps in `:up`/`:down`/`:left`/`:right` at a coordinate."
  @spec scroll(t(), coord(), atom() | String.t(), pos_integer(), keyword()) :: response()
  def scroll(%__MODULE__{} = cu, {x, y}, direction, amount, opts \\ []) do
    params = %{
      "action" => "scroll",
      "x" => x,
      "y" => y,
      "direction" => to_string(direction),
      "amount" => amount
    }

    run(cu, put_opts(params, opts), opts)
  end

  @doc "Press-drag from one screenshot-space coordinate to another."
  @spec drag(t(), coord(), coord(), keyword()) :: response()
  def drag(%__MODULE__{} = cu, {fx, fy}, {tx, ty}, opts \\ []) do
    params = %{
      "action" => "left_click_drag",
      "from" => %{"x" => fx, "y" => fy},
      "to" => %{"x" => tx, "y" => ty}
    }

    run(cu, put_opts(params, opts), opts)
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
  Report the accessibility element under a screenshot-space coordinate (role,
  title, description, value). Read-only; macOS only. Shadows `Kernel.inspect/2`.
  """
  @spec inspect(t(), coord(), keyword()) :: response()
  def inspect(%__MODULE__{} = cu, {x, y}, opts \\ []) do
    params = put_display(%{"action" => "inspect", "x" => x, "y" => y}, opts)
    run(cu, put_region(params, opts), opts)
  end

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
      actions: Map.get(identity, "actions", [])
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

  defp put_opts(params, opts), do: params |> put_display(opts) |> put_region(opts)

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
