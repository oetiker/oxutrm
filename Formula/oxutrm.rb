# Homebrew formula for oxutrm. This repository is its own tap.
#
# The release build rewrites the version line, the four sha256 lines by their
# marker comments and the bottle block between the BOTTLE markers. Keep the
# markers, and keep each url line directly above its sha256 line.
class Oxutrm < Formula
  desc "Remote terminal that survives bad networks, changing addresses and NAT"
  homepage "https://github.com/oetiker/oxutrm"
  version "0.4.0"
  license "Apache-2.0"

  # BOTTLE-START
  bottle do
    root_url "https://github.com/oetiker/oxutrm/releases/download/v0.4.0"
    sha256 cellar: :any_skip_relocation, arm64_sonoma: "ee2a8c7da5ed31ae4499fe21fea1da7696155554e2887bd158b5f7764c559ec8"
  end
  # BOTTLE-END

  on_macos do
    on_arm do
      url "https://github.com/oetiker/oxutrm/releases/download/v#{version}/oxutrm-#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "6985c7024ae8ea1eea24807a0faff39ea5fec46ad92ccb4f3f5912de3e44514a" # mac-arm
    end
    on_intel do
      url "https://github.com/oetiker/oxutrm/releases/download/v#{version}/oxutrm-#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "23e174ae680be6a7732e436e079606fa951f4cd1f08ec52f53563a2239540d38" # mac-x86
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/oetiker/oxutrm/releases/download/v#{version}/oxutrm-#{version}-x86_64-unknown-linux-musl.tar.gz"
      sha256 "5108fea2553c2286161767c30a4daf44ab64b2f1930bb64ed3452593c46bbb03" # linux-x86
    end
    on_arm do
      url "https://github.com/oetiker/oxutrm/releases/download/v#{version}/oxutrm-#{version}-aarch64-unknown-linux-musl.tar.gz"
      sha256 "de68883462c2418a12d723e901dc7b18064d2fb799a2a22cd6fb226488d0a1ff" # linux-arm
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
