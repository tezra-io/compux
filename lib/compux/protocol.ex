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
  #
  # v10 (M42 slice 6, the check): an action that dispatches input says what
  # evidence it wants back, and `check` REPLACES `screenshot_after` — deleted, not
  # kept beside it, so a request carrying the old field is refused rather than run
  # with no check at all. `image` returns the view the caller acted in (the crop of
  # the observation the action named, or the full display when that observation was
  # the full display or the action names none), settled before it is taken;
  # `semantic` re-reads the control an `element_ref` named and answers
  # `element_after`, with no capture; `none` is the receipt alone. The receipt says
  # which evidence it carries in `check` and what each phase cost in `timings_ms`.
  #
  # v11 (M42 slice 5, bound targets): the caller may bind ONE window and work
  # inside it. `select_target` takes a `window_id` from a `windows` listing and
  # answers a `target_id` (`t1`, `t2`, …) with a first observation of that window
  # alone; `release_target` ends it. An action that carries `target_id` is answered
  # from the window's own frames and in the window's own coordinates, even when
  # another window covers it, and the helper shows on screen that it is doing so.
  # `target_id` was a reserved field the sidecar refused outright until now, so the
  # version bumps and the handshake refuses the pairing. An action WITHOUT
  # `target_id` behaves exactly as it did at protocol 10 — the display-level path is
  # unchanged, and it is also how the full desktop stays reachable: there is no
  # "desktop" target, there is the absence of one.
  #
  # v12 (M86 P2, the model's surface; the whole contract is PROTOCOL.md): one
  # surface for macOS and Linux. `elements` takes a `scope`, a `query` (a find), a
  # `root` to drill into and a `skeleton` for a dense application, and names every
  # control by a normalised `role` with its `native_role` beside it and the `verbs`
  # it has a route for. Six verbs act on a reference — `focus`, `select`, `toggle`,
  # `expand`, `collapse`, `scroll_to` — and `type` may name a control too, typing
  # into it with no keyboard. `wait_for` waits on the tree instead of on pixels, and
  # `check` gains an object form, `{"kind": "semantic", "until": …}`, whose baseline
  # is taken before the input so an action and its wait are one request. Receipts
  # gain `surfaces`, and every refusal either platform can answer is named now, so
  # Linux adds a backend and no second break. A `role` that changed its vocabulary
  # is not additive, so the version bumps and the handshake refuses the pairing.
  @protocol_version 12

  @actions ~w(screenshot left_click right_click double_click mouse_move left_click_drag scroll type key wait inspect wait_for_change paste elements windows press set_value focus select toggle expand collapse scroll_to wait_for select_target release_target)

  # Read-only in both senses the wire needs: a consumer may auto-run one without a
  # confirmation step, and it dispatches no input, so it carries no `mutation_seq`
  # and earns no receipt. The operational verbs sit here too — `probe`, `idle_ms`,
  # `wait_for_idle` and `hello` are not model actions (they are absent from
  # `@actions`), but they change nothing on the screen and classifying them as
  # mutations would put a sequence number and a receipt on a permission probe.
  #
  # v11: `select_target` and `release_target` are here too. They change what the
  # helper is bound to and they dispatch nothing — no pointer moves, no key goes
  # down, nothing on the screen is touched — so putting a `mutation_seq` and a
  # receipt on them would claim input where there was none. A pause therefore does
  # not refuse them either, which is right: the person took the keyboard back, not
  # the bookkeeping.
  #
  # v12: `wait_for` is here too. It waits on the tree and sends nothing.
  @read_only ~w(screenshot mouse_move wait inspect wait_for_change elements windows
                select_target release_target wait_for
                probe idle_ms wait_for_idle hello)

  # v11: the actions that may name a bound window. Everything that looks at, or
  # acts inside, one window — and nothing else. `windows` enumerates the DESKTOP's
  # windows, which is how a target is found in the first place; `wait`,
  # `wait_for_change` and the two target verbs themselves have no target to name.
  @targetable ~w(screenshot elements inspect left_click right_click double_click
                 mouse_move left_click_drag scroll type key paste press set_value
                 focus select toggle expand collapse scroll_to wait_for)
  # v8: the actions addressed INTO an observation — their target is read out of a
  # reply the caller was handed. Each one names that observation with
  # `observation_id` and takes no `region`: the rectangle is the sidecar's to
  # remember, and a caller that echoes one back is the defect this replaces.
  @addressed ~w(left_click right_click double_click mouse_move left_click_drag scroll inspect
                press set_value focus select toggle expand collapse scroll_to)

  # v9: addressed by a CONTROL, never by a point. Their whole promise is that they
  # cannot miss, so a coordinate on one of them is a contradiction, not a hint.
  @element ~w(press set_value focus select toggle expand collapse scroll_to)

  # v12: the verbs on a reference that take no argument of their own.
  @plain_verbs ~w(focus select expand collapse scroll_to)

  # v9: addressed by a point OR by a control, and the sidecar re-reads the
  # control's bounds at the moment it acts. Both on one request is a conflict:
  # nothing may guess which one the caller meant.
  @pointer_or_element ~w(left_click right_click double_click mouse_move scroll)

  # The actions that PRODUCE coordinates. `region` stays theirs; an `observation_id`
  # beside it says which image the rectangle is read in. v12: `wait_for` lists what
  # it waited for, with references, as `elements` does.
  @viewing ~w(screenshot elements wait_for_change wait_for)

  # v10: the evidence an action brings back. Only an action that DISPATCHES INPUT
  # has anything to bring back, so the field is offered exactly there — `mouse_move`
  # is read-only on the wire and returns no check, and a read-only verb asking for
  # one has misunderstood what it is.
  @check_kinds ~w(image semantic none)

  @modifiers ~w(cmd ctrl alt shift)
  @scroll_directions ~w(up down left right)
  @max_type_bytes 10_000
  @max_wait_ms 10_000
  # `wait_for_change` must finish inside the caller's per-action deadline (30s in
  # Fermix), so its poll budget is capped well under that.
  @max_wait_for_change_ms 25_000
  @min_poll_ms 50
  @max_poll_ms 5_000

  # v12: where an `elements` walk, or a condition, starts (PROTOCOL.md §2.1).
  @scopes ~w(window app focused menu dialog)

  # v12: the normalised roles, the same on every platform (PROTOCOL.md §6.1). A
  # query names one of these, never a platform's own spelling.
  @roles ~w(button menubutton checkbox switch radio tab textfield textarea combobox menuitem
            link slider stepper disclosure row cell column list table tree scrollarea
            scrollbar splitter toolbar tablist group menu menubar window dialog alert popup
            text image heading progress datefield document application unknown)

  # v12: what a receipt's `surfaces` may name (PROTOCOL.md §6.2).
  @surface_kinds ~w(dialog alert menu popup)

  # v12: every `error` code the sidecar answers (PROTOCOL.md §7), the ones only the
  # Linux build answers at first included, so a caller can hold a sentence for each.
  @refusals ~w(malformed_frame untagged_frame unknown_frame_type request_too_large
               unknown_field unknown_action observation_required element_required
               addressing_conflict check_unsupported invalid_argument
               stale_generation stale_mutation paused cancelled busy
               unknown_observation expired_observation stale_observation
               point_outside_observation no_active_display display_asleep
               display_disconnected capture_stalled capture_geometry_mismatch
               capture_unavailable capture_budget_exceeded screen_recording_not_granted
               target_unavailable target_minimized target_obstructed
               control_surface_unavailable ax_binding_unavailable target_binding_unavailable
               stale_element element_disabled ax_action_unsupported state_unreadable
               focus_refused ax_action_failed ax_timed_out scope_not_found
               accessibility_unavailable ambiguous_element identity_unknown
               toolkit_accessibility_off text_outside_layout consent_refused consent_timeout)

  # v12: what each kind of `until` condition reads, beside `kind` and `timeout_ms`
  # (PROTOCOL.md §5.1). A field a kind does not read is refused, never dropped.
  @until_query ~w(role name scope)
  @until_text ~w(contains scope)
  @max_until_ms 25_000

  @doc "The wire-compatibility version this build speaks (see the moduledoc)."
  @spec protocol_version() :: pos_integer()
  def protocol_version, do: @protocol_version

  @spec actions() :: [String.t()]
  def actions, do: @actions

  @doc "Where an `elements` walk or an `until` condition may start (v12)."
  @spec scopes() :: [String.t()]
  def scopes, do: @scopes

  @doc "The normalised roles a control is named by, on every platform (v12)."
  @spec roles() :: [String.t()]
  def roles, do: @roles

  @doc "The kinds a receipt's `surfaces` name (v12)."
  @spec surface_kinds() :: [String.t()]
  def surface_kinds, do: @surface_kinds

  @doc """
  Every `error` code protocol 12 answers, including the ones only the Linux build
  answers at first, so a caller can hold a sentence for each before it meets one.
  """
  @spec refusals() :: [String.t()]
  def refusals, do: @refusals

  @doc """
  Read-only actions never mutate the screen, so they carry no post-action
  screenshot and a consumer may auto-run them without a confirmation step.
  """
  @spec read_only?(String.t()) :: boolean()
  def read_only?(action) when is_binary(action), do: action in @read_only

  @doc """
  Validate + canonicalize an action params map (string keys) into a sidecar
  request. Returns `{:ok, request}` or `{:error, reason}`. The caller fills the
  default `display`; this function validates the action's own arguments, the
  observation and control it is addressed at, and the `check` it asks for.
  """
  @spec validate(map()) :: {:ok, map()} | {:error, String.t()}
  def validate(params) when is_map(params) do
    case Map.get(params, "action") do
      action when action in @actions ->
        with :ok <- check_addressing(action, params),
             :ok <- check_binding(action, params),
             :ok <- check_evidence(action, params),
             {:ok, request} <- validate_action(action, params) do
          request =
            request
            |> put_observation(Map.get(params, "observation_id"))
            |> put_target_id(Map.get(params, "target_id"))

          {:ok, put_check(request, Map.get(params, "check"))}
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

  # v12: `type` types at the focus, or into the control a listing named. The
  # listing is named only beside the control: typing at the focus reads no image.
  defp check_addressing("type", params) do
    cond do
      Map.has_key?(params, "element_ref") and not observation?(params) ->
        {:error,
         "type with element_ref requires observation_id: the id of the elements reply " <>
           "that listed the control"}

      Map.has_key?(params, "observation_id") and not Map.has_key?(params, "element_ref") ->
        {:error,
         "type takes observation_id only beside element_ref — typing at the focus reads " <>
           "no coordinates"}

      Map.has_key?(params, "element_ref") and point?(params) ->
        {:error, conflict("type")}

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

  # v10: the evidence this action asks for. Checked here rather than inside each
  # action, because the rule is about what the action IS — whether it dispatches
  # input, and whether it named a control — and not about its own arguments.
  #
  # `screenshot_after` is refused by name. It was replaced by `check`, and this
  # function builds a canonical request from a whitelist, so dropping it in silence
  # would leave a caller that still sends it with no check at all and nothing said.
  defp check_evidence(action, params) do
    cond do
      Map.has_key?(params, "screenshot_after") ->
        {:error,
         ~s(screenshot_after was replaced by check: send check with "image", "semantic" ) <>
           ~s(or "none")}

      not Map.has_key?(params, "check") ->
        :ok

      read_only?(action) ->
        {:error, "#{action} takes no check — it dispatches no input, so it reports no evidence"}

      true ->
        check_kind(Map.get(params, "check"), params)
    end
  end

  # v12: the object form is a semantic check that also waits on the tree. It is
  # offered on any action that dispatches input, because what it reads is the
  # condition and not only the control the action named.
  defp check_kind(%{} = check, params) do
    with :ok <- only_fields(check, ~w(kind until), "check"),
         :ok <- semantic_kind(check) do
      validate_until(Map.get(check, "until"), params)
    end
  end

  defp check_kind(kind, _params) when kind not in @check_kinds,
    do: {:error, "check must be one of #{Enum.join(@check_kinds, ", ")}, or a semantic object"}

  defp check_kind("semantic", params) do
    if element?(params),
      do: :ok,
      else:
        {:error,
         "a semantic check re-reads the control the action named, so it needs an element_ref: " <>
           "address this action by reference, ask for an image check, or wait on the tree " <>
           ~s(with {"kind": "semantic", "until": …})}
  end

  defp check_kind(_kind, _params), do: :ok

  defp semantic_kind(%{"kind" => "semantic", "until" => _until}), do: :ok

  defp semantic_kind(_check),
    do:
      {:error,
       ~s(a check object is {"kind": "semantic", "until": {…}}: only a semantic check waits, ) <>
         "and it needs the condition it waits for"}

  # v11: the window this action is bound to, when it names one. A target changes
  # where an action LOOKS and where its coordinates mean something, so an action
  # that has no such notion is refused rather than run against the display while
  # its caller believes it named a window.
  defp check_binding(action, params) do
    cond do
      not Map.has_key?(params, "target_id") ->
        :ok

      action not in @targetable ->
        {:error, "#{action} takes no target_id — it does not act inside one window"}

      not nonempty_string?(Map.get(params, "target_id")) ->
        {:error, ~s|target_id must be a non-empty string, as select_target spells it (e.g. "t1")|}

      true ->
        :ok
    end
  end

  defp observation?(params), do: nonempty_string?(Map.get(params, "observation_id"))
  defp element?(params), do: nonempty_string?(Map.get(params, "element_ref"))

  defp nonempty_string?(value), do: is_binary(value) and value != ""

  defp point?(params), do: Enum.any?(~w(x y from to), &Map.has_key?(params, &1))

  defp put_observation(request, nil), do: request
  defp put_observation(request, id), do: Map.put(request, "observation_id", id)

  defp put_target_id(request, nil), do: request
  defp put_target_id(request, id), do: Map.put(request, "target_id", id)

  defp put_check(request, nil), do: request
  defp put_check(request, kind), do: Map.put(request, "check", kind)

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

  # v12: the verbs on a reference that take nothing but the control. Each is offered
  # only where the control has a route for it, and refused rather than replaced by a
  # click everywhere else.
  defp validate_action(action, params) when action in @plain_verbs do
    with {:ok, element} <- element_ref(params),
         {:ok, display} <- opt_display(params) do
      {:ok, put_display(%{"action" => action, "element_ref" => element}, display)}
    end
  end

  # v12: `on` is the state the control should END in, so a toggle that finds it there
  # already sends nothing and says so.
  defp validate_action("toggle", params) do
    with {:ok, element} <- element_ref(params),
         {:ok, display} <- opt_display(params) do
      case Map.get(params, "on") do
        on when is_boolean(on) ->
          request = %{"action" => "toggle", "element_ref" => element, "on" => on}
          {:ok, put_display(request, display)}

        _other ->
          {:error, "toggle requires on: true or false, the state the control should end in"}
      end
    end
  end

  # v12: wait on the tree until a condition holds, or the condition's own timeout.
  # Read-only: it sends nothing and it never sends anything again.
  defp validate_action("wait_for", params) do
    with {:ok, until} <- required_until(params),
         {:ok, display} <- opt_display(params),
         {:ok, region} <- opt_region(params) do
      request = %{"action" => "wait_for", "until" => until}
      {:ok, request |> put_display(display) |> put_region(region)}
    end
  end

  # v11: bind one window, by the `id` a `windows` listing gave it. The helper
  # answers a `target_id` and a first observation OF THAT WINDOW, so the caller's
  # next coordinates are pixels of the window rather than of the display behind it.
  #
  # There is no "desktop" window id: the desktop is the ABSENCE of a target, which
  # is the display-level path this protocol has always had. A second spelling for
  # it would be a second code path for one behaviour.
  defp validate_action("select_target", params) do
    case Map.get(params, "window_id") do
      id when is_integer(id) and id >= 0 ->
        {:ok, %{"action" => "select_target", "window_id" => id}}

      _other ->
        {:error,
         "select_target requires window_id: the non-negative integer id a windows " <>
           "listing gave the window"}
    end
  end

  # One target per helper, so this needs no id: it releases whatever is bound.
  defp validate_action("release_target", _params) do
    {:ok, %{"action" => "release_target"}}
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
  # v12: where the walk starts (`scope`), a find (`query`), a drill (`root`) and a
  # walk cut at depth 3 for a dense application (`skeleton`).
  defp validate_action("elements", params) do
    with {:ok, display} <- opt_display(params),
         {:ok, region} <- opt_region(params),
         {:ok, listing} <- listing(params) do
      request = put_region(put_display(%{"action" => "elements"}, display), region)
      {:ok, Map.merge(request, listing)}
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

  # v12: with an element_ref the text replaces that control's selection, or goes in
  # at its caret, through the accessibility API — `check_addressing` has already
  # made the reference come with the listing that names it.
  defp validate_action("type", params) do
    with {:ok, text} <- type_text(params),
         {:ok, element} <- opt_element_ref(params) do
      {:ok, maybe_put(%{"action" => "type", "text" => text}, "element_ref", element)}
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

  defp opt_element_ref(params) do
    if Map.has_key?(params, "element_ref"), do: element_ref(params), else: {:ok, nil}
  end

  defp type_text(params) do
    case Map.get(params, "text") do
      text when is_binary(text) and byte_size(text) > 0 and byte_size(text) <= @max_type_bytes ->
        {:ok, text}

      text when is_binary(text) ->
        {:error, "type.text must be 1..#{@max_type_bytes} bytes"}

      _other ->
        {:error, "type requires a non-empty string text"}
    end
  end

  # --- v12: what an `elements` walk reads -------------------------------------

  defp listing(params) do
    with {:ok, scope} <- opt_member(params, "scope", @scopes, "scope"),
         {:ok, query} <- opt_query(params),
         {:ok, skeleton} <- opt_bool(params, "skeleton"),
         {:ok, root} <- opt_root(params),
         :ok <- listing_combination(scope, query, skeleton, root, params) do
      {:ok,
       %{}
       |> maybe_put("scope", scope)
       |> maybe_put("query", query)
       |> maybe_put("skeleton", skeleton)
       |> maybe_put("root", root)}
    end
  end

  # The combinations the walk cannot honour, refused by name: a skeleton is cut at
  # depth 3 and a find has to see the whole tree; a root is its own scope; a root
  # is a reference, and a reference means nothing without the listing that minted it.
  defp listing_combination(scope, query, skeleton, root, params) do
    cond do
      skeleton && query ->
        {:error,
         "a skeleton cuts the walk at depth 3 and a query has to see the whole tree: send " <>
           "skeleton or query, not both"}

      root && scope ->
        {:error, "root names its own subtree, so scope is not accepted beside it"}

      root && not observation?(params) ->
        {:error,
         "root requires observation_id: the id of the elements reply that listed that control"}

      true ->
        :ok
    end
  end

  defp opt_root(params) do
    case Map.fetch(params, "root") do
      :error ->
        {:ok, nil}

      {:ok, root} when is_binary(root) and root != "" ->
        {:ok, root}

      {:ok, _other} ->
        {:error,
         ~s|root must be a non-empty string naming a control of the listing observation_id | <>
           ~s|names (e.g. "e12")|}
    end
  end

  defp opt_query(params) do
    case Map.fetch(params, "query") do
      :error -> {:ok, nil}
      {:ok, query} -> validate_query(query, "query")
    end
  end

  # A find by normalised role, by name, or both. Everything about it is checked,
  # because a query that silently lost a field matches more than it was asked to.
  defp validate_query(%{} = query, field) do
    with :ok <- only_fields(query, ~w(role name), field),
         :ok <- query_named(query, field),
         {:ok, _role} <- opt_member(query, "role", @roles, "#{field}.role"),
         :ok <- opt_text(query, "name", "#{field}.name") do
      {:ok, query}
    end
  end

  defp validate_query(_other, field),
    do: {:error, "#{field} must be an object with role and/or name"}

  defp query_named(query, field) do
    if Map.has_key?(query, "role") or Map.has_key?(query, "name"),
      do: :ok,
      else: {:error, "#{field} needs role, name, or both: an empty #{field} matches everything"}
  end

  # --- v12: the condition a wait reads ----------------------------------------

  defp required_until(params) do
    case Map.fetch(params, "until") do
      :error ->
        {:error, "wait_for requires until: the condition to wait for"}

      {:ok, until} ->
        with :ok <- validate_until(until, params), do: {:ok, until}
    end
  end

  defp validate_until(%{"kind" => kind} = until, params) when is_binary(kind) do
    with {:ok, fields} <- until_fields(kind, until),
         :ok <- only_fields(until, ["kind", "timeout_ms" | fields], "until"),
         :ok <- until_subject(kind, until, params),
         {:ok, _scope} <- opt_member(until, "scope", @scopes, "until.scope"),
         {:ok, _ms} <- opt_bounded(until, "timeout_ms", 1, @max_until_ms) do
      :ok
    end
  end

  defp validate_until(_other, _params),
    do: {:error, "until must be an object with a kind, e.g. %{\"kind\" => \"appears\", ...}"}

  # The fields each kind reads, beside `kind` and `timeout_ms`.
  defp until_fields("appears", _until), do: {:ok, @until_query}

  defp until_fields("disappears", until) do
    if Map.has_key?(until, "element_ref"), do: {:ok, ~w(element_ref)}, else: {:ok, @until_query}
  end

  defp until_fields("enabled", _until), do: {:ok, ~w(element_ref)}
  defp until_fields("value", _until), do: {:ok, ~w(element_ref value)}
  defp until_fields("window_title", _until), do: {:ok, @until_text}
  defp until_fields("text", _until), do: {:ok, @until_text}
  defp until_fields(kind, _until) when kind in ~w(menu_open menu_closed), do: {:ok, []}

  defp until_fields(kind, _until),
    do:
      {:error,
       "until.kind must be one of appears, disappears, enabled, value, window_title, " <>
         "menu_open, menu_closed, text; #{inspect(kind)} is not one"}

  # What the condition is ABOUT: a control of the request's own listing, a find, or
  # a piece of text.
  defp until_subject("disappears", until, params) do
    if Map.has_key?(until, "element_ref"),
      do: until_reference("disappears", until, params),
      else: until_query(until)
  end

  defp until_subject(kind, until, params) when kind in ~w(enabled value),
    do: until_reference(kind, until, params)

  defp until_subject("appears", until, _params), do: until_query(until)

  defp until_subject(kind, until, _params) when kind in ~w(window_title text),
    do: opt_text_required(until, "contains", "until.contains")

  defp until_subject(_menu, _until, _params), do: :ok

  defp until_reference(kind, until, params) do
    cond do
      not nonempty_string?(Map.get(until, "element_ref")) ->
        {:error, "until.element_ref must be a non-empty string, as an elements reply spells it"}

      not observation?(params) ->
        {:error,
         "until.element_ref names a control of the listing this request's observation_id " <>
           "names, and this request names none"}

      kind == "value" and not is_binary(Map.get(until, "value")) ->
        {:error, "until of kind value needs value: the string the control should hold"}

      true ->
        :ok
    end
  end

  defp until_query(until) do
    with {:ok, _query} <- validate_query(Map.take(until, ~w(role name)), "until"), do: :ok
  end

  # --- shared shape checks -----------------------------------------------------

  # A nested object the library canonicalises: every key it does not read is
  # refused, never dropped.
  defp only_fields(map, allowed, field) do
    case Map.keys(map) -- allowed do
      [] ->
        :ok

      unknown ->
        {:error,
         "#{field} takes only #{Enum.join(allowed, ", ")}; #{inspect(unknown)} is not one of them"}
    end
  end

  defp opt_member(map, key, members, field) do
    case Map.fetch(map, key) do
      :error ->
        {:ok, nil}

      {:ok, value} ->
        if value in members,
          do: {:ok, value},
          else: {:error, "#{field} must be one of #{Enum.join(members, ", ")}"}
    end
  end

  defp opt_text(map, key, field) do
    case Map.fetch(map, key) do
      :error -> :ok
      {:ok, _value} -> opt_text_required(map, key, field)
    end
  end

  defp opt_text_required(map, key, field) do
    case Map.get(map, key) do
      text when is_binary(text) and text != "" -> :ok
      _other -> {:error, "#{field} must be a non-empty string"}
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
