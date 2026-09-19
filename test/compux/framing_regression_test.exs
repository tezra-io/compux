defmodule Compux.FramingRegressionTest do
  @moduledoc """
  The two defects the old blocking `receive_response/4` loop carried, each pinned
  by a test that was RED against it.

  Both are properties of the reader, not of any one action, so they are asserted
  through the public driver against the perl fixture — no native binary, no
  network, no host state.
  """

  use ExUnit.Case, async: true

  alias Compux.PortDriver

  @fake Path.expand("../support/fake_sidecar.pl", __DIR__)

  # The fixture writes ten unterminated 64-byte chunks 250 ms apart and then goes
  # quiet, so the reader sees ten fragments spread over 2.5 s. The old loop put its
  # `after timeout` on EACH receive, so every fragment restarted the full budget and
  # the call returned at ~2.8 s on a 300 ms deadline. One absolute deadline is the
  # whole point: the caller's budget is the caller's budget.
  test "a fragmented response does not renew the deadline" do
    {:ok, state} =
      PortDriver.start(
        binary_path: @fake,
        timeout: 300,
        line_bytes: 64,
        max_response_bytes: 65_536
      )

    {elapsed_us, result} =
      :timer.tc(fn -> PortDriver.execute(state, %{"action" => "dribble"}) end)

    PortDriver.stop(state)

    assert {:error, {:timeout, 300}} = result

    assert div(elapsed_us, 1000) < 1_200,
           "fragments renewed the deadline: the 300 ms call took #{div(elapsed_us, 1000)} ms"
  end

  # The fixture answers with a single 320-byte response against a 256-byte cap and a
  # 64-byte line limit: four 64-byte `:noeol` fragments, then the `:eol` tail. The
  # old loop counted only `:noeol` bytes, so the final fragment was free and an
  # over-cap frame was decoded and returned as a success.
  test "the final fragment counts toward the response cap" do
    {:ok, state} =
      PortDriver.start(
        binary_path: @fake,
        timeout: 2_000,
        line_bytes: 64,
        max_response_bytes: 256
      )

    assert {:error, :sidecar_response_too_large} =
             PortDriver.execute(state, %{"action" => "oversize", "bytes" => 320})

    PortDriver.stop(state)
  end
end
