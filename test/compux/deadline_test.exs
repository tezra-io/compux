defmodule Compux.DeadlineTest do
  use ExUnit.Case, async: true

  alias Compux.Deadline

  test "remaining/1 counts down from the budget and never goes negative" do
    deadline = Deadline.start(50)
    assert Deadline.remaining(deadline) <= 50
    refute Deadline.expired?(deadline)

    Process.sleep(80)

    assert Deadline.remaining(deadline) == 0
    assert Deadline.expired?(deadline)
  end

  test "it is absolute: re-reading it does not extend it" do
    deadline = Deadline.start(200)
    first = Deadline.remaining(deadline)
    Process.sleep(60)
    second = Deadline.remaining(deadline)

    assert second < first,
           "remaining/1 must reflect elapsed time, not restart the budget"
  end

  test "budget_ms/1 reports what the deadline was started with" do
    assert Deadline.budget_ms(Deadline.start(1_234)) == 1_234
  end

  test "a zero budget is already spent" do
    assert Deadline.expired?(Deadline.start(0))
  end

  test "a negative budget is refused rather than silently floored" do
    assert_raise FunctionClauseError, fn -> Deadline.start(-1) end
  end
end
