defmodule Compux.Driver do
  @moduledoc """
  Behaviour for the OS-driver backend that captures the screen and injects
  mouse/keyboard input.

  The production driver is `Compux.PortDriver` — an Elixir Port to the `compux`
  Rust `enigo`+`xcap` sidecar. Defining the behaviour lets a caller own "a driver"
  rather than a Port directly, so it can be unit-tested against a stub that speaks
  the same `Compux.Protocol` request/response shape without any native code.

  Contract:
    * `start/1` opens the backend (spawns/attaches the sidecar) → an opaque handle.
      `opts` carries `:binary_path` (required) and may carry `:timeout`, `:args`,
      and `:env`.
    * `execute/2` runs one request map, returning the decoded response
      (`%{"ok" => true, ...}`) or `{:error, reason}`.
    * `stop/1` tears the backend down — for the real driver this kills the Port and
      releases any held keys/mouse buttons (BEAM death alone does not un-press a
      physically-held key). It must be idempotent and is invoked on every teardown
      path.
    * `control/2` (optional) installs or lifts an input barrier and answers from
      the backend's ACKNOWLEDGEMENT of it, not from having sent it. It must be
      callable from one process while `execute/2` runs in another — which is the
      point: a Pause that has to wait for the action it is pausing is not a pause.
      A backend that cannot acknowledge a barrier does not implement it.
  """

  @type state :: term()

  @callback start(opts :: keyword()) :: {:ok, state()} | {:error, term()}
  @callback execute(state(), request :: map()) :: {:ok, map()} | {:error, term()}
  @callback stop(state()) :: :ok
  @callback control(state(), :pause | :resume | :release) :: {:ok, map()} | {:error, term()}

  @optional_callbacks control: 2
end
