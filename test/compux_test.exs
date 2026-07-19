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
        screenshot_after: true
      )

      assert_received {:executed,
                       %{
                         "action" => "right_click",
                         "x" => 10,
                         "y" => 20,
                         "modifiers" => ["cmd", "shift"],
                         "screenshot_after" => true
                       }}

      Compux.click(cu, {1, 2}, button: :double)
      assert_received {:executed, %{"action" => "double_click"}}

      Compux.click(cu, {1, 2})
      assert_received {:executed, %{"action" => "left_click"}}
    end

    test "scroll / drag / type / key / wait / move / inspect", %{cu: cu} do
      Compux.scroll(cu, {5, 5}, :down, 3)
      assert_received {:executed, %{"action" => "scroll", "direction" => "down", "amount" => 3}}

      Compux.drag(cu, {0, 0}, {9, 9})

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

      Compux.move(cu, {3, 4})
      assert_received {:executed, %{"action" => "mouse_move", "x" => 3, "y" => 4}}

      Compux.inspect(cu, {7, 8})
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
      assert {:error, _reason} = Compux.click(cu, {-1, 2})
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
end
