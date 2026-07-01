defmodule Compux.Binary do
  @moduledoc """
  Resolves the `compux` sidecar executable for the host.

  Resolution order:

    1. **Dev build** — when `COMPUX_BUILD` is set, the local
       `native/compux/target/release/compux` (the `cargo build` loop).
    2. **Cached download** — otherwise a per-target precompiled binary, downloaded
       once from the GitHub release, verified against the committed sha256 in
       `checksum-compux.exs`, and cached under the user cache dir.

  This is the standalone-consumer path. An embedder that manages its own signed
  install (verifying provenance itself) should skip this and pass an explicit
  `:binary_path` to `Compux.start/1`.

  The sha256 map is baked in at compile time (rustler_precompiled-style), so every
  download is checksum-verified. `:fetcher`, `:checksums`, and `:cache_dir` are
  injectable for tests; production uses `:httpc` + the baked checksums + the user
  cache dir.
  """

  @command "compux"
  @release_base "https://github.com/tezra-io/compux/releases/download"

  # Bound the release download so a hung/slow fetch can't block the caller forever
  # (e.g. a setup-page "installing…" spinner that never resolves). The artifact is
  # small; these are generous.
  @connect_timeout_ms 15_000
  @download_timeout_ms 120_000

  @checksum_file Path.expand(Path.join([__DIR__, "..", "..", "checksum-compux.exs"]))
  @external_resource @checksum_file
  @checksums (case File.read(@checksum_file) do
                {:ok, contents} when byte_size(contents) > 0 ->
                  case Code.eval_string(contents) do
                    {%{} = map, _binding} -> map
                    _other -> %{}
                  end

                _absent_or_empty ->
                  %{}
              end)

  @source_root Path.expand(Path.join([__DIR__, "..", ".."]))

  @doc """
  The host target string used to select an artifact, e.g. `"macos-aarch64"`.
  Only Apple-Silicon macOS and x86_64 Linux are supported; anything else is a
  loud `{:error, {:unsupported_target, os, arch}}`.
  """
  @spec target() :: {:ok, String.t()} | {:error, term()}
  def target do
    with {:ok, os} <- host_os(),
         {:ok, arch} <- host_arch() do
      case {os, arch} do
        {"macos", "aarch64"} -> {:ok, "macos-aarch64"}
        {"linux", "x86_64"} -> {:ok, "linux-x86_64"}
        {os, arch} -> {:error, {:unsupported_target, os, arch}}
      end
    end
  end

  @doc "Resolve the sidecar path, raising with a clear message on failure."
  @spec path!(keyword()) :: String.t()
  def path!(opts \\ []) do
    case path(opts) do
      {:ok, resolved} ->
        resolved

      {:error, reason} ->
        raise "compux: cannot resolve the sidecar binary (#{inspect(reason)}). " <>
                "Set COMPUX_BUILD=1 to build from source, or install a release."
    end
  end

  @doc "Resolve the sidecar path (see the moduledoc for the order)."
  @spec path(keyword()) :: {:ok, String.t()} | {:error, term()}
  def path(opts \\ []) do
    if dev_build?() do
      dev_build_path()
    else
      ensure_downloaded(opts)
    end
  end

  @doc """
  Resolve the sidecar path WITHOUT downloading: the `COMPUX_BUILD` dev build if
  set, else the cached download if already present, else `{:error, :not_cached}`.

  Use this for a side-effect-free "is it installed?" check on a hot path (a status
  read must never trigger a network fetch); use `path/1` when a download-if-absent
  is acceptable (e.g. at spawn time, after readiness is already gated).
  """
  @spec cached_path(keyword()) :: {:ok, String.t()} | {:error, term()}
  def cached_path(opts \\ []) do
    if dev_build?(), do: dev_build_path(), else: cached_download(opts)
  end

  defp cached_download(opts) do
    with {:ok, target} <- target() do
      cache = cache_path(opts, target)
      if File.regular?(cache), do: {:ok, cache}, else: {:error, :not_cached}
    end
  end

  @doc "The local `cargo build --release` output path (the dev loop target)."
  @spec dev_build_path() :: {:ok, String.t()} | {:error, term()}
  def dev_build_path do
    candidate = Path.join([@source_root, "native", "compux", "target", "release", @command])

    if File.regular?(candidate),
      do: {:ok, candidate},
      else: {:error, {:dev_build_missing, candidate}}
  end

  # --- internals ------------------------------------------------------------

  defp dev_build? do
    case System.get_env("COMPUX_BUILD") do
      value when value in [nil, "", "0", "false"] -> false
      _set -> true
    end
  end

  defp ensure_downloaded(opts) do
    with {:ok, target} <- target() do
      cache = cache_path(opts, target)

      if File.regular?(cache),
        do: {:ok, cache},
        else: download(target, cache, opts)
    end
  end

  defp download(target, cache, opts) do
    checksums = Keyword.get(opts, :checksums, @checksums)
    fetch = Keyword.get(opts, :fetcher, &default_fetch/1)

    with {:ok, expected} <- expected_sha(checksums, target),
         {:ok, tarball} <- fetch.(artifact_url(target)),
         :ok <- verify(tarball, expected),
         {:ok, binary} <- untar_binary(tarball) do
      write_cache(cache, binary)
    end
  end

  defp expected_sha(checksums, target) do
    case Map.get(checksums, target) do
      sha when is_binary(sha) and byte_size(sha) > 0 -> {:ok, sha}
      _absent -> {:error, {:no_checksum_for_target, target}}
    end
  end

  defp verify(bytes, expected) do
    actual = :crypto.hash(:sha256, bytes) |> Base.encode16(case: :lower)

    if actual == String.downcase(expected),
      do: :ok,
      else: {:error, {:checksum_mismatch, expected: expected, actual: actual}}
  end

  defp untar_binary(tarball) do
    case :erl_tar.extract({:binary, tarball}, [:memory, :compressed]) do
      {:ok, entries} -> pick_binary(entries)
      {:error, reason} -> {:error, {:untar, reason}}
    end
  end

  defp pick_binary(entries) do
    case Enum.find(entries, &binary_entry?/1) do
      {_name, binary} -> {:ok, binary}
      nil -> {:error, {:binary_not_in_archive, @command}}
    end
  end

  defp binary_entry?({name, _bin}), do: Path.basename(to_string(name)) == @command

  defp write_cache(cache, binary) do
    File.mkdir_p!(Path.dirname(cache))
    File.write!(cache, binary)
    File.chmod!(cache, 0o755)
    {:ok, cache}
  end

  defp cache_path(opts, target) do
    root = Keyword.get(opts, :cache_dir, default_cache_dir())
    Path.join([root, version(), target, @command])
  end

  defp default_cache_dir, do: :filename.basedir(:user_cache, "compux")

  defp artifact_url(target) do
    version = version()
    "#{@release_base}/v#{version}/#{@command}-#{version}-#{target}.tar.gz"
  end

  # The loaded app version, resolved at runtime so it is correct no matter which
  # project embeds compux (a compile-time `Mix.Project.config()` is not).
  defp version, do: to_string(Application.spec(:compux, :vsn))

  defp default_fetch(url) do
    {:ok, _} = Application.ensure_all_started(:inets)
    {:ok, _} = Application.ensure_all_started(:ssl)

    http_opts = [
      ssl: [verify: :verify_peer, cacerts: :public_key.cacerts_get()],
      autoredirect: true,
      connect_timeout: @connect_timeout_ms,
      timeout: @download_timeout_ms
    ]

    request = {String.to_charlist(url), []}

    case :httpc.request(:get, request, http_opts, body_format: :binary) do
      {:ok, {{_version, 200, _reason}, _headers, body}} -> {:ok, body}
      {:ok, {{_version, status, _reason}, _headers, _body}} -> {:error, {:http_status, status}}
      {:error, reason} -> {:error, {:http_error, reason}}
    end
  end

  defp host_os do
    case :os.type() do
      {:unix, :darwin} -> {:ok, "macos"}
      {:unix, :linux} -> {:ok, "linux"}
      other -> {:error, {:unsupported_os, other}}
    end
  end

  defp host_arch do
    arch = List.to_string(:erlang.system_info(:system_architecture))

    cond do
      String.contains?(arch, "aarch64") or String.contains?(arch, "arm64") -> {:ok, "aarch64"}
      String.contains?(arch, "x86_64") or String.contains?(arch, "amd64") -> {:ok, "x86_64"}
      true -> {:error, {:unsupported_arch, arch}}
    end
  end
end
