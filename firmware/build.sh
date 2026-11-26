#!/bin/sh
# Build, verify and test the custom image. Run as root. Never touches a camera.
# usage: build.sh
set -eu
cd "$(dirname "$0")"
stock=stock/upgrade/Reolink-TrackMix-PoE.5428_2604282150.IPC_529SD78MP.paks
ptzd=../ptzd/target/armv7-unknown-linux-musleabihf/release/ptzd
out=build/image
if [ ! -f "$stock" ]; then
    echo "missing $stock: download stock 5428 from Reolink, see stock/README.md" >&2
    exit 1
fi
if [ -n "$(ls -A "$out" 2>/dev/null)" ]; then
    echo "$out is not empty: move the old images away first" >&2
    exit 1
fi
(cd ../ptzd && cargo build --release --target armv7-unknown-linux-musleabihf)
set -- --stock "$stock" --ptzd "$ptzd"
python3 tools/build_image.py "$@" --outdir "$out"
python3 tools/verify_image.py "$@" --image "$out"/IPC_529SD78MP.5435_2604282150.pak | tail -1
python3 tools/test_tools.py "$@" --image "$out"/IPC_529SD78MP.5435_2604282150.pak | tail -1
