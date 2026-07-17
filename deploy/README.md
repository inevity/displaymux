# lan-mouse + tv-multiview deployment

## Before running

**On the control node (example-user, running Ansible):**
```bash
ansible-galaxy collection install ansible.windows community.general
```

**On the Windows host, once, before this playbook can reach it at all:**
Enable OpenSSH Server and install Git, rustup, PowerShell 7, and Visual Studio
Build Tools with the MSVC x64 component. The playbook installs its selected
Rust toolchain only when the native `rustc.exe` is absent, then builds the
no-GTK executable on Windows itself.

**On macOS:**
Install Xcode command-line tools and Homebrew rustup. The playbook installs its
selected Rust toolchain only when the native `rustc` is absent, then builds the
no-GTK executable natively.

**Edit before running:**
- `inventory.ini` — real hostnames, IPs, users.
- `group_vars/all.yml` — TV address, HDMI mapping, server host, fingerprints,
  controller timeouts, and native build features.
- Windows SSH credentials — keep them in Ansible Vault or pass them as extra
  variables instead of committing new plaintext credentials.
- The sibling `../../lan-mouse` checkout is the source deployed by the
  playbook. There is no configured revision pin or installed build-ID cache.

Run:
```bash
ansible-playbook -i inventory.ini playbook.yml
```

The first play creates a git bundle from the local checkout, a locked Cargo
vendor tree and ZIP under `/tmp`, and a persistent controller token under
`~/.local/state/lan-mouse-deploy/`. `--limit localhost` runs only that artifact
preparation play; it does not deploy or restart any host. The vendor tree needs
roughly 640 MiB with the current lockfile, plus its archive and Cargo cache.
After artifact preparation, Linux, macOS, and Windows run their native test,
build, install, and service-restart sequences concurrently.

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
- Windows runs the native daemon through `LanMouseDaemon`; bounded persistent
  logs are under `%LOCALAPPDATA%\lan-mouse\logs` with five 10 MiB backups.

Every run replaces the build source and builds and installs it on all three
hosts against the same locked vendor archive. The GTK workspace member is
excluded, and no release archive, legacy `enter_hook`, or compatibility
protocol is used.
