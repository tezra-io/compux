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
  #
  # v4 added `windows` — a read-only listing of a display's on-screen windows whose
  # bounds come back as ready-to-use `region`s. It exists for PRECISION: a full
  # screenshot is downscaled to fit `MAX_EDGE`, so on a large or ultrawide display
  # the app the caller cares about arrives tiny, while a region crop is rescaled to
  # that budget on its own. Returning regions rather than raw geometry keeps every
  # coordinate in the one transform the sidecar already proves.
  #
  # v5 (M28 grounding integrity): `screenshot` gained three OPTIONAL fields —
  # `rulers` (draw the image's own coordinate grid on it), `marks` (badge the
  # accessibility click points and return their id table), and `annotate_point`
  # (mark an executed click in a check image). Additive at the JSON layer, but a
  # consumer advertising `marks` against an older sidecar would get silently
  # un-annotated images and no table, so the version bumps and the handshake
  # refuses the pairing loudly. The sidecar also replaced the pure long-edge sent
  # budget with the looser of long-edge and pixel-area rules (never upscaling),
  # so extreme aspect ratios stop arriving unreadably small.
  #
  # v6 (MILESTONE_32 capture mode): the sidecar gained the control actions
  # `observe_start` / `observe_stop` and an UNSOLICITED push wire —
  # `{"type":"event"|"ack", …}` frames streamed after `observe_start` is acked.
  # Like `probe`/`idle_ms` these are operational, NOT model verbs (excluded from
  # `@actions`), but the wire is no longer strictly one-response-per-request, so a
  # pre-v6 sidecar cannot speak it and the handshake refuses the pairing. The
  # consuming Fermix `Capturer` owns the Port and demuxes the discriminated frames.
  #
  # Sidecar 0.9.0 stays on v6: it adds the `browser.navigated` observation kind and the
  # browser context (`browser_id`/`window_ref`/`tab_ref`/`host`/`private_state`) on
  # `field.value`, both ADDITIVE fields on that same push wire, and accepts-and-ignores
  # the retired `sites` key of `observe_start` — no control action changed.
  #
  # v7 (M42 slice 2, transport and control): the action wire becomes TAGGED and
  # CORRELATED. Every line now carries a `type` (`request`, `response`, `control`,
  # `control_ack`, `session_event`, beside computer-history's unchanged `ack` and
  # `event`), every request a `request_id` the response echoes, and — after the
  # handshake — the `sidecar_generation`, `session_generation` and
  # `authorization_generation` a frame belongs to. A mutating request also carries
  # an increasing `mutation_seq`, and its response carries a `receipt` saying
  # whether input was dispatched. The caller's remaining budget rides as
  # `deadline_ms`, deliberately NOT `timeout_ms`, which two actions have used as
  # an argument of their own since v2. Requests and responses no longer pair by
  # ORDER, so the desync class — a late frame answering the next action — is gone;
  # `hello` is itself a request now, and its response returns the sidecar's boot
  # generation and a `capabilities` map. `Compux.Frame` owns the shapes.
  #
  # v8 (M42 slice 3, observation identity): every reply that hands the caller
  # coordinates names the image they were read from (`observation_id`), and every
  # coordinate sent back names that image. An action that ADDRESSES a point
  # (`left_click`, `right_click`, `double_click`, `mouse_move`, `left_click_drag`,
  # `scroll`, `inspect`) carries `observation_id` and no `region`: the sidecar
  # stores the transform with the image and uses it as stored, so nothing
  # re-derives it from a rectangle the caller echoed back. An action that PRODUCES
  # an image or a list of coordinates (`screenshot`, `elements`, `wait_for_change`)
  # keeps `region` and may name an observation beside it, and the rectangle is then
  # read in THAT image's pixels. Refusing `region` on a click is the wire change
  # that cannot be additive, so the version bumps and the handshake refuses the
  # pairing.
  #
  # v9 (M42 slice 4, semantic references): `elements` answers controls the caller
  # can NAME. Each element (and each mark) carries an `element_ref` — `e1`, `e2`,
  # … scoped to the observation it was listed in — beside its role, label, value,
  # whether it is enabled, what it can do, whether its value can be set, its
  # bounds and a short path of ancestor labels. Two actions address a control
  # rather than a point: `press` and `set_value`, offered only where the control
  # itself advertises support and refused with a typed error everywhere else,
  # never quietly replaced by a click. A pointer action may also carry an
  # `element_ref` INSTEAD of `x`/`y`, and the sidecar re-reads the control's
  # bounds and clicks its centre. Both addressing forms on one request is
  # `addressing_conflict`. `element_ref` was a reserved field the sidecar refused
  # outright until now, so the version bumps and the handshake refuses the
  # pairing.
  @protocol_version 9

  @actions ~w(screenshot left_click right_click double_click mouse_move left_click_drag scroll type key wait inspect wait_for_change paste elements windows press set_value)

  # Read-only in both senses the wire needs: a consumer may auto-run one without a
  # confirmation step, and it dispatches no input, so it carries no `mutation_seq`
  # and earns no receipt. The operational verbs sit here too — `probe`, `idle_ms`,
  # `wait_for_idle` and `hello` are not model actions (they are absent from
  # `@actions`), but they change nothing on the screen and classifying them as
  # mutations would put a sequence number and a receipt on a permission probe.
  @read_only ~w(screenshot mouse_move wait inspect wait_for_change elements windows
                probe idle_ms wait_for_idle hello)
  # v8: the actions addressed INTO an observation — their target is read out of a
  # reply the caller was handed. Each one names that observation with
  # `observation_id` and takes no `region`: the rectangle is the sidecar's to
  # remember, and a caller that echoes one back is the defect this replaces.
  @addressed ~w(left_click right_click double_click mouse_move left_click_drag scroll inspect
                press set_value)

  # v9: addressed by a CONTROL, never by a point. Their whole promise is that they
  # cannot miss, so a coordinate on one of them is a contradiction, not a hint.
  @element ~w(press set_value)

  # v9: addressed by a point OR by a control, and the sidecar re-reads the
  # control's bounds at the moment it acts. Both on one request is a conflict:
  # nothing may guess which one the caller meant.
  @pointer_or_element ~w(left_click right_click double_click mouse_move scroll)

  # The actions that PRODUCE coordinates. `region` stays theirs; an `observation_id`
  # beside it says which image the rectangle is read in.
  @viewing ~w(screenshot elements wait_for_change)

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
      action when action in @actions ->
        with :ok <- check_addressing(action, params),
             {:ok, request} <- validate_action(action, params) do
          {:ok, put_observation(request, Map.get(params, "observation_id"))}
        end

      nil ->
        {:error, "missing required field: action"}

      other ->
        {:error, "unknown action: #{inspect(other)}"}
    end
  end

  def validate(_other), do: {:error, "action params must be a map"}

  # v8/v9: which observation this action is addressed into, and — since v9 —
  # whether it names a point or a control. Checked before the action's own
  # arguments, because an action aimed at the wrong thing cannot be fixed by
  # having valid ones.
  defp check_addressing(action, params) when action in @addressed do
    cond do
      not observation?(params) ->
        {:error,
         "#{action} requires observation_id: the id of the reply its target was read from"}

      Map.has_key?(params, "region") ->
        {:error,
         "region is not accepted on #{action}: its coordinates are pixels in the image named " <>
           "by observation_id, which carries its own rectangle"}

      true ->
        check_target(action, params)
    end
  end

  defp check_addressing(action, params) when action in @viewing do
    cond do
      Map.has_key?(params, "observation_id") and not observation?(params) ->
        {:error, "observation_id must be a non-empty string"}

      Map.has_key?(params, "element_ref") ->
        {:error, "#{action} takes no element_ref — it produces references, it does not use one"}

      true ->
        :ok
    end
  end

  defp check_addressing(action, params) do
    cond do
      Map.has_key?(params, "observation_id") ->
        {:error, "#{action} takes no observation_id — it reads no coordinates"}

      Map.has_key?(params, "element_ref") ->
        {:error, "#{action} takes no element_ref — it addresses no control"}

      true ->
        :ok
    end
  end

  # `press` and `set_value` name a control and nothing else: a point beside the
  # reference means the caller addressed the action two ways at once.
  defp check_target(action, params) when action in @element do
    cond do
      not element?(params) ->
        {:error,
         "#{action} requires element_ref: the reference of the control, from the elements " <>
           "reply named by observation_id"}

      point?(params) ->
        {:error, conflict(action)}

      true ->
        :ok
    end
  end

  defp check_target(action, params) when action in @pointer_or_element do
    if element?(params) and point?(params), do: {:error, conflict(action)}, else: :ok
  end

  # `left_click_drag` names two points and `inspect` reports what is under one, so
  # neither has a meaning for a control reference.
  defp check_target(action, params) do
    if Map.has_key?(params, "element_ref"),
      do: {:error, "#{action} takes no element_ref — it addresses a point"},
      else: :ok
  end

  defp conflict(action) do
    "#{action} is addressed either by a point or by element_ref, never by both: " <>
      "send the coordinates, or the reference, not the two together"
  end

  defp observation?(params), do: nonempty_string?(Map.get(params, "observation_id"))
  defp element?(params), do: nonempty_string?(Map.get(params, "element_ref"))

  defp nonempty_string?(value), do: is_binary(value) and value != ""

  defp point?(params), do: Enum.any?(~w(x y from to), &Map.has_key?(params, &1))

  defp put_observation(request, nil), do: request
  defp put_observation(request, id), do: Map.put(request, "observation_id", id)

  # This module validates and classifies; it no longer writes a line. `encode_request/1`
  # produced the UNTAGGED protocol-6 shape, which a protocol-7 sidecar refuses —
  # one wire format means one encoder, and that is `Compux.Frame.encode/1`, for
  # the action wire and the computer-history connection alike.

  defp validate_action("screenshot", params) do
    with {:ok, display} <- opt_display(params),
         {:ok, region} <- opt_region(params),
         {:ok, quality} <- opt_jpeg_quality(params),
         {:ok, rulers} <- opt_bool(params, "rulers"),
         {:ok, marks} <- opt_bool(params, "marks"),
         {:ok, annotate} <- opt_annotate_point(params) do
      request = put_display(%{"action" => "screenshot"}, display)

      {:ok,
       request
       |> put_region(region)
       |> maybe_put("jpeg_quality", quality)
       |> maybe_put("rulers", rulers)
       |> maybe_put("marks", marks)
       |> maybe_put("annotate_point", annotate)}
    end
  end

  defp validate_action(action, params)
       when action in ~w(left_click right_click double_click mouse_move) do
    with {:ok, target} <- pointer_target(params),
         {:ok, modifiers} <- opt_modifiers(params),
         {:ok, display} <- opt_display(params) do
      request = Map.merge(%{"action" => action}, target)
      request = if modifiers == [], do: request, else: Map.put(request, "modifiers", modifiers)
      {:ok, put_display(request, display)}
    end
  end

  # v9: press the control the caller named. Offered only where the control's own
  # action list says it can be pressed — the sidecar refuses it everywhere else
  # and never falls back to clicking, which is the caller's decision to make.
  defp validate_action("press", params) do
    with {:ok, element} <- element_ref(params),
         {:ok, display} <- opt_display(params) do
      {:ok, put_display(%{"action" => "press", "element_ref" => element}, display)}
    end
  end

  # v9: set the control's value directly, then read it back. Offered only where
  # the control reports its value as settable.
  defp validate_action("set_value", params) do
    with {:ok, element} <- element_ref(params),
         {:ok, value} <- value_text(params),
         {:ok, display} <- opt_display(params) do
      request = %{"action" => "set_value", "element_ref" => element, "value" => value}
      {:ok, put_display(request, display)}
    end
  end

  defp validate_action("inspect", params) do
    with {:ok, x} <- coord(params, "x"),
         {:ok, y} <- coord(params, "y"),
         {:ok, display} <- opt_display(params) do
      {:ok, put_display(%{"action" => "inspect", "x" => x, "y" => y}, display)}
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

  # List the on-screen windows of a display, each with its bounds already expressed
  # as a `region` in that display's screenshot space — so a caller can crop to the
  # window it cares about instead of reading it out of a downscaled full screen.
  # Read-only, and takes no region itself (it is what PRODUCES regions).
  defp validate_action("windows", params) do
    with {:ok, display} <- opt_display(params) do
      {:ok, put_display(%{"action" => "windows"}, display)}
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
         {:ok, display} <- opt_display(params) do
      {:ok, put_display(%{"action" => "left_click_drag", "from" => from, "to" => to}, display)}
    end
  end

  defp validate_action("scroll", params) do
    with {:ok, target} <- pointer_target(params),
         {:ok, direction} <- scroll_direction(params),
         {:ok, amount} <- positive(params, "amount"),
         {:ok, display} <- opt_display(params) do
      request =
        Map.merge(%{"action" => "scroll", "direction" => direction, "amount" => amount}, target)

      {:ok, put_display(request, display)}
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

  # v9: a pointer action names its target one way or the other. `check_addressing`
  # has already refused both at once, so a reference here means the caller sent no
  # coordinates and a control is what it aimed at.
  defp pointer_target(params) do
    if element?(params) do
      with {:ok, element} <- element_ref(params), do: {:ok, %{"element_ref" => element}}
    else
      with {:ok, x} <- coord(params, "x"),
           {:ok, y} <- coord(params, "y"),
           do: {:ok, %{"x" => x, "y" => y}}
    end
  end

  defp element_ref(params) do
    case Map.get(params, "element_ref") do
      ref when is_binary(ref) and ref != "" ->
        {:ok, ref}

      _other ->
        {:error,
         "element_ref must be a non-empty string, as an elements reply spells it (e.g. \"e3\")"}
    end
  end

  # The same bound `type` and `paste` carry: a value the sidecar sets in one AX
  # call, not a stream.
  defp value_text(params) do
    case Map.get(params, "value") do
      value when is_binary(value) and byte_size(value) <= @max_type_bytes ->
        {:ok, value}

      value when is_binary(value) ->
        {:error, "set_value.value must be at most #{@max_type_bytes} bytes"}

      _other ->
        {:error, "set_value requires a string value"}
    end
  end

  defp coord(params, key) do
    case Map.get(params, key) do
      value when is_integer(value) and value >= 0 -> {:ok, value}
      _other -> {:error, "#{key} must be a non-negative integer (pixels in the named image)"}
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

  # Optional boolean flags (v5: `rulers`/`marks`). Only a literal `true` is
  # carried; false and absent are equivalent, so the wire stays minimal.
  defp opt_bool(params, key) do
    case Map.get(params, key) do
      nil -> {:ok, nil}
      true -> {:ok, true}
      false -> {:ok, nil}
      _other -> {:error, "#{key} must be a boolean"}
    end
  end

  # v5: mark an executed click in a check image — a point in THIS capture's own
  # sent pixel space.
  defp opt_annotate_point(params) do
    case Map.get(params, "annotate_point") do
      nil ->
        {:ok, nil}

      %{"x" => x, "y" => y} when is_integer(x) and is_integer(y) and x >= 0 and y >= 0 ->
        {:ok, %{"x" => x, "y" => y}}

      _other ->
        {:error, "annotate_point must be an object with non-negative integer x and y"}
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

  # A zoom rectangle, read in the image named by `observation_id` or — with none —
  # in the full-display image. It is accepted only by the actions that PRODUCE an
  # image or a list of coordinates; an action that addresses a point names its image
  # instead, so no rectangle is ever copied from one request into the next.
  # Opt into JPEG for this capture. Absent = PNG, the lossless default for reading
  # fine UI text; a BULK periodic caller (a continuous screen feed) sets it, because
  # a full-desktop PNG is an order of magnitude larger and saturates the uplink at
  # any real cadence. It changes only the encoding — never the sent dimensions, on
  # which every coordinate depends.
  defp opt_jpeg_quality(params) do
    case Map.get(params, "jpeg_quality") do
      nil -> {:ok, nil}
      q when is_integer(q) and q >= 1 and q <= 100 -> {:ok, q}
      _other -> {:error, "jpeg_quality must be an integer 1-100"}
    end
  end

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
       "(pixels in the image named by observation_id, or the full-display image with none)"}
  end

  defp put_region(request, nil), do: request
  defp put_region(request, region), do: Map.put(request, "region", region)
end
