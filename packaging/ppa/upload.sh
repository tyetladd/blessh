#!/bin/sh
# Signs and uploads the source packages of packaging/ppa/build-source.sh to a
# Launchpad PPA, from an Ubuntu container that uses this machine's GnuPG
# keys (debsign, dput).
#
#   packaging/ppa/upload.sh <launchpad user>/<ppa> <series>...
#
# The key must be the one registered on Launchpad, and its user ID must be
# DEBFULLNAME <DEBEMAIL> as written into debian/changelog.
set -eu
cd "$(dirname "$0")/../.."
PPA=${1:?launchpad-user/ppa-name}
shift
[ $# -gt 0 ] || { echo "name at least one series" >&2; exit 1; }
for s in "$@"; do
    ls dist/ppa/"$s"/*_source.changes > /dev/null
done
docker run --rm -it -v "$PWD/dist/ppa:/ppa" -v "$HOME/.gnupg:/root/.gnupg" ubuntu:24.04 sh -ec "
    apt-get update -qq >/dev/null
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq devscripts dput gnupg >/dev/null
    export GPG_TTY=\$(tty)
    for s in $*; do
        cd /ppa/\$s
        debsign --re-sign *_source.changes
        dput ppa:$PPA *_source.changes
    done"
