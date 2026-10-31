defmodule Compux.FrameTest do
  use ExUnit.Case, async: true

  alias Compux.Frame
  alias Compux.Frame.{Control, ControlAck, History, Request, Response, SessionEvent}

  defp decoded(json) do
    assert {:ok, frame} = Frame.decode(json)
    frame
  end

  defp encoded(frame) do
    assert {:ok, line} = Frame.encode(frame)
    assert String.ends_with?(line, "\n")
    assert {:ok, map} = Jason.decode(line)
    map
  end

  describe "encode/1 — request" do
    test "carries the envelope beside the action's own arguments" do
      map =
        encoded(%Request{
          request_id: "r17",
          args: %{"action" => "left_click", "x" => 1, "y" => 2},
          deadline_ms: 30_000,
          sidecar_generation: "boot-a",
          session_generation: 1,
          authorization_generation: 4,
          mutation_seq: 9
        })

      assert map == %{
               "type" => "request",
               "request_id" => "r17",
               "action" => "left_click",
               "x" => 1,
               "y" => 2,
               "deadline_ms" => 30_000,
               "sidecar_generation" => "boot-a",
               "session_generation" => 1,
               "authorization_generation" => 4,
               "mutation_seq" => 9
             }
    end

    test "hello carries the protocol version and no generations" do
      map =
        encoded(%Request{
          request_id: "r1",
          args: %{"action" => "hello"},
          deadline_ms: 10_000,
          protocol_version: 9
        })

      assert map["protocol_version"] == 9
      refute Map.has_key?(map, "sidecar_generation")
      refute Map.has_key?(map, "session_generation")
      refute Map.has_key?(map, "authorization_generation")
      refute Map.has_key?(map, "mutation_seq")
    end

    # The budget is `deadline_ms` because `wait_for_change` and `wait_for_idle`
    # own `timeout_ms` as an ARGUMENT. One key cannot carry both meanings, and an
    # action that names one must reach the sidecar untouched.
    test "an action's own timeout_ms survives beside the caller's budget" do
      map =
        encoded(%Request{
          request_id: "r2",
          args: %{"action" => "wait_for_change", "timeout_ms" => 25_000},
          deadline_ms: 30_000
        })

      assert map["timeout_ms"] == 25_000
      assert map["deadline_ms"] == 30_000
    end

    test "an argument that would shadow an envelope field is refused, not merged over" do
      for key <-
            ~w(request_id type sidecar_generation session_generation mutation_seq deadline_ms) do
        args = %{"action" => "left_click", key => "x"}

        assert {:error, {:reserved_field, ^key}} =
                 Frame.encode(%Request{request_id: "r1", args: args})
      end
    end

    test "args without an action fail loud" do
      assert {:error, {:invalid_field, "action", nil}} =
               Frame.encode(%Request{request_id: "r1", args: %{"x" => 1}})
    end

    test "an unencodable argument is reported, not raised" do
      assert {:error, {:unencodable_frame, _message}} =
               Frame.encode(%Request{
                 request_id: "r1",
                 args: %{"action" => "type", "t" => self()}
               })
    end
  end

  describe "encode/1 — control" do
    test "carries the action and all three generations" do
      map =
        encoded(%Control{
          request_id: "c2",
          action: :resume,
          sidecar_generation: "boot-a",
          session_generation: 1,
          authorization_generation: 5
        })

      assert map == %{
               "type" => "control",
               "request_id" => "c2",
               "action" => "resume",
               "sidecar_generation" => "boot-a",
               "session_generation" => 1,
               "authorization_generation" => 5
             }
    end
  end

  describe "decode/1 — response" do
    test "a success keeps the action's payload and drops the envelope" do
      frame =
        decoded(
          ~s({"type":"response","request_id":"r17","sidecar_generation":"boot-a",) <>
            ~s("session_generation":1,"ok":true,"image":"AAA","width":100})
        )

      assert %Response{request_id: "r17", ok: true, sidecar_generation: "boot-a"} = frame
      assert frame.payload == %{"ok" => true, "image" => "AAA", "width" => 100}
    end

    test "a failure carries its code, detail and receipt" do
      frame =
        decoded(
          ~s({"type":"response","request_id":"r17","sidecar_generation":"boot-a",) <>
            ~s("session_generation":1,"ok":false,"error":"cancelled","detail":"pause",) <>
            ~s("receipt":{"dispatch":"partial","effect":"unknown"}})
        )

      assert frame.ok == false
      assert frame.error == "cancelled"
      assert frame.detail == "pause"
      assert frame.receipt == %{"dispatch" => "partial", "effect" => "unknown"}
      assert frame.payload["receipt"]["dispatch"] == "partial"
    end

    test "a failure with no error code is malformed" do
      assert {:error, {:invalid_field, "error", nil}} =
               Frame.decode(~s({"type":"response","request_id":"r1","ok":false}))
    end

    test "a response with no request id is malformed" do
      assert {:error, {:invalid_request_id, nil}} =
               Frame.decode(~s({"type":"response","ok":true}))
    end

    test "a request id longer than 64 characters is refused" do
      long = String.duplicate("r", 65)

      assert {:error, {:invalid_request_id, ^long}} =
               Frame.decode(~s({"type":"response","request_id":"#{long}","ok":true}))
    end

    test "ok must be a boolean, not a truthy value" do
      assert {:error, {:invalid_field, "ok", 1}} =
               Frame.decode(~s({"type":"response","request_id":"r1","ok":1}))
    end

    # `protocol_version` is the hello answer, not envelope noise, so it survives.
    test "a hello response keeps its protocol version and capabilities" do
      frame =
        decoded(
          ~s({"type":"response","request_id":"r1","ok":true,"protocol_version":9,) <>
            ~s("sidecar_generation":"boot-a","compux_version":"0.9.2",) <>
            ~s("capabilities":{"controls":["pause"]}})
        )

      assert frame.payload["protocol_version"] == 9
      assert frame.payload["capabilities"] == %{"controls" => ["pause"]}
      refute Map.has_key?(frame.payload, "sidecar_generation")
      assert frame.sidecar_generation == "boot-a"
    end
  end

  describe "decode/1 — control_ack" do
    test "names the action, the new authority and what is still running" do
      frame =
        decoded(
          ~s({"type":"control_ack","request_id":"c2","sidecar_generation":"boot-a",) <>
            ~s("session_generation":1,"action":"pause","ok":true,) <>
            ~s("authorization_generation":5,"in_flight_request_id":"r17"})
        )

      assert %ControlAck{action: :pause, ok: true, authorization_generation: 5} = frame
      assert frame.in_flight_request_id == "r17"
    end

    test "a null in_flight_request_id means nothing was running" do
      frame =
        decoded(
          ~s({"type":"control_ack","request_id":"c2","action":"release","ok":true,) <>
            ~s("authorization_generation":6,"in_flight_request_id":null})
        )

      assert frame.in_flight_request_id == nil
    end

    test "an unknown control verb is refused" do
      assert {:error, {:unknown_control_action, "halt"}} =
               Frame.decode(
                 ~s({"type":"control_ack","request_id":"c2","action":"halt","ok":true,) <>
                   ~s("authorization_generation":1})
               )
    end

    test "an acknowledgement with no authority generation is malformed" do
      assert {:error, {:invalid_field, "authorization_generation", nil}} =
               Frame.decode(
                 ~s({"type":"control_ack","request_id":"c2","action":"pause","ok":true})
               )
    end
  end

  describe "decode/1 — session_event and the history families" do
    test "a session event keeps its kind, sequence and payload" do
      frame =
        decoded(
          ~s({"type":"session_event","sidecar_generation":"boot-a","session_generation":1,) <>
            ~s("event_seq":3,"kind":"target_unavailable","reason":"window_closed"})
        )

      assert %SessionEvent{kind: "target_unavailable", event_seq: 3} = frame
      assert frame.payload["reason"] == "window_closed"
    end

    # An `ack` and an `event` both say `ok`-ish things and neither answers an
    # action. They decode as their own family precisely so a consumer can refuse
    # them by family rather than have one stand in for a reply.
    test "computer-history ack and event decode as history, never as a response" do
      assert %History{type: "ack"} =
               decoded(~s({"type":"ack","action":"observe_start","ok":true,"protocol_version":9}))

      assert %History{type: "event"} =
               decoded(~s({"type":"event","v":1,"ts":1,"seq":4,"kind":"observer.gap"}))
    end
  end

  describe "decode/1 — refusals" do
    test "an unknown tag is refused" do
      assert {:error, {:unknown_frame_type, "telemetry"}} =
               Frame.decode(~s({"type":"telemetry","ok":true}))
    end

    test "a frame with no tag is refused, however successful it looks" do
      assert {:error, :missing_frame_type} = Frame.decode(~s({"ok":true,"pong":true}))
    end

    test "invalid JSON is refused" do
      assert {:error, {:invalid_json, _message}} = Frame.decode("{not json")
    end

    test "a JSON value that is not an object is refused" do
      assert {:error, {:malformed_frame, [1, 2]}} = Frame.decode("[1,2]")
    end
  end

  describe "round trip" do
    test "an outbound request decodes back to its arguments" do
      frame = %Request{
        request_id: "r5",
        args: %{"action" => "scroll", "x" => 1, "y" => 2, "direction" => "down", "amount" => 3},
        deadline_ms: 1_000,
        sidecar_generation: "boot-a",
        session_generation: 1,
        authorization_generation: 1,
        mutation_seq: 2
      }

      assert {:ok, line} = Frame.encode(frame)
      assert %Request{} = back = decoded(line)
      assert back.args == frame.args
      assert back.mutation_seq == 2
      assert back.deadline_ms == 1_000
    end

    test "an outbound control decodes back to its verb" do
      assert {:ok, line} = Frame.encode(%Control{request_id: "c1", action: :pause})
      assert %Control{request_id: "c1", action: :pause} = decoded(line)
    end
  end

  # The computer-history consumer owns a raw Port, never sends `hello`, and so has
  # no generations to quote — but it speaks the one wire format. The inbound
  # frames below are the shapes the sidecar emits today: `observe_ack/3` in
  # `native/compux/src/main.rs`, `event_frame/5` in `native/compux/src/capture.rs`.
  describe "the computer-history connection" do
    test "observe_start encodes as a typed request with no generations" do
      map =
        encoded(%Request{
          request_id: "o1",
          args: %{"action" => "observe_start", "params" => %{"apps" => ["com.apple.Safari"]}}
        })

      assert map == %{
               "type" => "request",
               "request_id" => "o1",
               "action" => "observe_start",
               "params" => %{"apps" => ["com.apple.Safari"]}
             }
    end

    test "observe_stop encodes the same way, and needs no budget either" do
      map = encoded(%Request{request_id: "o2", args: %{"action" => "observe_stop"}})

      assert map == %{"type" => "request", "request_id" => "o2", "action" => "observe_stop"}
    end

    test "the ack decodes as history, verbatim, envelope included" do
      frame = decoded(~s({"type":"ack","action":"observe_start","ok":true,"protocol_version":9}))

      assert %History{type: "ack"} = frame

      assert frame.payload == %{
               "type" => "ack",
               "action" => "observe_start",
               "ok" => true,
               "protocol_version" => 9
             }
    end

    # A refused observe_start is still an `ack`, never the generic
    # `{ok:false,error}` response — which is why the consumer reads it by family.
    test "a refused ack keeps its family and carries the reason" do
      frame =
        decoded(
          ~s({"type":"ack","action":"observe_start","ok":false,"protocol_version":9,) <>
            ~s("error":"screen recording permission is required"})
        )

      assert %History{type: "ack"} = frame
      assert frame.payload["ok"] == false
      assert frame.payload["error"] == "screen recording permission is required"
    end

    test "an app-scoped event decodes with its envelope and app identity intact" do
      frame =
        decoded(
          ~s({"type":"event","v":1,"ts":1789171200000,"seq":4,"boot_id":"history-a",) <>
            ~s("kind":"field.value","app":{"bundle_id":"com.apple.Safari","name":"Safari",) <>
            ~s("pid":501},"role":"AXTextField","text":"hello"})
        )

      assert %History{type: "event"} = frame
      assert frame.payload["v"] == 1
      assert frame.payload["ts"] == 1_789_171_200_000
      assert frame.payload["seq"] == 4
      assert frame.payload["boot_id"] == "history-a"
      assert frame.payload["kind"] == "field.value"
      assert frame.payload["app"]["bundle_id"] == "com.apple.Safari"
      assert frame.payload["role"] == "AXTextField"
      assert frame.payload["type"] == "event", "the envelope survives byte for byte"
    end

    test "an observer.gap event decodes unchanged" do
      frame =
        decoded(
          ~s({"type":"event","v":1,"ts":1789171200000,"seq":4,"boot_id":"history-a",) <>
            ~s("kind":"observer.gap","gap_reason":"secure_input",) <>
            ~s("gap_from_ts":1789171199000,"gap_to_ts":1789171200000})
        )

      assert %History{type: "event"} = frame
      assert frame.payload["gap_reason"] == "secure_input"
      assert frame.payload["gap_from_ts"] == 1_789_171_199_000
    end
  end

  test "kind/1 names the family that arrived" do
    assert Frame.kind(%Response{request_id: "r1", ok: true, payload: %{}}) == :response
    assert Frame.kind(%History{type: "event", payload: %{}}) == :event
    assert Frame.kind(%Request{request_id: "r1", args: %{}}) == :request
  end

  test "control_actions/0 is the wire's verb list" do
    assert Frame.control_actions() == [:pause, :resume, :release]
  end
end
