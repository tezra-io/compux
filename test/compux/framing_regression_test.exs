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

  # The arithmetic is the whole test. Against a 1024-byte cap and a 64-byte line
  # limit, a 1064-byte frame arrives as sixteen 64-byte `:noeol` fragments —
  # 1024 bytes, EXACTLY the cap and so not over it — and then a 40-byte `:eol`
  # tail. Only counting that tail crosses the boundary, and the old loop never
  # did: it summed `:noeol` bytes alone, so the last fragment was free and an
  # over-cap frame was decoded and returned as a success. The 1000-byte case has
  # the same fragment shape and sits under the cap, so the two differ ONLY in
  # whether the tail tips the total.
  #
  # (The original red run used 256/320, the same shape an octave down. The
  # constants moved when the fixture's `hello` grew to its faithful 271 bytes,
  # which a 256-byte cap refuses before any test can start.)
  test "the final fragment counts toward the response cap" do
    {:ok, state} =
      PortDriver.start(
        binary_path: @fake,
        timeout: 2_000,
        line_bytes: 64,
        max_response_bytes: 1_024
      )

    assert {:ok, %{"ok" => true}} =
             PortDriver.execute(state, %{"action" => "oversize", "bytes" => 1_001})

    assert {:error, :sidecar_response_too_large} =
             PortDriver.execute(state, %{"action" => "oversize", "bytes" => 1_065})

    PortDriver.stop(state)
  end
end
