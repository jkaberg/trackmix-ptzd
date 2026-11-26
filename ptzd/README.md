# ptzd

A static Rust daemon that serves ONVIF PTZ to Frigate 0.18.3 and drives the Reolink
TrackMix pan/tilt motors through `/dev/lens_zfi`. Status: `docs/design.md` §1.

- `docs/design.md`: design, operation, on-camera tests T1 to T16, config keys,
  and what the finding ids in comments mean. "plan §N" in comments.
- `../research/ptzd-hardware.md`: driver and firmware facts, tagged C or U, with
  addresses. "hw doc" in comments, hazards H1 to H11.

## Layout

| path | what |
|---|---|
| `crates/core` | shared types and traits. Change signatures last |
| `crates/hw` | `/dev/lens_zfi` (`LensZfiFd`), `Guarded` checks, `KernelSim` |
| `crates/motion` | state machine, limits, presets, persistence |
| `crates/onvif` | HTTP, SOAP, WS-UsernameToken, device/media/ptz services |
| `crates/reolink` | localhost Reolink API, login passthrough, self-movers, cfg files |
| `src` | `ptzd` binary: config, run, control socket, log |
| `runit` | service scripts, installed by the image as `/etc/ptzd/*` |
| `tests` | end-to-end tests with the real ONVIF library |

## Build and test

Rust 1.82 or newer. Dependencies are pinned in `Cargo.lock`; omit `--offline` on the
first build.

    cargo test --workspace --offline
    cargo run -- run --mock --config FILE     # simulated motors and Reolink API
    rustup target add armv7-unknown-linux-musleabihf
    cargo build --release --target armv7-unknown-linux-musleabihf

The armv7 build needs no cross compiler: `.cargo/config.toml` uses rust-lld.

With the real ONVIF client (python 3):

    python3 -m venv DIR && DIR/bin/pip install onvif-zeep-async==4.0.4
    export VENV_PY=DIR/bin/python              # absolute path
    sh tests/e2e.sh                            # about 2 minutes
    "$VENV_PY" crates/onvif/tests/conformance.py

With `--mock` set at least `runtime_dir` and `state_dir` in FILE, so no real
paths are touched. `conformance.py` drives the onvif crate's `fake_server`
example, built only if missing; rebuild after changes with
`cargo build -p ptzd-onvif --example fake_server`.

## Rules

- Evidence: every hardware or protocol constant cites its source (hw doc
  section, address, Frigate file:line) in a short comment. Anything not verified
  is a named config value or constant marked `// UNPROVEN (Tn)` with the test
  that settles it. No guessed numbers.
- std threads, no async. Edition 2021. `#![forbid(unsafe_code)]` everywhere except
  the ioctl module in `crates/hw`.
- Dependencies: only `quick-xml`, `sha1_smol`, `base64`, `nix`, `serde`,
  `serde_json`, `log`. Anything else must be justified; the stripped armv7 binary
  stays under 1.5 MB.
- No `unwrap()` or `expect()` on runtime paths: a panic aborts the daemon while the
  head may be moving.
- Tests write only to temp dirs and never talk to a camera.
- Never research how the camera stores or checks passwords. Auth uses only the
  public Reolink `Login` call.
- Comments: terse, no em-dashes.
