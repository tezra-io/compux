defmodule Compux.ProtocolTest do
  use ExUnit.Case, async: true

  alias Compux.Protocol

  describe "protocol_version/0" do
    test "is a positive integer" do
      assert is_integer(Protocol.protocol_version())
      assert Protocol.protocol_version() >= 1
    end
  end

  describe "actions/0 and read_only?/1" do
    test "covers the v1 + V2 verbs" do
      for a <-
            ~w(screenshot left_click right_click double_click mouse_move
               left_click_drag scroll type key wait inspect) do
        assert a in Protocol.actions()
      end
    end

    test "the read-only set is screenshot/mouse_move/wait/inspect" do
      assert Protocol.read_only?("screenshot")
      assert Protocol.read_only?("inspect")
      assert Protocol.read_only?("mouse_move")
      assert Protocol.read_only?("wait")
      refute Protocol.read_only?("left_click")
      refute Protocol.read_only?("type")
    end
  end

  describe "validate/1 — structure" do
    test "rejects a non-map" do
      assert {:error, _} = Protocol.validate("nope")
    end

    test "rejects a missing action" do
      assert {:error, "missing required field: action"} = Protocol.validate(%{})
    end

    test "rejects an unknown action" do
      assert {:error, "unknown action: " <> _} = Protocol.validate(%{"action" => "explode"})
    end
  end

  describe "validate/1 — screenshot + region" do
    test "bare screenshot" do
      assert {:ok, %{"action" => "screenshot"}} = Protocol.validate(%{"action" => "screenshot"})
    end

    test "carries display and region" do
      params = %{
        "action" => "screenshot",
        "display" => 1,
        "region" => %{"x" => 0, "y" => 0, "w" => 100, "h" => 50}
      }

      assert {:ok, req} = Protocol.validate(params)
      assert req["display"] == 1
      assert req["region"] == %{"x" => 0, "y" => 0, "w" => 100, "h" => 50}
    end

    test "rejects a non-positive region dimension" do
      params = %{"action" => "screenshot", "region" => %{"x" => 0, "y" => 0, "w" => 0, "h" => 50}}
      assert {:error, _} = Protocol.validate(params)
    end
  end

  describe "validate/1 — clicks and inspect" do
    for action <- ~w(left_click right_click double_click mouse_move inspect) do
      test "#{action} requires x and y" do
        assert {:error, _} = Protocol.validate(%{"action" => unquote(action)})

        assert {:ok, req} =
                 Protocol.validate(%{"action" => unquote(action), "x" => 10, "y" => 20})

        assert req["x"] == 10 and req["y"] == 20
      end
    end

    test "a click carries modifiers when present" do
      params = %{"action" => "left_click", "x" => 1, "y" => 2, "modifiers" => ["cmd", "shift"]}
      assert {:ok, req} = Protocol.validate(params)
      assert req["modifiers"] == ["cmd", "shift"]
    end

    test "a click rejects an unknown modifier" do
      params = %{"action" => "left_click", "x" => 1, "y" => 2, "modifiers" => ["hyper"]}
      assert {:error, _} = Protocol.validate(params)
    end

    test "negative coordinates are rejected" do
      assert {:error, _} = Protocol.validate(%{"action" => "left_click", "x" => -1, "y" => 2})
    end
  end

  describe "validate/1 — drag/scroll/type/key/wait" do
    test "left_click_drag needs from/to points" do
      assert {:error, _} = Protocol.validate(%{"action" => "left_click_drag"})

      params = %{
        "action" => "left_click_drag",
        "from" => %{"x" => 0, "y" => 0},
        "to" => %{"x" => 5, "y" => 5}
      }

      assert {:ok, req} = Protocol.validate(params)
      assert req["from"] == %{"x" => 0, "y" => 0}
    end

    test "scroll needs a valid direction and a positive amount" do
      bad = %{"action" => "scroll", "x" => 0, "y" => 0, "direction" => "sideways", "amount" => 3}
      assert {:error, _} = Protocol.validate(bad)

      ok = %{"action" => "scroll", "x" => 0, "y" => 0, "direction" => "down", "amount" => 3}
      assert {:ok, req} = Protocol.validate(ok)
      assert req["direction"] == "down" and req["amount"] == 3
    end

    test "type needs a non-empty string" do
      assert {:error, _} = Protocol.validate(%{"action" => "type", "text" => ""})
      assert {:ok, %{"text" => "hi"}} = Protocol.validate(%{"action" => "type", "text" => "hi"})
    end

    test "key needs a non-empty chord" do
      assert {:error, _} = Protocol.validate(%{"action" => "key", "chord" => ""})

      assert {:ok, %{"chord" => "ctrl+s"}} =
               Protocol.validate(%{"action" => "key", "chord" => "ctrl+s"})
    end

    test "wait needs a positive ms" do
      assert {:error, _} = Protocol.validate(%{"action" => "wait", "ms" => 0})
      assert {:ok, %{"ms" => 250}} = Protocol.validate(%{"action" => "wait", "ms" => 250})
    end
  end

  describe "encode_request/1 and decode_response/1" do
    test "encode appends a newline and stays valid JSON" do
      line = Protocol.encode_request(%{"action" => "screenshot"})
      assert String.ends_with?(line, "\n")
      assert {:ok, %{"action" => "screenshot"}} = Jason.decode(String.trim(line))
    end

    test "decodes an ok response" do
      assert {:ok, %{"ok" => true, "data" => "x"}} =
               Protocol.decode_response(~s({"ok":true,"data":"x"}))
    end

    test "decodes a failure response to its error reason" do
      assert {:error, "no_active_display"} =
               Protocol.decode_response(~s({"ok":false,"error":"no_active_display"}))
    end

    test "a malformed shape fails loud" do
      assert {:error, "malformed sidecar response: " <> _} =
               Protocol.decode_response(~s({"weird":1}))
    end

    test "invalid JSON fails loud" do
      assert {:error, "invalid JSON from sidecar: " <> _} =
               Protocol.decode_response("{not json")
    end
  end
end
