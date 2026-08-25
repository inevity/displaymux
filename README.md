# DisplayMux

DisplayMux coordinates keyboard, pointer, clipboard, and a shared multi-input
display as one host-switch transaction. It combines a generalized Lan Mouse
fork with a display-controller daemon and native deployment automation.

## Capabilities

- Share keyboard and pointer input across Linux, macOS, and Windows hosts.
- Transfer clipboard text through an authenticated peer transport.
- Coordinate display input selection with remote-input readiness.
- Keep keyboard and pointer ownership local until the requested switch is
  verified and committed.
- Recover to the configured server when a request expires, a peer disconnects,
  or display verification fails.
- Deploy native builds or digest-verified GitHub Release artifacts with the same
  Ansible playbook.

## How It Works

```text
Lan Mouse edge intent
        |
        v
authenticated controller request
        |
        v
tv-multiview observes display state + peer readiness
        |
        v
verified grant and atomic input commit
        |
        v
remote host receives keyboard, pointer, and clipboard ownership
```

The active display route and keyboard/pointer ownership are separate pieces of
state. A user may select a display input manually while the server retains
keyboard/pointer ownership; DisplayMux only performs automatic fallback while
resolving an active switch transaction.

## Repository Components

- [`lan-mouse/`](lan-mouse/README.md): cross-platform input and clipboard
  transport. The default build retains the GTK application; no-GTK binaries
  are additional service assets.
- [`tv-multiview/`](tv-multiview/README.md): controller daemon that owns display
  observation, display input changes, two-phase switch grants, and fail-local
  recovery. Its current display adapter supports LG OLED webOS displays.
- [`deploy/`](deploy/README.md): Ansible native-build and verified
  GitHub-release deployment modes for Linux, macOS, and Windows hosts.
- [`docs/`](docs/) and [`tla/`](tla/README.md): architecture decisions,
  implementation plans, and formal safety/liveness models.

Lan Mouse talks to `tv-multiview` through its authenticated HTTP controller
client. Only `tv-multiview` talks the device-specific control protocol to the
display. Keyboard/pointer ownership remains on the configured Lan Mouse server
until the controller verifies the display and remote-input readiness
transition.

## Platform Support

| Component | Linux | macOS | Windows |
|---|---:|---:|---:|
| Lan Mouse GTK application | Yes | Yes | Yes |
| Lan Mouse no-GTK service | Yes | Yes | Yes |
| Clipboard transport | Yes | Yes | Yes |
| Packaged `tv-multiview` service | Yes | Not yet | Not yet |
| Native Ansible deployment | Yes | Yes | Yes |

The architecture colocates `tv-multiview` with the configured Lan Mouse
hub/server host; controller ownership is not intrinsically tied to Linux. The
current deployment selects Linux as that server and currently packages the
display-controller service only for Linux. macOS and Windows display-controller
service/release integration remains separate from the architecture.

## Current Display Support and Implementation Status

The current display adapter supports only LG webOS TVs, specifically OLED
models that expose multiple HDMI inputs through SSAP. Other display families
and displays without the required HDMI/SSAP capabilities are not currently
supported. The TV is a display in the DisplayMux domain; being a television
does not define its architectural role.

`tv-multiview` display-controller progress is distinct from Lan Mouse peer
support:

- **Linux integration:** implemented, built and tested in CI, packaged as the
  current display-controller release asset, and integrated with systemd
  deployment.

Required controller-platform TODOs:

- [ ] Implement and pass native macOS `tv-multiview` CI builds and tests.
- [ ] Add macOS display-controller service supervision, release assets, and live
  validation.
- [ ] Implement and pass native Windows `tv-multiview` CI builds and tests.
- [ ] Add Windows display-controller service supervision, release assets, and
  live validation.

Until these TODOs are complete, macOS and Windows operate as Lan Mouse peers
but are not completed display-controller platforms.

## Usage

### 1. Configure Lan Mouse

Lan Mouse reads `config.toml` from these default locations:

- Linux and macOS: `~/.config/lan-mouse/config.toml`
- Windows: `%LOCALAPPDATA%\lan-mouse\config.toml`

Each host needs the same controller URL/token and must authorize the TLS
fingerprints of hosts that may connect to it. The client `position` is where
that client is located relative to the machine whose file you are editing.

Example server configuration with a macOS host on the right and a Windows host
on the left:

```toml
port = 4243
release_bind = ["KeyA", "KeyS", "KeyD", "KeyF"]
emulation_display = "REPLACE_SERVER_DISPLAY_NAME"

[clipboard]
enabled = true
max_bytes = 3145728

[switch_controller]
url = "http://REPLACE_CONTROLLER_ADDRESS:8765"
token = "REPLACE_CONTROLLER_TOKEN"
local_host = "linux"
server_host = "linux"
http_timeout_ms = 3000
request_timeout_ms = 75000
poll_interval_ms = 250
edge_double_tap_ms = 3000
lease_ttl_ms = 90000
renew_interval_ms = 5000

[authorized_fingerprints]
"REPLACE_MACOS_TLS_FINGERPRINT" = "mac-host"
"REPLACE_WINDOWS_TLS_FINGERPRINT" = "windows-host"

[[clients]]
hostname = "mac-host"
ips = ["REPLACE_MACOS_ADDRESS"]
port = 4243
position = "right"
activate_on_startup = true
switch_target = "mac"

[[clients]]
hostname = "windows-host"
ips = ["REPLACE_WINDOWS_ADDRESS"]
port = 4243
position = "left"
activate_on_startup = true
switch_target = "windows"
```

Example configuration for the macOS peer, where the server is to its left:

```toml
port = 4243
emulation_display = "REPLACE_MACOS_DISPLAY_NAME"

[clipboard]
enabled = true
max_bytes = 3145728

[switch_controller]
url = "http://REPLACE_CONTROLLER_ADDRESS:8765"
token = "REPLACE_CONTROLLER_TOKEN"
local_host = "mac"
server_host = "linux"
http_timeout_ms = 3000
request_timeout_ms = 75000
poll_interval_ms = 250
edge_double_tap_ms = 3000
lease_ttl_ms = 90000
renew_interval_ms = 5000

[authorized_fingerprints]
"REPLACE_SERVER_TLS_FINGERPRINT" = "server-host"

[[clients]]
hostname = "server-host"
ips = ["REPLACE_SERVER_ADDRESS"]
port = 4243
position = "left"
activate_on_startup = true
switch_target = "linux"
```

For the Windows peer on the server's left, use the same peer configuration with
`local_host = "windows"`, the Windows display name, and `position = "right"`
for its server client.

The fingerprint shown by Lan Mouse on one host must be placed in
`authorized_fingerprints` on the host accepting that connection. See the
[Lan Mouse connection guide](lan-mouse/README.md#usage) for the authorization
flow. Start each configured instance with:

```bash
lan-mouse daemon
```

The Ansible deployment under [`deploy/`](deploy/README.md) renders these files
from sanitized inventory and group-variable examples when managing all hosts.

### 2. Move between hosts

With the example layout:

- Move toward the server's **right** edge to target macOS.
- Move toward the server's **left** edge to target Windows.
- On macOS, move left to return to the server.
- On Windows, move right to return to the server.

Leaving the server for a non-server host uses a deliberate two-crossing guard:

1. Push through the configured edge once. DisplayMux primes the intent, releases
   capture, and performs no controller switch.
2. Move away from that edge. The native capture backend must report the retreat.
3. Push through the same edge again within `edge_double_tap_ms` (3000 ms in the
   example).
4. DisplayMux verifies that the same target and peer session are still current,
   and that the peer is online with both keyboard and pointer emulation ready.
5. For a target with a display route, the controller verifies the selected route
   and signal before issuing a bounded grant. A route-free peer skips the display
   command but retains the readiness and lease gates.
6. Lan Mouse revalidates the grant and peer-session identity at commit time, then
   transfers keyboard and pointer ownership together.

If any check fails or expires, keyboard/pointer ownership stays on or returns to
the configured server. A manual display selection made while no switch
transaction is active remains authoritative and is not automatically undone.

## Build and Test

The repository is one Cargo workspace with one lockfile. Rustup reads the root
`rust-toolchain.toml` and installs the pinned Rust toolchain and required
components:

```bash
cargo check --locked --workspace
cargo test --locked --workspace
cargo build --locked -p lan-mouse
cargo build --locked --profile lan-mouse-release -p lan-mouse
cargo build --locked --release -p tv-multiview
```

The first Lan Mouse build uses its default features, including GTK. Native
service builds disable default features and select only the platform-specific
input backends documented under [`deploy/`](deploy/README.md). Platform-specific
system libraries required by the GTK and input backends are documented in the
Lan Mouse component repository history and deployment tasks.

For a smaller controller-only check:

```bash
cargo check --locked -p tv-multiview
cargo test --locked -p tv-multiview
```

## Deployment

Start with the sanitized examples in `deploy/`:

```bash
cd deploy
cp inventory.example.ini inventory.ini
cp group_vars/all.example.yml group_vars/all.yml
ansible-playbook -i inventory.ini playbook.yml
```

Real inventory, credentials, fingerprints, display addresses, and display
mappings remain ignored local files. See `deploy/README.md` for native-build and
immutable release-manifest deployment semantics.

The playbook supports two installation modes:

- `native_build` builds every native host from one Git commit, root lockfile,
  and vendor archive.
- `github_release` resolves one immutable release identity and verifies every
  downloaded asset digest before installation.

Deployment is intentionally separate from publication. Creating a GitHub
Release does not install binaries, restart services, or change the display input.

## Configuration and Network Boundary

Start from the committed example files only. Keep the real files local:

```text
deploy/inventory.example.ini      -> deploy/inventory.ini
deploy/group_vars/all.example.yml -> deploy/group_vars/all.yml
```

The default deployment uses UDP port `4243` for Lan Mouse input transport and
authenticated TCP port `8765` for controller requests. Firewall policy for the
current Linux deployment host remains operator-owned.

Do not commit passwords, controller tokens, certificate fingerprints, display
addresses, display identifiers, or real inventory. The repository ignore rules
cover the standard local configuration and generated native-build artifacts.

## Releases

One release tag identifies the workspace source and complete native artifact
set. Default GTK archives remain available alongside no-GTK Linux, Windows, and
macOS assets. The initial controller asset is Linux x86_64 only. Every archive
contains applicable license and README material, and the generated release
manifest binds the release ID, tag commit, names, sizes, and SHA-256 digests.

Release automation creates and verifies a draft before publication. It does not
deploy binaries or restart hosts.

- A manually dispatched `staging-*` tag produces a prerelease.
- Pushing a `v*` tag produces a final release.
- Staging and final tags may point to the same commit so the final release
  rebuilds the exact source that passed staging.

Release tags are immutable publication identities. A source change requires a
new staging run and a new final version tag.

## Engineering Documentation

- [Domain language](CONTEXT.md)
- [Display-switch implementation plan](docs/plan_main_fullscreen_multiview_switch_implementation.md)
- [Atomic Lan Mouse input gate](docs/plan_lan_mouse_atomic_input_gate.md)
- [Clipboard handoff plan](docs/clipboardplan.md)
- [Protocol compatibility design](docs/protocolcompatibilitydesign.md)
- [TLA+ models](tla/README.md)

## Contributing

Keep behavioral changes focused and add tests for the invariant being changed.
Before submitting a change, run the applicable subset of:

```bash
cargo check --locked --workspace
cargo test --locked --workspace
python3 -m unittest discover -s tests -v
python3 -m unittest discover -s deploy/tests -v
```

Changes to deployment should also pass an Ansible syntax check with the
sanitized example inventory. Never use a real inventory in CI.

## License and Attribution

DisplayMux's original components are licensed under GPL-3.0-or-later; see
`LICENSE`. The imported Lan Mouse history, attribution, and component license
are retained under `lan-mouse/`. Third-party dependencies remain governed by
their own licenses.
