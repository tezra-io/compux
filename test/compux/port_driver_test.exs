defmodule Compux.PortDriverTest do
  use ExUnit.Case, async: true

  alias Compux.PortDriver

  @fake Path.expand("../support/fake_sidecar.pl", __DIR__)

  describe "start/1" do
    test "fails loud when the binary is absent" do
      assert {:error, {:sidecar_missing, "/no/such/compux"}} =
               PortDriver.start(binary_path: "/no/such/compux")
    end
  end

  describe "execute/2" do
    test "round-trips one request to a decoded response" do
      {:ok, state} = PortDriver.start(binary_path: @fake)

      assert {:ok, %{"ok" => true, "pong" => true}} =
               PortDriver.execute(state, %{"action" => "screenshot"})

      assert :ok = PortDriver.stop(state)
    end

    test "surfaces a sidecar exit as {:sidecar_exited, status}" do
      {:ok, state} = PortDriver.start(binary_path: @fake)

      assert {:error, {:sidecar_exited, 7}} =
               PortDriver.execute(state, %{"action" => "boom"})
    end

    test "returns {:timeout, ms} when the sidecar does not answer in time" do
      {:ok, state} = PortDriver.start(binary_path: @fake, timeout: 100)

      assert {:error, {:timeout, 100}} =
               PortDriver.execute(state, %{"action" => "hang"})

      PortDriver.stop(state)
    end

    test "returns :sidecar_unavailable once the port is closed" do
      {:ok, state} = PortDriver.start(binary_path: @fake)
      assert :ok = PortDriver.stop(state)

      assert {:error, :sidecar_unavailable} =
               PortDriver.execute(state, %{"action" => "screenshot"})
    end
  end

  describe "stop/1" do
    test "is idempotent" do
      {:ok, state} = PortDriver.start(binary_path: @fake)
      assert :ok = PortDriver.stop(state)
      assert :ok = PortDriver.stop(state)
    end

    test "kills a sidecar that is blocked in an action and ignoring stdin EOF" do
      # The leak class from live 2026-07-01: a sidecar stuck inside a native
      # capture never reads stdin, so Port.close alone leaks the OS process (and
      # a leaked stuck ScreenCaptureKit client wedges capture system-wide). The
      # fixture's "hang" (a 10s sleep, not reading) stands in for that state:
      # after stop/1 the OS process must be GONE promptly, not sleeping it off.
      {:ok, state} = PortDriver.start(binary_path: @fake, timeout: 50)
      {:os_pid, os_pid} = Port.info(state.port, :os_pid)

      assert {:error, {:timeout, 50}} = PortDriver.execute(state, %{"action" => "hang"})
      assert :ok = PortDriver.stop(state)

      assert os_process_exits_within?(os_pid, 2_000),
             "sidecar os process #{os_pid} still alive after stop/1"
    end
  end

  # Bounded poll (max ~2s) for the child's exit; `ps -p` is a read-only check on
  # a process this test itself spawned.
  defp os_process_exits_within?(os_pid, budget_ms) when budget_ms > 0 do
    case System.cmd("ps", ["-p", Integer.to_string(os_pid)], stderr_to_stdout: true) do
      {_out, 0} ->
        Process.sleep(100)
        os_process_exits_within?(os_pid, budget_ms - 100)

      {_out, _nonzero} ->
        true
    end
  end

  defp os_process_exits_within?(_os_pid, _budget_ms), do: false
end
