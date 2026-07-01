defmodule Compux.MixProject do
  use Mix.Project

  @version "0.2.0"
  @source_url "https://github.com/tezra-io/compux"

  def project do
    [
      app: :compux,
      version: @version,
      elixir: "~> 1.17",
      elixirc_options: [warnings_as_errors: true],
      elixirc_paths: elixirc_paths(Mix.env()),
      start_permanent: Mix.env() == :prod,
      deps: deps(),
      name: "Compux",
      description: description(),
      package: package(),
      source_url: @source_url,
      docs: docs()
    ]
  end

  def application do
    # :inets + :ssl back the `Compux.Binary` checksum-verified downloader (:httpc).
    [extra_applications: [:logger, :inets, :ssl]]
  end

  defp elixirc_paths(:test), do: ["lib", "test/support"]
  defp elixirc_paths(_env), do: ["lib"]

  defp deps do
    [
      {:jason, "~> 1.4"},
      {:credo, "~> 1.7", only: [:dev, :test], runtime: false}
    ]
  end

  defp description do
    "Native screen-capture + input-injection (computer use) for Elixir, backed by a " <>
      "crash-isolated Rust sidecar spawned over a Port (not a NIF). macOS-first."
  end

  defp package do
    [
      licenses: ["MIT"],
      links: %{"GitHub" => @source_url},
      files: ~w(lib native checksum-compux.exs mix.exs README.md LICENSE)
    ]
  end

  defp docs do
    [main: "readme", extras: ["README.md"]]
  end
end
