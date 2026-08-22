Skill: tla-brain. Modes: Research. Triggers: exploring protocol compatibility, isolated native deployment, and cross-OS clipboard architecture.

**Commits**
The requested commits are complete and confirmed:

- `7cc0f68 fix: require deliberate edge switch intent`
- `27ff0c2 docs: require deliberate edge switch intent`
- `9bd842f deploy: pin deliberate edge switch build`

**1. Version Compatibility**
The current coupling is real:

- Runtime compatibility is exact commit equality in [client.rs](/home/example/explore/desktopimprove/lan-mouse/src/client.rs:14).
- One global `lan_mouse_revision` controls every host in [all.yml](/home/example/explore/desktopimprove/osswitch/lan-mouse-deploy/group_vars/all.yml:5).
- Linux, macOS, and Windows build identities all include that revision. Therefore any commit invalidates every native build.
- `strategy: free` in [playbook.yml](/home/example/explore/desktopimprove/osswitch/lan-mouse-deploy/playbook.yml:504) only makes macOS and Windows concurrent. It does not isolate their versions.

Do not remove exact source pinning entirely. Separate these concepts:

- **Build identity:** exact commit, lock hash, target, features, compiler and binary digest. Used for reproducibility and diagnostics.
- **Protocol compatibility:** explicit protocol epoch and capabilities. Used to permit control.
- **Desired deployment:** independent artifact manifest for each host.

Add a new append-only protocol event:

```rust
ProtocolHello {
    protocol_epoch: u32,
    offered_capabilities: u64,
    required_capabilities: u64,
}
```

Compatibility becomes:

```text
same protocol_epoch
AND local.required is a subset of peer.offered
AND peer.required is a subset of local.offered
```

Keep the existing commit `Hello` for logging, but never use commit equality as an input-control guard. Core capabilities such as atomic keyboard/pointer ownership and readiness epochs are required. Clipboard is optional.

This is not legacy compatibility: peers missing the new protocol handshake fail closed. There is one coordinated initial cutover, with no old mutating path retained. Afterwards:

- A Windows-only fix builds and activates only Windows.
- Linux and macOS continue running their existing compatible artifacts.
- A protocol-breaking change bumps `protocol_epoch`, requiring an intentional coordinated cutover.
- SemVer alone is insufficient because it does not encode negotiated runtime behavior.

Deployment should be split into two operations:

1. **Build/stage:** build selected native targets into immutable versioned paths while all services continue running.
2. **Activate:** at the end, atomically select the staged artifact and restart only hosts whose desired artifact digest changed.

Replace the global revision with per-host release manifests containing the source commit, binary SHA-256, protocol epoch, capabilities and native build inputs. Keep one previous artifact for host-local rollback. Do not infer affected operating systems from changed file paths; explicitly select release targets.

Required design properties:

```text
RemoteInputCommit(h) => Compatible(server, h) /\ InputReady(h)
BuildOrStage(h)      => UNCHANGED Runtime(h)
h not in ActivateSet => UNCHANGED Runtime(h)
ProtocolMismatch     => input remains on the lan-mouse server
```

**2. Shared Clipboard**
Clipboard synchronization is not implemented. [README.md](/home/example/explore/desktopimprove/lan-mouse/README.md:432) lists it as unfinished; existing GTK clipboard calls only copy configuration values.

Recommended semantics: **clipboard follows the atomic input owner**.

- Only the current input owner may publish clipboard changes.
- The lan-mouse server is the hub, regardless of its operating system.
- On ownership transfer, the latest clipboard value is prepared for the target.
- Clipboard failure never delays, rejects, or rolls back keyboard/mouse switching.
- Stale publishers are rejected using the same ownership/lease epoch concept.

Start with UTF-8 text only. Represent updates with `{owner_epoch, origin_host, session_epoch, sequence, length, sha256}` and suppress loops using the origin, sequence and digest.

Do not send clipboard content through the current 21-byte DTLS input frame in [lan-mouse-proto](/home/example/explore/desktopimprove/lan-mouse/lan-mouse-proto/src/lib.rs:11). Use a separate authenticated, reliable TCP/TLS stream; keep DTLS dedicated to latency-sensitive input and control events.

For native access, `arboard` supports text across Linux, macOS and Windows, with Wayland requiring its data-control feature. It also documents Linux ownership lifetime and recommends avoiding concurrent Windows access, so each process should have one serialized clipboard actor. [arboard documentation](https://docs.rs/arboard/latest/arboard/struct.Clipboard.html), [Wayland notes](https://docs.rs/crate/arboard/latest/source/README.md).

Change detection should use Windows `WM_CLIPBOARDUPDATE`, Wayland `ext-data-control-v1`, and macOS `NSPasteboard.changeCount`. [Microsoft listener API](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-addclipboardformatlistener), [Apple changeCount](https://developer.apple.com/documentation/appkit/nspasteboard/changecount), [Wayland data-control protocol](https://wayland.app/protocols/wlr-data-control-unstable-v1).

Add `clipboard_text_v1` as an optional negotiated capability. This provides the first practical proof that compatible hosts can run different commits: input switching still works when the capability is absent, while clipboard activates only between peers that advertise it.

Recommended implementation order: protocol epoch/capabilities, per-host staged deployment, then clipboard transport and native clipboard actors.

<oai-mem-citation>
<citation_entries>
MEMORY.md:57-65|note=[lan-mouse fullscreen switch task routing]
MEMORY.md:90-96|note=[prior protocol and native verification context]
</citation_entries>
<rollout_ids>
019f458c-40ba-78f1-920b-c9999358a2dd
</rollout_ids>
</oai-mem-citation>
