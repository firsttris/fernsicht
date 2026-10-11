# Notes for Claude

## Documentation

The project's documentation is in `docs/` (https://firsttris.github.io/fernsicht/). Read what
touches the change before starting: `architecture.md` and `modules.md` for how the pieces fit,
`video.md` for capture, encoders and the renderer, `protocol.md` for the packets, `security.md`
for pairing and sessions, `development.md` for building and checking, `testing.md` for the test
levels, `performance.md` and `next-steps.md` for what is planned. `handover.md` has the machines,
what has been seen working live and what has not. `code-review.md` lists the open review findings;
work through them from top to bottom and set each one's status when it is done.

## Reference implementations

Base low-level details on the sources and specifications, not on memory:

- **Vulkan:** the specification and its valid-usage IDs (VUIDs); name the VUID in a comment where it
  settles a detail.
- **FFmpeg:** `libavcodec/nvenc.c`, `vaapi_encode*.c`, `libavutil/hwcontext_*.c` for what an option
  really does.
- **Kernel DRM/KMS:** `include/uapi/drm/drm.h`, `drm_mode.h`, the kernel's KMS documentation.
- **Low-latency streaming:** Sunshine and Moonlight (`moonlight-common-c`) for what a feature costs
  and how they solved it; WebRTC (libwebrtc, str0m) for congestion control, NACK and RTX.

Clone them shallowly (`git clone --depth 1`) into the scratchpad and name the source file in
comments and commit messages where it settles a detail.

## Working rules

- **Never without an explicit OK** move the mouse or keyboard, play sound or open windows on a
  desktop where the user sits. Ask first and wait. A virtual display (Xvfb) is fine.
- **Commands as root** (`sudo`, `pkexec`) are run by the user; don't work around that.
- **The user's processes** (e.g. their running host) are not stopped; use other ports for tests.
- **GPU tests on a runner machine** (the NVIDIA box runs the `gpu-nvidia` runner): only when the
  runner is idle (no `Runner.Worker` process), and with `--test-threads=1` as CI does. NVENC allows
  only a few sessions at once; tests running beside the runner's fail or hang it.
- **Committing and pushing:** commit on `main`; check `git status --short` for stray files
  (`target/`, build output) first. Push over SSH to both branches:
  `git push git@github.com:firsttris/fernsicht.git HEAD:main` and `HEAD:ccr-a9432ae9-5iz8to`.
  Afterwards check CI.
- **Language:** documentation, README, code comments, identifiers and commit messages in English;
  the app's user interface stays German.

## With every change

Update what the change affects, in the same commit, and only that:

- the tests: unit, property, integration or E2E tests for what changed (GPU paths also in
  `crates/codec/tests`, `tests/e2e/tests/gpu.rs`, which run on the GPU runners);
- the docs in `docs/` that describe it (and `code-review.md`'s status when it fixes a finding);
- the README when a feature a user sees is added, changed or removed, or a number it quotes changes.

## Code style

Functional, not object-oriented:

- **Data in, data out.** Pure functions that take data and return data; side effects (sockets,
  devices, GPU, files) at the edges, in thin functions that call the pure ones. Pure logic gets
  unit tests.
- **Rust:** plain structs and enums with `match`, iterators instead of index loops, free functions
  instead of methods that don't need `self`. Traits only where implementations are really swapped
  (the backends `FrameSource`, `Encoder`, `Decoder`, `Presenter`, `InputSink`, `AudioSource`,
  `AudioSink`, and their test doubles); no trait hierarchies, no traits for a single type. A
  struct that owns a hardware handle (RAII) may have methods; keep the logic in free functions it
  calls.
- **TypeScript/React:** function components and hooks; no classes. State machines as a pure
  `step(state, event) → { state, effects }` or a closure from a `create…()` function; errors as
  values (discriminated unions), not subclasses. Lookup tables instead of long `switch`es.
- **Size:** a file does one thing; split by domain once it grows past ~400 lines, a function past
  ~80 (a component past ~150).
- **A refactor changes no behaviour.** Prove it: the same tests before and after, a sorted line
  diff for moves.
- **Latency is the product.** No allocation, lock or syscall on a per-frame or per-packet path
  without a reason in a comment; measure before and after (overlay, `cargo bench`, GPU tests).

## Checks

Green before every push:

```sh
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
pnpm format:check && pnpm -r typecheck && pnpm test
pnpm --filter @fernsicht/client-ui build     # the app needs the built UI
cd apps/desktop && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
shellcheck packaging/*.sh scripts/*.sh dev/*.sh dev/runner/*.sh
mkdocs build --strict                        # when docs/ or mkdocs.yml changed
```

Code behind the `vaapi`, `nvidia`, `kms` and `window` features is only checked where its
libraries are installed; build it with those features too when touching it.
