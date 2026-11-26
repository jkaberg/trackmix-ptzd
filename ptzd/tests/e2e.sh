#!/bin/sh
# Builds ptzd and runs tests/e2e.py (ptzd run --mock driven by the real
# onvif-zeep-async 4.0.4 and conformance.py's Frigate port).
# VENV_PY = a python with onvif-zeep-async==4.0.4 (see README.md).
set -e
cd "$(dirname "$0")/.."
PY=${VENV_PY:-python3}
"$PY" -c 'import onvif' || { echo "set VENV_PY to a python with onvif-zeep-async==4.0.4" >&2; exit 2; }
cargo build --offline -q
PTZD=$PWD/target/debug/ptzd exec "$PY" tests/e2e.py
