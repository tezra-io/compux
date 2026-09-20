defmodule Compux.PortDriverTest do
  use ExUnit.Case, async: true

  # Several of these prove a refusal, and a refusal logs loudly by design.
  @moduletag :capture_log

  alias Compux.PortDriver
  alias Compux.Transport

  @fake Path.expand("../support/fake_sidecar.pl", __DIR__)

  describe "start/1" do
    test "fails loud when the binary is absent" do
      assert {:error, {:sidecar_missing, "/no/such/compux"}} =
               PortDriver.start(binary_path: "/no/such/compux")
    end

    test "refuses a sidecar that speaks another protocol version" do
      assert {:error, {:protocol_mismatch, %{sidecar: 1}}} =
               PortDriver.start(
                 binary_path: @fake,
                 env: [{~c"FAKE_PROTOCOL_VERSION", ~c"1"}]
               )
    end

    # The transport is linked to the caller, so the sidecar still dies with the
    # process that owns the driver — a Port left behind wedges capture for
    # everyone, which is why nothing here is started unlinked.
    test "the sidecar goes down with the process that started it" do
      test_pid = self()

      owner =
        spawn(fn ->
          {:ok, state} = PortDriver.start(binary_path: @fake)
          {:ok, os_pid} = Transport.os_pid(state.transport)
          send(test_pid, {:started, state.transport, os_pid})
          Process.sleep(:infinity)
        end)

      assert_receive {:started, transport, os_pid}, 5_000
      Process.exit(owner, :kill)

      assert transport_gone_within?(transport, 2_000)
      assert os_process_gone_within?(os_pid, 3_000)
    end
  end

  describe "the caller of start/1 is the owner" do
    # An idle exit reaches nobody through a reply: the capture-stall fail-fast
    # flushes its response and only THEN exits 75, so the status arrives as a
    # message or not at all.
    test "it hears about an idle sidecar exit, with the status" do
      {:ok, state} = PortDriver.start(binary_path: @fake)
      transport = state.transport

      assert {:ok, %{"pong" => true}} = PortDriver.execute(state, %{"action" => "flush_exit"})
      assert_receive {:compux_sidecar_exit, ^transport, 75}, 2_000

      PortDriver.stop(state)
    end

    test "a stop it asked for tells it nothing" do
      {:ok, state} = PortDriver.start(binary_path: @fake)
      assert :ok = PortDriver.stop(state)
      refute_receive {:compux_sidecar_exit, _transport, _}, 500
    end
  end

  describe "execute/2" do
    test "round-trips one request to a decoded response" do
      {:ok, state} = PortDriver.start(binary_path: @fake)

      assert {:ok, %{"ok" => true, "pong" => true}} =
               PortDriver.execute(state, %{"action" => "ping"})

      assert :ok = PortDriver.stop(state)
    end

    test "surfaces a sidecar exit as {:sidecar_exited, status}" do
      {:ok, state} = PortDriver.start(binary_path: @fake)

      assert {:error, {:sidecar_exited, 7}} =
               PortDriver.execute(state, %{"action" => "boom"})

      PortDriver.stop(state)
    end

    test "returns {:timeout, ms} when the sidecar does not answer in time" do
      {:ok, state} = PortDriver.start(binary_path: @fake, timeout: 100)

      assert {:error, {:timeout, 100}} =
               PortDriver.execute(state, %{"action" => "hang"})

      PortDriver.stop(state)
    end

    test "returns :sidecar_unavailable once the sidecar is gone" do
      {:ok, state} = PortDriver.start(binary_path: @fake)
      assert :ok = PortDriver.stop(state)

      assert {:error, :sidecar_unavailable} =
               PortDriver.execute(state, %{"action" => "ping"})
    end
  end

  describe "control/2" do
    test "answers from the sidecar's acknowledgement" do
      {:ok, state} = PortDriver.start(binary_path: @fake)

      assert {:ok, ack} = PortDriver.control(state, :pause)
      assert ack.action == :pause
      assert ack.ok == true
      assert ack.in_flight_request_id == nil

      PortDriver.stop(state)
    end

    test "an unacknowledged control is unconfirmed, never assumed installed" do
      {:ok, state} =
        PortDriver.start(
          binary_path: @fake,
          env: [{~c"FAKE_CONTROL_MODE", ~c"silent"}]
        )

      assert {:error, :control_unconfirmed} = PortDriver.control(state, :pause)
      PortDriver.stop(state)
    end

    test "refuses a verb the wire does not have" do
      {:ok, state} = PortDriver.start(binary_path: @fake)
      assert_raise FunctionClauseError, fn -> PortDriver.control(state, :halt) end
      PortDriver.stop(state)
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
      # capture never reads stdin, so closing the pipes alone leaks the OS process
      # (and a leaked stuck ScreenCaptureKit client wedges capture system-wide).
      # The fixture's "hang" (a 10s sleep, not reading) stands in for that state:
      # after stop/1 the OS process must be GONE promptly, not sleeping it off.
      {:ok, state} = PortDriver.start(binary_path: @fake, timeout: 50)
      {:ok, os_pid} = Transport.os_pid(state.transport)

      assert {:error, {:timeout, 50}} = PortDriver.execute(state, %{"action" => "hang"})
      assert :ok = PortDriver.stop(state)

      assert os_process_gone_within?(os_pid, 3_000),
             "sidecar os process #{os_pid} still alive after stop/1"
    end
  end

  defp transport_gone_within?(pid, budget_ms) when budget_ms > 0 do
    if Process.alive?(pid) do
      Process.sleep(50)
      transport_gone_within?(pid, budget_ms - 50)
    else
      true
    end
  end

  defp transport_gone_within?(_pid, _budget_ms), do: false

  # Bounded poll (max ~3s) for the child's exit; `ps -p` is a read-only check on
  # a process this test itself spawned.
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
