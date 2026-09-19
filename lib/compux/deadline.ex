defmodule Compux.Deadline do
  @moduledoc """
  One absolute monotonic deadline per request.

  A request's budget is fixed the moment it is sent. Nothing that arrives while it
  runs — a response fragment, a control acknowledgement, an unsolicited event —
  extends it. That is the whole reason this is a value rather than a `receive`'s
  `after` clause: the old reader put its timeout on each `receive`, so every
  fragment of a slowly-written response restarted the full budget and a 300 ms
  call could take seconds.

  The clock is `System.monotonic_time/1`, which never moves backwards and is
  immune to a wall-clock adjustment. It is the BEAM's own clock domain: never
  compare one of these numbers with a timestamp the sidecar produced.
  """

  @enforce_keys [:at_ms, :budget_ms]
  defstruct [:at_ms, :budget_ms]

  @type t :: %__MODULE__{at_ms: integer(), budget_ms: non_neg_integer()}

  @doc "Start a deadline `budget_ms` from now."
  @spec start(non_neg_integer()) :: t()
  def start(budget_ms) when is_integer(budget_ms) and budget_ms >= 0 do
    %__MODULE__{at_ms: now_ms() + budget_ms, budget_ms: budget_ms}
  end

  @doc "Milliseconds left, floored at zero — never negative, so it is safe as a timeout."
  @spec remaining(t()) :: non_neg_integer()
  def remaining(%__MODULE__{at_ms: at_ms}), do: max(at_ms - now_ms(), 0)

  @doc "Whether the budget is spent."
  @spec expired?(t()) :: boolean()
  def expired?(%__MODULE__{} = deadline), do: remaining(deadline) == 0

  @doc "The budget this deadline was started with, for the `{:timeout, ms}` report."
  @spec budget_ms(t()) :: non_neg_integer()
  def budget_ms(%__MODULE__{budget_ms: budget_ms}), do: budget_ms

  defp now_ms, do: System.monotonic_time(:millisecond)
end
