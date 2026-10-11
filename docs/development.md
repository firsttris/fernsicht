# Development

How to build Fernsicht from source, run it without installing, check changes and make a release.
Testing is described in detail on its own page: [testing](testing.md).

## Requirements

- Rust (stable) and, for the web parts and the app, Node.js with pnpm (`corepack enable`).
- For hardware video: FFmpeg and libva development headers (`vaapi`), the NVIDIA driver at runtime
  (`nvidia`, CUDA is loaded dynamically).
- For the app: WebKitGTK (Tauri).

On **Bazzite** (or any immutable Fedora) everything lives in a Distrobox container:
`dev/setup.sh` builds the dev container and creates the box, `dev/setup.sh --nvidia` the variant with
NVIDIA's libraries. `dev/gpu-check.sh` shows what the GPU can do and tests hardware encode and decode.

## Build and run from source

```sh
cargo build --release

# Host, the first time with --pair: shows a PIN for pairing
./target/release/fernsicht-host-agent --pair

# Client (second terminal or second computer): find hosts in the network …
./target/release/fernsicht-client discover
# … pair once (name from discover, or an address) …
./target/release/fernsicht-client pair <host> <PIN>
# … then connect by name or address
./target/release/fernsicht-client <host-name> --fps 60
# with 1 % artificial packet loss
./target/release/fernsicht-client <host-name> --loss 0.01 --duration 10
```

Without features this runs the test pattern and the synthetic codec, so it works on any machine.
With real screen and hardware video:

```sh
# AMD/Intel: real video from the monitor (as root, see KMS capture)
cargo build --release -p fernsicht-host-agent --features vaapi,kms
sudo ./target/release/fernsicht-host-agent --capture kms --encoder vaapi
cargo build --release -p fernsicht-client --features vaapi,window
./target/release/fernsicht-client <host-name>   # window, Ctrl+Alt+Shift+Q quits
# no window, video into a file (ffplay -framerate 60 ~/test.h264)
./target/release/fernsicht-client <host-name> --headless --record ~/test.h264

# NVIDIA: client with NVDEC, host with NVENC
cargo build --release -p fernsicht-client --features nvidia,window
cargo build --release -p fernsicht-host-agent --features nvidia,kms
sudo ./target/release/fernsicht-host-agent --capture kms --encoder nvenc
```

Each feature is listed in [modules](modules.md#build-features); all options are on the
[command line](command-line.md) page.

### The app

```sh
pnpm install && pnpm --filter @fernsicht/client-ui build
cargo build --release -p fernsicht-client --features vaapi,window   # the stream window
cargo build --release --manifest-path apps/desktop/Cargo.toml        # the app
FERNSICHT_CLIENT=target/release/fernsicht-client apps/desktop/target/release/fernsicht
```

The app looks for the client next to itself, then in the `PATH`. `FERNSICHT_CLIENT` shows it the way
as long as nothing is installed.

### The interfaces in a browser

With demo data, no host needed:

```sh
pnpm install
pnpm dev:client   # the app's interface on http://localhost:1420
pnpm dev:viewer   # the web viewer on http://localhost:5174 (demo; the real one is served by the host)
```

### Installing a local build

```sh
./packaging/build.sh
./packaging/install-app.sh            # Fernsicht in the start menu
sudo ./packaging/install-host.sh      # only on computers you want to control
fernsicht-host-agent pair             # a PIN for a new device
fernsicht-host-agent status           # connection, paired devices
```

See [installation](installation.md) for the details.

## Checks

What CI runs, to run before pushing:

```sh
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
pnpm install && pnpm format:check && pnpm -r typecheck && pnpm test
pnpm test:coverage && pnpm test:e2e
pnpm --filter @fernsicht/client-ui build     # the app needs the built UI
cd apps/desktop && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
shellcheck packaging/*.sh scripts/*.sh
```

CI runs on every push; nightly it adds 15 minutes of fuzzing per target and the soak test. The GPU
tests run on a self-hosted runner ([GPU runner](gpu-runner.md)).

## Screenshots

The screenshots in the README and these docs are made by Playwright from the app's interface and the
web viewer with demo data:

```sh
pnpm screenshots
```

`scripts/screenshots.sh` builds both, serves them on ports 4311 and 4312 and runs
`web/e2e/screenshots.mjs`, which writes `docs/screenshot-*.png`, the README banner and the social
preview image. The **Screenshots** workflow does the same in GitHub Actions and commits changed
images.

## Documentation

These pages are built with [MkDocs Material](https://squidfunk.github.io/mkdocs-material/) and
published to GitHub Pages by the **Docs** workflow on every push to `main`. Locally:

```sh
python -m venv .venv && . .venv/bin/activate
pip install -r requirements-docs.txt
mkdocs serve        # http://127.0.0.1:8000
```

## Releases

The **Bump version** workflow in GitHub Actions raises the version and sets the tag; **Release** then
builds the AppImage and publishes it. Locally `./packaging/appimage.sh` builds the same AppImage.

## Tips

Drops on keyframes (`RcvbufErrors` in `/proc/net/snmp`) mean the UDP buffers are too small; raise
them:

```sh
sudo sysctl -w net.core.rmem_max=8388608 net.core.wmem_max=8388608
```

The rules for working on the project and the state of the open live tests are in the
[handover notes](handover.md).
