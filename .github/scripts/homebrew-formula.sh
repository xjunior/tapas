#!/usr/bin/env bash
# Prints the Homebrew formula for a release.
#
#   .github/scripts/homebrew-formula.sh <tag> <dir>
#
# <dir> holds the release's tapas-<tag>-<target>.tar.gz.sha256 files.
set -euo pipefail

tag="$1"
dir="$2"
version="${tag#v}"
base="https://github.com/xjunior/tapas/releases/download/${tag}"

asset() {
  local sum
  sum="$(cut -d ' ' -f 1 "${dir}/tapas-${tag}-$1.tar.gz.sha256")"
  printf '      url "%s/tapas-%s-%s.tar.gz"\n      sha256 "%s"\n' "$base" "$tag" "$1" "$sum"
}

cat <<RUBY
class Tapas < Formula
  desc "Pulls a sample out of a MySQL database into a SQL file, keeping every reference intact"
  homepage "https://github.com/xjunior/tapas"
  version "${version}"
  license "MIT"

  on_macos do
    on_arm do
$(asset aarch64-apple-darwin)
    end
    on_intel do
$(asset x86_64-apple-darwin)
    end
  end

  on_linux do
    on_arm do
$(asset aarch64-unknown-linux-musl)
    end
    on_intel do
$(asset x86_64-unknown-linux-musl)
    end
  end

  def install
    bin.install "tapas"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/tapas --version")
  end
end
RUBY
