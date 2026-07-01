defmodule Compux.StubDriver do
  @moduledoc false
  # A `Compux.Driver` that answers without any native code, so the `Compux` facade
  # (handshake, request building, version gate) is fully unit-testable.
  #
  # `start/1` opts: `:protocol_version` (default 1), `:compux_version`, `:log` (a pid
  # sent `{:executed, request}` / `:stopped`), `:responses` (action => reply),
  # `:fail_hello` (make the handshake error).
  @behaviour Compux.Driver

  @impl true
  def start(opts) do
    {:ok,
     %{
       protocol_version: Keyword.get(opts, :protocol_version, 1),
       compux_version: Keyword.get(opts, :compux_version, "0.0.0-stub"),
       log: Keyword.get(opts, :log),
       responses: Keyword.get(opts, :responses, %{}),
       fail_hello: Keyword.get(opts, :fail_hello, false)
     }}
  end

  @impl true
  def execute(state, %{"action" => action} = request) do
    if state.log, do: send(state.log, {:executed, request})
    reply(state, action, request)
  end

  @impl true
  def stop(state) do
    if state[:log], do: send(state.log, :stopped)
    :ok
  end

  defp reply(%{fail_hello: true}, "hello", _request), do: {:error, :no_hello}

  defp reply(state, "hello", _request) do
    {:ok,
     %{
       "ok" => true,
       "protocol_version" => state.protocol_version,
       "compux_version" => state.compux_version,
       "actions" => Compux.Protocol.actions()
     }}
  end

  defp reply(_state, "probe", _request) do
    {:ok,
     %{
       "ok" => true,
       "platform" => "test",
       "display_server" => "test",
       "screen_capture" => true,
       "input_control" => false
     }}
  end

  defp reply(state, action, _request),
    do: Map.get(state.responses, action, {:ok, %{"ok" => true}})
end
