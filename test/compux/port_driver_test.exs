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
  end
end
