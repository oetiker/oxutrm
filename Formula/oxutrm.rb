# Homebrew formula for oxutrm. This repository is its own tap.
#
# The release build rewrites the version line, the four sha256 lines by their
# marker comments and the bottle block between the BOTTLE markers. Keep the
# markers, and keep each url line directly above its sha256 line.
class Oxutrm < Formula
  desc "Remote terminal that survives bad networks, changing addresses and NAT"
  homepage "https://github.com/oetiker/oxutrm"
  version "0.3.0"
  license "Apache-2.0"

  # BOTTLE-START
  bottle do
    root_url "https://github.com/oetiker/oxutrm/releases/download/v0.3.0"
    sha256 cellar: :any_skip_relocation, arm64_sonoma: "8bfe193ce0d051a88b26a58be25663833e57af7b9fd064003728e4589fb6ed22"
  end
  # BOTTLE-END

  on_macos do
    on_arm do
      url "https://github.com/oetiker/oxutrm/releases/download/v#{version}/oxutrm-#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "4377e918fb80873ea47304746d5664363a41d2d831dc343500d4c745ae3d83d5" # mac-arm
    end
    on_intel do
      url "https://github.com/oetiker/oxutrm/releases/download/v#{version}/oxutrm-#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "51ce6ebff9003e37f1782185abd29b841866bad0ff758cae23ffc53d7c7a608b" # mac-x86
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/oetiker/oxutrm/releases/download/v#{version}/oxutrm-#{version}-x86_64-unknown-linux-musl.tar.gz"
      sha256 "81b056ac5e7fb0a671dec23ca2daf335ce151cc94f4eb2fe983e6ff79a1af085" # linux-x86
    end
    on_arm do
      url "https://github.com/oetiker/oxutrm/releases/download/v#{version}/oxutrm-#{version}-aarch64-unknown-linux-musl.tar.gz"
      sha256 "2fe849e44df99f6a742a74695a08d9313e62e7db236aa52eefcab6b2a2f3eeb3" # linux-arm
    end
  end

  def install
    bin.install "oxutrm"
    Dir["man/*.[1-9]"].each { |page| (man/"man#{page[-1]}").install page }
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/oxutrm --version")
  end
end
