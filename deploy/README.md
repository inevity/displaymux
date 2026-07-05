# lan-mouse + tv-multiview deployment

## Before running

**On the control node (example-user, running Ansible):**
```
ansible-galaxy collection install ansible.windows community.general
pip install pywinrm --break-system-packages
```

**On the Windows host, once, before this playbook can reach it at all:**
WinRM has to already be listening — Ansible can't configure the thing it
needs to already be able to talk to. Run Ansible's bootstrap script in an
elevated PowerShell prompt on the Windows box itself:
`https://github.com/ansible/ansible-documentation/blob/devel/examples/scripts/ConfigureRemotingForAnsible.ps1`

**Edit before running:**
- `inventory.ini` — real hostnames, IPs, users.
- `group_vars/all.yml` — same, plus `tv_ip`, `mac_arch` (aarch64/x86_64), HDMI
  mapping if it doesn't match `linux=HDMI_1, mac=HDMI_2, windows=HDMI_3`.
- Windows password — deliberately not in inventory.ini. Pass at runtime
  (`-e ansible_password=...`) or use ansible-vault.

Run:
```
ansible-playbook -i inventory.ini playbook.yml
```

## After running (can't be automated, has to happen once by hand)

1. **TV pairing**: the first time `tv-multiview.service` connects, the G4
   will show a pairing prompt. Accept it on the TV. The key gets cached
   after that.
2. **macOS Accessibility**: System Settings → Privacy & Security →
   Accessibility → enable it for lan-mouse. Until this is granted the
   LaunchAgent runs but captures/emulates nothing.
3. **Firewall**: open UDP `4242` (lan-mouse) and TCP `8765` (the daemon) on
   example-user. Not automated here on purpose — didn't want to guess at
   nftables/firewalld/ufw and step on however it's actually configured.

## Points worth double-checking after first run

- `lm_binary_search` / `lm_exe_search` in the mac/Windows plays find the
  actual binary by pattern-matching inside the extracted archive rather
  than a hardcoded path, since the exact internal bundle layout of the
  release archives isn't something I could confirm with certainty. Sane
  default, but check `files[0].path` picked the right one if lan-mouse
  doesn't start.
- `win_scheduled_task`'s parameters are from memory, not re-verified
  against live docs in this session — run `ansible-doc
  ansible.windows.win_scheduled_task` if it errors on apply.
- Everything else (lan-mouse's `daemon` subcommand, the `enter_hook` config
  key, the exact systemd unit, `%LOCALAPPDATA%\lan-mouse` on Windows vs
  `~/.config/lan-mouse` on Linux/macOS, bscpylgtv's `subscribe()` and
  `set_input()`) was checked directly against the current source of both
  projects, not recalled from memory.
