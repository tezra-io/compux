defmodule Compux.Protocol do
  @moduledoc """
  The line-framed JSON action protocol between a caller and the `compux` OS-driver
  sidecar: one request line to the sidecar's stdin, one response line back. This
  module is the CONTRACT the Rust `enigo`+`xcap` sidecar (`native/compux`)
  implements — it is pure (validation + encode/decode + read-only classification)
  and fully testable without the binary.

  Validation is fail-loud: an out-of-shape action is rejected with a clear reason
  rather than forwarded to a process that drives real input.

  `protocol_version/0` is the wire-compatibility gate. It is a monotonic integer,
  bumped ONLY on a wire-incompatible change — NOT the package version. A consumer
  compares it against the sidecar's reported `protocol_version` (the `hello`
  handshake performed by `Compux.start/1`) and refuses a mismatched binary, so the
  compiled-in encoder and the installed sidecar can never silently drift.
  """

  # v3 added the operational idle-detection actions `idle_ms` + `wait_for_idle`
  # (coexistence — let a policy layer yield the seat to a present human). They are
  # NOT model verbs (excluded from `@actions`, like `probe`), but the wire changed,
  # so the version bumps and a mismatched sidecar is refused at the handshake.
  @protocol_version 3

  @actions ~w(screenshot left_click right_click double_click mouse_move left_click_drag scroll type key wait inspect wait_for_change paste elements)
  @read_only ~w(screenshot mouse_move wait inspect wait_for_change elements)
  @modifiers ~w(cmd ctrl alt shift)
  @scroll_directions ~w(up down left right)
  @max_type_bytes 10_000
  @max_wait_ms 10_000
  # `wait_for_change` must finish inside the caller's per-action deadline (30s in
  # Fermix), so its poll budget is capped well under that.
  @max_wait_for_change_ms 25_000
  @min_poll_ms 50
  @max_poll_ms 5_000

  @doc "The wire-compatibility version this build speaks (see the moduledoc)."
  @spec protocol_version() :: pos_integer()
  def protocol_version, do: @protocol_version

  @spec actions() :: [String.t()]
  def actions, do: @actions

  @doc """
  Read-only actions never mutate the screen, so they carry no post-action
  screenshot and a consumer may auto-run them without a confirmation step.
  """
  @spec read_only?(String.t()) :: boolean()
  def read_only?(action) when is_binary(action), do: action in @read_only

  @doc """
  Validate + canonicalize an action params map (string keys) into a sidecar
  request. Returns `{:ok, request}` or `{:error, reason}`. The caller fills the
  default `display` and the transport `screenshot_after` flag before encoding;
  this function validates only the action's own arguments.
  """
  @spec validate(map()) :: {:ok, map()} | {:error, String.t()}
  def validate(params) when is_map(params) do
    case Map.get(params, "action") do
      action when action in @actions -> validate_action(action, params)
      nil -> {:error, "missing required field: action"}
      other -> {:error, "unknown action: #{inspect(other)}"}
    end
  end

  def validate(_other), do: {:error, "action params must be a map"}

  @doc "Encode a validated request to a single JSON line for the sidecar's stdin."
  @spec encode_request(map()) :: binary()
  def encode_request(request) when is_map(request), do: Jason.encode!(request) <> "\n"

  @doc """
  Decode one sidecar response line. A success carries `ok: true`; a failure
  carries `ok: false` + `error`. Anything else (or invalid JSON) fails loud so a
  malformed sidecar can never look like a successful action.
  """
  @spec decode_response(binary()) :: {:ok, map()} | {:error, String.t()}
  def decode_response(line) when is_binary(line) do
    case Jason.decode(String.trim(line)) do
      {:ok, %{"ok" => true} = resp} ->
        {:ok, resp}

      {:ok, %{"ok" => false, "error" => error}} ->
        {:error, to_string(error)}

      {:ok, %{"error" => error}} ->
        {:error, to_string(error)}

      {:ok, other} ->
        {:error, "malformed sidecar response: #{inspect(other)}"}

      {:error, %Jason.DecodeError{} = error} ->
        {:error, "invalid JSON from sidecar: #{Exception.message(error)}"}
    end
  end

  defp validate_action("screenshot", params) do
    with {:ok, display} <- opt_display(params),
         {:ok, region} <- opt_region(params) do
      request = put_display(%{"action" => "screenshot"}, display)
      {:ok, put_region(request, region)}
    end
  end

  defp validate_action(action, params)
       when action in ~w(left_click right_click double_click mouse_move) do
    with {:ok, x} <- coord(params, "x"),
         {:ok, y} <- coord(params, "y"),
         {:ok, modifiers} <- opt_modifiers(params),
         {:ok, display} <- opt_display(params),
         {:ok, region} <- opt_region(params) do
      request = %{"action" => action, "x" => x, "y" => y}
      request = if modifiers == [], do: request, else: Map.put(request, "modifiers", modifiers)
      {:ok, put_region(put_display(request, display), region)}
    end
  end

  defp validate_action("inspect", params) do
    with {:ok, x} <- coord(params, "x"),
         {:ok, y} <- coord(params, "y"),
         {:ok, display} <- opt_display(params),
         {:ok, region} <- opt_region(params) do
      request = put_display(%{"action" => "inspect", "x" => x, "y" => y}, display)
      {:ok, put_region(request, region)}
    end
  end

  # Block until the screen (or `region`) differs from a baseline, or `timeout_ms`
  # elapses; returns the resulting screenshot. Read-only.
  defp validate_action("wait_for_change", params) do
    with {:ok, display} <- opt_display(params),
         {:ok, region} <- opt_region(params),
         {:ok, timeout_ms} <- opt_bounded(params, "timeout_ms", 1, @max_wait_for_change_ms),
         {:ok, poll_ms} <- opt_bounded(params, "poll_ms", @min_poll_ms, @max_poll_ms) do
      request =
        %{"action" => "wait_for_change"}
        |> put_display(display)
        |> put_region(region)
        |> maybe_put("timeout_ms", timeout_ms)
        |> maybe_put("poll_ms", poll_ms)

      {:ok, request}
    end
  end

  # Enumerate the accessibility elements (role/label/bounds) under a window or
  # `region` so the caller can target by element, not raw pixels. Read-only.
  defp validate_action("elements", params) do
    with {:ok, display} <- opt_display(params),
         {:ok, region} <- opt_region(params) do
      {:ok, put_region(put_display(%{"action" => "elements"}, display), region)}
    end
  end

  # Like `type`, but sets the clipboard and issues a paste — fast + unicode-safe
  # for long text (char-by-char typing can exceed the action deadline).
  defp validate_action("paste", params) do
    case Map.get(params, "text") do
      text when is_binary(text) and byte_size(text) > 0 and byte_size(text) <= @max_type_bytes ->
        {:ok, %{"action" => "paste", "text" => text}}

      text when is_binary(text) ->
        {:error, "paste.text must be 1..#{@max_type_bytes} bytes"}

      _other ->
        {:error, "paste requires a non-empty string text"}
    end
  end

  defp validate_action("left_click_drag", params) do
    with {:ok, from} <- point(params, "from"),
         {:ok, to} <- point(params, "to"),
         {:ok, display} <- opt_display(params),
         {:ok, region} <- opt_region(params) do
      request = put_display(%{"action" => "left_click_drag", "from" => from, "to" => to}, display)
      {:ok, put_region(request, region)}
    end
  end

  defp validate_action("scroll", params) do
    with {:ok, x} <- coord(params, "x"),
         {:ok, y} <- coord(params, "y"),
         {:ok, direction} <- scroll_direction(params),
         {:ok, amount} <- positive(params, "amount"),
         {:ok, display} <- opt_display(params),
         {:ok, region} <- opt_region(params) do
      request = %{
        "action" => "scroll",
        "x" => x,
        "y" => y,
        "direction" => direction,
        "amount" => amount
      }

      {:ok, put_region(put_display(request, display), region)}
    end
  end

  defp validate_action("type", params) do
    case Map.get(params, "text") do
      text when is_binary(text) and byte_size(text) > 0 and byte_size(text) <= @max_type_bytes ->
        {:ok, %{"action" => "type", "text" => text}}

      text when is_binary(text) ->
        {:error, "type.text must be 1..#{@max_type_bytes} bytes"}

      _other ->
        {:error, "type requires a non-empty string text"}
    end
  end

  defp validate_action("key", params) do
    case Map.get(params, "chord") do
      chord when is_binary(chord) and chord != "" -> {:ok, %{"action" => "key", "chord" => chord}}
      _other -> {:error, ~s(key requires a non-empty string chord, e.g. "ctrl+s")}
    end
  end

  defp validate_action("wait", params) do
    case Map.get(params, "ms") do
      ms when is_integer(ms) and ms > 0 and ms <= @max_wait_ms ->
        {:ok, %{"action" => "wait", "ms" => ms}}

      _other ->
        {:error, "wait.ms must be a positive integer ≤ #{@max_wait_ms}"}
    end
  end

  defp coord(params, key) do
    case Map.get(params, key) do
      value when is_integer(value) and value >= 0 -> {:ok, value}
      _other -> {:error, "#{key} must be a non-negative integer (screenshot pixel space)"}
    end
  end

  defp point(params, key) do
    case Map.get(params, key) do
      %{"x" => x, "y" => y} when is_integer(x) and is_integer(y) and x >= 0 and y >= 0 ->
        {:ok, %{"x" => x, "y" => y}}

      _other ->
        {:error, ~s(#{key} must be an object with non-negative integer x and y)}
    end
  end

  defp positive(params, key) do
    case Map.get(params, key) do
      value when is_integer(value) and value > 0 -> {:ok, value}
      _other -> {:error, "#{key} must be a positive integer"}
    end
  end

  defp scroll_direction(params) do
    case Map.get(params, "direction") do
      direction when direction in @scroll_directions -> {:ok, direction}
      _other -> {:error, "scroll.direction must be one of #{Enum.join(@scroll_directions, ", ")}"}
    end
  end

  defp opt_modifiers(params) do
    case Map.get(params, "modifiers") do
      nil ->
        {:ok, []}

      modifiers when is_list(modifiers) ->
        if Enum.all?(modifiers, &(&1 in @modifiers)),
          do: {:ok, modifiers},
          else: {:error, "modifiers must be a subset of #{inspect(@modifiers)}"}

      _other ->
        {:error, "modifiers must be a list of strings"}
    end
  end

  defp opt_display(params) do
    case Map.get(params, "display") do
      nil -> {:ok, nil}
      display when is_integer(display) and display >= 0 -> {:ok, display}
      _other -> {:error, "display must be a non-negative integer"}
    end
  end

  defp put_display(request, nil), do: request
  defp put_display(request, display), do: Map.put(request, "display", display)

  # An optional integer bounded to `min..max`; absent → `{:ok, nil}` (the sidecar
  # fills a default), present-and-in-range → `{:ok, int}`, otherwise a loud error.
  defp opt_bounded(params, key, min, max) do
    case Map.get(params, key) do
      nil -> {:ok, nil}
      value when is_integer(value) and value >= min and value <= max -> {:ok, value}
      _other -> {:error, "#{key} must be an integer in #{min}..#{max}"}
    end
  end

  defp maybe_put(request, _key, nil), do: request
  defp maybe_put(request, key, value), do: Map.put(request, key, value)

  # A zoom rectangle in the full-screenshot pixel space — the coordinates the model
  # reads off a normal screenshot. Passing the same `region` on a `screenshot` and the
  # follow-up click maps the click back through the crop.
  defp opt_region(params) do
    case Map.get(params, "region") do
      nil -> {:ok, nil}
      %{"x" => x, "y" => y, "w" => w, "h" => h} -> validate_region(x, y, w, h)
      _other -> region_error()
    end
  end

  defp validate_region(x, y, w, h) do
    if region_dims_valid?(x, y, w, h) do
      {:ok, %{"x" => x, "y" => y, "w" => w, "h" => h}}
    else
      region_error()
    end
  end

  defp region_dims_valid?(x, y, w, h) do
    Enum.all?([x, y, w, h], &is_integer/1) and x >= 0 and y >= 0 and w > 0 and h > 0
  end

  defp region_error do
    {:error,
     "region must be an object with non-negative integer x,y and positive integer w,h " <>
       "(in the full-screenshot pixel space)"}
  end

  defp put_region(request, nil), do: request
  defp put_region(request, region), do: Map.put(request, "region", region)
end
