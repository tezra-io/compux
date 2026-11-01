defmodule Compux.Protocol12Test do
  # Protocol 12 (M86 P2): the model's surface, defined once for both platforms —
  # scope, query, root and skeleton on `elements`, the verbs on a reference,
  # `wait_for` and the semantic check that waits, and the names a caller can rely
  # on. PROTOCOL.md is the contract these tests hold the library to.
  use ExUnit.Case, async: true

  alias Compux.Protocol

  @verbs ~w(focus select toggle expand collapse scroll_to)

  # A valid request for one verb on a reference, so a test can remove or
  # contradict exactly the field it is about.
  defp verb_params(action, extra \\ %{}) do
    base = %{"action" => action, "observation_id" => "7c1e-12", "element_ref" => "e3"}
    base = if action == "toggle", do: Map.put(base, "on", true), else: base
    Map.merge(base, extra)
  end

  defp elements(extra), do: Protocol.validate(Map.put(extra, "action", "elements"))

  defp wait_for(until, extra \\ %{}) do
    Protocol.validate(Map.merge(%{"action" => "wait_for", "until" => until}, extra))
  end

  describe "the version and the verbs" do
    test "this library speaks protocol 12" do
      assert Protocol.protocol_version() == 12
    end

    test "the verbs on a reference are model actions that act on the machine" do
      for action <- @verbs do
        assert action in Protocol.actions(), action
        refute Protocol.read_only?(action), "#{action} changes the application"
      end
    end

    # A wait sends nothing, so it carries no sequence number and earns no receipt.
    test "wait_for is a read-only model action" do
      assert "wait_for" in Protocol.actions()
      assert Protocol.read_only?("wait_for")
    end
  end

  describe "verbs on a reference" do
    test "each carries the observation and the reference, and nothing else" do
      for action <- ~w(focus select expand collapse scroll_to) do
        assert {:ok, request} = Protocol.validate(verb_params(action))

        assert request == %{
                 "action" => action,
                 "observation_id" => "7c1e-12",
                 "element_ref" => "e3"
               }
      end
    end

    test "toggle names the state the control should end in" do
      for on <- [true, false] do
        assert {:ok, request} = Protocol.validate(verb_params("toggle", %{"on" => on}))
        assert request["on"] == on
      end

      for bad <- [nil, "true", 1] do
        params = verb_params("toggle", %{"on" => bad})
        assert {:error, reason} = Protocol.validate(params), "on #{inspect(bad)}"
        assert reason =~ "on"
      end
    end

    for action <- ~w(focus select toggle expand collapse scroll_to) do
      test "#{action} without an element_ref is refused by name" do
        params = Map.delete(verb_params(unquote(action)), "element_ref")
        assert {:error, reason} = Protocol.validate(params)
        assert reason =~ "element_ref"
      end

      test "#{action} without an observation_id is refused" do
        params = Map.delete(verb_params(unquote(action)), "observation_id")
        assert {:error, reason} = Protocol.validate(params)
        assert reason =~ "observation_id"
      end

      test "#{action} refuses a point beside its reference" do
        params = verb_params(unquote(action), %{"x" => 1, "y" => 2})
        assert {:error, reason} = Protocol.validate(params)
        assert reason =~ "never by both"
      end

      test "#{action} takes a check of every kind, and a target" do
        for check <- ["image", "semantic", "none"] do
          params = verb_params(unquote(action), %{"check" => check, "target_id" => "t1"})
          assert {:ok, request} = Protocol.validate(params), "check #{check}"
          assert request["check"] == check
          assert request["target_id"] == "t1"
        end
      end
    end
  end

  # `type` types at the focus, as it always has, or — with an element_ref — into one
  # control through the accessibility API, with no keyboard.
  describe "type with element_ref" do
    test "carries the control, the observation it was listed in, and the text" do
      params = %{
        "action" => "type",
        "observation_id" => "7c1e-12",
        "element_ref" => "e2",
        "text" => "hello"
      }

      assert {:ok, request} = Protocol.validate(params)
      assert request == params
    end

    test "an element_ref without the observation that listed it is refused" do
      params = %{"action" => "type", "element_ref" => "e2", "text" => "hello"}
      assert {:error, reason} = Protocol.validate(params)
      assert reason =~ "observation_id"
    end

    test "an observation_id without an element_ref is refused: typing at the focus reads no image" do
      params = %{"action" => "type", "observation_id" => "7c1e-12", "text" => "hello"}
      assert {:error, reason} = Protocol.validate(params)
      assert reason =~ "observation_id"
    end

    test "a point beside the reference is a conflict" do
      params = %{
        "action" => "type",
        "observation_id" => "7c1e-12",
        "element_ref" => "e2",
        "text" => "hi",
        "x" => 1,
        "y" => 2
      }

      assert {:error, reason} = Protocol.validate(params)
      assert reason =~ "never by both"
    end

    test "a semantic check is offered, because a control was named" do
      params = %{
        "action" => "type",
        "observation_id" => "7c1e-12",
        "element_ref" => "e2",
        "text" => "hi",
        "check" => "semantic"
      }

      assert {:ok, %{"check" => "semantic"}} = Protocol.validate(params)
    end
  end

  describe "elements: scope, query, root and skeleton" do
    test "absent, every new parameter is absent on the wire" do
      assert {:ok, %{"action" => "elements"} = request} = elements(%{})
      assert map_size(request) == 1
    end

    test "each scope is carried" do
      for scope <- Protocol.scopes() do
        assert {:ok, %{"scope" => ^scope}} = elements(%{"scope" => scope})
      end

      assert Protocol.scopes() == ~w(window app focused menu dialog)
    end

    test "an unknown scope is refused by name" do
      for bad <- ["screen", "Window", 1, true] do
        assert {:error, reason} = elements(%{"scope" => bad}), "scope #{inspect(bad)}"
        assert reason =~ "scope"
      end
    end

    test "a query names a normalised role, a name, or both" do
      for query <- [
            %{"role" => "button"},
            %{"name" => "save"},
            %{"role" => "textfield", "name" => "Name"}
          ] do
        assert {:ok, %{"query" => ^query}} = elements(%{"query" => query})
      end
    end

    # A typo must not widen a find into "everything": an unknown key, an empty
    # query and a role this wire does not have are refused rather than dropped.
    test "a query that would match by accident is refused" do
      for bad <- [
            %{},
            %{"nmae" => "save"},
            %{"role" => "AXButton"},
            %{"role" => "Button"},
            %{"name" => ""},
            %{"name" => 7},
            "save"
          ] do
        assert {:error, reason} = elements(%{"query" => bad}), "query #{inspect(bad)}"
        assert reason =~ "query"
      end
    end

    test "a skeleton is carried only when literally true" do
      assert {:ok, %{"skeleton" => true}} = elements(%{"skeleton" => true})
      assert {:ok, request} = elements(%{"skeleton" => false})
      refute Map.has_key?(request, "skeleton")
      assert {:error, reason} = elements(%{"skeleton" => "yes"})
      assert reason =~ "skeleton"
    end

    # A skeleton cuts the walk at depth 3, and a find has to see the whole tree.
    test "a skeleton and a query together are refused" do
      assert {:error, reason} = elements(%{"skeleton" => true, "query" => %{"role" => "button"}})
      assert reason =~ "skeleton"
      assert reason =~ "query"
    end

    test "a root is a reference read in the listing the observation names" do
      assert {:ok, request} = elements(%{"root" => "e12", "observation_id" => "7c1e-3"})
      assert request["root"] == "e12"
      assert request["observation_id"] == "7c1e-3"

      assert {:ok, request} =
               elements(%{"root" => "e12", "observation_id" => "7c1e-3", "skeleton" => true})

      assert request["skeleton"] == true

      assert {:ok, request} =
               elements(%{
                 "root" => "e12",
                 "observation_id" => "7c1e-3",
                 "query" => %{"role" => "button"}
               })

      assert request["query"] == %{"role" => "button"}
    end

    test "a root alone means nothing, and a root names its own scope" do
      assert {:error, reason} = elements(%{"root" => "e12"})
      assert reason =~ "observation_id"

      assert {:error, reason} =
               elements(%{"root" => "e12", "observation_id" => "7c1e-3", "scope" => "app"})

      assert reason =~ "root"
      assert reason =~ "scope"

      for bad <- ["", 12, nil] do
        params = %{"root" => bad, "observation_id" => "7c1e-3"}
        assert {:error, reason} = elements(params), "root #{inspect(bad)}"
        assert reason =~ "root"
      end
    end
  end

  describe "the until condition" do
    test "each kind with exactly its own fields" do
      for until <- [
            %{"kind" => "appears", "role" => "dialog"},
            %{"kind" => "appears", "name" => "Done", "scope" => "window"},
            %{"kind" => "disappears", "element_ref" => "e12"},
            %{"kind" => "disappears", "role" => "button", "name" => "Cancel"},
            %{"kind" => "enabled", "element_ref" => "e7"},
            %{"kind" => "value", "element_ref" => "e3", "value" => "42"},
            %{"kind" => "value", "element_ref" => "e3", "value" => ""},
            %{"kind" => "window_title", "contains" => "Untitled"},
            %{"kind" => "menu_open"},
            %{"kind" => "menu_closed", "timeout_ms" => 2_000},
            %{"kind" => "text", "contains" => "Saved", "scope" => "dialog"}
          ] do
        assert {:ok, %{"until" => ^until}} = wait_for(until, %{"observation_id" => "7c1e-3"}),
               "until #{inspect(until)}"
      end
    end

    test "a condition missing what it is about is refused" do
      for until <- [
            %{"kind" => "appears"},
            %{"kind" => "disappears"},
            %{"kind" => "enabled"},
            %{"kind" => "value", "element_ref" => "e3"},
            %{"kind" => "window_title"},
            %{"kind" => "text", "contains" => ""},
            %{"kind" => "nonsense"},
            %{"role" => "button"},
            "appears"
          ] do
        assert {:error, reason} = wait_for(until, %{"observation_id" => "7c1e-3"}),
               "until #{inspect(until)}"

        assert reason =~ "until"
      end
    end

    # A field a kind does not read is a request the helper would not honour, so it
    # is refused rather than dropped: a scope on a control read directly, a value
    # on an appearance, a typo.
    test "a field the kind does not read is refused" do
      for until <- [
            %{"kind" => "enabled", "element_ref" => "e7", "scope" => "app"},
            %{"kind" => "appears", "role" => "button", "value" => "x"},
            %{"kind" => "menu_open", "contains" => "File"},
            %{"kind" => "appears", "rol" => "button"},
            %{"kind" => "appears", "role" => "AXButton"}
          ] do
        assert {:error, reason} = wait_for(until, %{"observation_id" => "7c1e-3"}),
               "until #{inspect(until)}"

        assert reason =~ "until"
      end
    end

    test "the timeout is bounded like wait_for_change's" do
      for ok <- [1, 25_000] do
        until = %{"kind" => "menu_open", "timeout_ms" => ok}
        assert {:ok, _} = wait_for(until)
      end

      for bad <- [0, 25_001, "5000"] do
        until = %{"kind" => "menu_open", "timeout_ms" => bad}
        assert {:error, reason} = wait_for(until), "timeout_ms #{inspect(bad)}"
        assert reason =~ "timeout_ms"
      end
    end

    # A reference is scoped to the listing that minted it, and the request names
    # that listing. With none named there is nothing to read it in.
    test "a reference in a condition needs the listing the request names" do
      assert {:error, reason} = wait_for(%{"kind" => "enabled", "element_ref" => "e7"})
      assert reason =~ "observation_id"
    end
  end

  describe "wait_for" do
    test "carries its condition, the listing, the window and the region" do
      params = %{
        "observation_id" => "7c1e-3",
        "target_id" => "t1",
        "region" => %{"x" => 0, "y" => 0, "w" => 100, "h" => 50}
      }

      until = %{"kind" => "appears", "role" => "button"}
      assert {:ok, request} = wait_for(until, params)
      assert request == Map.merge(params, %{"action" => "wait_for", "until" => until})
    end

    test "without a condition there is nothing to wait for" do
      assert {:error, reason} = Protocol.validate(%{"action" => "wait_for"})
      assert reason =~ "until"
    end

    test "it sends nothing, so it takes no check and names no control at the top" do
      until = %{"kind" => "menu_open"}
      assert {:error, reason} = wait_for(until, %{"check" => "none"})
      assert reason =~ "check"

      assert {:error, reason} = wait_for(until, %{"element_ref" => "e3"})
      assert reason =~ "element_ref"
    end
  end

  # The object form of `check`: a semantic check that waits on the tree, with its
  # baseline taken before the input — on any action that dispatches input, not only
  # one that names a control.
  describe "check: semantic with until" do
    test "is carried on actions addressed every way" do
      until = %{"kind" => "appears", "role" => "dialog"}
      check = %{"kind" => "semantic", "until" => until}

      for params <- [
            %{"action" => "key", "chord" => "cmd+s"},
            %{"action" => "left_click", "observation_id" => "7c1e-1", "x" => 4, "y" => 9},
            verb_params("toggle"),
            %{"action" => "press", "observation_id" => "7c1e-1", "element_ref" => "e1"}
          ] do
        assert {:ok, request} = Protocol.validate(Map.put(params, "check", check)),
               params["action"]

        assert request["check"] == check
      end
    end

    test "only the semantic kind waits, and it needs a condition" do
      until = %{"kind" => "menu_open"}

      for bad <- [
            %{"kind" => "image", "until" => until},
            %{"kind" => "none", "until" => until},
            %{"kind" => "semantic"},
            %{"kind" => "semantic", "until" => until, "extra" => 1},
            %{"until" => until}
          ] do
        params = %{"action" => "key", "chord" => "cmd+s", "check" => bad}
        assert {:error, reason} = Protocol.validate(params), "check #{inspect(bad)}"
        assert reason =~ "check"
      end
    end

    test "a condition by reference needs the action's own listing" do
      check = %{"kind" => "semantic", "until" => %{"kind" => "disappears", "element_ref" => "e3"}}

      assert {:error, reason} =
               Protocol.validate(%{"action" => "key", "chord" => "esc", "check" => check})

      assert reason =~ "observation_id"

      params = %{
        "action" => "press",
        "observation_id" => "7c1e-1",
        "element_ref" => "e3",
        "check" => check
      }

      assert {:ok, %{"check" => ^check}} = Protocol.validate(params)
    end

    test "a read-only action takes no check of either form" do
      check = %{"kind" => "semantic", "until" => %{"kind" => "menu_open"}}
      assert {:error, reason} = Protocol.validate(%{"action" => "elements", "check" => check})
      assert reason =~ "check"
    end
  end

  describe "bound targets reach the new actions" do
    test "every verb and wait_for may name a window" do
      for action <- @verbs do
        params = Map.put(verb_params(action), "target_id", "t1")
        assert {:ok, %{"target_id" => "t1"}} = Protocol.validate(params), action
      end

      assert {:ok, %{"target_id" => "t1"}} =
               wait_for(%{"kind" => "menu_open"}, %{"target_id" => "t1"})
    end
  end

  # The names a caller renders and tests against, from the one place that has them.
  describe "the names" do
    test "the normalised roles and the surface kinds" do
      roles = Protocol.roles()
      assert "button" in roles
      assert "unknown" in roles

      for role <- ~w(checkbox radio textfield textarea combobox menuitem link tab slider row cell
                     group switch disclosure dialog menu window text) do
        assert role in roles, role
      end

      assert roles == Enum.uniq(roles)
      assert Protocol.surface_kinds() == ~w(dialog alert menu popup)
    end

    test "every refusal protocol 12 names, the Linux-first ones included" do
      refusals = Protocol.refusals()

      for code <- ~w(ambiguous_element identity_unknown focus_refused target_binding_unavailable
                     toolkit_accessibility_off text_outside_layout consent_refused consent_timeout
                     state_unreadable scope_not_found accessibility_unavailable unknown_action
                     stale_element element_disabled ax_action_unsupported ax_action_failed
                     ax_timed_out invalid_argument paused cancelled busy stale_generation
                     stale_mutation unknown_observation expired_observation stale_observation
                     point_outside_observation observation_required element_required
                     addressing_conflict check_unsupported unknown_field target_unavailable
                     pixels_unavailable no_active_window keyboard_not_neutral) do
        assert code in refusals, code
      end

      assert refusals == Enum.uniq(refusals)
    end
  end
end
