defmodule Compux.PortDriver do
  @moduledoc """
  The production `Compux.Driver`: an Elixir Port to the `compux` OS-driver sidecar
  binary. One newline-delimited JSON request to the sidecar's stdin, one
  newline-delimited JSON response back.

  The sidecar runs as a separate OS process (NOT a NIF) on purpose: a GUI driver
  can segfault on a TCC denial or an `xcap`/`enigo` quirk, and a Port crash is a
  recoverable `:exit_status` message rather than a downed BEAM node. `start/1`
  fails loud if the configured binary is absent rather than degrading.

  Framing: a base64 screenshot response can be several MB, so the reader sets a
  large line limit and still accumulates `:noeol` fragments up to a hard cap —
  newline-delimited (JSON has no embedded newlines) but robust to large single
  responses.

  Timeout: each `execute/2` waits at most `:timeout` ms (default #{30_000}) and
  returns `{:error, {:timeout, ms}}` on expiry. The library does NOT emit timeout
  telemetry — that is the consumer's concern (it has the correlation id).
  """

  @behaviour Compux.Driver

  alias Compux.Protocol

  @max_response_bytes 16_777_216
  @default_timeout_ms 30_000

  @impl true
  def start(opts) do
    path = Keyword.fetch!(opts, :binary_path)

    if File.regular?(path) do
      port = Port.open({:spawn_executable, path}, port_options(opts))
      {:ok, %{port: port, timeout: Keyword.get(opts, :timeout, @default_timeout_ms)}}
    else
      {:error, {:sidecar_missing, path}}
    end
  end

  @impl true
  def execute(%{port: port, timeout: timeout}, request) when is_map(request) do
    Port.command(port, Protocol.encode_request(request))
    receive_response(port, timeout)
  rescue
    # Port.command raises if the port is already closed (sidecar died).
    ArgumentError -> {:error, :sidecar_unavailable}
  end

  @impl true
  def stop(%{port: port}) do
    os_pid = port_os_pid(port)
    if Port.info(port), do: Port.close(port)
    # Port.close only closes the pipes — it never ends the OS process. A sidecar
    # blocked inside a native capture (ScreenCaptureKit on a sleeping display)
    # doesn't notice stdin EOF, and a leaked stuck client wedges SCK for every
    # later capture SYSTEM-WIDE (observed live, 2026-07-01: 7 leaked sidecars →
    # every capture ~30s until they were killed). We own the process: end it
    # definitively. SIGKILL is safe — the sidecar holds no state to flush — and
    # killing an already-exited pid is a harmless no-op.
    if os_pid, do: kill_os_process(os_pid)
    :ok
  rescue
    ArgumentError -> :ok
  end

  defp port_os_pid(port) do
    case Port.info(port, :os_pid) do
      {:os_pid, os_pid} -> os_pid
      nil -> nil
    end
  end

  defp kill_os_process(os_pid) do
    System.cmd("kill", ["-KILL", Integer.to_string(os_pid)], stderr_to_stdout: true)
    :ok
  end

  defp port_options(opts) do
    [
      {:line, @max_response_bytes},
      :binary,
      :exit_status,
      :use_stdio,
      {:args, Keyword.get(opts, :args, [])},
      {:env, Keyword.get(opts, :env, [])}
    ]
  end

  defp receive_response(port, timeout, acc \\ [], size \\ 0) do
    receive do
      {^port, {:data, {:eol, chunk}}} ->
        Protocol.decode_response(IO.iodata_to_binary([acc, chunk]))

      {^port, {:data, {:noeol, chunk}}} ->
        accumulate(port, timeout, acc, size, chunk)

      {^port, {:exit_status, status}} ->
        {:error, {:sidecar_exited, status}}
    after
      timeout -> {:error, {:timeout, timeout}}
    end
  end

  defp accumulate(port, timeout, acc, size, chunk) do
    new_size = size + byte_size(chunk)

    if new_size > @max_response_bytes do
      {:error, :sidecar_response_too_large}
    else
      receive_response(port, timeout, [acc, chunk], new_size)
    end
  end
end
