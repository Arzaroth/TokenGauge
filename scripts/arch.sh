#!/bin/bash
# Usage: scripts/arch.sh bin <x.y.z> <dist>
#        scripts/arch.sh source <x.y.z> <dist> <source tarball>
#
# Runs on Arch Linux as a regular user (makepkg refuses root), with
# base-devel, pacman-contrib and glib2 installed, plus cargo, nodejs and pnpm
# for `source`.
#
# bin: renders packaging/arch/tokengauge-bin for the release archives in
# <dist> and builds the x86_64 and aarch64 packages from it. source: renders
# packaging/arch/tokengauge for the given source tarball (the release's, or a
# `git archive` of the tree being tested) and builds it for this machine.
# Either way, the packages and the rendered PKGBUILD and .SRCINFO land in
# <dist>, the latter under <dist>/arch/<pkgname>.

set -euo pipefail

mode="${1:-}"
version="${2:-}"
if [[ ! $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] ||
  [[ $mode == bin && $# -ne 3 ]] || [[ $mode == source && $# -ne 4 ]] ||
  [[ $mode != bin && $mode != source ]]; then
  echo "usage: scripts/arch.sh bin <x.y.z> <dist> | source <x.y.z> <dist> <tarball>" >&2
  exit 2
fi
mkdir -p "$3"
dist="$(readlink -f "$3")"
tarball=""
[[ $mode == source ]] && tarball="$(readlink -f "$4")"

cd "$(dirname "$(readlink -f "$0")")/.."
pkgname=tokengauge
[[ $mode == bin ]] && pkgname=tokengauge-bin
work="$PWD/target/arch/$pkgname"
rendered="$dist/arch/$pkgname"

rm -rf "$work" "$rendered"
mkdir -p "$work" "$rendered"
sed "s/^pkgver=.*/pkgver=$version/" "packaging/arch/$pkgname/PKGBUILD" >"$work/PKGBUILD"

if [[ $mode == bin ]]; then
  for arch in x86_64 aarch64; do
    cp "$dist/tokengauge-v$version-linux-$arch.tar.gz" "$work/"
  done
else
  cp "$tarball" "$work/tokengauge-$version.tar.gz"
fi

cd "$work"
updpkgsums
makepkg --printsrcinfo >.SRCINFO
cp PKGBUILD .SRCINFO "$rendered/"

PKGDEST="$dist" makepkg --force --cleanbuild --noconfirm
if [[ $mode == bin ]]; then
  # Nothing in the -bin package is compiled, so the aarch64 one is built here
  # too, by telling makepkg it is on aarch64.
  sed -e 's/^CARCH=.*/CARCH="aarch64"/' -e 's/^CHOST=.*/CHOST="aarch64-unknown-linux-gnu"/' \
    /etc/makepkg.conf >makepkg-aarch64.conf
  PKGDEST="$dist" makepkg --force --cleanbuild --noconfirm --config makepkg-aarch64.conf
fi
ls -l "$dist"/*.pkg.tar.zst
