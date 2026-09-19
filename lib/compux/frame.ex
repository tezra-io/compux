defmodule Compux.Frame do
  @moduledoc """
  The tagged frames of the compux wire, as typed values.

  Every line on the pipe is one JSON object carrying a `type`. Decoding returns
  the frame's family, not just "did it say ok" — an unsuccessful action can still
  carry a receipt proving input may have reached the screen, and a computer-history
  `event` is not a response to anything even though it also says `ok`. The reader
  it replaces treated any map with `ok: true` as a successful action response, so
  an `ack`, an `event` or a stray frame could stand in for a reply the caller was
  waiting on.

  Families:

    * `request` and `control` — outbound; this module encodes them.
    * `response`, `control_ack`, `session_event` — inbound; this module decodes them.
    * `ack` and `event` — the computer-history push families, decoded as opaque
      history frames so a consumer of the action wire can REFUSE them by family
      rather than by guessing at their fields.

  Anything else is an unknown tag and fails loud. This module is pure: it neither
  reads nor writes a Port.

  ## Two envelope fields worth knowing

    * `deadline_ms` carries the caller's remaining budget. It is NOT `timeout_ms`,
      because `wait_for_change` and `wait_for_idle` have carried a `timeout_ms`
      ARGUMENT since protocol 2 and it means something else entirely (how long the
      action itself may poll). One key cannot mean both.
    * `protocol_version` rides on `hello` alone, which is the one request sent
      before any generation exists.

  ## A connection that never handshakes

  The computer-history consumer owns a raw Port (`Compux.Port.open/1`), reads its
  own lines, and never sends `hello` — so it has no generations to quote. It
  still speaks the one wire format, by encoding an ordinary `Request` and leaving
  every generation `nil`. `encode/1` omits an absent envelope field rather than
  demanding it, so this needs no second request shape and no second encoder:

      {:ok, line} =
        Compux.Frame.encode(%Compux.Frame.Request{
          request_id: "o1",
          args: %{"action" => "observe_start", "params" => %{"apps" => ["com.apple.Safari"]}}
        })
      #=> {"type":"request","request_id":"o1","action":"observe_start","params":{…}}\\n

  Its request ids are its own and nothing correlates them against the action
  wire. The sidecar answers with the UNCHANGED `ack` family and streams the
  UNCHANGED `event` family, both of which `decode/1` returns as `History` with
  the whole frame — `type` included — kept verbatim in `payload`, so that
  consumer can use this decoder without its envelope being rewritten underneath
  it.
  """

  defmodule Request do
    @moduledoc false
    @enforce_keys [:request_id, :args]
    defstruct [
      :request_id,
      :args,
      :deadline_ms,
      :protocol_version,
      :sidecar_generation,
      :session_generation,
      :authorization_generation,
      :mutation_seq
    ]

    @type t :: %__MODULE__{
            request_id: String.t(),
            args: map(),
            deadline_ms: non_neg_integer() | nil,
            protocol_version: pos_integer() | nil,
            sidecar_generation: String.t() | nil,
            session_generation: pos_integer() | nil,
            authorization_generation: non_neg_integer() | nil,
            mutation_seq: pos_integer() | nil
          }
  end

  defmodule Response do
    @moduledoc false
    @enforce_keys [:request_id, :ok, :payload]
    defstruct [
      :request_id,
      :ok,
      :payload,
      :error,
      :detail,
      :receipt,
      :sidecar_generation,
      :session_generation
    ]

    @type t :: %__MODULE__{
            request_id: String.t(),
            ok: boolean(),
            payload: map(),
            error: String.t() | nil,
            detail: String.t() | nil,
            receipt: map() | nil,
            sidecar_generation: String.t() | nil,
            session_generation: pos_integer() | nil
          }
  end

  defmodule Control do
    @moduledoc false
    @enforce_keys [:request_id, :action]
    defstruct [
      :request_id,
      :action,
      :sidecar_generation,
      :session_generation,
      :authorization_generation
    ]

    @type t :: %__MODULE__{
            request_id: String.t(),
            action: :pause | :resume | :release,
            sidecar_generation: String.t() | nil,
            session_generation: pos_integer() | nil,
            authorization_generation: non_neg_integer() | nil
          }
  end

  defmodule ControlAck do
    @moduledoc false
    @enforce_keys [:request_id, :action, :ok, :authorization_generation]
    defstruct [
      :request_id,
      :action,
      :ok,
      :authorization_generation,
      :in_flight_request_id,
      :sidecar_generation,
      :session_generation
    ]

    @type t :: %__MODULE__{
            request_id: String.t(),
            action: :pause | :resume | :release,
            ok: boolean(),
            authorization_generation: non_neg_integer(),
            in_flight_request_id: String.t() | nil,
            sidecar_generation: String.t() | nil,
            session_generation: pos_integer() | nil
          }
  end

  defmodule SessionEvent do
    @moduledoc false
    @enforce_keys [:kind, :payload]
    defstruct [
      :kind,
      :event_seq,
      :payload,
      :sidecar_generation,
      :session_generation,
      :authorization_generation
    ]

    @type t :: %__MODULE__{
            kind: String.t(),
            event_seq: non_neg_integer() | nil,
            payload: map(),
            sidecar_generation: String.t() | nil,
            session_generation: pos_integer() | nil,
            authorization_generation: non_neg_integer() | nil
          }
  end

  defmodule History do
    @moduledoc false
    @enforce_keys [:type, :payload]
    defstruct [:type, :payload]

    @type t :: %__MODULE__{type: String.t(), payload: map()}
  end

  @type t ::
          Request.t()
          | Response.t()
          | Control.t()
          | ControlAck.t()
          | SessionEvent.t()
          | History.t()

  @control_actions [:pause, :resume, :release]
  @history_types ~w(ack event)

  # Minted by `encode/1` for a request; an action argument may never use one of
  # these names, or the envelope and the action would fight over one key.
  @request_envelope ~w(type request_id sidecar_generation session_generation
                       authorization_generation mutation_seq deadline_ms protocol_version)

  # Stripped from a decoded response before the payload reaches a caller: frames,
  # ids and generations are this library's business. `protocol_version` is NOT
  # here — on a `hello` response it is the answer, not an envelope.
  @response_envelope ~w(type request_id sidecar_generation session_generation
                        authorization_generation mutation_seq)

  @max_request_id_bytes 64
  @request_id_pattern ~r/\A[\x20-\x7E]{1,#{@max_request_id_bytes}}\z/

  @doc "The control verbs the wire admits."
  @spec control_actions() :: [:pause | :resume | :release]
  def control_actions, do: @control_actions

  @doc "The family of a decoded frame, for a report that has to name what arrived."
  @spec kind(t()) :: atom()
  def kind(%Request{}), do: :request
  def kind(%Response{}), do: :response
  def kind(%Control{}), do: :control
  def kind(%ControlAck{}), do: :control_ack
  def kind(%SessionEvent{}), do: :session_event
  def kind(%History{type: "ack"}), do: :ack
  def kind(%History{type: "event"}), do: :event

  @doc """
  Encode an outbound frame to one JSON line, newline included.

  Only the two outbound families encode; handing this an inbound frame is a
  programmer error and raises, because nothing in this library should ever be
  writing a response.
  """
  @spec encode(Request.t() | Control.t()) :: {:ok, binary()} | {:error, term()}
  def encode(%Request{} = frame) do
    with {:ok, args} <- request_args(frame.args) do
      args
      |> Map.merge(%{"type" => "request", "request_id" => frame.request_id})
      |> put_present("deadline_ms", frame.deadline_ms)
      |> put_present("protocol_version", frame.protocol_version)
      |> put_present("mutation_seq", frame.mutation_seq)
      |> Map.merge(generations(frame))
      |> to_line()
    end
  end

  def encode(%Control{} = frame) do
    %{
      "type" => "control",
      "request_id" => frame.request_id,
      "action" => Atom.to_string(frame.action)
    }
    |> Map.merge(generations(frame))
    |> to_line()
  end

  @doc """
  Decode one line into a typed frame. Invalid JSON, a missing or unknown `type`,
  and a family whose required fields are absent or out of shape all fail loud —
  a caller may not treat any of them as a reply.
  """
  @spec decode(binary()) :: {:ok, t()} | {:error, term()}
  def decode(line) when is_binary(line) do
    case Jason.decode(String.trim(line)) do
      {:ok, map} when is_map(map) ->
        from_map(map)

      {:ok, other} ->
        {:error, {:malformed_frame, other}}

      {:error, %Jason.DecodeError{} = error} ->
        {:error, {:invalid_json, Exception.message(error)}}
    end
  end

  defp from_map(%{"type" => "response"} = map), do: decode_response(map)
  defp from_map(%{"type" => "control_ack"} = map), do: decode_control_ack(map)
  defp from_map(%{"type" => "session_event"} = map), do: decode_session_event(map)
  defp from_map(%{"type" => "request"} = map), do: decode_request(map)
  defp from_map(%{"type" => "control"} = map), do: decode_control(map)

  defp from_map(%{"type" => type} = map) when type in @history_types,
    do: {:ok, %History{type: type, payload: map}}

  defp from_map(%{"type" => type}), do: {:error, {:unknown_frame_type, type}}
  defp from_map(_map), do: {:error, :missing_frame_type}

  defp decode_response(map) do
    with {:ok, request_id} <- request_id(map, "request_id"),
         {:ok, ok} <- boolean_field(map, "ok"),
         {:ok, error} <- error_code(map, ok),
         {:ok, detail} <- optional_string(map, "detail"),
         {:ok, receipt} <- optional_map(map, "receipt") do
      {:ok,
       %Response{
         request_id: request_id,
         ok: ok,
         error: error,
         detail: detail,
         receipt: receipt,
         sidecar_generation: Map.get(map, "sidecar_generation"),
         session_generation: Map.get(map, "session_generation"),
         payload: Map.drop(map, @response_envelope)
       }}
    end
  end

  defp decode_control_ack(map) do
    with {:ok, request_id} <- request_id(map, "request_id"),
         {:ok, action} <- control_action(map),
         {:ok, ok} <- boolean_field(map, "ok"),
         {:ok, generation} <- non_neg_integer(map, "authorization_generation"),
         {:ok, in_flight} <- optional_request_id(map, "in_flight_request_id") do
      {:ok,
       %ControlAck{
         request_id: request_id,
         action: action,
         ok: ok,
         authorization_generation: generation,
         in_flight_request_id: in_flight,
         sidecar_generation: Map.get(map, "sidecar_generation"),
         session_generation: Map.get(map, "session_generation")
       }}
    end
  end

  defp decode_session_event(map) do
    with {:ok, kind} <- non_empty_string(map, "kind"),
         {:ok, event_seq} <- optional_non_neg_integer(map, "event_seq") do
      {:ok,
       %SessionEvent{
         kind: kind,
         event_seq: event_seq,
         payload: Map.drop(map, ~w(type)),
         sidecar_generation: Map.get(map, "sidecar_generation"),
         session_generation: Map.get(map, "session_generation"),
         authorization_generation: Map.get(map, "authorization_generation")
       }}
    end
  end

  defp decode_request(map) do
    with {:ok, request_id} <- request_id(map, "request_id"),
         {:ok, _action} <- non_empty_string(map, "action") do
      {:ok,
       %Request{
         request_id: request_id,
         args: Map.drop(map, @request_envelope),
         deadline_ms: Map.get(map, "deadline_ms"),
         protocol_version: Map.get(map, "protocol_version"),
         sidecar_generation: Map.get(map, "sidecar_generation"),
         session_generation: Map.get(map, "session_generation"),
         authorization_generation: Map.get(map, "authorization_generation"),
         mutation_seq: Map.get(map, "mutation_seq")
       }}
    end
  end

  defp decode_control(map) do
    with {:ok, request_id} <- request_id(map, "request_id"),
         {:ok, action} <- control_action(map) do
      {:ok,
       %Control{
         request_id: request_id,
         action: action,
         sidecar_generation: Map.get(map, "sidecar_generation"),
         session_generation: Map.get(map, "session_generation"),
         authorization_generation: Map.get(map, "authorization_generation")
       }}
    end
  end

  # --- outbound helpers -----------------------------------------------------

  defp request_args(args) when is_map(args) do
    with {:ok, _action} <- non_empty_string(args, "action") do
      case Enum.find(@request_envelope, &Map.has_key?(args, &1)) do
        nil -> {:ok, args}
        key -> {:error, {:reserved_field, key}}
      end
    end
  end

  defp request_args(other), do: {:error, {:malformed_request_args, other}}

  defp generations(frame) do
    %{}
    |> put_present("sidecar_generation", frame.sidecar_generation)
    |> put_present("session_generation", frame.session_generation)
    |> put_present("authorization_generation", frame.authorization_generation)
  end

  defp put_present(map, _key, nil), do: map
  defp put_present(map, key, value), do: Map.put(map, key, value)

  defp to_line(map) do
    case Jason.encode(map) do
      {:ok, json} -> {:ok, json <> "\n"}
      {:error, error} -> {:error, {:unencodable_frame, Exception.message(error)}}
    end
  end

  # --- field validation -----------------------------------------------------

  defp request_id(map, key) do
    case Map.get(map, key) do
      value when is_binary(value) ->
        if Regex.match?(@request_id_pattern, value),
          do: {:ok, value},
          else: {:error, {:invalid_request_id, value}}

      other ->
        {:error, {:invalid_request_id, other}}
    end
  end

  defp optional_request_id(map, key) do
    case Map.get(map, key) do
      nil -> {:ok, nil}
      _value -> request_id(map, key)
    end
  end

  defp control_action(map) do
    case Map.get(map, "action") do
      "pause" -> {:ok, :pause}
      "resume" -> {:ok, :resume}
      "release" -> {:ok, :release}
      other -> {:error, {:unknown_control_action, other}}
    end
  end

  defp boolean_field(map, key) do
    case Map.get(map, key) do
      value when is_boolean(value) -> {:ok, value}
      other -> {:error, {:invalid_field, key, other}}
    end
  end

  # A failure has to say what failed. `ok: false` with no code is a frame no
  # caller can act on, so it is malformed rather than a nameless error.
  defp error_code(_map, true), do: {:ok, nil}
  defp error_code(map, false), do: non_empty_string(map, "error")

  defp non_empty_string(map, key) do
    case Map.get(map, key) do
      value when is_binary(value) and value != "" -> {:ok, value}
      other -> {:error, {:invalid_field, key, other}}
    end
  end

  defp optional_string(map, key) do
    case Map.get(map, key) do
      nil -> {:ok, nil}
      _value -> non_empty_string(map, key)
    end
  end

  defp optional_map(map, key) do
    case Map.get(map, key) do
      nil -> {:ok, nil}
      value when is_map(value) -> {:ok, value}
      other -> {:error, {:invalid_field, key, other}}
    end
  end

  defp non_neg_integer(map, key) do
    case Map.get(map, key) do
      value when is_integer(value) and value >= 0 -> {:ok, value}
      other -> {:error, {:invalid_field, key, other}}
    end
  end

  defp optional_non_neg_integer(map, key) do
    case Map.get(map, key) do
      nil -> {:ok, nil}
      _value -> non_neg_integer(map, key)
    end
  end
end
