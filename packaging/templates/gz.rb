# Rendered by packaging/render.sh from the gz-cli release @TAG@; edit the
# template in Sarvesh-GanesanW/gz-cli, not this file.
class Gz < Formula
  desc "Command-line interface for the Groundzero data platform"
  homepage "https://github.com/Sarvesh-GanesanW/gz-cli"
  version "@VERSION@"
  license :cannot_represent

  on_macos do
    on_arm do
      url "https://github.com/Sarvesh-GanesanW/gz-cli/releases/download/@TAG@/gz-macos-arm64"
      sha256 "@SHA_MACOS_ARM64@"
    end
    on_intel do
      url "https://github.com/Sarvesh-GanesanW/gz-cli/releases/download/@TAG@/gz-macos-x86_64"
      sha256 "@SHA_MACOS_X86_64@"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/Sarvesh-GanesanW/gz-cli/releases/download/@TAG@/gz-linux-x86_64"
      sha256 "@SHA_LINUX_X86_64@"
    end
  end

  def install
    # Releases ship bare binaries named per platform; install one as `gz`.
    binary = Dir["gz-*"].first
    chmod 0755, binary
    bin.install binary => "gz"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/gz --version")
  end
end
