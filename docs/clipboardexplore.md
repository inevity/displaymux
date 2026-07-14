Skill: tla-brain. Modes: Research. Triggers: deep clipboard interoperability and architecture investigation across Lan Mouse, Deskflow, and native OS clipboard backends.

**Why Compatibility Exists**

Projects can share the same goal while using incompatible:

- Network protocols and message framing
- Authentication and discovery
- Input-event encoding
- Clipboard ownership semantics
- Version negotiation and failure recovery

Deskflow, Synergy 1, Input Leap, and Barrier share protocol ancestry. Compatibility lets users mix their servers and clients or migrate one machine at a time.

Lan Mouse has a different protocol and input architecture. Since we control and deploy all Lan Mouse hosts together, **Deskflow compatibility provides no current product value**. Implementing it would add substantial complexity without improving the required workflow.

The Deskflow clone is therefore only a reference implementation for clipboard behavior, not a replacement or interoperability target.

**Clipboard Finding**

Deskflow uses global clipboard ownership and permits inactive clients to announce changes, as shown in [Server.cpp](/home/example/explore/desktopimprove/osswitch/deskflow/src/lib/deskflow/Server.cpp:1148). We should not copy that behavior because a background host could overwrite the clipboard belonging to the current input owner.

For Lan Mouse, the correct semantic is:

> Clipboard follows the atomic keyboard-and-pointer owner during a committed host transition.

That means:

1. Only the current input owner can supply a clipboard snapshot.
2. Inactive-host clipboard changes are ignored.
3. A switch captures the source clipboard and stages it for the target.
4. Input ownership commits without waiting for clipboard.
5. The target applies the snapshot only if the switch remains current and its local clipboard has not changed.
6. Any clipboard failure leaves the destination clipboard unchanged.

**Protocol Boundary**

Clipboard must not be added to the existing 21-byte UDP input-event protocol in [lib.rs](/home/example/explore/desktopimprove/lan-mouse/lan-mouse-proto/src/lib.rs:13).

Use a separate authenticated TCP/TLS companion channel:

- Reuse the existing Lan Mouse certificate/fingerprint trust.
- Use `(authority_session_id, ownership_epoch)` as the fencing token.
- Negotiate protocol version, supported formats, and maximum receive size.
- Validate declared length before allocation.
- Keep only one pending snapshot; newer ownership epochs supersede older ones.
- Close only the clipboard channel on malformed data. Input DTLS remains operational.

V1 should support canonical UTF-8 text only, with a configurable default limit around Deskflow’s 3 MiB limit. Images, files, compression, and clipboard history should wait until the ownership protocol is proven.

**Race Protection**

The design must reject:

- A snapshot from an aborted or superseded switch
- Data from a previous server process session
- A source clipboard that changed while being read
- Application after the destination clipboard changed locally
- Duplicate application of the same snapshot
- Private, concealed, unsupported, oversized, or corrupt content
- Late return data after another ownership transition

Explicit empty clipboard and clipboard-read failure must be different states. Only a proven empty clipboard may clear the destination.

**Native Constraints**

- Windows requires a dedicated clipboard thread/window and must run in the interactive user window station. Clipboard access is serialized and may need bounded retries. [Microsoft clipboard documentation](https://learn.microsoft.com/en-us/windows/win32/dataxchg/clipboard)
- macOS should monitor `NSPasteboard.changeCount`. Current macOS privacy controls can require per-application authorization for programmatic reads, so the daemon needs a stable application identity. [Apple NSPasteboard updates](https://developer.apple.com/documentation/updates/appkit)
- Wayland should prefer `ext-data-control`, with `wlr-data-control` for current Hyprland deployments. Portal clipboard access is tied to a compatible RemoteDesktop/InputCapture session. [XDG Clipboard portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.Clipboard.html)
- X11 requires proper selection ownership and bounded incremental reads.
- `arboard` alone is insufficient because its Wayland read path can consume clipboard data before an application-level size cap is enforced.

**Safety Properties**

The formal design should require:

- `InputIndependence`: clipboard never blocks or changes input ownership.
- `ActiveSourceOnly`: only the current input owner exports data.
- `NoStaleApply`: applied token equals the current ownership token.
- `DestinationPreservation`: failures and local-copy races never clear or overwrite the target.
- `BoundedMemory`: declared and received sizes are checked before growth.
- `NoPrivateExport`: recognized secret/private formats are never transferred.
- `FailureIsolation`: clipboard failures leave TV, cursor, capture, and input state unchanged.

The three pinned commits have already been deployed. This investigation made no source changes and ran no binaries or tests.

<oai-mem-citation>
<citation_entries>
MEMORY.md:57-65|note=[lan-mouse fullscreen switch task routing]
MEMORY.md:90-96|note=[prior protocol and native verification context]
</citation_entries>
<rollout_ids>
019f458c-40ba-78f1-920b-c9999358a2dd
</rollout_ids>
</oai-mem-citation>

## Merged Clipboard Material From `reviesionandclipboradexplore.md`

The following source section is preserved verbatim so that the earlier revision/clipboard exploration remains auditable alongside the deeper investigation.

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

### Reconciliation With the Deeper Investigation

- The earlier owner-following rule remains normative.
- `clipboard_text_v1` remains optional and must never participate in input readiness.
- The deeper investigation narrows publishing to ownership transitions rather than continuous background mirroring.
- `arboard` remains useful reference code but is not the complete backend abstraction: bounded native reads and platform-specific ownership behavior require lower-level adapters.
- The detailed design must distinguish `Empty` from `Unavailable`; a failed read must never clear the target clipboard.
