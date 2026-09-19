defmodule CompuxTest do
  use ExUnit.Case, async: true

  alias Compux.StubDriver

  defp start!(extra \\ []) do
    {:ok, cu} = Compux.start([driver: StubDriver, binary_path: "unused", log: self()] ++ extra)
    cu
  end

  describe "start/1 handshake" do
    test "returns a handle carrying the sidecar identity" do
      cu = start!()
      info = Compux.info(cu)
      assert %Compux{} = cu
      assert info.protocol_version == Compux.protocol_version()
      assert info.compux_version == "0.0.0-stub"
      assert "screenshot" in info.actions
      assert_received {:executed, %{"action" => "hello"}}
    end

    test "refuses a protocol-version mismatch and stops the driver" do
      assert {:error, {:protocol_mismatch, %{library: lib, sidecar: 999}}} =
               Compux.start(
                 driver: StubDriver,
                 binary_path: "unused",
                 log: self(),
                 protocol_version: 999
               )

      assert lib == Compux.protocol_version()
      assert_received :stopped
    end

    test "propagates a handshake error" do
      assert {:error, :no_hello} =
               Compux.start(driver: StubDriver, binary_path: "unused", fail_hello: true)
    end
  end

  describe "typed actions build the right protocol request" do
    setup do
      {:ok, cu: start!()}
    end

    test "screenshot carries region + display", %{cu: cu} do
      assert {:ok, %{"ok" => true}} = Compux.screenshot(cu, display: 1, region: {0, 0, 100, 50})

      assert_received {:executed,
                       %{
                         "action" => "screenshot",
                         "display" => 1,
                         "region" => %{"x" => 0, "y" => 0, "w" => 100, "h" => 50}
                       }}
    end

    test "click button variants + modifiers + screenshot_after", %{cu: cu} do
      Compux.click(cu, {10, 20},
        button: :right,
        modifiers: [:cmd, :shift],
        screenshot_after: true,
        observation_id: "7c1e-12"
      )

      assert_received {:executed,
                       %{
                         "action" => "right_click",
                         "x" => 10,
                         "y" => 20,
                         "modifiers" => ["cmd", "shift"],
                         "screenshot_after" => true,
                         "observation_id" => "7c1e-12"
                       }}

      Compux.click(cu, {1, 2}, button: :double, observation_id: "7c1e-12")
      assert_received {:executed, %{"action" => "double_click"}}

      Compux.click(cu, {1, 2}, observation_id: "7c1e-12")
      assert_received {:executed, %{"action" => "left_click"}}
    end

    # Every coordinate names the image it was read in. The facade will not build a
    # request without one, so a caller cannot send a click whose space is unknown.
    test "an action that addresses a point needs the image it was read in", %{cu: cu} do
      assert {:error, reason} = Compux.click(cu, {10, 20})
      assert reason =~ "observation_id"
      refute_received {:executed, %{"action" => "left_click"}}

      assert {:error, _} = Compux.drag(cu, {0, 0}, {9, 9})
      assert {:error, _} = Compux.scroll(cu, {5, 5}, :down, 3)
      assert {:error, _} = Compux.move(cu, {3, 4})
      assert {:error, _} = Compux.inspect(cu, {7, 8})
    end

    test "scroll / drag / type / key / wait / move / inspect", %{cu: cu} do
      Compux.scroll(cu, {5, 5}, :down, 3, observation_id: "7c1e-12")
      assert_received {:executed, %{"action" => "scroll", "direction" => "down", "amount" => 3}}

      Compux.drag(cu, {0, 0}, {9, 9}, observation_id: "7c1e-12")

      assert_received {:executed,
                       %{
                         "action" => "left_click_drag",
                         "from" => %{"x" => 0},
                         "to" => %{"x" => 9}
                       }}

      Compux.type(cu, "hi")
      assert_received {:executed, %{"action" => "type", "text" => "hi"}}

      Compux.key(cu, "ctrl+s")
      assert_received {:executed, %{"action" => "key", "chord" => "ctrl+s"}}

      Compux.wait(cu, 100)
      assert_received {:executed, %{"action" => "wait", "ms" => 100}}

      Compux.move(cu, {3, 4}, observation_id: "7c1e-12")
      assert_received {:executed, %{"action" => "mouse_move", "x" => 3, "y" => 4}}

      Compux.inspect(cu, {7, 8}, observation_id: "7c1e-12")
      assert_received {:executed, %{"action" => "inspect", "x" => 7, "y" => 8}}
    end

    test "wait_for_change / elements / paste build the right request", %{cu: cu} do
      Compux.wait_for_change(cu, region: {0, 0, 10, 10}, timeout_ms: 3000, poll_ms: 100)

      assert_received {:executed,
                       %{
                         "action" => "wait_for_change",
                         "timeout_ms" => 3000,
                         "poll_ms" => 100,
                         "region" => %{"w" => 10}
                       }}

      Compux.elements(cu)
      assert_received {:executed, %{"action" => "elements"}}

      Compux.paste(cu, "long text")
      assert_received {:executed, %{"action" => "paste", "text" => "long text"}}
    end

    test "invalid params fail loud before hitting the driver", %{cu: cu} do
      assert {:error, _reason} = Compux.click(cu, {-1, 2}, observation_id: "7c1e-12")
      refute_received {:executed, %{"action" => "left_click"}}
    end
  end

  describe "probe/1" do
    test "normalizes the sidecar probe response" do
      cu = start!()

      assert {:ok, %{platform: "test", screen_capture: true, input_control: false}} =
               Compux.probe(cu)
    end
  end

  describe "idle_ms/1 (operational)" do
    test "returns the reported millisecond count" do
      cu = start!(responses: %{"idle_ms" => {:ok, %{"ok" => true, "idle_ms" => 1234}}})
      assert {:ok, 1234} = Compux.idle_ms(cu)
      assert_received {:executed, %{"action" => "idle_ms"}}
    end

    test "fails loud on a malformed response" do
      cu = start!(responses: %{"idle_ms" => {:ok, %{"ok" => true}}})
      assert {:error, {:malformed_idle_response, _}} = Compux.idle_ms(cu)
    end

    test "propagates a driver error" do
      cu =
        start!(responses: %{"idle_ms" => {:error, "idle detection is only supported on macOS"}})

      assert {:error, "idle detection is only supported on macOS"} = Compux.idle_ms(cu)
    end
  end

  describe "wait_for_idle/2 (operational)" do
    test "builds the request with the given bounds and returns the result" do
      cu =
        start!(
          responses: %{
            "wait_for_idle" => {:ok, %{"ok" => true, "idle" => true, "idle_ms" => 1500}}
          }
        )

      assert {:ok, %{"idle" => true, "idle_ms" => 1500}} =
               Compux.wait_for_idle(cu, idle_ms: 1000, timeout_ms: 3000, poll_ms: 100)

      assert_received {:executed,
                       %{
                         "action" => "wait_for_idle",
                         "idle_ms" => 1000,
                         "timeout_ms" => 3000,
                         "poll_ms" => 100
                       }}
    end

    test "omits absent bounds (sidecar fills defaults)" do
      cu = start!(responses: %{"wait_for_idle" => {:ok, %{"ok" => true, "idle" => false}}})
      assert {:ok, %{"idle" => false}} = Compux.wait_for_idle(cu)
      assert_received {:executed, %{"action" => "wait_for_idle"} = request}
      assert request == %{"action" => "wait_for_idle"}
    end
  end

  describe "stop/1" do
    test "delegates to the driver" do
      cu = start!()
      assert :ok = Compux.stop(cu)
    end
  end

  # The facade over the REAL driver, transport and a real Port. The stub above
  # proves the request building; this proves the handshake, the correlated wire
  # and the teardown actually fit together.
  describe "over the production driver" do
    @fake Path.expand("support/fake_sidecar.pl", __DIR__)

    test "handshakes, acts and stops against a real Port" do
      assert {:ok, cu} = Compux.start(binary_path: @fake)

      info = Compux.info(cu)
      assert info.protocol_version == Compux.protocol_version()
      assert info.sidecar_generation == "boot-test"
      assert info.capabilities["controls"] == ["pause", "resume", "release"]
      # The bounds of the sidecar's observation table reach the caller as they are.
      assert info.capabilities["observations"] == %{"max" => 3, "ttl_ms" => 30_000}

      # The image names itself, and a click that names it is accepted.
      assert {:ok, shot} = Compux.screenshot(cu)
      assert shot["observation_kind"] == "image"
      assert is_binary(shot["observation_id"])

      assert {:ok, %{"ok" => true}} =
               Compux.click(cu, {10, 20}, observation_id: shot["observation_id"])

      # One that names an image this sidecar never minted is refused, with nothing
      # dispatched — the refusal the library must carry through as a failure, not
      # as a reply the caller could mistake for a click.
      assert {:error, {:action_failed, refusal}} =
               Compux.click(cu, {10, 20}, observation_id: "nobody-1")

      assert refusal["error"] == "unknown_observation"
      assert refusal["receipt"]["dispatch"] == "not_sent"

      assert :ok = Compux.stop(cu)
    end

    test "refuses a sidecar on another protocol version" do
      assert {:error, {:protocol_mismatch, %{sidecar: 1}}} =
               Compux.start(binary_path: @fake, env: [{~c"FAKE_PROTOCOL_VERSION", ~c"1"}])
    end

    # v9: a control is addressed by name. The reference is only ever meaningful
    # beside the observation that listed it, and an accessibility action reports
    # its own input method and the effect its read-back earned.
    test "presses and sets a control by reference, and refuses one nobody listed" do
      assert {:ok, cu} = Compux.start(binary_path: @fake)

      assert {:ok, list} = Compux.elements(cu)
      image = list["observation_id"]
      [element] = list["elements"]
      assert element["element_ref"] == "e1"
      assert element["actions"] == ["press"]

      assert {:ok, pressed} = Compux.press(cu, element["element_ref"], observation_id: image)
      assert pressed["receipt"]["input_method"] == "ax"
      assert pressed["receipt"]["effect"] == "not_observed"
      assert pressed["receipt"]["foreground_changed"] == false
      refute Map.has_key?(pressed, "verified"), "a press verifies nothing"

      assert {:ok, set} =
               Compux.set_value(cu, element["element_ref"], "typed", observation_id: image)

      assert set["receipt"]["effect"] == "verified"
      assert set["verified"] == true
      assert set["value"] == "typed", "the read-back, not the request echoed"

      # The same control through the pointer: still the pointer's input method.
      assert {:ok, clicked} = Compux.click(cu, {:element, "e1"}, observation_id: image)
      assert clicked["receipt"]["input_method"] == "foreground_hid"

      # A reference this observation never listed is refused, nothing dispatched.
      assert {:error, {:action_failed, refusal}} = Compux.press(cu, "e9", observation_id: image)

      assert refusal["error"] == "stale_element"
      assert refusal["receipt"]["dispatch"] == "not_sent"

      assert :ok = Compux.stop(cu)
    end
  end
end
