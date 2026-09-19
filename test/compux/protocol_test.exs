defmodule Compux.ProtocolTest do
  use ExUnit.Case, async: true

  alias Compux.Protocol

  # A valid request for one of the actions that address a point in a named image,
  # so a test can remove exactly the field it is about.
  defp addressed_params(action) do
    base = %{"action" => action, "observation_id" => "7c1e-12"}

    case action do
      "left_click_drag" ->
        Map.merge(base, %{"from" => %{"x" => 1, "y" => 2}, "to" => %{"x" => 3, "y" => 4}})

      "scroll" ->
        Map.merge(base, %{"x" => 1, "y" => 2, "direction" => "down", "amount" => 3})

      _pointer ->
        Map.merge(base, %{"x" => 1, "y" => 2})
    end
  end

  # A valid request addressed at a CONTROL rather than a point, so a test can
  # remove or contradict exactly the field it is about.
  defp element_params(action, extra \\ %{}) do
    Map.merge(%{"action" => action, "observation_id" => "7c1e-12", "element_ref" => "e3"}, extra)
  end

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

    test "the read-only set includes screenshot/mouse_move/wait/inspect/wait_for_change/elements" do
      assert Protocol.read_only?("screenshot")
      assert Protocol.read_only?("inspect")
      assert Protocol.read_only?("mouse_move")
      assert Protocol.read_only?("wait")
      assert Protocol.read_only?("wait_for_change")
      assert Protocol.read_only?("elements")
      refute Protocol.read_only?("left_click")
      refute Protocol.read_only?("type")
      refute Protocol.read_only?("paste")
    end

    # The same list decides whether a request carries a `mutation_seq` and earns a
    # receipt, so the operational verbs have to be in it: a permission probe is
    # not a mutation, and sequencing one would put a receipt on it.
    test "the operational verbs are read-only too, though they are not model actions" do
      for action <- ~w(probe idle_ms wait_for_idle hello) do
        assert Protocol.read_only?(action), "#{action} dispatches no input"
        refute action in Protocol.actions(), "#{action} is not a model verb"
      end
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

    # v5 grounding-integrity fields: carried only when meaningfully set, so the
    # wire stays minimal and a request without them is byte-identical to v4's.
    test "carries rulers/marks only when literally true" do
      assert {:ok, request} =
               Protocol.validate(%{"action" => "screenshot", "rulers" => true, "marks" => true})

      assert request["rulers"] == true
      assert request["marks"] == true

      assert {:ok, request} =
               Protocol.validate(%{"action" => "screenshot", "rulers" => false, "marks" => false})

      refute Map.has_key?(request, "rulers")
      refute Map.has_key?(request, "marks")
    end

    test "rejects non-boolean rulers/marks" do
      assert {:error, _} = Protocol.validate(%{"action" => "screenshot", "rulers" => "yes"})
      assert {:error, _} = Protocol.validate(%{"action" => "screenshot", "marks" => 1})
    end

    test "carries a valid annotate_point and rejects malformed ones" do
      assert {:ok, %{"annotate_point" => %{"x" => 12, "y" => 34}}} =
               Protocol.validate(%{
                 "action" => "screenshot",
                 "annotate_point" => %{"x" => 12, "y" => 34}
               })

      assert {:error, _} =
               Protocol.validate(%{"action" => "screenshot", "annotate_point" => %{"x" => 12}})

      assert {:error, _} =
               Protocol.validate(%{
                 "action" => "screenshot",
                 "annotate_point" => %{"x" => -1, "y" => 2}
               })
    end
  end

  describe "validate/1 — clicks and inspect" do
    for action <- ~w(left_click right_click double_click mouse_move inspect) do
      test "#{action} requires x and y" do
        assert {:error, _} =
                 Protocol.validate(%{"action" => unquote(action), "observation_id" => "7c1e-1"})

        assert {:ok, req} =
                 Protocol.validate(%{
                   "action" => unquote(action),
                   "x" => 10,
                   "y" => 20,
                   "observation_id" => "7c1e-1"
                 })

        assert req["x"] == 10 and req["y"] == 20
      end
    end

    test "a click carries modifiers when present" do
      params = %{
        "action" => "left_click",
        "x" => 1,
        "y" => 2,
        "modifiers" => ["cmd", "shift"],
        "observation_id" => "7c1e-1"
      }

      assert {:ok, req} = Protocol.validate(params)
      assert req["modifiers"] == ["cmd", "shift"]
    end

    test "a click rejects an unknown modifier" do
      params = %{
        "action" => "left_click",
        "x" => 1,
        "y" => 2,
        "modifiers" => ["hyper"],
        "observation_id" => "7c1e-1"
      }

      assert {:error, _} = Protocol.validate(params)
    end

    test "negative coordinates are rejected" do
      assert {:error, _} =
               Protocol.validate(%{
                 "action" => "left_click",
                 "x" => -1,
                 "y" => 2,
                 "observation_id" => "7c1e-1"
               })
    end
  end

  # v8: a coordinate names the image it was read from. An action that addresses a
  # point carries `observation_id` and no `region`; an action that PRODUCES an
  # image takes both, and `region` then means "in the image that id names".
  describe "validate/1 — addressing (v8)" do
    for action <-
          ~w(left_click right_click double_click mouse_move inspect left_click_drag scroll) do
      test "#{action} carries the observation it was read from" do
        assert {:ok, req} = Protocol.validate(addressed_params(unquote(action)))
        assert req["observation_id"] == "7c1e-12"
      end

      test "#{action} without an observation_id is refused" do
        params = Map.delete(addressed_params(unquote(action)), "observation_id")
        assert {:error, reason} = Protocol.validate(params)
        assert reason =~ "observation_id"
      end

      test "#{action} refuses a region — the image is named, not described" do
        params =
          Map.put(
            addressed_params(unquote(action)),
            "region",
            %{"x" => 0, "y" => 0, "w" => 10, "h" => 10}
          )

        assert {:error, reason} = Protocol.validate(params)
        assert reason =~ "region"
      end

      test "#{action} refuses an observation_id that is not a non-empty string" do
        for bad <- ["", 7, nil] do
          params = Map.put(addressed_params(unquote(action)), "observation_id", bad)
          assert {:error, _} = Protocol.validate(params)
        end
      end
    end

    for action <- ~w(screenshot elements wait_for_change) do
      test "#{action} takes an observation_id beside its region" do
        params = %{
          "action" => unquote(action),
          "observation_id" => "7c1e-12",
          "region" => %{"x" => 0, "y" => 0, "w" => 10, "h" => 10}
        }

        assert {:ok, req} = Protocol.validate(params)
        assert req["observation_id"] == "7c1e-12"
        assert req["region"]["w"] == 10
      end

      test "#{action} still works with neither" do
        assert {:ok, req} = Protocol.validate(%{"action" => unquote(action)})
        refute Map.has_key?(req, "observation_id")
      end
    end

    test "windows names no observation — it is what produces them" do
      assert {:error, _} =
               Protocol.validate(%{"action" => "windows", "observation_id" => "7c1e-12"})
    end
  end

  # v9: a control has a name, not only a place. `press` and `set_value` address
  # one and never a point; a pointer action may address either, and both at once
  # is a contradiction nothing may resolve by guessing.
  describe "validate/1 — element references (v9)" do
    test "press and set_value are model actions that are not read-only" do
      for action <- ~w(press set_value) do
        assert action in Protocol.actions()
        refute Protocol.read_only?(action), "#{action} acts on the machine"
      end
    end

    test "press carries the observation and the reference" do
      assert {:ok, req} = Protocol.validate(element_params("press"))
      assert req == %{"action" => "press", "element_ref" => "e3", "observation_id" => "7c1e-12"}
    end

    test "set_value carries its value, empty string included" do
      for value <- ["hello", ""] do
        assert {:ok, req} = Protocol.validate(element_params("set_value", %{"value" => value}))
        assert req["value"] == value
        assert req["element_ref"] == "e3"
      end
    end

    test "set_value without a string value is refused" do
      for bad <- [nil, 7, %{}] do
        params = element_params("set_value", %{"value" => bad})
        assert {:error, reason} = Protocol.validate(params)
        assert reason =~ "value"
      end
    end

    test "set_value refuses a value past the type bound" do
      params = element_params("set_value", %{"value" => String.duplicate("x", 10_001)})
      assert {:error, reason} = Protocol.validate(params)
      assert reason =~ "bytes"
    end

    for action <- ~w(press set_value) do
      test "#{action} without an element_ref is refused by name" do
        params = Map.delete(element_params(unquote(action), %{"value" => "v"}), "element_ref")
        assert {:error, reason} = Protocol.validate(params)
        assert reason =~ "element_ref"
      end

      test "#{action} without an observation_id is refused" do
        params = Map.delete(element_params(unquote(action), %{"value" => "v"}), "observation_id")
        assert {:error, reason} = Protocol.validate(params)
        assert reason =~ "observation_id"
      end

      test "#{action} refuses a point beside its reference" do
        params = element_params(unquote(action), %{"value" => "v", "x" => 1, "y" => 2})
        assert {:error, reason} = Protocol.validate(params)
        assert reason =~ "never by both"
      end

      test "#{action} refuses an element_ref that is not a non-empty string" do
        for bad <- ["", 7, nil] do
          params = element_params(unquote(action), %{"value" => "v", "element_ref" => bad})
          assert {:error, _} = Protocol.validate(params)
        end
      end
    end

    for action <- ~w(left_click right_click double_click mouse_move scroll) do
      test "#{action} may be addressed by a control instead of a point" do
        extra =
          if unquote(action) == "scroll", do: %{"direction" => "down", "amount" => 2}, else: %{}

        assert {:ok, req} = Protocol.validate(element_params(unquote(action), extra))
        assert req["element_ref"] == "e3"
        refute Map.has_key?(req, "x")
        refute Map.has_key?(req, "y")
      end

      test "#{action} refuses a point and a control on one request" do
        extra =
          if unquote(action) == "scroll", do: %{"direction" => "down", "amount" => 2}, else: %{}

        params = element_params(unquote(action), Map.merge(extra, %{"x" => 1, "y" => 2}))

        assert {:error, reason} = Protocol.validate(params)
        assert reason =~ "never by both"
      end
    end

    # A drag names two points and `inspect` reports what is under one, so neither
    # has a meaning for a reference — refused rather than quietly ignored.
    for action <- ~w(left_click_drag inspect) do
      test "#{action} takes no element_ref" do
        params = Map.put(addressed_params(unquote(action)), "element_ref", "e3")
        assert {:error, reason} = Protocol.validate(params)
        assert reason =~ "element_ref"
      end
    end

    for action <- ~w(screenshot elements wait_for_change windows type paste key wait) do
      test "#{action} takes no element_ref either" do
        params = %{"action" => unquote(action), "element_ref" => "e3"}
        assert {:error, reason} = Protocol.validate(params)
        assert reason =~ "element_ref"
      end
    end
  end

  describe "validate/1 — drag/scroll/type/key/wait" do
    test "left_click_drag needs from/to points" do
      assert {:error, _} =
               Protocol.validate(%{"action" => "left_click_drag", "observation_id" => "7c1e-1"})

      params = %{
        "action" => "left_click_drag",
        "from" => %{"x" => 0, "y" => 0},
        "to" => %{"x" => 5, "y" => 5},
        "observation_id" => "7c1e-1"
      }

      assert {:ok, req} = Protocol.validate(params)
      assert req["from"] == %{"x" => 0, "y" => 0}
    end

    test "scroll needs a valid direction and a positive amount" do
      bad = %{
        "action" => "scroll",
        "x" => 0,
        "y" => 0,
        "direction" => "sideways",
        "amount" => 3,
        "observation_id" => "7c1e-1"
      }

      assert {:error, _} = Protocol.validate(bad)

      ok = %{
        "action" => "scroll",
        "x" => 0,
        "y" => 0,
        "direction" => "down",
        "amount" => 3,
        "observation_id" => "7c1e-1"
      }

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

  # One wire format means one encoder and one decoder, and neither is here any
  # more. `encode_request/1` wrote the UNTAGGED protocol-6 line a protocol-7
  # sidecar refuses; `decode_response/1` called any `ok: true` map a response, so
  # an `ack` or an `event` could stand in for an action's reply. Both live in
  # `Compux.Frame` now, which is what every caller uses.
  describe "the protocol neither encodes nor decodes a frame" do
    test "encode_request/1 is gone" do
      refute function_exported?(Protocol, :encode_request, 1)
    end

    test "decode_response/1 is gone" do
      refute function_exported?(Protocol, :decode_response, 1)
    end
  end

  describe "validate/1 — v2 actions" do
    test "wait_for_change: bare, and with region + bounded timeout/poll" do
      assert {:ok, %{"action" => "wait_for_change"}} =
               Protocol.validate(%{"action" => "wait_for_change"})

      params = %{
        "action" => "wait_for_change",
        "region" => %{"x" => 0, "y" => 0, "w" => 10, "h" => 10},
        "timeout_ms" => 5000,
        "poll_ms" => 200
      }

      assert {:ok, req} = Protocol.validate(params)
      assert req["timeout_ms"] == 5000
      assert req["poll_ms"] == 200
      assert req["region"]["w"] == 10
    end

    test "wait_for_change rejects an out-of-range timeout or poll" do
      assert {:error, _} =
               Protocol.validate(%{"action" => "wait_for_change", "timeout_ms" => 999_999})

      assert {:error, _} = Protocol.validate(%{"action" => "wait_for_change", "poll_ms" => 1})
    end

    test "elements: bare and with region" do
      assert {:ok, %{"action" => "elements"}} = Protocol.validate(%{"action" => "elements"})

      assert {:ok, %{"region" => %{"w" => 50}}} =
               Protocol.validate(%{
                 "action" => "elements",
                 "region" => %{"x" => 0, "y" => 0, "w" => 50, "h" => 50}
               })
    end

    test "paste needs a non-empty string" do
      assert {:error, _} = Protocol.validate(%{"action" => "paste", "text" => ""})

      assert {:ok, %{"action" => "paste", "text" => "hi"}} =
               Protocol.validate(%{"action" => "paste", "text" => "hi"})
    end

    test "windows: bare defaults to the sidecar's display, or names one" do
      assert {:ok, request} = Protocol.validate(%{"action" => "windows"})
      assert request == %{"action" => "windows"}

      assert {:ok, %{"action" => "windows", "display" => 2}} =
               Protocol.validate(%{"action" => "windows", "display" => 2})
    end

    test "windows takes no region — it is what PRODUCES regions" do
      assert {:ok, request} =
               Protocol.validate(%{
                 "action" => "windows",
                 "region" => %{"x" => 0, "y" => 0, "w" => 50, "h" => 50}
               })

      refute Map.has_key?(request, "region")
    end

    test "windows is read-only: it captures nothing and moves nothing" do
      assert Protocol.read_only?("windows")
    end

    test "screenshot: jpeg_quality is optional, bounded, and defaults to PNG" do
      assert {:ok, request} = Protocol.validate(%{"action" => "screenshot"})
      refute Map.has_key?(request, "jpeg_quality"), "PNG stays the default"

      assert {:ok, %{"jpeg_quality" => 60}} =
               Protocol.validate(%{"action" => "screenshot", "jpeg_quality" => 60})

      for bad <- [0, 101, "high", 60.5] do
        assert {:error, _} =
                 Protocol.validate(%{"action" => "screenshot", "jpeg_quality" => bad}),
               "jpeg_quality #{inspect(bad)} must be refused"
      end
    end
  end
end
