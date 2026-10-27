class Coop < Formula
  desc "Isolated VM environments for coding agents"
  homepage "https://github.com/trailofbits/coop"
  url "https://github.com/trailofbits/coop/archive/refs/tags/v0.9.1.tar.gz"
  sha256 "aaa52ac23150be99ac1d82b83999f4dd993d46b876e5d4dfa360eb924aaead7d"
  license "Apache-2.0"

  depends_on "cmake" => :build
  depends_on "rust" => :build

  on_macos do
    depends_on arch: :arm64
    depends_on "lima"
  end

  # v0.9.1 does not handle COOP_PACKAGE_MANAGER or guard self-update/uninstall.
  # Backport both so the installed binary delegates package management to brew.
  # Remove patch :DATA and the patch below __END__ when the pinned release includes them.
  patch :DATA

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

__END__
--- a/build.rs
+++ b/build.rs
@@ -8,4 +8,7 @@
 //!   integration tests can compile a visibly non-release "test" variant.
 //! - `COOP_VERSION_STR` — pre-formatted version string for clap's `--version` output.
+//!
+//! `COOP_PACKAGE_MANAGER=homebrew` embeds package ownership via `coop_homebrew`.
+//! This is independent of release/dev provenance and cannot change at runtime.

 use std::env;
@@ -14,5 +17,15 @@
 use std::process::Command;

-fn main() {
+fn main() -> Result<(), String> {
+    println!("cargo:rerun-if-env-changed=COOP_PACKAGE_MANAGER");
+    println!("cargo:rustc-check-cfg=cfg(coop_homebrew)");
+    match env::var("COOP_PACKAGE_MANAGER") {
+        Ok(manager) if manager == "homebrew" => println!("cargo:rustc-cfg=coop_homebrew"),
+        Err(env::VarError::NotPresent) => {}
+        Ok(_) | Err(env::VarError::NotUnicode(_)) => {
+            return Err("COOP_PACKAGE_MANAGER must be unset or 'homebrew'".to_string());
+        }
+    }
+
     // Invalidate the build script (and thus the embedded version) whenever any of git's
     // state-tracking files change. We resolve the real gitdir first so this also works
@@ -45,4 +58,5 @@
     println!("cargo:rustc-env=COOP_BUILD_KIND={kind}");
     println!("cargo:rustc-env=COOP_VERSION_STR={version_str}");
+    Ok(())
 }

--- a/src/update.rs
+++ b/src/update.rs
@@ -93,4 +93,9 @@
 pub fn is_dev_build() -> bool {
     env!("COOP_BUILD_KIND") == "dev"
+}
+
+/// Distribution ownership is embedded at build time.
+pub(crate) const fn is_homebrew_build() -> bool {
+    cfg!(coop_homebrew)
 }

@@ -634,4 +639,7 @@

 pub fn run(opts: &UpdateOpts) -> Result<()> {
+    if is_homebrew_build() {
+        bail!("This is a Homebrew-managed installation. Use `brew upgrade coop` to update it.");
+    }
     if is_dev_build() {
         bail!(
@@ -817,4 +825,4 @@

 fn background_check_disabled() -> bool {
-    if is_dev_build() {
+    if is_homebrew_build() || is_dev_build() {
         return true;
--- a/src/commands/admin.rs
+++ b/src/commands/admin.rs
@@ -106,4 +106,14 @@
     use std::io::IsTerminal as _;

+    if update::is_homebrew_build() {
+        bail!(
+            "Cannot uninstall a Homebrew-managed binary with `coop uninstall`. \
+             No data has been removed.\n\
+             Use `brew uninstall coop` to remove the package. \
+             Homebrew preserves VM instances, images, and configuration. \
+             To delete the VMs too, first run `coop destroy --all` with your usual --config."
+        );
+    }
+
     let binary_path =
         std::env::current_exe().context("Failed to resolve current executable path for removal")?;
