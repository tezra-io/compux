defmodule Compux.PortTest do
  use ExUnit.Case, async: true

  alias Compux.Port, as: SidecarPort

  @fake Path.expand("../support/fake_sidecar.pl", __DIR__)

  describe "check_binary/1" do
    test "refuses a path that is not a regular file" do
      assert {:error, {:sidecar_missing, "/no/such/compux"}} =
               SidecarPort.check_binary("/no/such/compux")
    end

    test "accepts the fixture" do
      assert :ok = SidecarPort.check_binary(@fake)
    end
  end

  describe "open/1" do
    test "hands back the Port and the OS pid the caller needs" do
      assert {:ok, handle} = SidecarPort.open(binary_path: @fake)
      assert is_port(handle.port)
      assert is_integer(handle.os_pid) and handle.os_pid > 0
      assert SidecarPort.open?(handle)

      SidecarPort.kill(handle)
    end

    test "fails loud on a missing binary rather than spawning" do
      assert {:error, {:sidecar_missing, "/no/such/compux"}} =
               SidecarPort.open(binary_path: "/no/such/compux")
    end

    # A raw-Port consumer reads its own lines, so the line limit is what decides
    # whether it gets whole frames or fragments.
    test "line_bytes fragments a longer line" do
      {:ok, handle} = SidecarPort.open(binary_path: @fake, line_bytes: 64)
      port = handle.port

      Elixir.Port.command(
        port,
        ~s({"type":"request","request_id":"r1","action":"oversize","bytes":200}\n)
      )

      assert_receive {^port, {:data, {:noeol, chunk}}}, 2_000
      assert byte_size(chunk) == 64
      assert_receive {^port, {:data, {:eol, _tail}}}, 2_000

      SidecarPort.kill(handle)
    end
  end

  describe "kill/1" do
    test "ends the OS process, not just the pipes" do
      {:ok, handle} = SidecarPort.open(binary_path: @fake)
      os_pid = handle.os_pid

      assert :ok = SidecarPort.kill(handle)
      refute SidecarPort.open?(handle)
      assert os_process_gone_within?(os_pid, 2_000)
    end

    test "is idempotent on a handle that is already dead" do
      {:ok, handle} = SidecarPort.open(binary_path: @fake)
      assert :ok = SidecarPort.kill(handle)
      assert :ok = SidecarPort.kill(handle)
      assert :ok = SidecarPort.close(handle)
    end
  end

  describe "sigkill/1" do
    # The transport SIGKILLs and then WAITS for the exit, so the Port has to stay
    # open across the kill: a closed Port delivers nothing further.
    test "leaves the Port open so the exit status still arrives" do
      {:ok, handle} = SidecarPort.open(binary_path: @fake)
      port = handle.port

      assert :ok = SidecarPort.sigkill(handle)
      assert_receive {^port, {:exit_status, _status}}, 2_000

      SidecarPort.close(handle)
    end
  end

  # Bounded poll on a process this test itself spawned; `ps -p` reads nothing else.
  defp os_process_gone_within?(os_pid, budget_ms) when budget_ms > 0 do
    case System.cmd("ps", ["-p", Integer.to_string(os_pid)], stderr_to_stdout: true) do
      {_out, 0} ->
        Process.sleep(100)
        os_process_gone_within?(os_pid, budget_ms - 100)

      {_out, _nonzero} ->
        true
    end
  end

  defp os_process_gone_within?(_os_pid, _budget_ms), do: false
end
