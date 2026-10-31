defmodule Compux.PortDriver do
  @moduledoc """
  The production `Compux.Driver`: the behaviour, implemented over
  `Compux.Transport`.

  The sidecar runs as a separate OS process (NOT a NIF) on purpose: a GUI driver
  can segfault on a TCC denial or an `xcap`/`enigo` quirk, and a Port crash is a
  recoverable message rather than a downed BEAM node. `start/1` fails loud if the
  configured binary is absent rather than degrading, and it refuses a sidecar
  whose protocol version is not this build's.

  This module holds no transport mechanism of its own. The Port, the framing, the
  request ids, the generations and the one absolute deadline per request all live
  in `Compux.Transport`, which `start/1` links to the calling process so the
  sidecar still dies with its owner.

    * `execute/2` waits at most `:timeout` ms (default #{30_000}) and returns
      `{:error, {:timeout, ms}}` on expiry. The library emits no timeout
      telemetry — that is the consumer's concern, it has the correlation id.
    * `control/2` waits at most #{5_000} ms for the acknowledgement and returns
      `{:error, :control_unconfirmed}` if it does not arrive.

  The state is opaque; a caller that needs the sidecar's OS pid asks
  `Compux.Transport.os_pid/1` for it rather than reaching for a Port.

  ## The caller of `start/1` is the owner

  Whoever calls `start/1` receives
  `{:compux_sidecar_exit, transport_pid, status}` whenever the sidecar ends by
  itself — including an IDLE exit, which no reply could have carried and which is
  the only way to learn a capture-stall status 75 — and
  `{:compux_session_event, transport_pid, event}` for unsolicited state. Both
  shapes are documented on `Compux.Transport`. Pass `:owner` to send them
  somewhere else.
  """

  @behaviour Compux.Driver

  alias Compux.Transport

  @default_timeout_ms 30_000

  # A barrier that is not acknowledged promptly is not a barrier. This is a
  # ceiling on an answer the sidecar gives from its control reader without
  # touching the OS, not a tuning knob.
  @control_timeout_ms 5_000

  @impl true
  def start(opts) when is_list(opts) do
    # The process that starts the driver is the one that has to hear about an
    # idle sidecar exit, so it is the owner unless the caller names another.
    opts = Keyword.put_new(opts, :owner, self())

    with {:ok, transport} <- Transport.start_link(opts) do
      {:ok, %{transport: transport, timeout: Keyword.get(opts, :timeout, @default_timeout_ms)}}
    end
  end

  @impl true
  def execute(%{transport: transport, timeout: timeout}, request) when is_map(request),
    do: Transport.request(transport, request, timeout)

  @impl true
  def control(%{transport: transport}, action) when action in [:pause, :resume, :release],
    do: Transport.control(transport, action, @control_timeout_ms)

  @impl true
  def stop(%{transport: transport}), do: Transport.stop(transport)
end
