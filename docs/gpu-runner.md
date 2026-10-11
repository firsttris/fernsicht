# GPU runners

This guide sets up a Bazzite machine so that

1. you (or Claude Code) can develop there in a **Distrobox** with access to
   the GPU, and
2. a **self-hosted GitHub runner** runs the GPU tests from CI on real
   hardware, that is VAAPI on the Radeon RX 7800 XT and NVENC/NVDEC on the
   GTX 1080.

Both machines are set up the same way. Only the switch `amd` or `nvidia`
differs. Each machine takes about 15 minutes.

## How it is structured

```text
Bazzite (host, stays unchanged)
├── Distrobox "fernsicht"            ← for development, shares your $HOME
│     Image: localhost/fernsicht-dev
└── Podman container "fernsicht-runner-amd|nvidia"  ← for CI, isolated
      Image: localhost/fernsicht-runner (= dev image + GitHub runner)
      runs as a systemd user service (Quadlet), starts with the machine
```

Why two containers? A Distrobox deliberately shares your whole home
directory with the host, including `~/.ssh`, your browser profile and
passwords. That is convenient for development. For a runner that executes
code from a **public** repo, it would be a risk. So the runner runs in its
own Podman container. It sees only the GPU and its own volumes, no
directories from the host.

## Requirements

| | AMD machine (RX 7800 XT) | NVIDIA machine (GTX 1080) |
|---|---|---|
| Bazzite image | regular Bazzite | Bazzite **with the closed NVIDIA driver** (`bazzite-nvidia`, *not* `-open`: the open kernel modules only support RTX 20xx and newer) |
| Check | `ls /dev/dri` shows `renderD128` | `nvidia-smi` shows the GTX 1080 |
| Additionally | nothing | CDI specification, see below |

**NVIDIA only:** Podman passes the GPU into containers via CDI. Check that
the specification exists:

```sh
nvidia-ctk cdi list        # must contain "nvidia.com/gpu=all"
```

If it does not, generate it once. Repeat this after every NVIDIA driver
update, unless Bazzite does it for you:

```sh
sudo nvidia-ctk cdi generate --output=/etc/cdi/nvidia.yaml
```

!!! note "GTX 1080"
    NVIDIA has announced the 580 driver branch as the last one with support
    for Pascal cards. That is enough for development. The card can encode
    and decode H.264 and HEVC, but not AV1.

## Step 1: Clone the repo

On the host, in a terminal (Ptyxis/Konsole):

```sh
git clone https://github.com/firsttris/fernsicht.git ~/fernsicht
cd ~/fernsicht
```

## Step 2: Distrobox for development

```sh
dev/setup.sh            # AMD machine
dev/setup.sh --nvidia   # NVIDIA machine
```

This builds the image `localhost/fernsicht-dev` with Rust, Node, VAAPI,
Vulkan and FFmpeg with all codecs (from RPM Fusion, like Bazzite itself)
and creates the Distrobox. Then:

```sh
distrobox enter fernsicht          # or fernsicht-nvidia
cd ~/fernsicht
dev/gpu-check.sh                   # what can the GPU do?
cargo test --workspace             # the existing test suite
```

`dev/gpu-check.sh` shows the GPU, the driver, and the VAAPI and Vulkan Video
capabilities. It also encodes and decodes two seconds of 1080p60 in
hardware: on AMD this uses VAAPI, on NVIDIA it uses NVENC/NVDEC. On AMD,
`vaapi-h264-encode` and `vaapi-h264-decode` should show **PASS**, on NVIDIA
`nvenc-h264-encode` and `nvdec-h264-decode`.

You can also start Claude Code inside the Distrobox. It has the same GPU
available there and can test hardware code directly.

## Step 3: Get a runner token

1. Open <https://github.com/firsttris/fernsicht/settings/actions/runners/new>
   (*Settings → Actions → Runners → New self-hosted runner*).
2. Under "Configure" there is a command with `--token XXXXX`. Copy only this
   token. You do not need the rest of the page; the script handles the
   download and configuration.

The token is valid for one hour and is needed only once, to register.

## Step 4: Set up the runner

**On the host**, not in the Distrobox:

```sh
cd ~/fernsicht
dev/runner/setup-runner.sh --gpu amd       # AMD machine
dev/runner/setup-runner.sh --gpu nvidia    # NVIDIA machine
```

The script asks for the token, but only after it has built the images
(10–20 minutes the first time). Because the token is valid for only one
hour, it is best to enter it beforehand, hidden. That way it also does not
end up in your shell history:

```sh
# bash
read -rsp "Token: " RUNNER_TOKEN && export RUNNER_TOKEN
# fish (default shell on many Bazzite installations)
read -gxsP "Token: " RUNNER_TOKEN
```

The script then does the following:

1. It checks GPU access (render node, or `nvidia-smi` and CDI).
2. It builds `localhost/fernsicht-dev` and, on top of it,
   `localhost/fernsicht-runner` with the current version of the GitHub
   runner.
3. It registers the runner once with the labels
   `self-hosted, linux, gpu-amd` or `gpu-nvidia`. The registration is stored
   in the volume `fernsicht-runner-<gpu>` and survives image updates.
4. It installs the systemd user service `fernsicht-runner-<gpu>` as a
   Quadlet under `~/.config/containers/systemd/`, enables "lingering" (the
   service also runs when you are not logged in) and starts it.
5. It runs `gpu-check.sh` in the runner container. If that works, the GPU
   also reaches the CI jobs.

## Step 5: Verify

- <https://github.com/firsttris/fernsicht/settings/actions/runners> should
  show the runner as **Idle**.
- Under <https://github.com/firsttris/fernsicht/actions/workflows/gpu.yml>,
  click **Run workflow**. The job `GPU · amd` or `GPU · nvidia` runs on your
  machine. Its summary shows the same table as `gpu-check.sh`.

On the machine itself:

```sh
systemctl --user status fernsicht-runner-amd
journalctl --user -u fernsicht-runner-amd -f
```

### Which machines the workflow uses

Without further configuration, the workflow sends jobs only to the AMD
runner. Once the NVIDIA machine is set up, create the repository variable
`GPU_RUNNERS` with the value `["amd","nvidia"]` under *Settings → Secrets
and variables → Actions → Variables*. From then on, both run.

## When the GPU jobs run

The workflow
[`.github/workflows/gpu.yml`](https://github.com/firsttris/fernsicht/blob/main/.github/workflows/gpu.yml)
runs

- on pushes to `main` that change code or `dev/`,
- every night (this catches Mesa and driver updates from Bazzite),
- on demand (*Run workflow*).

It **never** runs on pull requests, not even on those from forks.

If a machine is off, its job waits in the queue. GitHub cancels it after
24 hours, but you can also cancel it in the Actions tab. The regular CI
keeps running independently of this.

## Security

The repo is public, and a self-hosted runner executes code from the repo on
your machine. The safeguards:

- **Who can start code:** Only people who may push to `main`, that is, you.
  Pull requests do not trigger the workflow, and the job additionally checks
  the repo and the event.
- **What the code sees:** The container is rootless. Root in the container
  is your user on the host, but without any mounted host directories. Only
  the GPU and the volumes `fernsicht-runner-*` are visible.
- **What you should also configure:** Under *Settings → Actions → General*,
  for "Fork pull request workflows from outside collaborators", choose
  **"Require approval for all external contributors"**.
- **Known trade-off:** SELinux labels are disabled for the container
  (`label=disable`), because otherwise SELinux blocks GPU access. This is
  NVIDIA's documented setting for CDI devices. The isolation through
  rootless Podman and the absence of host mounts remains in place.

## Operation

| Task | Command |
|---|---|
| Rebuild after a Bazzite update | `dev/runner/setup-runner.sh --gpu amd` (does not register again) |
| Pause | `systemctl --user stop fernsicht-runner-amd` |
| Remove | `dev/runner/setup-runner.sh --gpu amd --remove`, then delete the runner on GitHub |
| Clear the build cache | `podman volume rm fernsicht-runner-cache` (stop the service first) |

The build cache (`/cache` in the container: Cargo registry and `target/`)
makes the runs after the first one fast.

## Known limitations

- **KMS capture needs `CAP_SYS_ADMIN` on the host.** The rootless runner
  does not have it. Capture tests via KMS therefore run in the Distrobox or
  on the host:
  `sudo setcap cap_sys_admin+p target/release/fernsicht-host-agent`.
  Encode, decode and Vulkan work in the runner.
- **Real glass-to-glass latency** is still measured by a person with a
  phone's slow-motion camera (see [latency-baseline.md](latency-baseline.md)).
  The runner measures the stages, not the screen.

## Troubleshooting

**"No access to /dev/dri/renderD128" (AMD).** On Fedora Atomic (Bazzite), system groups live
in `/usr/lib/group` and cannot be changed directly with `usermod`. This is
how you get into the `render` group; log in again afterwards:

```sh
ls -l /dev/dri/renderD128                       # show group and permissions
grep -E '^render:' /usr/lib/group | sudo tee -a /etc/group
sudo usermod -aG render "$USER"
```

**`vaapi-h264-encode` FAIL, but the GPU is there.** Then the image uses the
Mesa VA driver without H.264. Check with
`rpm -q mesa-va-drivers-freeworld` (it must be installed). Rebuild the
image with `dev/setup.sh` or `setup-runner.sh`.

**`nvidia-smi` fails in the container.** Usually the CDI specification is
out of date after a driver update:
`sudo nvidia-ctk cdi generate --output=/etc/cdi/nvidia.yaml`, then
`systemctl --user restart fernsicht-runner-nvidia`.

**Runner shows "Offline" on GitHub.** Check `systemctl --user status
fernsicht-runner-<gpu>` and `loginctl show-user "$USER" | grep Linger`
(must be `Linger=yes`).

**Token expired.** Get a new token (step 3) and run the script again. If
the runner is not registered yet, it asks for the token.
