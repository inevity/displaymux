# lan-mouse + tv-multiview deployment

## Aim and scope

This Ansible playbook deploys the current three-host DisplayMux topology. It
installs a no-GTK Lan Mouse daemon on the Linux hub and the macOS and Windows
peers, installs `tv-multiview` on the Linux controller host, renders the
authenticated configurations, reconciles peer trust, configures native service
supervision and bounded logs, restarts changed runtimes, and verifies service
health and peer connectivity.

Two binary sources are supported:

- `native_build` distributes one source bundle and Cargo vendor archive, then
  builds the native binaries on their target hosts.
- `github_release` downloads the published native archives, verifies their
  manifest-provided SHA-256 digests, and skips native compilation.

Release mode deploys no-GTK Lan Mouse on Linux x86_64/ARM64, macOS
Intel/Apple Silicon, and Windows x86_64. It deploys `tv-multiview` only on the
Linux x86_64 controller. The Windows and macOS controller archives are
published for manual use but are not yet installed or supervised by this
playbook.

## Before running

**On the control node (example-user, running Ansible):**
```bash
ansible-galaxy collection install ansible.windows community.general
```

**On the Windows host, once, before this playbook can reach it at all:**
Enable OpenSSH Server and install PowerShell 7. Native-build mode additionally
requires Git, rustup, and Visual Studio Build Tools with the MSVC x64
component. The playbook installs its selected Rust toolchain only when the
native `rustc.exe` is absent.

**On macOS:**
Install the Xcode command-line tools. Native-build mode additionally requires
Homebrew rustup. The playbook installs its selected Rust toolchain only when
the native `rustc` is absent.

**Create local configuration before running:**

```bash
cp inventory.example.ini inventory.ini
cp group_vars/all.example.yml group_vars/all.yml
```

- `inventory.ini` contains real hostnames, addresses, and users.
- `group_vars/all.yml` contains the TV address, input mapping, server host,
  fingerprints, controller timeouts, install method, and native build features.
- Keep `lan_mouse_rust_toolchain` aligned with the version pinned by the root
  `rust-toolchain.toml` so native builds and release builds use the same compiler.
- Windows SSH credentials — keep them in Ansible Vault or pass them as extra
  variables instead of committing new plaintext credentials.
- Native-build mode uses the DisplayMux repository containing this `deploy/`
  directory. There is no configured source revision or lock-digest pin.

## Usage

First validate the playbook and prepare only controller-side inputs:

```bash
ansible-playbook -i inventory.ini playbook.yml --syntax-check
ansible-playbook -i inventory.ini playbook.yml --limit localhost
```

`--limit localhost` creates the persistent controller token and either prepares
the native source/vendor inputs or resolves and validates the selected GitHub
Release. It does not install or restart software on the managed hosts.

The default native-build configuration is:

```yaml
lan_mouse_install_method: native_build
```

To deploy published artifacts instead, use:

```yaml
lan_mouse_install_method: github_release
lan_mouse_github_repository: inevity/displaymux
lan_mouse_github_release_tag: REPLACE_WITH_RELEASE_TAG
```

`latest` is also supported. An explicit immutable tag is recommended for a
repeatable deployment. `GITHUB_TOKEN` is optional for this public repository;
export it when authenticated GitHub API rate limits are needed.

Run the complete deployment with:

```bash
ansible-playbook -i inventory.ini playbook.yml
```

Release mode resolves `latest` or the configured tag exactly once on the
Ansible controller. It rejects drafts, identity mismatches, duplicate or
missing assets, undeclared remote assets, and invalid digests. It validates the
release ID, tag commit, complete remote asset set, and
`osswitch-release-manifest.json`, then gives each host immutable tag URLs and
SHA-256 digests. The platform tasks verify the digest while downloading,
install the selected archive, and record the resolved identity. The controller
resolves the same tag again after the parallel host deployment and fails if the
release changed during the run. macOS re-signs the verified Lan Mouse binary
with its persistent local identity so the existing Accessibility grant remains
valid.

The first play always prepares the persistent controller token under
`~/.local/state/lan-mouse-deploy/`. In native-build mode it also creates a git
bundle from the monorepo commit and a root-lockfile Cargo vendor archive under
the ignored `deploy/` paths `osswitch-source.bundle` and
`osswitch-vendor-<lock-hash>.zip`.
Linux, macOS, and Windows run their selected install and service-restart
sequences concurrently.

### What GitHub Release mode installs

- Linux: the architecture-matched no-GTK Lan Mouse archive into
  `~/.local/bin/lan-mouse`, plus the Linux x86_64 controller archive into
  `~/.local/bin/tv-multiview`.
- macOS: the architecture-matched no-GTK Lan Mouse archive into
  `~/.local/bin/lan-mouse`, re-signed with the configured persistent identity.
- Windows: the no-GTK x86_64 Lan Mouse archive into the configured
  `lan_mouse_windows_install_dir` (by default `D:\lanmouse`).

GTK application archives and the Windows/macOS `tv-multiview` archives are not
consumed by this playbook.

## After running (one-time manual steps)

1. **LG client key**: the Linux host must already have
   `~/.config/lg-buddy/.aiopylgtv.sqlite`. The playbook deliberately fails
   instead of opening an unattended TV pairing flow.
2. **macOS Accessibility**: System Settings → Privacy & Security →
   Accessibility → enable it for lan-mouse once. The playbook creates and
   reuses a persistent local code-signing identity, so later rebuilt binaries
   retain the same designated requirement and do not require another grant.
   Replacing the Keychain identity or reinstalling the Mac requires a new
   one-time grant.
3. **Linux firewall**: admit UDP `4243` for lan-mouse and authenticated TCP
   `8765` for the controller. macOS and Windows application rules are managed
   by the playbook; Linux firewall policy remains externally owned.

## Runtime and logs

- Linux runs `tv-multiview.service` and `lan-mouse.service` as systemd user
  units. Inspect them with `journalctl --user -u tv-multiview -u lan-mouse`.
- macOS runs the native daemon as `com.feschber.lan-mouse`; persistent logs
  are `~/Library/Logs/lan-mouse.log` and `lan-mouse.err.log`, each with five
  10 MiB backups.
- Windows installs the native daemon at `D:\lanmouse\lan-mouse.exe` and runs it through
  `LanMouseDaemon`; native build assets are kept under
  `D:\lanmouse\build`. The task starts at interactive user logon. A
  parallel unlock-triggered invocation asks the existing daemon to re-enable
  input emulation without replacing its supervised process. The wrapper keeps
  the daemon alive across genuine process exits, while temporary Windows input
  denial leaves the daemon connected and not-ready until input emulation
  recovers. Bounded persistent logs remain under
  `%LOCALAPPDATA%\lan-mouse\logs` with five 10 MiB backups. The playbook does
  not enable automatic Windows logon: before the first interactive logon after
  a cold boot, Windows `SendInput` cannot control the secure login desktop and
  the peer must remain not ready.

Native-build runs replace the disposable build source and build all three hosts
against the same root lockfile and vendor archive. GitHub-release runs install
the controller-resolved, digest-verified assets without a native Lan Mouse or
TV-controller build. Release identity markers are written on each host. In both
modes the deployed Lan Mouse binary is the no-GTK variant and no legacy
`enter_hook` or compatibility protocol is used.
