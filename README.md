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
| Native `tv-multiview` CI | Yes | Yes | Yes |
| Native `tv-multiview` release archive | Yes | Yes | Yes |
| Packaged `tv-multiview` service | Yes | Not yet | Not yet |
| Native Ansible deployment | Yes | Yes | Yes |

Managed `tv-multiview` service integration is currently available only when
Linux is selected as the Lan Mouse hub/server. macOS and Windows controller
archives are available, but managed service integration is not yet provided on
those platforms.

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
- **Native controller CI:** configured for Linux x86_64, Windows x86_64, macOS
  Intel, and macOS Apple Silicon.
- **Native controller release packages:** configured for Linux x86_64, Windows
  x86_64, macOS Intel, and macOS Apple Silicon.

Controller-platform implementation progress:

- [x] Add native macOS `tv-multiview` CI builds and tests.
- [x] Add native macOS `tv-multiview` release assets.
- [ ] Add macOS display-controller service supervision and live validation.
- [x] Add native Windows `tv-multiview` CI builds and tests.
- [x] Add native Windows `tv-multiview` release assets.
- [ ] Add Windows display-controller service supervision and live validation.

The macOS and Windows controller packages are available for manual use, but
those operating systems are not completed display-controller platforms until
their service supervision and live validation TODOs are complete.

## Usage

### 1. Validate a release archive

Download the archive and `osswitch-release-manifest.json` from the same
release. Find the archive's entry in the manifest, calculate the archive's
SHA-256 digest, and compare it with the entry's `sha256` value. Do not install
the archive if the values differ.

### 2. Install a `.tar.gz` archive

After validation, extract the archive. On Linux, `/usr/local/bin` is the
suggested destination for a standalone executable; install any included Debian
package or AppImage using its normal system location.

### 3. Install a `.zip` archive

After validation, extract the archive. On macOS, use `/usr/local/bin` for a
standalone executable or `/Applications` for an application bundle. On Windows,
use `%LOCALAPPDATA%\DisplayMux\<component>` and keep bundled DLL files beside
the executable.

### 4. Configure Lan Mouse

Lan Mouse reads `config.toml` from these default locations:

- Linux and macOS: `~/.config/lan-mouse/config.toml`
- Windows: `%LOCALAPPDATA%\lan-mouse\config.toml`

The Ansible deployment derives every generated role from one variable in
`deploy/group_vars/all.yml`:

```yaml
lan_mouse_server_host: linux  # linux|mac|windows
```

After the inventory, displays, fingerprints, and controller key paths have been
configured once, changing only `lan_mouse_server_host` causes the selected host
to receive the Lan Mouse hub and `tv-multiview` configuration. The other two
hosts receive Lan Mouse client configurations that point to the selected host.
Controller URLs, bind address, trust entries, client identities, and edge
positions are generated from the same selector.

The fingerprint shown by Lan Mouse on one host must be placed in
`authorized_fingerprints` on the host accepting that connection. See the
[Lan Mouse connection guide](lan-mouse/README.md#usage) for the authorization
flow. Start each configured instance with:

```bash
lan-mouse daemon
```

The Ansible deployment under [`deploy/`](deploy/README.md) renders these files
from the sanitized inventory and group-variable examples.

### 5. Move between hosts

Movement follows `lan_mouse_host_positions` in the Ansible configuration. The
selected hub uses its row for both clients; each generated client uses the
inverse entry to return to the hub.

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

The Ansible playbook manages one Linux, one macOS, and one Windows host. The
`lan_mouse_server_host` variable selects which generated configuration is the
hub/controller and which two are clients. The playbook installs selected
binaries, renders authenticated configuration, reconciles certificate trust,
configures native service supervision and logs, restarts changed runtimes, and
verifies the implemented service paths.

Install the required Ansible collections, then create ignored local
configuration from the sanitized examples:

```bash
ansible-galaxy collection install ansible.windows community.general
cd deploy
cp inventory.example.ini inventory.ini
cp group_vars/all.example.yml group_vars/all.yml
```

Populate the inventory and group variables with the three hosts, display
mapping, fingerprints, controller settings, and one installation mode. Real
inventory, credentials, fingerprints, display addresses, and display mappings
remain ignored local files.

The playbook supports two installation modes:

- `native_build` builds every native host from one Git commit, root lockfile,
  and vendor archive.
- `github_release` resolves one immutable release identity and verifies every
  downloaded asset digest before installation.

For a native build, retain the default:

```yaml
lan_mouse_install_method: native_build
```

For GitHub Release deployment, set an explicit published tag when practical:

```yaml
lan_mouse_install_method: github_release
lan_mouse_github_repository: inevity/displaymux
lan_mouse_github_release_tag: REPLACE_WITH_RELEASE_TAG
```

`latest` is also accepted. `GITHUB_TOKEN` is optional for this public
repository and can be exported to raise GitHub API rate limits. Validate the
playbook and resolve controller-side inputs before the full deployment:

```bash
ansible-playbook -i inventory.ini playbook.yml --syntax-check
ansible-playbook -i inventory.ini playbook.yml --limit localhost
ansible-playbook -i inventory.ini playbook.yml
```

In `github_release` mode, Ansible deploys no-GTK Lan Mouse archives on Linux,
macOS, and Windows. When Linux is selected as `lan_mouse_server_host`, it also
deploys the managed `tv-multiview-linux-x86_64.tar.gz` service. For macOS or
Windows selection, Ansible generates the controller configuration but does not
yet supervise the controller process. Every download is checked against
`osswitch-release-manifest.json`; release identity is recorded on each host and
resolved again after deployment to reject a release that changed mid-run.

After deployment, Linux must report Lan Mouse active and, when Linux is the
selected controller, `tv-multiview` active. macOS must have the LaunchAgent
loaded, and Windows must have the `LanMouseDaemon` scheduled task. Detailed
prerequisites, one-time permissions, install locations, logs, and recovery
checks are in [`deploy/README.md`](deploy/README.md).

Deployment is intentionally separate from publication. Creating a GitHub
Release does not install binaries, restart services, or change the display input.

## Configuration and Network Boundary

Start from the committed example files only. Keep the real files local:

```text
deploy/inventory.example.ini      -> deploy/inventory.ini
deploy/group_vars/all.example.yml -> deploy/group_vars/all.yml
```

The default deployment uses UDP port `4243` for Lan Mouse input transport and
authenticated TCP port `8765` for controller requests. Firewall policy on
Linux remains operator-owned.

Do not commit passwords, controller tokens, certificate fingerprints, display
addresses, display identifiers, or real inventory. The repository ignore rules
cover the standard local configuration and generated native-build artifacts.

## Releases

One release tag identifies the workspace source and complete native artifact
set. Default GTK archives remain available alongside no-GTK Linux, Windows, and
macOS assets. Controller archives are published for Linux x86_64, Windows
x86_64, macOS Intel, and macOS Apple Silicon; Ansible service deployment
currently consumes only the Linux controller archive. Every archive contains
applicable license and README material, and the generated release manifest
binds the release ID, tag commit, names, sizes, and SHA-256 digests.

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
