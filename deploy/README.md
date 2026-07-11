# lan-mouse + tv-multiview deployment

## Before running

**On the control node (example-user, running Ansible):**
```bash
ansible-galaxy collection install ansible.windows community.general
```

**On the Windows host, once, before this playbook can reach it at all:**
Enable OpenSSH Server and install Git, rustup, PowerShell 7, and Visual Studio
Build Tools with the MSVC x64 component. Ansible connects through SSH and
builds the no-GTK executable on Windows itself.

**On macOS:**
Install Xcode command-line tools and Homebrew rustup. The playbook builds the
no-GTK executable natively with the selected rustup toolchain.

**Edit before running:**
- `inventory.ini` — real hostnames, IPs, users.
- `group_vars/all.yml` — pinned lan-mouse revision, TV address, HDMI mapping,
  server host, fingerprints, controller timeouts, and native build features.
- Windows SSH credentials — keep them in Ansible Vault or pass them as extra
  variables instead of committing new plaintext credentials.
- The sibling `../../lan-mouse` checkout must be clean and exactly at
  `lan_mouse_revision`; artifact preparation fails closed otherwise.

Run:
```bash
ansible-playbook -i inventory.ini playbook.yml
```

The first play creates a pinned git bundle, a locked Cargo vendor tree and ZIP
under `/tmp`, and a persistent controller token under
`~/.local/state/lan-mouse-deploy/`. `--limit localhost` runs only that artifact
preparation play; it does not deploy or restart any host. The vendor tree needs
roughly 640 MiB with the current lockfile, plus its archive and Cargo cache.

## After running (can't be automated, has to happen once by hand)

1. **LG client key**: the Linux host must already have
   `~/.config/lg-buddy/.aiopylgtv.sqlite`. The playbook deliberately fails
   instead of opening an unattended TV pairing flow.
2. **macOS Accessibility**: System Settings → Privacy & Security →
   Accessibility → enable it for lan-mouse. Until this is granted the
   LaunchAgent runs but captures/emulates nothing.
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

All three hosts build the same pinned source bundle against the same locked
vendor archive. The GTK workspace member is excluded, and no release archive,
legacy `enter_hook`, or compatibility protocol is used.
