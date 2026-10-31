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

    test "click button variants + modifiers + check", %{cu: cu} do
      Compux.click(cu, {10, 20},
        button: :right,
        modifiers: [:cmd, :shift],
        check: :image,
        observation_id: "7c1e-12"
      )

      assert_received {:executed,
                       %{
                         "action" => "right_click",
                         "x" => 10,
                         "y" => 20,
                         "modifiers" => ["cmd", "shift"],
                         "check" => "image",
                         "observation_id" => "7c1e-12"
                       }}

      Compux.click(cu, {1, 2}, button: :double, observation_id: "7c1e-12")
      assert_received {:executed, %{"action" => "double_click"}}

      Compux.click(cu, {1, 2}, observation_id: "7c1e-12")
      assert_received {:executed, %{"action" => "left_click"}}
    end

    # The evidence an action brings back is asked for by name, on the same call.
    # A kind this wire does not have never reaches the driver.
    test "check rides every call that dispatches input", %{cu: cu} do
      Compux.type(cu, "hi", check: :none)
      assert_received {:executed, %{"action" => "type", "check" => "none"}}

      Compux.press(cu, "e3", observation_id: "7c1e-12", check: :semantic)
      assert_received {:executed, %{"action" => "press", "check" => "semantic"}}

      Compux.set_value(cu, "e3", "42", observation_id: "7c1e-12", check: "image")
      assert_received {:executed, %{"action" => "set_value", "check" => "image"}}

      Compux.paste(cu, "text")
      assert_received {:executed, executed}
      refute Map.has_key?(executed, "check")

      assert {:error, reason} = Compux.click(cu, {1, 2}, observation_id: "7c1e-12", check: :bogus)
      assert reason =~ "check"
      refute_received {:executed, %{"action" => "left_click"}}
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

    test "select_target names the window and release_target names nothing", %{cu: cu} do
      Compux.select_target(cu, 4711)
      assert_received {:executed, %{"action" => "select_target", "window_id" => 4711}}

      Compux.release_target(cu)
      assert_received {:executed, %{"action" => "release_target"}}
    end

    # The bound window rides on one option, added in one place, so every verb that
    # can name one takes it the same way.
    test "a bound target rides on every action that acts inside a window", %{cu: cu} do
      Compux.screenshot(cu, target_id: "t1")
      assert_received {:executed, %{"action" => "screenshot", "target_id" => "t1"}}

      Compux.click(cu, {10, 20}, observation_id: "7c1e-12", target_id: "t1")
      assert_received {:executed, %{"action" => "left_click", "target_id" => "t1"}}

      Compux.type(cu, "hello", target_id: "t1")
      assert_received {:executed, %{"action" => "type", "target_id" => "t1"}}

      Compux.press(cu, "e3", observation_id: "7c1e-12", target_id: "t1")
      assert_received {:executed, %{"action" => "press", "target_id" => "t1"}}
    end

    # `windows` is how a target is FOUND; naming one on it is a contradiction, and
    # it is refused here rather than carried to the helper.
    test "an action with no window to name refuses the option", %{cu: cu} do
      assert {:error, reason} = Compux.windows(cu, target_id: "t1")
      assert reason =~ "target_id"
      refute_received {:executed, %{"action" => "windows"}}
    end
  end

  # v12: the verbs on a reference, the listing's new options and the wait. Each
  # request is compared WHOLE, so an option that leaked onto the wire is caught too.
  describe "v12 verbs, listings and waits build the right request" do
    setup do
      {:ok, cu: start!()}
    end

    test "each plain verb sends exactly the control and the listing that named it", %{cu: cu} do
      for {verb, action} <- [
            {&Compux.focus/3, "focus"},
            {&Compux.select/3, "select"},
            {&Compux.expand/3, "expand"},
            {&Compux.collapse/3, "collapse"},
            {&Compux.scroll_to/3, "scroll_to"}
          ] do
        assert {:ok, _} = verb.(cu, "e3", observation_id: "7c1e-4")
        assert_received {:executed, %{"action" => ^action} = request}

        assert request == %{
                 "action" => action,
                 "element_ref" => "e3",
                 "observation_id" => "7c1e-4"
               }
      end
    end

    test "a verb without the listing that named its control never reaches the driver",
         %{cu: cu} do
      assert {:error, reason} = Compux.focus(cu, "e3")
      assert reason =~ "observation_id"
      refute_received {:executed, %{"action" => "focus"}}
    end

    test "toggle names the state the control should end in", %{cu: cu} do
      Compux.toggle(cu, "e5", false, observation_id: "7c1e-4", check: :semantic, target_id: "t1")
      assert_received {:executed, %{"action" => "toggle"} = request}

      assert request == %{
               "action" => "toggle",
               "element_ref" => "e5",
               "on" => false,
               "observation_id" => "7c1e-4",
               "check" => "semantic",
               "target_id" => "t1"
             }
    end

    test "type goes into a control by reference, or at the focus as before", %{cu: cu} do
      Compux.type(cu, "hello", element_ref: "e2", observation_id: "7c1e-4")
      assert_received {:executed, %{"action" => "type"} = request}

      assert request == %{
               "action" => "type",
               "text" => "hello",
               "element_ref" => "e2",
               "observation_id" => "7c1e-4"
             }

      assert {:error, reason} = Compux.type(cu, "hello", element_ref: "e2")
      assert reason =~ "observation_id"
      refute_received {:executed, %{"action" => "type"}}

      Compux.type(cu, "hi")
      assert_received {:executed, %{"action" => "type"} = request}
      assert request == %{"action" => "type", "text" => "hi"}
    end

    test "elements takes a scope, a query, a skeleton and a root", %{cu: cu} do
      Compux.elements(cu, scope: :dialog, query: %{"role" => "button", "name" => "Save"})
      assert_received {:executed, %{"action" => "elements"} = request}

      assert request == %{
               "action" => "elements",
               "scope" => "dialog",
               "query" => %{"role" => "button", "name" => "Save"}
             }

      Compux.elements(cu, scope: "app", skeleton: true)
      assert_received {:executed, %{"action" => "elements"} = request}
      assert request == %{"action" => "elements", "scope" => "app", "skeleton" => true}

      Compux.elements(cu, root: "e12", observation_id: "7c1e-4")
      assert_received {:executed, %{"action" => "elements"} = request}
      assert request == %{"action" => "elements", "root" => "e12", "observation_id" => "7c1e-4"}

      assert {:error, reason} = Compux.elements(cu, scope: :desktop)
      assert reason =~ "scope"
      refute_received {:executed, %{"action" => "elements"}}
    end

    test "wait_for sends the condition and the listing its reference belongs to", %{cu: cu} do
      until = %{"kind" => "enabled", "element_ref" => "e7", "timeout_ms" => 2000}
      Compux.wait_for(cu, until, observation_id: "7c1e-4", target_id: "t1")
      assert_received {:executed, %{"action" => "wait_for"} = request}

      assert request == %{
               "action" => "wait_for",
               "until" => until,
               "observation_id" => "7c1e-4",
               "target_id" => "t1"
             }

      # It sends no input, so it has no evidence to ask for.
      assert {:error, reason} = Compux.wait_for(cu, until, observation_id: "7c1e-4", check: :none)
      assert reason =~ "check"
      refute_received {:executed, %{"action" => "wait_for"}}
    end

    test "a condition that never held is still a wait that ran" do
      not_met = %{"ok" => true, "met" => false, "looks" => 21, "waited_ms" => 2004}
      cu = start!(responses: %{"wait_for" => {:ok, not_met}})

      assert {:ok, ^not_met} = Compux.wait_for(cu, %{"kind" => "menu_open"})
    end

    test "any input action can wait on the tree after it, in one request", %{cu: cu} do
      check = %{"kind" => "semantic", "until" => %{"kind" => "appears", "role" => "dialog"}}

      Compux.key(cu, "cmd+s", check: check)
      assert_received {:executed, %{"action" => "key"} = request}
      assert request == %{"action" => "key", "chord" => "cmd+s", "check" => check}

      bogus = %{"kind" => "image", "until" => %{"kind" => "menu_open"}}
      assert {:error, reason} = Compux.key(cu, "cmd+s", check: bogus)
      assert reason =~ "check"
      refute_received {:executed, %{"action" => "key"}}
    end
  end

  describe "probe/1" do
    test "normalizes the sidecar probe response" do
      cu = start!()

      assert {:ok, %{platform: "test", screen_capture: true, input_control: false} = probe} =
               Compux.probe(cu)

      # A build that reports no accessibility facts (macOS) has none of them.
      assert %{
               x11_endpoint: nil,
               display_adopted: nil,
               accessibility_bus: nil,
               accessibility_packages: nil,
               toolkit_notes: nil
             } = probe
    end

    test "carries the Linux accessibility facts beside the display" do
      linux = %{
        "ok" => true,
        "platform" => "linux",
        "display_server" => "x11",
        "x11_endpoint" => true,
        "display_adopted" => false,
        "screen_capture" => true,
        "input_control" => true,
        "accessibility_bus" => %{
          "state" => "answering",
          "applications" => 3,
          "status" => %{"is_enabled" => false, "screen_reader_enabled" => false}
        },
        "accessibility_packages" => %{
          "source" => "dpkg",
          "at_spi2_core" => "2.44.0-3",
          "atk_bridge" => "2.38.0-3"
        },
        "toolkit_notes" => [%{"toolkit" => "qt", "note" => "start it with the switch"}]
      }

      cu = start!(responses: %{"probe" => {:ok, linux}})

      assert {:ok, probe} = Compux.probe(cu)
      assert probe.x11_endpoint == true
      assert probe.display_adopted == false

      assert probe.accessibility_bus == %{
               state: "answering",
               applications: 3,
               cause: nil,
               detail: nil,
               status: %{is_enabled: false, screen_reader_enabled: false}
             }

      assert probe.accessibility_packages == %{
               source: "dpkg",
               at_spi2_core: "2.44.0-3",
               atk_bridge: "2.38.0-3"
             }

      assert probe.toolkit_notes == [%{toolkit: "qt", note: "start it with the switch"}]
    end

    test "names why the accessibility bus is unavailable" do
      unavailable = %{
        "ok" => true,
        "platform" => "linux",
        "display_server" => "none",
        "x11_endpoint" => false,
        "display_adopted" => true,
        "screen_capture" => false,
        "input_control" => false,
        "accessibility_bus" => %{
          "state" => "unavailable",
          "cause" => "not_installed",
          "detail" => "no org.a11y.Bus",
          "status" => nil
        },
        "accessibility_packages" => %{"source" => nil, "at_spi2_core" => nil, "atk_bridge" => nil},
        "toolkit_notes" => []
      }

      cu = start!(responses: %{"probe" => {:ok, unavailable}})

      assert {:ok, probe} = Compux.probe(cu)
      assert probe.display_adopted == true

      assert probe.accessibility_bus == %{
               state: "unavailable",
               applications: nil,
               cause: "not_installed",
               detail: "no org.a11y.Bus",
               status: nil
             }

      assert probe.accessibility_packages == %{source: nil, at_spi2_core: nil, atk_bridge: nil}
      assert probe.toolkit_notes == []
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
      [element, _bold] = list["elements"]
      assert element["element_ref"] == "e1"
      assert element["verbs"] == ["press", "focus"]

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

    # v12: a listing's controls come with a normalised role and their verbs, a
    # toggle reads the state first, and what opened after the input rides on the
    # receipt. Each field arrives as the helper wrote it.
    test "lists controls by role and verb, toggles by state and reports surfaces" do
      assert {:ok, cu} = Compux.start(binary_path: @fake)

      assert {:ok, list} = Compux.elements(cu, scope: :window)
      image = list["observation_id"]
      [_save, bold] = list["elements"]
      assert bold["role"] == "checkbox"
      assert bold["native_role"] == "AXCheckBox"
      assert bold["verbs"] == ["toggle", "focus"]
      assert bold["checked"] == true

      # Already on: nothing sent, and the receipt says why.
      assert {:ok, held} = Compux.toggle(cu, "e2", true, observation_id: image)
      assert held["receipt"]["dispatch"] == "not_sent"
      assert held["receipt"]["already"] == true
      assert held["receipt"]["state_before"] == "on"
      refute Map.has_key?(held["receipt"], "surfaces"), "nothing sent, nothing looked at"

      assert {:ok, flipped} = Compux.toggle(cu, "e2", false, observation_id: image)
      assert flipped["receipt"]["dispatch"] == "sent"
      assert flipped["receipt"]["effect"] == "changed"
      assert flipped["receipt"]["state_after"] == "off"
      assert flipped["receipt"]["surfaces"] == []

      # No route on this control: refused, and never a click instead.
      assert {:error, {:action_failed, refusal}} =
               Compux.toggle(cu, "e1", true, observation_id: image)

      assert refusal["error"] == "ax_action_unsupported"
      assert refusal["receipt"]["dispatch"] == "not_sent"

      assert {:ok, waited} = Compux.wait_for(cu, %{"kind" => "menu_open", "timeout_ms" => 100})
      assert waited["met"] == false

      assert :ok = Compux.stop(cu)
    end
  end
end
