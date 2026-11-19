class Coop < Formula
  desc "Isolated VM environments for coding agents"
  homepage "https://github.com/trailofbits/coop"
  url "https://github.com/trailofbits/coop/archive/refs/tags/v0.10.0.tar.gz"
  sha256 "34f4448d9dad829c4dae84e5a8e2dd4ae6363986752748840062c406e7760b3b"
  license "Apache-2.0"

  depends_on "cmake" => :build
  depends_on "rust" => :build

  on_macos do
    depends_on arch: :arm64
    depends_on "lima"
  end

  def fetch
    system "cargo", "fetch", *std_cargo_fetch_args
  end

  def install
    ENV["COOP_PACKAGE_MANAGER"] = "homebrew"
    # Source archives have no Git metadata; this is a checksummed release.
    ENV["COOP_FORCE_BUILD_KIND"] = "release"
    system "cargo", "install", *std_cargo_args
    system "cargo", "install", *std_cargo_args(path: "coop-proxy")

    generate_completions_from_executable(bin/"coop", "completions")
  end

  def caveats
    <<~EOS
      Build the VM template before first use:
        coop setup

      On macOS, Rosetta 2 is required for x86_64 guests. On Linux, install
      the host prerequisites and configure KVM access as documented at:
        https://github.com/trailofbits/coop/blob/main/docs/getting-started.md#prerequisites

      Homebrew uninstall preserves VM instances, images, and configuration.
      To delete VM instances, run `coop destroy --all` before uninstalling.
    EOS
  end

  test do
    ENV["LIMA_HOME"] = testpath/"lima"
    ENV["COOP_UPDATE_API_BASE_URL"] = "disabled-by-homebrew-test"
    assert_match "coop #{version} (", shell_output("#{bin}/coop --version")

    config = testpath/"config.toml"
    system bin/"coop", "--config", config, "init"
    assert_path_exists config
    config.open("a") do |file|
      file.write <<~TOML

        [profiles.homebrew-test]
        apt_packages = ["jq"]
      TOML
    end
    output = shell_output("#{bin}/coop --config #{config} profiles show homebrew-test")
    assert_match "Profile: homebrew-test (custom)", output
    assert_match "apt_packages: jq", output

    assert_match "brew upgrade coop", shell_output("#{bin}/coop update --check 2>&1", 1)

    # A relocated binary must retain package ownership, even for purge flags.
    test_bin = testpath/"relocated"
    test_bin.mkpath
    cp bin/"coop", test_bin/"coop"
    sentinel = testpath/".coop/sentinel"
    sentinel.write "preserve data"
    assert_match "brew uninstall coop", shell_output("#{test_bin}/coop uninstall --yes --purge 2>&1", 1)
    assert_equal "preserve data", sentinel.read
    assert_path_exists test_bin/"coop"
    assert_match "missing field", pipe_output("#{bin}/coop-proxy 2>&1", "{}", 1)
  end
end
