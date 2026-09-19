defmodule Compux.Transport do
  @moduledoc """
  The one Port-owning process for the request / response wire.

  It opens the sidecar, performs `hello`, refuses a version mismatch, and from
  then on is the only thing that reads the pipe. Callers hand it an action map
  and a budget; frames, request ids, generations, fragment reassembly and
  deadlines never leave this module.

  What it guarantees:

    * **One absolute deadline per request.** Fragments, acknowledgements and
      events never extend it (`Compux.Deadline`).
    * **The 16 MiB frame cap is counted on every fragment**, the last one
      included.
    * **A reply reaches the request that asked for it**, by `request_id`. A
      response that arrives after its deadline is dropped and can never be paired
      with a later request — the desync class this wire exists to retire.
    * **One request in flight.** A second answers `{:error, :busy}`. Controls are
      admitted regardless, so Pause is answerable while an action runs.
    * **Every outstanding call is completed** when the sidecar exits, when a
      frame poisons the transport, and when it is stopped. Nothing is left to
      time out on its own.

  What poisons it: an unknown frame tag, a malformed frame, a frame naming a
  request this transport never sent, a generation that is not ours, an over-cap
  frame, and a control whose acknowledgement never came. A poisoned transport
  completes everything outstanding with a typed error, ends the sidecar, and
  answers `{:error, :sidecar_unavailable}` from then on. It does not reconnect —
  the owner starts a new one, which is recovery, never a replay of the last
  action.

  ## Stale versus unknown

  A frame that names nothing outstanding is one of two very different things, and
  the counter tells them apart:

    * every id this transport writes is `r<n>` or `c<n>` drawn from ONE monotonic
      counter, so a well-formed id **below** the counter that is not outstanding
      can only be a request of ours that already ended — a reply that lost its
      race with its own deadline. It is **dropped and logged**, never paired with
      the request that comes next;
    * an id **at or above** the counter, or one that is not that shape at all, is
      an id this transport never minted. Nothing it could answer exists, so it is
      a genuine protocol violation and it **poisons**.

  The counter is the whole record: nothing is remembered per request. An earlier
  version kept a bounded list of timed-out ids, which meant that after enough
  timeouts the oldest id was evicted and its eventual late reply looked like a
  frame from nowhere — a healthy transport poisoned by its own past.

  It is started linked to its caller, so the Port still dies with its owner, and
  it traps exits so the OS process is reaped rather than merely orphaned.

  ## Messages to the owner

      {:compux_sidecar_exit, transport_pid, non_neg_integer()}
      {:compux_sidecar_exit, transport_pid, {:poisoned, reason}}
      {:compux_session_event, transport_pid, %Compux.Frame.SessionEvent{}}

  The owner is the process that called `start_link/1` unless `:owner` says
  otherwise, and it is told **once** whenever the sidecar ends — including an
  IDLE exit, with nothing outstanding, which no reply could ever carry. That case
  is load-bearing: the capture-stall fail-fast flushes its response and only then
  exits 75, so a consumer's capture-health breaker is fed by that status and by
  nothing else.

  The two payloads say who ended the process, which is why this is one message
  with two shapes rather than two messages:

    * an **integer** is the status the sidecar chose for itself, and the only
      case in which a code such as 75 carries the sidecar's own meaning;
    * `{:poisoned, reason}` is this transport having killed it over an unusable
      wire, where an exit code could only ever be the signal we sent.

  Two silences are deliberate. A `stop/1` the owner asked for sends nothing —
  that exit is the answer to the call. Nothing is sent before the handshake
  completes either: no owner is listening yet, and `start_link/1` returns the
  reason instead.

  Nothing emits a session event in this protocol version; the family is decoded
  and forwarded so a consumer can be written against it.
  """

  use GenServer

  require Logger

  alias Compux.{Deadline, Frame, Protocol}
  alias Compux.Port, as: SidecarPort

  @control_actions Frame.control_actions()

  # A request line the sidecar will not read is refused here rather than written
  # and lost. Text arguments are already capped at 10,000 bytes by `Protocol`.
  @max_request_bytes 65_536
  @default_max_response_bytes 16_777_216

  # The handshake is one round trip on a freshly spawned process; a budget this
  # size only fires when the binary is wedged before it reads its first line.
  @default_handshake_ms 10_000

  # How long past a caller's own budget the enclosing `GenServer.call` waits. The
  # transport always answers within the budget, so this only ever covers
  # scheduling; it exists so no call is unbounded.
  @call_grace_ms 2_000

  # `stop/1` SIGKILLs and then waits this long for the OS process to be reaped
  # before answering. Bounded: a wedged native call must not hold a teardown open.
  @exit_wait_ms 2_000
  @stop_call_ms 5_000

  # §6 of the design bounds control traffic. One request in flight means at most
  # a couple of controls can be meaningful at once; this is the hard ceiling.
  @max_pending_controls 8

  # Every id this transport writes: one prefix, one monotonic number, no padding.
  # `next_id` is the next unused number, which makes the counter itself the record
  # of what was ever minted — see `finished?/2`.
  @minted_id ~r/\A[rc]([1-9]\d*)\z/

  # Both sides start here and the sidecar's gate owns every later value, which it
  # publishes in each `control_ack`.
  @initial_authorization_generation 1

  # One transport is one session: the Port dies with it, so a later transport can
  # never receive a frame minted for this one.
  @session_generation 1

  @enforce_keys [:port, :owner, :max_response_bytes, :handshake_timeout_ms]
  defstruct [
    :port,
    :owner,
    :max_response_bytes,
    :handshake_timeout_ms,
    :identity,
    :sidecar_generation,
    :pending_request,
    :handshake_waiter,
    phase: :handshake,
    handshake: :pending,
    session_generation: @session_generation,
    authorization_generation: @initial_authorization_generation,
    mutation_seq: 0,
    next_id: 1,
    pending_controls: %{},
    acc: [],
    acc_size: 0
  ]

  @type control_action :: :pause | :resume | :release
  @type ack :: %{
          action: control_action(),
          ok: boolean(),
          authorization_generation: non_neg_integer(),
          in_flight_request_id: String.t() | nil
        }

  @doc """
  Spawn the sidecar and complete the handshake, or fail with the reason it could
  not be completed. Linked to the caller.

  `opts` takes `:binary_path` (required), `:args`, `:env`, `:line_bytes`,
  `:max_response_bytes`, `:handshake_timeout` and `:owner` (the process that
  receives session events, the caller by default).
  """
  @spec start_link(keyword()) :: {:ok, pid()} | {:error, term()}
  def start_link(opts) when is_list(opts) do
    with :ok <- SidecarPort.check_binary(Keyword.fetch!(opts, :binary_path)),
         {:ok, transport} <- GenServer.start_link(__MODULE__, {opts, self()}) do
      settle(transport, handshake_timeout(opts))
    end
  end

  @doc """
  Run one action within `timeout_ms`. A second request while one is in flight is
  refused `{:error, :busy}` rather than queued.

  A failed action answers `{:error, {:action_failed, payload}}`, where the payload
  is the response's own fields — `"error"`, an optional `"detail"`, and the
  `"receipt"` that says whether input was dispatched.
  """
  @spec request(GenServer.server(), map(), pos_integer()) :: {:ok, map()} | {:error, term()}
  def request(transport, %{"action" => action} = args, timeout_ms)
      when is_binary(action) and is_integer(timeout_ms) and timeout_ms > 0 do
    call(transport, {:request, args, timeout_ms}, timeout_ms + @call_grace_ms)
  end

  @doc """
  Send a control and answer from its acknowledgement. Always admitted, whatever
  is in flight.

  An acknowledgement that does not arrive within `timeout_ms` is
  `{:error, :control_unconfirmed}` and poisons the transport: an unanswered
  barrier means the sidecar's gate state is unknown, and acting as though the
  control landed is the one thing a Pause may never do.
  """
  @spec control(GenServer.server(), control_action(), pos_integer()) ::
          {:ok, ack()} | {:error, :control_unconfirmed | term()}
  def control(transport, action, timeout_ms)
      when action in @control_actions and is_integer(timeout_ms) and timeout_ms > 0 do
    call(transport, {:control, action, timeout_ms}, timeout_ms + @call_grace_ms)
  end

  @doc "The sidecar identity read at the handshake: protocol, version, actions, capabilities."
  @spec identity(GenServer.server()) :: {:ok, map()} | {:error, term()}
  def identity(transport), do: call(transport, :identity, @call_grace_ms)

  @doc "The sidecar's OS pid, for a caller that has to prove the process is gone."
  @spec os_pid(GenServer.server()) :: {:ok, non_neg_integer()} | {:error, term()}
  def os_pid(transport), do: call(transport, :os_pid, @call_grace_ms)

  @doc """
  End the sidecar and stop. Closes the pipes, SIGKILLs, and waits a bounded time
  for the OS process to be reaped before answering. Idempotent.
  """
  @spec stop(GenServer.server()) :: :ok
  def stop(transport) do
    GenServer.call(transport, :stop, @stop_call_ms)
  catch
    # It is already gone, and the sidecar it owned went with it — which is
    # exactly what stop/1 promises. Any other exit reason still propagates.
    :exit, {reason, _call} when reason in [:noproc, :normal, :shutdown] -> :ok
  end

  # A transport that has stopped is a sidecar that is gone, which is the one
  # answer this whole module gives for "there is nothing to send to". A caller
  # holding a stale handle gets it rather than an exit it never asked to trap.
  # Every other exit reason — a timeout, a crash — still propagates.
  defp call(transport, message, timeout) do
    GenServer.call(transport, message, timeout)
  catch
    :exit, {reason, _call} when reason in [:noproc, :normal, :shutdown] ->
      {:error, :sidecar_unavailable}
  end

  # --- lifecycle ------------------------------------------------------------

  @impl true
  def init({opts, caller}) do
    Process.flag(:trap_exit, true)

    case SidecarPort.open(opts) do
      {:ok, port} -> send_hello(new_state(port, opts, caller))
      {:error, reason} -> {:stop, reason}
    end
  end

  @impl true
  def terminate(_reason, %__MODULE__{phase: :closed}), do: :ok
  def terminate(_reason, %__MODULE__{} = state), do: SidecarPort.kill(state.port)

  defp new_state(port, opts, caller) do
    %__MODULE__{
      port: port,
      owner: Keyword.get(opts, :owner, caller),
      max_response_bytes: Keyword.get(opts, :max_response_bytes, @default_max_response_bytes),
      handshake_timeout_ms: handshake_timeout(opts)
    }
  end

  defp handshake_timeout(opts), do: Keyword.get(opts, :handshake_timeout, @default_handshake_ms)

  defp send_hello(state) do
    {id, state} = mint_id(state, "r")

    frame = %Frame.Request{
      request_id: id,
      args: %{"action" => "hello"},
      deadline_ms: state.handshake_timeout_ms,
      protocol_version: Protocol.protocol_version()
    }

    case write(state, frame) do
      :ok ->
        {:ok, track_request(state, id, nil, Deadline.start(state.handshake_timeout_ms), :hello)}

      {:error, reason} ->
        {:stop, reason}
    end
  end

  defp settle(transport, timeout_ms) do
    case GenServer.call(transport, :await_handshake, timeout_ms + @call_grace_ms) do
      :ok -> {:ok, transport}
      {:error, reason} -> abandon(transport, reason)
    end
  end

  defp abandon(transport, reason) do
    GenServer.stop(transport, :normal, @stop_call_ms)
    {:error, reason}
  end

  # --- calls ----------------------------------------------------------------

  @impl true
  def handle_call(:await_handshake, from, %__MODULE__{handshake: :pending} = state),
    do: {:noreply, %{state | handshake_waiter: from}}

  def handle_call(:await_handshake, _from, %__MODULE__{handshake: outcome} = state),
    do: {:reply, outcome, state}

  # The handshake already read the identity; asking again is answered from it
  # rather than by spending a second round trip on a value that cannot change
  # without a reboot.
  def handle_call({:request, %{"action" => "hello"}, _ms}, _from, %{phase: :ready} = state),
    do: {:reply, {:ok, state.identity}, state}

  def handle_call({:request, args, timeout_ms}, from, state),
    do: admit_request(state, args, timeout_ms, from)

  def handle_call({:control, action, timeout_ms}, from, state),
    do: admit_control(state, action, timeout_ms, from)

  def handle_call(:identity, _from, state), do: {:reply, identity_reply(state), state}

  def handle_call(:os_pid, _from, state), do: {:reply, os_pid_reply(state.port), state}

  def handle_call(:stop, _from, state), do: shutdown(state)

  defp identity_reply(%__MODULE__{identity: nil}), do: {:error, :sidecar_unavailable}
  defp identity_reply(%__MODULE__{identity: identity}), do: {:ok, identity}

  defp os_pid_reply(%SidecarPort{os_pid: nil}), do: {:error, :no_os_pid}
  defp os_pid_reply(%SidecarPort{os_pid: os_pid}), do: {:ok, os_pid}

  defp shutdown(%__MODULE__{phase: :closed} = state), do: {:stop, :normal, :ok, state}

  defp shutdown(state) do
    SidecarPort.sigkill(state.port)
    await_exit(state.port)
    SidecarPort.close(state.port)
    state = fail_outstanding(state, {:error, :sidecar_unavailable})
    {:stop, :normal, :ok, %{state | phase: :closed}}
  end

  # The Port stays open across the SIGKILL precisely so this message can arrive;
  # a closed Port delivers nothing and the wait would always run its full budget.
  defp await_exit(%SidecarPort{port: port}) do
    receive do
      {^port, {:exit_status, _status}} -> :ok
    after
      @exit_wait_ms ->
        Logger.warning("compux: the sidecar did not exit within #{@exit_wait_ms} ms of SIGKILL")
        :timeout
    end
  end

  # --- admission ------------------------------------------------------------

  defp admit_request(%__MODULE__{phase: :closed} = state, _args, _ms, _from),
    do: {:reply, {:error, :sidecar_unavailable}, state}

  # One in flight. During the handshake that one is `hello`, so a request sent
  # before the transport is ready lands here too.
  defp admit_request(%__MODULE__{pending_request: pending} = state, _args, _ms, _from)
       when pending != nil,
       do: {:reply, {:error, :busy}, state}

  defp admit_request(state, args, timeout_ms, from) do
    {id, minted} = mint_id(state, "r")
    {mutation_seq, minted} = next_mutation_seq(minted, args)
    frame = action_frame(minted, id, args, timeout_ms, mutation_seq)

    case write(minted, frame) do
      :ok -> {:noreply, track_request(minted, id, from, Deadline.start(timeout_ms), :action)}
      {:error, reason} -> {:reply, {:error, reason}, closed(state, {:error, reason})}
    end
  end

  defp admit_control(%__MODULE__{phase: :closed} = state, _action, _ms, _from),
    do: {:reply, {:error, :sidecar_unavailable}, state}

  defp admit_control(%__MODULE__{phase: :handshake} = state, _action, _ms, _from),
    do: {:reply, {:error, :busy}, state}

  defp admit_control(state, action, timeout_ms, from) do
    if map_size(state.pending_controls) >= @max_pending_controls,
      do: {:reply, {:error, :control_queue_full}, state},
      else: send_control(state, action, timeout_ms, from)
  end

  defp send_control(state, action, timeout_ms, from) do
    {id, minted} = mint_id(state, "c")

    frame = %Frame.Control{
      request_id: id,
      action: action,
      sidecar_generation: minted.sidecar_generation,
      session_generation: minted.session_generation,
      authorization_generation: minted.authorization_generation
    }

    case write(minted, frame) do
      :ok -> {:noreply, track_control(minted, id, from, timeout_ms)}
      {:error, reason} -> {:reply, {:error, reason}, closed(state, {:error, reason})}
    end
  end

  defp action_frame(state, id, args, timeout_ms, mutation_seq) do
    %Frame.Request{
      request_id: id,
      args: args,
      deadline_ms: timeout_ms,
      sidecar_generation: state.sidecar_generation,
      session_generation: state.session_generation,
      authorization_generation: state.authorization_generation,
      mutation_seq: mutation_seq
    }
  end

  # Mutating is the complement of `Protocol.read_only?/1` — one list answers both
  # "may a consumer auto-run this" and "does this carry a sequence and a receipt".
  defp next_mutation_seq(state, %{"action" => action}) do
    if Protocol.read_only?(action) do
      {nil, state}
    else
      seq = state.mutation_seq + 1
      {seq, %{state | mutation_seq: seq}}
    end
  end

  defp write(state, frame) do
    with {:ok, line} <- Frame.encode(frame),
         :ok <- within_request_bound(line) do
      command(state.port, line)
    end
  end

  defp within_request_bound(line) when byte_size(line) > @max_request_bytes,
    do: {:error, :request_too_large}

  defp within_request_bound(_line), do: :ok

  defp command(%SidecarPort{port: port}, line) do
    Elixir.Port.command(port, line)
    :ok
  rescue
    # Port.command/2 raises once the Port is closed: the sidecar is gone, and
    # nothing was written.
    ArgumentError -> {:error, :sidecar_unavailable}
  end

  defp track_request(state, id, from, deadline, kind) do
    timer = Process.send_after(self(), {:deadline, :request, id}, Deadline.remaining(deadline))

    %{
      state
      | pending_request: %{id: id, from: from, deadline: deadline, timer: timer, kind: kind}
    }
  end

  defp track_control(state, id, from, timeout_ms) do
    timer = Process.send_after(self(), {:deadline, :control, id}, timeout_ms)
    pending = %{from: from, timer: timer}
    %{state | pending_controls: Map.put(state.pending_controls, id, pending)}
  end

  defp mint_id(state, prefix),
    do: {prefix <> Integer.to_string(state.next_id), %{state | next_id: state.next_id + 1}}

  # --- inbound --------------------------------------------------------------

  @impl true
  def handle_info({port, _message}, %__MODULE__{phase: :closed, port: %{port: port}} = state),
    do: {:noreply, state}

  def handle_info({port, {:data, {kind, chunk}}}, %__MODULE__{port: %{port: port}} = state),
    do: absorb(state, kind, chunk)

  def handle_info({port, {:exit_status, status}}, %__MODULE__{port: %{port: port}} = state),
    do: {:noreply, sidecar_exited(state, status)}

  def handle_info({:deadline, :request, id}, state), do: {:noreply, request_deadline(state, id)}

  def handle_info({:deadline, :control, id}, state), do: {:noreply, control_deadline(state, id)}

  # Trapping exits is how the OS process gets reaped, so the Port's own exit
  # signal arrives here as a message — including the short-lived port `System.cmd`
  # opens for the SIGKILL. `{:exit_status, n}` above carries the fact that matters.
  def handle_info({:EXIT, port, _reason}, state) when is_port(port), do: {:noreply, state}

  # Nothing else should reach a process that owns one Port and its own timers. Say
  # so rather than crash the session over a stray message we did not ask for.
  def handle_info(message, state) do
    Logger.warning("compux transport ignored an unexpected message: #{inspect(message)}")
    {:noreply, state}
  end

  # Every fragment counts toward the cap, the final one included: counting only
  # the unterminated ones made the last fragment free and let an over-cap frame
  # through as a success.
  defp absorb(state, kind, chunk) do
    size = state.acc_size + byte_size(chunk)

    cond do
      size > state.max_response_bytes ->
        {:noreply, poison(state, :sidecar_response_too_large)}

      kind == :noeol ->
        {:noreply, %{state | acc: [state.acc, chunk], acc_size: size}}

      true ->
        {:noreply, complete_line(state, chunk)}
    end
  end

  defp complete_line(state, chunk) do
    line = IO.iodata_to_binary([state.acc, chunk])
    handle_line(%{state | acc: [], acc_size: 0}, line)
  end

  defp handle_line(state, line) do
    case Frame.decode(line) do
      {:ok, %Frame.Response{} = response} -> route_response(state, response)
      {:ok, %Frame.ControlAck{} = ack} -> route_ack(state, ack)
      {:ok, %Frame.SessionEvent{} = event} -> forward_event(state, event)
      {:ok, other} -> poison(state, {:unexpected_frame, Frame.kind(other)})
      {:error, reason} -> poison(state, {:malformed_frame, reason})
    end
  end

  defp route_response(%__MODULE__{phase: :handshake} = state, response),
    do: handshake_response(state, response)

  defp route_response(state, response) do
    if pending_request?(state, response.request_id),
      do: answer_request(state, response),
      else: unmatched(state, response.request_id)
  end

  defp pending_request?(%__MODULE__{pending_request: %{id: id}}, id), do: true
  defp pending_request?(_state, _id), do: false

  # A frame naming nothing outstanding is one of two very different things, and
  # the counter tells them apart without keeping a list of anything.
  defp unmatched(state, request_id) do
    if finished?(state, request_id),
      do: drop_finished(state, request_id),
      else: poison(state, {:unknown_request_id, request_id})
  end

  # Ours, and already over. Every id this transport writes is `r<n>` or `c<n>`
  # from ONE counter, so a well-formed id below the counter that is not
  # outstanding can only be a request of ours that already ended — a reply that
  # lost its race with its own deadline. It is dropped, never paired with the
  # request that comes next.
  defp finished?(state, request_id) do
    case Regex.run(@minted_id, request_id) do
      [_whole, digits] -> String.to_integer(digits) < state.next_id
      nil -> false
    end
  end

  defp drop_finished(state, request_id) do
    Logger.warning("compux: dropped a late frame for #{request_id}, a request that already ended")
    state
  end

  defp answer_request(state, response) do
    if generations_match?(state, response),
      do: reply_request(state, response),
      else: poison(state, {:stale_generation, response.request_id})
  end

  defp reply_request(state, response) do
    pending = state.pending_request
    cancel_timer(pending.timer)
    GenServer.reply(pending.from, response_result(response))
    %{state | pending_request: nil}
  end

  defp response_result(%Frame.Response{ok: true, payload: payload}), do: {:ok, payload}
  defp response_result(%Frame.Response{payload: payload}), do: {:error, {:action_failed, payload}}

  defp route_ack(state, ack) do
    case Map.fetch(state.pending_controls, ack.request_id) do
      {:ok, pending} -> answer_control(state, pending, ack)
      :error -> unmatched(state, ack.request_id)
    end
  end

  defp answer_control(state, pending, ack) do
    if generations_match?(state, ack),
      do: deliver_ack(state, pending, ack),
      else: poison(state, {:stale_generation, ack.request_id})
  end

  defp deliver_ack(state, pending, ack) do
    cancel_timer(pending.timer)
    GenServer.reply(pending.from, ack_result(ack))

    %{
      state
      | pending_controls: Map.delete(state.pending_controls, ack.request_id),
        authorization_generation: ack.authorization_generation
    }
  end

  defp ack_result(%Frame.ControlAck{ok: true} = ack), do: {:ok, ack_map(ack)}
  defp ack_result(ack), do: {:error, {:control_refused, ack_map(ack)}}

  defp ack_map(ack) do
    %{
      action: ack.action,
      ok: ack.ok,
      authorization_generation: ack.authorization_generation,
      in_flight_request_id: ack.in_flight_request_id
    }
  end

  defp forward_event(state, event) do
    if generations_match?(state, event) do
      send(state.owner, {:compux_session_event, self(), event})
      state
    else
      poison(state, {:stale_generation, event.kind})
    end
  end

  defp generations_match?(state, frame) do
    frame.sidecar_generation == state.sidecar_generation and
      frame.session_generation == state.session_generation
  end

  # --- handshake ------------------------------------------------------------

  defp handshake_response(%__MODULE__{pending_request: %{id: id, timer: timer}} = state, response) do
    if response.request_id == id,
      do: check_hello(clear_hello(state, timer), response),
      else: poison(state, {:unknown_request_id, response.request_id})
  end

  defp handshake_response(state, response),
    do: poison(state, {:unknown_request_id, response.request_id})

  defp clear_hello(state, timer) do
    cancel_timer(timer)
    %{state | pending_request: nil}
  end

  defp check_hello(state, %Frame.Response{ok: false} = response),
    do: fail_handshake(state, {:handshake_refused, response.error})

  defp check_hello(state, response) do
    ours = Protocol.protocol_version()
    theirs = Map.get(response.payload, "protocol_version")

    if theirs == ours,
      do: adopt_identity(state, response),
      else: fail_handshake(state, {:protocol_mismatch, %{library: ours, sidecar: theirs}})
  end

  defp adopt_identity(state, %Frame.Response{sidecar_generation: boot} = response)
       when is_binary(boot) and boot != "" do
    # The hello response comes through the sidecar's ordinary envelope, so it
    # already states the session generation every later frame will carry. Reading
    # it HERE turns a disagreement into a named handshake refusal, instead of a
    # `stale_generation` poison on the first real action of a session that looked
    # healthy.
    if response.session_generation == state.session_generation,
      do: ready(state, response, boot),
      else: fail_handshake(state, session_mismatch(state, response))
  end

  defp adopt_identity(state, response),
    do: fail_handshake(state, {:invalid_sidecar_generation, response.sidecar_generation})

  defp ready(state, response, boot) do
    state = %{
      state
      | phase: :ready,
        sidecar_generation: boot,
        identity: Map.put(response.payload, "sidecar_generation", boot)
    }

    settle_handshake(state, :ok)
  end

  defp session_mismatch(state, response) do
    {:session_generation_mismatch,
     %{library: state.session_generation, sidecar: response.session_generation}}
  end

  defp fail_handshake(state, reason) do
    Logger.error("compux: the sidecar handshake failed: #{inspect(reason)}")
    SidecarPort.kill(state.port)
    state = %{state | phase: :closed, pending_request: nil, acc: [], acc_size: 0}

    state
    |> fail_controls({:error, reason})
    |> settle_handshake({:error, reason})
  end

  defp settle_handshake(state, outcome) do
    if state.handshake_waiter, do: GenServer.reply(state.handshake_waiter, outcome)
    %{state | handshake: outcome, handshake_waiter: nil}
  end

  # --- deadlines ------------------------------------------------------------

  defp request_deadline(%__MODULE__{pending_request: %{id: id, kind: :hello}} = state, id),
    do: fail_handshake(state, {:timeout, state.handshake_timeout_ms})

  # Nothing is recorded about the abandoned id: it stays below the counter for
  # the life of the transport, which is what makes its late reply recognisable.
  defp request_deadline(%__MODULE__{pending_request: %{id: id} = pending} = state, id) do
    GenServer.reply(pending.from, {:error, {:timeout, Deadline.budget_ms(pending.deadline)}})
    %{state | pending_request: nil}
  end

  defp request_deadline(state, _id), do: state

  defp control_deadline(state, id) do
    case Map.fetch(state.pending_controls, id) do
      {:ok, pending} -> unconfirmed_control(state, id, pending)
      :error -> state
    end
  end

  # An unanswered barrier means the gate's state is unknown. Carrying on would be
  # claiming a Pause that may never have installed.
  defp unconfirmed_control(state, id, pending) do
    GenServer.reply(pending.from, {:error, :control_unconfirmed})
    state = %{state | pending_controls: Map.delete(state.pending_controls, id)}
    poison(state, {:control_unconfirmed, id})
  end

  defp cancel_timer(nil), do: :ok
  defp cancel_timer(timer), do: Process.cancel_timer(timer)

  # --- ending ---------------------------------------------------------------

  defp poison(state, reason) do
    Logger.error("compux transport poisoned: #{inspect(reason)}; the sidecar was ended")
    SidecarPort.kill(state.port)
    announce(state, {:error, reason}, {:poisoned, reason})
  end

  defp sidecar_exited(state, status) do
    SidecarPort.close(state.port)
    announce(state, {:error, {:sidecar_exited, status}}, status)
  end

  # Complete everything outstanding FIRST, then tell the owner once — so a caller
  # that is both waiting on a request and watching for the exit sees its reply
  # before the news. Before the handshake completes there is nobody to tell:
  # `start_link/1` hands that caller the reason as its return value.
  defp announce(%__MODULE__{phase: :ready} = state, reply, status) do
    state = closed(state, reply)
    send(state.owner, {:compux_sidecar_exit, self(), status})
    state
  end

  defp announce(state, reply, _status), do: closed(state, reply)

  defp closed(state, reply) do
    state = fail_outstanding(state, reply)
    %{state | phase: :closed, acc: [], acc_size: 0}
  end

  defp fail_outstanding(state, reply), do: state |> fail_request(reply) |> fail_controls(reply)

  defp fail_request(%__MODULE__{pending_request: nil} = state, _reply), do: state

  defp fail_request(%__MODULE__{pending_request: %{kind: :hello, timer: timer}} = state, reply) do
    state |> clear_hello(timer) |> settle_handshake(reply)
  end

  defp fail_request(%__MODULE__{pending_request: pending} = state, reply) do
    cancel_timer(pending.timer)
    GenServer.reply(pending.from, reply)
    %{state | pending_request: nil}
  end

  defp fail_controls(state, reply) do
    Enum.each(state.pending_controls, fn {_id, pending} ->
      cancel_timer(pending.timer)
      GenServer.reply(pending.from, reply)
    end)

    %{state | pending_controls: %{}}
  end
end
