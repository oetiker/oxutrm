# Homebrew formula for oxutrm. This repository is its own tap.
#
# The release build rewrites the version line, the four sha256 lines by their
# marker comments and the bottle block between the BOTTLE markers. Keep the
# markers, and keep each url line directly above its sha256 line.
class Oxutrm < Formula
  desc "Remote terminal that survives bad networks, changing addresses and NAT"
  homepage "https://github.com/oetiker/oxutrm"
  version "0.5.0"
  license "Apache-2.0"

  # BOTTLE-START
  bottle do
    root_url "https://github.com/oetiker/oxutrm/releases/download/v0.5.0"
    sha256 cellar: :any_skip_relocation, arm64_sonoma: "52220fcf10dd47c956a9f71cbe1251582d852f2abaefd1324c83e8edb305ab47"
  end
  # BOTTLE-END

  on_macos do
    on_arm do
      url "https://github.com/oetiker/oxutrm/releases/download/v#{version}/oxutrm-#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "4f101af7777dfdb90c980bb040c480324668902175b36f878be7b2b3faa10bfa" # mac-arm
    end
    on_intel do
      url "https://github.com/oetiker/oxutrm/releases/download/v#{version}/oxutrm-#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "ea8f19abd0f44f0ea205efb62fd9430fbfe66c42e4d5c9c1e40037e46ff5d903" # mac-x86
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/oetiker/oxutrm/releases/download/v#{version}/oxutrm-#{version}-x86_64-unknown-linux-musl.tar.gz"
      sha256 "0dabb92bce5d8c57bd39e64ef0bc5d94c6b264c86960d878d75c7e9f5fe05ee5" # linux-x86
    end
    on_arm do
      url "https://github.com/oetiker/oxutrm/releases/download/v#{version}/oxutrm-#{version}-aarch64-unknown-linux-musl.tar.gz"
      sha256 "e647950528d1641549d3f6d14ef7308dce7ec43f3a22e50f848a58bcbddd4b73" # linux-arm
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
