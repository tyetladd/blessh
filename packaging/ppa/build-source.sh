#!/bin/sh
# Builds an Ubuntu source package for a Launchpad PPA, in an Ubuntu container
# (dpkg-buildpackage -S), from the tarballs of packaging/vendor-tarball.sh.
#
#   packaging/ppa/build-source.sh <series> [ppa revision]
#
# series: noble (24.04, builds with rustc-1.89) or resolute, stonking and
# later (the default cargo is new enough). The maintainer, who signs the
# upload, comes from DEBFULLNAME and DEBEMAIL. Output: dist/ppa/<series>/.
# Unsigned (-us -uc); sign with `debsign` before `dput`.
set -eu
cd "$(dirname "$0")/../.."
SERIES=${1:?series}
REV=${2:-1}
: "${DEBFULLNAME:?set DEBFULLNAME}" "${DEBEMAIL:?set DEBEMAIL}"
VER=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
case $SERIES in
noble) RUST="cargo-1.89, rustc-1.89" ;;
*) RUST="cargo (>= 1.89), rustc (>= 1.89)" ;;
esac
DEBVER="$VER-0ppa$REV~${SERIES}1"
OUT=dist/ppa/$SERIES
rm -rf "$OUT" && mkdir -p "$OUT"
[ -f "dist/src/blessh-$VER.tar.gz" ] || packaging/vendor-tarball.sh

# Upstream tree plus vendor/ is the orig tarball (made in the container).
W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT
tar -C "$W" -xzf "dist/src/blessh-$VER.tar.gz"
tar -C "$W/blessh-$VER" -xJf "dist/src/blessh-$VER-vendor.tar.xz"

cp -R packaging/ppa/debian "$W/blessh-$VER/debian"
for s in postinst prerm postrm; do cp "packaging/linux/debian/$s" "$W/blessh-$VER/debian/$s"; done
sed -e "s/@MAINTAINER@/$DEBFULLNAME <$DEBEMAIL>/" -e "s/@RUST@/$RUST/" \
    "$W/blessh-$VER/debian/control.in" > "$W/blessh-$VER/debian/control"
rm "$W/blessh-$VER/debian/control.in"
cat > "$W/blessh-$VER/debian/changelog" <<CL
blessh ($DEBVER) $SERIES; urgency=medium

  * blessh $VER for the PPA: resumable sessions on L2CAP channels (--resume).

 -- $DEBFULLNAME <$DEBEMAIL>  $(LC_ALL=C date -R)
CL

docker run --rm -v "$W:/w" -w "/w/blessh-$VER" ubuntu:24.04 sh -ec "
    apt-get update -qq >/dev/null
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq dpkg-dev debhelper >/dev/null
    # GNU tar here: macOS tar adds metadata that dpkg-source reads as changes.
    tar -C /w -cJf /w/blessh_$VER.orig.tar.xz --exclude=blessh-$VER/debian blessh-$VER
    dpkg-buildpackage -S -d -us -uc -sa >/dev/null
    chown -R $(id -u):$(id -g) /w"
cp "$W"/blessh_"$DEBVER"* "$W/blessh_$VER.orig.tar.xz" "$OUT/"
ls -l "$OUT"
