defmodule Compux.BinaryTest do
  # async: false — some cases toggle the global COMPUX_BUILD env; keep them serial.
  use ExUnit.Case, async: false

  import Bitwise

  alias Compux.Binary

  describe "target/0" do
    test "resolves a supported host, or fails loud on an unsupported one" do
      case Binary.target() do
        {:ok, target} -> assert target in ["macos-aarch64", "linux-x86_64"]
        {:error, {:unsupported_target, _os, _arch}} -> :ok
      end
    end
  end

  describe "artifact_name/2" do
    test "a zipped .app on macOS, a bare-binary tarball elsewhere" do
      assert Binary.artifact_name("0.5.0", "macos-aarch64") == "compux-0.5.0-macos-aarch64.zip"
      assert Binary.artifact_name("0.5.0", "linux-x86_64") == "compux-0.5.0-linux-x86_64.tar.gz"
    end
  end

  describe "dev build (COMPUX_BUILD)" do
    # Hermetic: whether a local `cargo build --release` output EXISTS is host
    # state (present on a dev machine, absent on the Elixir CI runner), so these
    # assert the path RESOLUTION — the unit under test — and accept both the
    # built and not-built host, never requiring the cargo artifact.
    test "dev_build_path/0 resolves the local cargo release output path" do
      case Binary.dev_build_path() do
        {:ok, path} ->
          assert String.ends_with?(path, "native/compux/target/release/compux")
          assert File.regular?(path)

        {:error, {:dev_build_missing, path}} ->
          assert String.ends_with?(path, "native/compux/target/release/compux")
      end
    end

    test "path/1 resolves via the dev build when COMPUX_BUILD is set" do
      System.put_env("COMPUX_BUILD", "1")
      on_exit(fn -> System.delete_env("COMPUX_BUILD") end)

      case Binary.path() do
        {:ok, path} ->
          assert String.ends_with?(path, "native/compux/target/release/compux")

        {:error, {:dev_build_missing, path}} ->
          assert String.ends_with?(path, "native/compux/target/release/compux")
      end
    end
  end

  describe "download + checksum verify" do
    setup do
      System.delete_env("COMPUX_BUILD")
      tmp = Path.join(System.tmp_dir!(), "compux-test-#{System.unique_integer([:positive])}")
      on_exit(fn -> File.rm_rf(tmp) end)
      %{tmp: tmp}
    end

    test "downloads, verifies sha256, extracts, and resolves the executable", %{tmp: tmp} do
      {:ok, target} = Binary.target()
      {artifact, sha} = fake_artifact()

      assert {:ok, cached} =
               Binary.path(
                 cache_dir: tmp,
                 checksums: %{target => sha},
                 fetcher: fn _url -> {:ok, artifact} end
               )

      # Resolves to a runnable executable at the target-appropriate inner path
      # (inside Fermix.app on macOS, a bare binary on Linux).
      assert File.regular?(cached)
      assert executable?(cached)
      assert Path.basename(cached) == "compux"

      # a second resolve is a pure cache hit (fetcher would crash if called)
      assert {:ok, ^cached} =
               Binary.path(
                 cache_dir: tmp,
                 checksums: %{target => sha},
                 fetcher: fn _url -> raise "should not fetch on cache hit" end
               )
    end

    test "fails loud on a checksum mismatch", %{tmp: tmp} do
      {:ok, target} = Binary.target()
      {artifact, _sha} = fake_artifact()

      assert {:error, {:checksum_mismatch, _details}} =
               Binary.path(
                 cache_dir: tmp,
                 checksums: %{target => String.duplicate("0", 64)},
                 fetcher: fn _url -> {:ok, artifact} end
               )
    end

    test "fails loud when no checksum is pinned for the target", %{tmp: tmp} do
      assert {:error, {:no_checksum_for_target, _target}} =
               Binary.path(cache_dir: tmp, checksums: %{}, fetcher: fn _url -> {:ok, ""} end)
    end

    # macOS second integrity gate: even with a matching sha256, an unsigned/tampered
    # bundle must fail codesign verify AND leave nothing behind for a later resolve.
    test "fails loud on an unsigned .app and caches nothing (macOS)", %{tmp: tmp} do
      case Binary.target() do
        {:ok, "macos-" <> _ = target} ->
          {artifact, sha} = fake_app_zip(sign: false)

          assert {:error, {:codesign_verify_failed, _code, _out}} =
                   Binary.path(
                     cache_dir: tmp,
                     checksums: %{target => sha},
                     fetcher: fn _url -> {:ok, artifact} end
                   )

          # the failed extract published nothing — a retry re-downloads, never trusts
          # the unverified staging artifact.
          refute match?({:ok, _path}, Binary.cached_path(cache_dir: tmp))

        _linux ->
          :ok
      end
    end
  end

  describe "cached_path/1 (no download)" do
    setup do
      System.delete_env("COMPUX_BUILD")
      tmp = Path.join(System.tmp_dir!(), "compux-cache-#{System.unique_integer([:positive])}")
      on_exit(fn -> File.rm_rf(tmp) end)
      %{tmp: tmp}
    end

    test "returns {:error, :not_cached} when nothing is cached", %{tmp: tmp} do
      assert {:error, :not_cached} = Binary.cached_path(cache_dir: tmp)
    end

    test "returns the cached path once present, without fetching", %{tmp: tmp} do
      {:ok, target} = Binary.target()
      {artifact, sha} = fake_artifact()

      {:ok, cached} =
        Binary.path(
          cache_dir: tmp,
          checksums: %{target => sha},
          fetcher: fn _ -> {:ok, artifact} end
        )

      assert {:ok, ^cached} = Binary.cached_path(cache_dir: tmp)
    end
  end

  # A release artifact matching the host target: a real ad-hoc-signed Fermix.app
  # (zipped with ditto) on macOS so the ditto-extract + codesign-verify path is
  # actually exercised; a bare-binary tarball on Linux.
  defp fake_artifact do
    case Binary.target() do
      {:ok, "macos-" <> _} -> fake_app_zip()
      _ -> fake_tarball("compux-binary-bytes")
    end
  end

  defp fake_tarball(contents) do
    tar = Path.join(System.tmp_dir!(), "compux-src-#{System.unique_integer([:positive])}.tar.gz")
    # Reclaim on every path (incl. a mid-fixture raise), not just the happy path.
    on_exit(fn -> File.rm_rf(tar) end)
    :ok = :erl_tar.create(String.to_charlist(tar), [{~c"compux", contents}], [:compressed])
    bytes = File.read!(tar)
    sha = :sha256 |> :crypto.hash(bytes) |> Base.encode16(case: :lower)
    {bytes, sha}
  end

  defp fake_app_zip(opts \\ []) do
    work = Path.join(System.tmp_dir!(), "compux-fix-#{System.unique_integer([:positive])}")
    on_exit(fn -> File.rm_rf(work) end)
    app = Path.join(work, "Fermix.app")
    macos = Path.join(app, "Contents/MacOS")
    File.mkdir_p!(macos)
    # A real Mach-O is required for the bundle to codesign; /usr/bin/true is a small one.
    File.cp!(System.find_executable("true"), Path.join(macos, "compux"))
    File.write!(Path.join(app, "Contents/Info.plist"), minimal_plist())

    if Keyword.get(opts, :sign, true) do
      {_out, 0} =
        System.cmd("codesign", ["--force", "--deep", "--sign", "-", app], stderr_to_stdout: true)
    end

    zip = Path.join(work, "artifact.zip")

    {_out, 0} =
      System.cmd("ditto", ["-c", "-k", "--keepParent", app, zip], stderr_to_stdout: true)

    bytes = File.read!(zip)
    sha = :sha256 |> :crypto.hash(bytes) |> Base.encode16(case: :lower)
    {bytes, sha}
  end

  defp minimal_plist do
    """
    <?xml version="1.0" encoding="UTF-8"?>
    <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
    <plist version="1.0"><dict>
    <key>CFBundleIdentifier</key><string>io.tezra.fermix.computer-use</string>
    <key>CFBundleExecutable</key><string>compux</string>
    <key>CFBundleName</key><string>Fermix</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    </dict></plist>
    """
  end

  defp executable?(path) do
    %File.Stat{mode: mode} = File.stat!(path)
    (mode &&& 0o111) != 0
  end
end
