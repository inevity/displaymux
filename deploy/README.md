# lan-mouse + tv-multiview deployment

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

**Edit before running:**
- `inventory.ini` — real hostnames, IPs, users.
- `group_vars/all.yml` — TV address, HDMI mapping, server host, fingerprints,
  controller timeouts, install method, and native build features.
- Windows SSH credentials — keep them in Ansible Vault or pass them as extra
  variables instead of committing new plaintext credentials.
- The sibling `../../lan-mouse` checkout is the source deployed by the
  playbook. There is no configured revision pin or installed build-ID cache.

Run:
```bash
ansible-playbook -i inventory.ini playbook.yml
```

The default `lan_mouse_install_method: native_build` preserves the existing
local-checkout build. To deploy no-GTK binaries from a public GitHub Release
instead, set the repository and select either `latest` or a release tag:

```yaml
lan_mouse_install_method: github_release
lan_mouse_github_repository: owner/lan-mouse
lan_mouse_github_release_tag: latest
```

Release mode downloads the matching Linux, macOS, or Windows asset directly on
each host. It does not copy source, vendor Lan Mouse dependencies, or build Lan
Mouse. The macOS host still re-signs the downloaded binary with its stable local
identity so the existing Accessibility grant remains valid.

The first play always prepares the persistent controller token under
`~/.local/state/lan-mouse-deploy/`. In native-build mode it also creates a git
bundle from the local checkout and a locked Cargo vendor archive under `/tmp`.
`--limit localhost` runs only that preparation play; it does not deploy or
restart any host. The native-build vendor tree needs roughly 640 MiB with the
current lockfile, plus its archive and Cargo cache. Linux, macOS, and Windows
then run their selected install and service-restart sequences concurrently.

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
- Windows installs the native daemon at
  `D:\Programs\lan-mouse\lan-mouse.exe` and runs it through
  `LanMouseDaemon`; native build assets are kept under
  `D:\BuildCache\lan-mouse`. The task starts at interactive user logon. A
  parallel unlock-triggered invocation asks the existing daemon to re-enable
  input emulation without replacing its supervised process. The wrapper keeps
  the daemon alive across genuine process exits, while temporary Windows input
  denial leaves the daemon connected and not-ready until input emulation
  recovers. Bounded persistent logs remain under
  `%LOCALAPPDATA%\lan-mouse\logs` with five 10 MiB backups. The playbook does
  not enable automatic Windows logon: before the first interactive logon after
  a cold boot, Windows `SendInput` cannot control the secure login desktop and
  the peer must remain not ready.

Native-build runs replace the build source and build all three hosts against
the same locked vendor archive. GitHub-release runs install the selected
release assets without a native Lan Mouse build. In both modes the GTK
workspace member is excluded and no legacy `enter_hook` or compatibility
protocol is used.
