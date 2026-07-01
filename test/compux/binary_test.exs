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

  describe "dev build (COMPUX_BUILD)" do
    test "dev_build_path/0 points at the local cargo release output" do
      assert {:ok, path} = Binary.dev_build_path()
      assert String.ends_with?(path, "native/compux/target/release/compux")
      # P1 built it in this repo.
      assert File.regular?(path)
    end

    test "path/1 uses the dev build when COMPUX_BUILD is set" do
      System.put_env("COMPUX_BUILD", "1")
      on_exit(fn -> System.delete_env("COMPUX_BUILD") end)

      assert {:ok, path} = Binary.path()
      assert String.ends_with?(path, "native/compux/target/release/compux")
    end
  end

  describe "download + checksum verify" do
    setup do
      System.delete_env("COMPUX_BUILD")
      tmp = Path.join(System.tmp_dir!(), "compux-test-#{System.unique_integer([:positive])}")
      on_exit(fn -> File.rm_rf(tmp) end)
      %{tmp: tmp}
    end

    test "downloads, verifies sha256, extracts, and caches", %{tmp: tmp} do
      {:ok, target} = Binary.target()
      {tarball, sha} = fake_tarball("compux-binary-bytes")

      assert {:ok, cached} =
               Binary.path(
                 cache_dir: tmp,
                 checksums: %{target => sha},
                 fetcher: fn _url -> {:ok, tarball} end
               )

      assert File.regular?(cached)
      assert File.read!(cached) == "compux-binary-bytes"
      assert executable?(cached)

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
      {tarball, _sha} = fake_tarball("real-bytes")

      assert {:error, {:checksum_mismatch, _details}} =
               Binary.path(
                 cache_dir: tmp,
                 checksums: %{target => String.duplicate("0", 64)},
                 fetcher: fn _url -> {:ok, tarball} end
               )
    end

    test "fails loud when no checksum is pinned for the target", %{tmp: tmp} do
      assert {:error, {:no_checksum_for_target, _target}} =
               Binary.path(cache_dir: tmp, checksums: %{}, fetcher: fn _url -> {:ok, ""} end)
    end
  end

  defp fake_tarball(contents) do
    tar = Path.join(System.tmp_dir!(), "compux-src-#{System.unique_integer([:positive])}.tar.gz")
    :ok = :erl_tar.create(String.to_charlist(tar), [{~c"compux", contents}], [:compressed])
    bytes = File.read!(tar)
    File.rm(tar)
    sha = :sha256 |> :crypto.hash(bytes) |> Base.encode16(case: :lower)
    {bytes, sha}
  end

  defp executable?(path) do
    %File.Stat{mode: mode} = File.stat!(path)
    (mode &&& 0o111) != 0
  end
end
