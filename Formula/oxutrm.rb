# Homebrew formula for oxutrm. This repository is its own tap.
#
# The release build rewrites the version line, the four sha256 lines by their
# marker comments and the bottle block between the BOTTLE markers. Keep the
# markers, and keep each url line directly above its sha256 line.
class Oxutrm < Formula
  desc "Remote terminal that survives bad networks, changing addresses and NAT"
  homepage "https://github.com/oetiker/oxutrm"
  version "0.5.1"
  license "Apache-2.0"

  # BOTTLE-START
  bottle do
    root_url "https://github.com/oetiker/oxutrm/releases/download/v0.5.1"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "867b549bbdd0009198f84cb2559115222f0e9397efa4a799b9d67c39b1bedbe3"
  end
  # BOTTLE-END

  on_macos do
    on_arm do
      url "https://github.com/oetiker/oxutrm/releases/download/v#{version}/oxutrm-#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "4531c7e1217851c904099b81269ab2451ca30edd7fce3c139b84d69a7c28e3f2" # mac-arm
    end
    on_intel do
      url "https://github.com/oetiker/oxutrm/releases/download/v#{version}/oxutrm-#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "8b618d5bda84f328c1e270e4a9e282cd3e34fccc38cc087841c0cf7a948d8086" # mac-x86
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/oetiker/oxutrm/releases/download/v#{version}/oxutrm-#{version}-x86_64-unknown-linux-musl.tar.gz"
      sha256 "a0c0276b405d1dfa611cd745f940035c65596599a7707936e6b44d4dcaee3094" # linux-x86
    end
    on_arm do
      url "https://github.com/oetiker/oxutrm/releases/download/v#{version}/oxutrm-#{version}-aarch64-unknown-linux-musl.tar.gz"
      sha256 "4fec4d303de347e86ad06624479af8126f073a6cd2d05f480d6e85c1a08c7687" # linux-arm
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
