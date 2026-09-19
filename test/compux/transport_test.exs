defmodule Compux.TransportTest do
  use ExUnit.Case, async: true

  # Several of these prove a refusal, and a refusal logs loudly by design.
  @moduletag :capture_log

  alias Compux.Frame.SessionEvent
  alias Compux.Transport

  @fake Path.expand("../support/fake_sidecar.pl", __DIR__)

  defp start!(opts \\ []) do
    {:ok, transport} = Transport.start_link([binary_path: @fake] ++ opts)
    transport
  end

  defp control_mode(mode), do: [env: [{~c"FAKE_CONTROL_MODE", String.to_charlist(mode)}]]

  describe "handshake" do
    test "reads the identity, the boot generation and the capabilities" do
      transport = start!()

      assert {:ok, identity} = Transport.identity(transport)
      assert identity["protocol_version"] == Compux.Protocol.protocol_version()
      assert identity["compux_version"] == "0.0.0-test"
      assert identity["sidecar_generation"] == "boot-test"
      assert identity["capabilities"]["controls"] == ["pause", "resume", "release"]

      Transport.stop(transport)
    end

    test "refuses a version mismatch before anything else is sent" do
      assert {:error, {:protocol_mismatch, %{library: library, sidecar: 1}}} =
               Transport.start_link(
                 binary_path: @fake,
                 env: [{~c"FAKE_PROTOCOL_VERSION", ~c"1"}]
               )

      assert library == Compux.Protocol.protocol_version()
    end

    test "refuses a missing binary without spawning a process" do
      assert {:error, {:sidecar_missing, "/no/such/compux"}} =
               Transport.start_link(binary_path: "/no/such/compux")
    end

    # The hello response comes through the sidecar's ordinary envelope, so it
    # already states the session generation every later frame will carry.
    test "reads the session generation the sidecar will stamp on every frame" do
      transport = start!()
      assert {:ok, echo} = Transport.request(transport, %{"action" => "echo"}, 2_000)
      assert echo["seen_sidecar_generation"] == "boot-test"
      Transport.stop(transport)
    end

    # Asking again is answered from the handshake rather than by spending a
    # second round trip on a value that cannot change without a reboot.
    test "a second hello is answered from the identity already read" do
      transport = start!()
      assert {:ok, identity} = Transport.request(transport, %{"action" => "hello"}, 1_000)
      assert identity["sidecar_generation"] == "boot-test"
      Transport.stop(transport)
    end
  end

  describe "request/3" do
    test "round-trips one action, correlated by request id" do
      transport = start!()

      assert {:ok, %{"ok" => true, "pong" => true}} =
               Transport.request(transport, %{"action" => "ping"}, 2_000)

      Transport.stop(transport)
    end

    test "carries the generations, the budget, and a mutation sequence that increases" do
      transport = start!()

      assert {:ok, first} = Transport.request(transport, %{"action" => "echo"}, 2_000)
      assert first["seen_sidecar_generation"] == "boot-test"
      assert first["seen_deadline_ms"] == 2_000
      assert first["seen_authorization_generation"] == 1
      assert first["seen_mutation_seq"] == 1

      assert {:ok, second} = Transport.request(transport, %{"action" => "echo"}, 2_000)
      assert second["seen_mutation_seq"] == 2

      Transport.stop(transport)
    end

    # `Protocol.read_only?/1` is the ONE list: a read dispatches nothing, so it
    # carries no sequence number and does not move the high-water mark a mutation
    # is checked against.
    test "a read-only action carries no mutation sequence and does not consume one" do
      transport = start!()

      assert {:ok, first} = Transport.request(transport, %{"action" => "echo"}, 2_000)
      assert first["seen_mutation_seq"] == 1

      assert {:ok, shot} = Transport.request(transport, %{"action" => "windows"}, 2_000)
      assert shot["seen_mutation_seq"] == nil

      assert {:ok, second} = Transport.request(transport, %{"action" => "echo"}, 2_000)
      assert second["seen_mutation_seq"] == 2

      Transport.stop(transport)
    end

    test "reassembles a fragmented response" do
      transport = start!(line_bytes: 64, max_response_bytes: 65_536)

      assert {:ok, response} =
               Transport.request(transport, %{"action" => "oversize", "bytes" => 600}, 2_000)

      assert byte_size(response["pad"]) > 64
      Transport.stop(transport)
    end

    test "a response that never comes answers {:timeout, ms}" do
      transport = start!()

      assert {:error, {:timeout, 200}} =
               Transport.request(transport, %{"action" => "hang"}, 200)

      Transport.stop(transport)
    end

    test "a failed action carries its code, detail and receipt" do
      transport = start!()

      assert {:error, {:action_failed, payload}} =
               Transport.request(transport, %{"action" => "refuse"}, 2_000)

      assert payload["error"] == "paused"
      assert payload["detail"] == "a pause is installed"
      assert payload["receipt"]["dispatch"] == "not_sent"

      Transport.stop(transport)
    end

    # The fixture derives the receipt the way the sidecar does — off the request's
    # own `mutation_seq`, with evidence only when a check really brought some back
    # — so EVERY mutating success in this suite carries one. A library change that
    # dropped or rewrote receipts now fails broadly, not in one test.
    test "a mutating success carries its receipt and a read-only one carries none" do
      transport = start!()

      assert {:ok, %{"receipt" => receipt}} =
               Transport.request(transport, %{"action" => "echo"}, 2_000)

      assert receipt["dispatch"] == "sent"
      assert receipt["input_method"] == "foreground_hid"
      assert receipt["effect"] == "unknown", "nothing was captured, so nothing was observed"
      assert receipt["check"] == %{"kind" => "none"}, "the receipt always says which evidence"

      assert {:ok, read} = Transport.request(transport, %{"action" => "windows"}, 2_000)
      refute Map.has_key?(read, "receipt")

      Transport.stop(transport)
    end

    test "a check that brought an image back makes the effect not_observed" do
      transport = start!()

      assert {:ok, %{"receipt" => receipt, "data" => "AAA"}} =
               Transport.request(transport, %{"action" => "capture", "check" => "image"}, 2_000)

      assert receipt["effect"] == "not_observed"

      # What the view did while the evidence was taken rides the receipt, and the
      # transport carries it through unaltered.
      assert receipt["check"] == %{"kind" => "image", "settle" => "stable", "changed" => false}

      Transport.stop(transport)
    end

    # Deliberately NOT this library's rule. The receipt is a fact the transport
    # carries; what a missing one MEANS is policy, and Fermix already treats it as
    # a protocol fault. Enforcing it in both layers would put one rule in two
    # repositories, and refusing here would destroy the response of an action that
    # already ran — the one thing this wire must never do. So the absence reaches
    # the caller visibly and unaltered, and the caller adjudicates.
    test "a mutating success with no receipt reaches the caller unaltered" do
      transport = start!()

      assert {:ok, payload} = Transport.request(transport, %{"action" => "no_receipt"}, 2_000)
      assert payload["ok"] == true
      refute Map.has_key?(payload, "receipt")

      Transport.stop(transport)
    end

    test "a sidecar death answers {:sidecar_exited, status}" do
      transport = start!()

      assert {:error, {:sidecar_exited, 7}} =
               Transport.request(transport, %{"action" => "boom"}, 2_000)

      Transport.stop(transport)
    end

    test "once the sidecar is gone every later request says so" do
      transport = start!()

      assert {:error, {:sidecar_exited, 7}} =
               Transport.request(transport, %{"action" => "boom"}, 2_000)

      assert {:error, :sidecar_unavailable} =
               Transport.request(transport, %{"action" => "ping"}, 2_000)

      Transport.stop(transport)
    end
  end

  describe "one request in flight" do
    test "a second request is refused :busy, not queued" do
      transport = start!()
      task = Task.async(fn -> Transport.request(transport, %{"action" => "defer"}, 5_000) end)

      assert_receive {:compux_session_event, ^transport, %SessionEvent{kind: "request_deferred"}},
                     2_000

      assert {:error, :busy} = Transport.request(transport, %{"action" => "ping"}, 1_000)

      Transport.control(transport, :pause, 2_000)
      Task.await(task, 5_000)
      Transport.stop(transport)
    end
  end

  describe "controls" do
    # execute/2 from one process while control/2 runs in another is the whole
    # point: a Pause that waits for the action it is pausing is not a pause.
    test "a control is admitted and acknowledged while a request is in flight" do
      transport = start!()
      task = Task.async(fn -> Transport.request(transport, %{"action" => "defer"}, 5_000) end)

      assert_receive {:compux_session_event, ^transport, %SessionEvent{kind: "request_deferred"}},
                     2_000

      assert {:ok, ack} = Transport.control(transport, :pause, 2_000)
      assert ack.action == :pause
      assert ack.ok == true
      assert ack.authorization_generation == 2
      assert ack.in_flight_request_id != nil

      assert {:error, {:action_failed, payload}} = Task.await(task, 5_000)
      assert payload["error"] == "cancelled"
      assert payload["receipt"]["dispatch"] == "partial"

      Transport.stop(transport)
    end

    # The gate counts, one per control, from its own initial 1 — so the value is
    # evidence that a control was applied, not a constant the transport could
    # have invented.
    test "the acknowledged authority generation counts and rides on the next request" do
      transport = start!()

      assert {:ok, %{authorization_generation: 2}} = Transport.control(transport, :pause, 2_000)

      assert {:ok, echo} = Transport.request(transport, %{"action" => "echo"}, 2_000)
      assert echo["seen_authorization_generation"] == 2

      assert {:ok, %{authorization_generation: 3}} = Transport.control(transport, :resume, 2_000)

      assert {:ok, echo} = Transport.request(transport, %{"action" => "echo"}, 2_000)
      assert echo["seen_authorization_generation"] == 3

      Transport.stop(transport)
    end

    test "a control that is never acknowledged is unconfirmed and poisons the transport" do
      transport = start!(control_mode("silent"))

      assert {:error, :control_unconfirmed} = Transport.control(transport, :pause, 200)

      assert {:error, :sidecar_unavailable} =
               Transport.request(transport, %{"action" => "ping"}, 1_000)

      Transport.stop(transport)
    end
  end

  describe "the sidecar exits with work outstanding" do
    test "every outstanding call is completed, none is left to time out" do
      transport = start!(control_mode("exit"))
      task = Task.async(fn -> Transport.request(transport, %{"action" => "defer"}, 30_000) end)

      assert_receive {:compux_session_event, ^transport, %SessionEvent{kind: "request_deferred"}},
                     2_000

      assert {:error, {:sidecar_exited, 9}} = Transport.control(transport, :pause, 5_000)
      assert {:error, {:sidecar_exited, 9}} = Task.await(task, 5_000)

      Transport.stop(transport)
    end
  end

  # The owner learns of an exit from a MESSAGE, not from a reply. An idle exit
  # carries no reply at all, and the capture-stall fail-fast is exactly that: it
  # flushes its response and only THEN exits 75, so a consumer's capture-health
  # breaker is fed by that status and by nothing else.
  describe "the owner is told when the sidecar ends" do
    test "an idle exit notifies once, with the status the sidecar chose" do
      transport = start!()

      assert {:ok, %{"pong" => true}} =
               Transport.request(transport, %{"action" => "flush_exit"}, 2_000)

      assert_receive {:compux_sidecar_exit, ^transport, 75}, 2_000
      refute_receive {:compux_sidecar_exit, ^transport, _}, 300

      Transport.stop(transport)
    end

    test "an exit with a request outstanding both completes it and notifies" do
      transport = start!()

      assert {:error, {:sidecar_exited, 7}} =
               Transport.request(transport, %{"action" => "boom"}, 2_000)

      assert_receive {:compux_sidecar_exit, ^transport, 7}, 2_000
      refute_receive {:compux_sidecar_exit, ^transport, _}, 300

      Transport.stop(transport)
    end

    test "a stop the owner asked for does not notify" do
      transport = start!()
      assert :ok = Transport.stop(transport)
      refute_receive {:compux_sidecar_exit, ^transport, _}, 500
    end

    # A status here would only ever be the signal we sent, so the payload names
    # the reason we sent it instead.
    test "a poison notifies with the reason, not with a signal number" do
      transport = start!()

      assert {:error, {:unknown_request_id, _}} =
               Transport.request(transport, %{"action" => "unknown_id"}, 2_000)

      assert_receive {:compux_sidecar_exit, ^transport,
                      {:poisoned, {:unknown_request_id, "r-nobody-sent-this"}}},
                     2_000

      refute_receive {:compux_sidecar_exit, ^transport, _}, 300

      Transport.stop(transport)
    end

    # Nobody is listening for messages yet: `start_link/1` hands that caller the
    # reason as its return value.
    test "a failure before the handshake completes notifies nobody" do
      assert {:error, {:protocol_mismatch, _}} =
               Transport.start_link(
                 binary_path: @fake,
                 env: [{~c"FAKE_PROTOCOL_VERSION", ~c"1"}]
               )

      refute_receive {:compux_sidecar_exit, _transport, _}, 500
    end

    test "the :owner option sends the news somewhere else" do
      parent = self()
      elsewhere = spawn_link(fn -> relay(parent) end)
      transport = start!(owner: elsewhere)

      assert {:ok, _response} = Transport.request(transport, %{"action" => "flush_exit"}, 2_000)

      assert_receive {:relayed, {:compux_sidecar_exit, ^transport, 75}}, 2_000
      refute_receive {:compux_sidecar_exit, ^transport, _}, 300

      Transport.stop(transport)
    end
  end

  defp relay(parent) do
    receive do
      message -> send(parent, {:relayed, message})
    end
  end

  describe "frames that poison the transport" do
    test "a response whose id is not even our shape" do
      transport = start!()

      assert {:error, {:unknown_request_id, "r-nobody-sent-this"}} =
               Transport.request(transport, %{"action" => "unknown_id"}, 2_000)

      assert {:error, :sidecar_unavailable} =
               Transport.request(transport, %{"action" => "ping"}, 1_000)

      Transport.stop(transport)
    end

    # Our shape, our prefix — and a number this transport has not reached, so it
    # cannot be a request of ours that ended. It never existed.
    test "a response naming an id above anything we have minted" do
      transport = start!()

      assert {:error, {:unknown_request_id, "r9999"}} =
               Transport.request(transport, %{"action" => "future_id"}, 2_000)

      assert {:error, :sidecar_unavailable} =
               Transport.request(transport, %{"action" => "ping"}, 1_000)

      Transport.stop(transport)
    end

    # The capture verbs answer in the `ack` family and the transport must never
    # send one, so an ack arriving here means the wire is not what we think.
    test "an interleaved computer-history ack is refused too" do
      transport = start!()

      assert {:error, {:unexpected_frame, :ack}} =
               Transport.request(transport, %{"action" => "history_ack"}, 2_000)

      Transport.stop(transport)
    end

    test "a malformed line" do
      transport = start!()

      assert {:error, {:malformed_frame, {:invalid_json, _}}} =
               Transport.request(transport, %{"action" => "malformed"}, 2_000)

      Transport.stop(transport)
    end

    # The desync class this wire exists to retire: an unsolicited history frame
    # says `ok`-ish things and must never stand in for an action's reply.
    test "an interleaved computer-history event is refused, never taken for a response" do
      transport = start!()

      assert {:error, {:unexpected_frame, :event}} =
               Transport.request(transport, %{"action" => "history_event"}, 2_000)

      Transport.stop(transport)
    end

    test "a response from another sidecar generation" do
      transport = start!()

      assert {:error, {:stale_generation, _id}} =
               Transport.request(transport, %{"action" => "stale_boot"}, 2_000)

      Transport.stop(transport)
    end

    test "a response from another session generation" do
      transport = start!()

      assert {:error, {:stale_generation, _id}} =
               Transport.request(transport, %{"action" => "stale_session"}, 2_000)

      Transport.stop(transport)
    end

    # Sixteen 64-byte fragments are 1024 bytes, exactly the cap and not over it;
    # only the 40-byte tail crosses the boundary. Counting it is the difference.
    test "a frame over the response cap, counted on the final fragment" do
      transport = start!(line_bytes: 64, max_response_bytes: 1_024)

      assert {:error, :sidecar_response_too_large} =
               Transport.request(transport, %{"action" => "oversize", "bytes" => 1_065}, 2_000)

      Transport.stop(transport)
    end
  end

  describe "a late response" do
    # It arrives after its own request gave up. It is dropped by id, so the
    # request that comes next gets ITS answer and not the stale one.
    test "is dropped and never paired with the request that follows" do
      transport = start!()

      assert {:error, {:timeout, 150}} =
               Transport.request(transport, %{"action" => "late"}, 150)

      assert {:ok, response} = Transport.request(transport, %{"action" => "ping"}, 3_000)
      assert response["pong"] == true
      refute Map.has_key?(response, "late")

      Transport.stop(transport)
    end

    # The rule is the counter, not a list. An earlier version remembered the last
    # 16 abandoned ids, so the 17th timeout evicted the oldest and its eventual
    # reply looked like a frame from nowhere — a healthy transport poisoned by its
    # own past. Twenty requests are left to time out here, well past that bound,
    # and every one of their late replies must still be recognised as ours.
    test "many timeouts do not turn a late reply into a protocol violation" do
      transport = start!()

      for _ <- 1..20 do
        assert {:error, {:timeout, 30}} =
                 Transport.request(transport, %{"action" => "stash"}, 30)
      end

      assert {:ok, flushed} = Transport.request(transport, %{"action" => "flush_stash"}, 3_000)
      assert flushed["flushed"] == true
      refute Map.has_key?(flushed, "stale")

      assert {:ok, response} = Transport.request(transport, %{"action" => "ping"}, 3_000)
      assert response["pong"] == true

      Transport.stop(transport)
    end
  end

  describe "stop/1" do
    test "ends the OS process and is idempotent" do
      transport = start!()
      assert {:ok, os_pid} = Transport.os_pid(transport)

      assert :ok = Transport.stop(transport)
      assert :ok = Transport.stop(transport)
      refute Process.alive?(transport)
      assert os_process_gone_within?(os_pid, 3_000)
    end

    test "ends a sidecar that is blocked in an action and ignoring stdin" do
      transport = start!()
      {:ok, os_pid} = Transport.os_pid(transport)

      assert {:error, {:timeout, 100}} = Transport.request(transport, %{"action" => "hang"}, 100)
      assert :ok = Transport.stop(transport)
      assert os_process_gone_within?(os_pid, 3_000)
    end
  end

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
