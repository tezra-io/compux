defmodule Compux.Port do
  @moduledoc """
  Opening and ending the sidecar OS process.

  Two callers own a Port to the `compux` binary and neither should hand-roll the
  options or the reaping: `Compux.Transport`, which owns the request/response
  wire, and a consumer that owns a raw Port for an unsolicited push stream (the
  computer-history capturer reads its own lines and must keep doing so). So
  `open/1` hands back both the Port and the OS pid, and the teardown verbs are
  here rather than in either caller.

  Note on the name: this module is `Compux.Port`, the stdlib's is `Port`. Inside
  here the stdlib one is written `Elixir.Port` so no reader has to work out which
  is which; a caller that needs both should alias this one.
  """

  @enforce_keys [:port, :os_pid]
  defstruct [:port, :os_pid]

  @type t :: %__MODULE__{port: port(), os_pid: non_neg_integer() | nil}

  # A line longer than this arrives as `:noeol` fragments the reader reassembles.
  # It is the sidecar's frame ceiling, so by default nothing a healthy sidecar
  # writes ever fragments; a test lowers it to exercise reassembly for real.
  @default_line_bytes 16_777_216

  @doc """
  Refuse a missing binary before anything spawns. Separate from `open/1` so a
  caller that starts a process around the Port can fail without spawning it.
  """
  @spec check_binary(Path.t()) :: :ok | {:error, {:sidecar_missing, Path.t()}}
  def check_binary(path) when is_binary(path) do
    if File.regular?(path), do: :ok, else: {:error, {:sidecar_missing, path}}
  end

  @doc """
  Spawn the sidecar. `opts` takes `:binary_path` (required), `:args`, `:env` and
  `:line_bytes`. The Port belongs to the calling process; it dies with it.
  """
  @spec open(keyword()) :: {:ok, t()} | {:error, {:sidecar_missing, Path.t()}}
  def open(opts) when is_list(opts) do
    path = Keyword.fetch!(opts, :binary_path)

    with :ok <- check_binary(path) do
      port = Elixir.Port.open({:spawn_executable, path}, options(opts))
      {:ok, %__MODULE__{port: port, os_pid: read_os_pid(port)}}
    end
  end

  @doc """
  Close the pipes and end the OS process. Idempotent, and safe on a port that
  already died.

  Closing alone is not enough. `Elixir.Port.close/1` closes the pipes and never
  ends the process: a sidecar blocked inside a native capture (ScreenCaptureKit
  on a sleeping display) does not notice stdin EOF, and a leaked stuck client
  wedges capture SYSTEM-WIDE (observed live, 2026-07-01: 7 leaked sidecars, every
  capture ~30 s until they were killed). We own the process, so we end it.
  SIGKILL is safe — the sidecar holds no state to flush — and killing a pid that
  has already exited is a harmless no-op.
  """
  @spec kill(t()) :: :ok
  def kill(%__MODULE__{} = handle) do
    close(handle)
    sigkill(handle)
  end

  @doc """
  SIGKILL the OS process and leave the Port open, so the owner still receives the
  `{port, {:exit_status, n}}` that proves the process is gone. A closed Port
  delivers nothing further, so a caller that wants to WAIT for the exit kills
  first and closes after.
  """
  @spec sigkill(t()) :: :ok
  def sigkill(%__MODULE__{os_pid: nil}), do: :ok

  def sigkill(%__MODULE__{os_pid: os_pid}) do
    System.cmd("kill", ["-KILL", Integer.to_string(os_pid)], stderr_to_stdout: true)
    :ok
  end

  @doc "Close the pipes if the Port is still open. Idempotent."
  @spec close(t()) :: :ok
  def close(%__MODULE__{port: port}) do
    if Elixir.Port.info(port), do: Elixir.Port.close(port)
    :ok
  rescue
    # The port died between the info/1 probe and the close/1. That is the state
    # close/1 was asked to reach, so it is reached.
    ArgumentError -> :ok
  end

  @doc "Whether the Port is still open."
  @spec open?(t()) :: boolean()
  def open?(%__MODULE__{port: port}), do: Elixir.Port.info(port) != nil

  defp options(opts) do
    [
      {:line, Keyword.get(opts, :line_bytes, @default_line_bytes)},
      :binary,
      :exit_status,
      :use_stdio,
      {:args, Keyword.get(opts, :args, [])},
      {:env, Keyword.get(opts, :env, [])}
    ]
  end

  defp read_os_pid(port) do
    case Elixir.Port.info(port, :os_pid) do
      {:os_pid, os_pid} -> os_pid
      nil -> nil
    end
  end
end
