# Main Plan: Publish the Osswitch Monorepo

## Plan Control

- Status: revised after safety audit and ready for review. Repository migration,
  workspace conversion, GitHub creation, publication, release creation,
  deployment, and service restart have not started.
- Requested document: `osswitch/docs/publishplan.md`.
- Controlling objective: create one public `osswitch` Git repository that
  preserves the useful Lan Mouse and current osswitch histories, contains one
  root Cargo workspace and one root `Cargo.lock`, publishes the supported native
  binaries from one release tag, and exposes only sanitized deployment source.
- Approved repository model: one monorepo, not three Git repositories and not a
  repository containing submodules.
- Approved Cargo model: one root Cargo workspace. `lan-mouse` and
  `tv-multiview` are packages in that workspace; there is no nested workspace.
- Approved synchronization policy: no upstream Lan Mouse synchronization
  mechanism is required. Historical Lan Mouse attribution and license remain.
- Approved release boundary: one monorepo commit and one release tag define the
  compatible source and artifact set.
- Runtime boundary: publication work does not deploy binaries, restart services,
  switch the TV, or alter any running Linux, macOS, or Windows host. A later
  deployment requires separate authorization.
- Remote boundary: no commit or ref may be pushed anywhere until the offline
  history, secret, identity, license, workspace, and workflow gates pass. The
  first remote is private staging. GitHub Actions must be disabled and confirmed
  disabled before any initial ref import into either the staging or final
  repository. Creating the separate public repository and pushing its
  allowlisted refs requires a second, explicit authorization after native CI and
  prerelease verification pass; staging remains private.
- Source repositories remain read-only migration inputs. Failed migration work
  is discarded and regenerated; it is never repaired by rewriting the source
  repositories.
- Final-checkout boundary: "move out of `desktopimprove`" means clone the
  verified monorepo into a new standalone checkout outside every source and
  migration tree. It never means move, rename, or delete the source directories.
- Every run uses a newly created, marked disposable migration root and a
  separate verified backup root. Cleanup can remove only named disposable
  children after their marker and canonical path are revalidated; backups are
  never inside the cleanup boundary.

Open external values required before publication:

- GitHub owner or organization;
- final repository name, with `osswitch` as the current design name;
- root license decision for original `tv-multiview`, deployment, docs, and TLA+
  content;
- public author, committer, tagger, and historical-message identity policy;
- final standalone local checkout path, recorded in the private run ledger;
- initial public release version.

## Decision Baseline

1. The final repository has one Git history graph with two imported ancestry
   lines: Lan Mouse and the filtered osswitch history from `desktopimprove`.
2. The selected Lan Mouse commit graph is retained byte-for-byte, including
   commit IDs and signature bytes. One new mechanical child commit moves its
   selected tip tree under `lan-mouse/`; historical commits are not passed
   through a history rewriter.
3. `tv-multiview`, deployment, osswitch docs and TLA+, and
   `desktopimprove/docs/clipboardplan.md` are extracted together from
   `desktopimprove` so commits spanning those paths remain atomic.
4. The current `lan-mouse-deploy` directory becomes `deploy/`.
5. The current Lan Mouse workspace root becomes a normal package manifest at
   `lan-mouse/Cargo.toml`. One virtual root `Cargo.toml` owns all workspace
   members and profiles.
6. The final repository has one root `Cargo.lock`. Historical component lockfiles
   remain visible in earlier imported commits but not at the published branch
   head.
7. The existing GTK Lan Mouse application remains the default-feature build.
   No-GTK native assets remain additional release artifacts.
8. `tv-multiview` initially publishes only the Linux archive used by its current
   service deployment. Windows and macOS controller assets are deferred until
   controller service integration exists on those systems.
9. Deployment publishes no binary archive. It is versioned source contained in
   every repository tag.
10. Native-build and GitHub-release deployment remain two supported choices.
    Artifact deployment is additive and does not remove native build support.
11. No source-revision compatibility variable or hard-coded revision pin is
    introduced. A release tag identifies an artifact set; runtime protocol and
    capability negotiation remain the compatibility authority.
12. Each publishable component has its own source commit set, mapping evidence,
    path-history verification, and import exit gate.
13. Per-component evidence does not mean per-component rewriting. All selected
    `desktopimprove` paths are filtered together so commits spanning components
    remain one commit.
14. After the public repository is verified, a fresh standalone clone becomes
    the local development checkout. Existing source repositories remain
    read-only recovery inputs until separately authorized cleanup.

## Current-State Evidence

The migration must re-check these values at execution time. They describe the
2026-07-18 planning baseline.

### Git Roots and Histories

- `$LAN_MOUSE_SOURCE` is an independent Git repository on `main`. Its configured
  `origin` is the upstream Lan Mouse URL.
- `osswitch/tv-multiview` and `osswitch/lan-mouse-deploy` are not independent
  repositories. Their Git root is `$DESKTOPIMPROVE_SOURCE`, currently on
  `master`.
- `osswitch/lan-mouse-deploy/.git` is an empty invalid marker, not a repository.
  It is excluded from every input and final tree; deployment history comes only
  from committed parent-repository objects.
- Lan Mouse `main` has 579 reachable commits at the planning baseline. It is 50
  commits ahead of the configured upstream `origin/main`; those local commits
  are publication inputs and must be present in the external backup.
- The planning audit found 225 signed Lan Mouse commits. Rewriting their trees
  would invalidate those signatures, so their commit objects must remain exact.
- Lan Mouse has 55 selected tags at the planning baseline. All are reachable
  from selected `main`; 28 are annotated and none were signed. Execution must
  re-check both reachability and signed-tag count before mapping them.
- The `desktopimprove` branch has 88 commits selected by the union of:
  `osswitch/tv-multiview`, `osswitch/lan-mouse-deploy`, `osswitch/docs`,
  `osswitch/tla`, and `docs/clipboardplan.md`.
- The five-selector audit found 16 commits that touch at least two publication
  inputs: 12 touch exactly two inputs and four touch exactly three. Execution
  must recompute this overlap set from the frozen source commit rather than
  trust the planning count; separate extraction would duplicate changes and
  lose their atomic relationship.
- The parent worktree contains unrelated tracked and untracked work. Migration
  operates from committed refs and must not stage, remove, reset, or include
  unrelated files.

### Publication Input and History Inventory

Every final component has an explicit import obligation:

1. **Lan Mouse**: independent `$LAN_MOUSE_SOURCE` history becomes
   `lan-mouse/`. The live baseline is 579 commits. P1 preserves the exact commit
   graph and adds one layout commit.
2. **TV controller**: `osswitch/tv-multiview/` becomes `tv-multiview/`. Its live
   parent-history set is 20 commits: 17 unique to this selector and 3 shared with
   another selected input.
3. **Deployment**: `osswitch/lan-mouse-deploy/` becomes `deploy/`. Its live set
   is 46 commits: 38 unique and 8 shared. P3 removes or rewrites private content
   without dropping these commit nodes.
4. **Documentation**: `osswitch/docs/` becomes `docs/` and contributes 25
   commits. `docs/clipboardplan.md` also maps into final `docs/` and contributes
   11 commits. These selectors overlap each other and other components; both
   require separate mapping evidence even though they share one output tree.
5. **TLA+ models**: `osswitch/tla/` becomes `tla/`. Its live set is 6 commits;
   all 6 also touch another selected input, so none may be split into a separate
   rewrite or represented only by a copied snapshot.

The five `desktopimprove` selectors have an 88-commit union. Sixteen commits
touch at least two selected inputs: 12 touch exactly two and 4 touch exactly
three. Therefore P2 performs one physical filter transaction while P2A through
P2D provide per-component history acceptance gates.

The intended standalone checkout is absent at this planning baseline. Execution
must recheck that its private-ledger path is still absent and outside all source,
migration, backup, and private-ledger trees.

### Cargo and Release State

- `lan-mouse/Cargo.toml` currently owns a workspace containing the Lan Mouse
  package and eight internal packages.
- `tv-multiview/Cargo.toml` is currently a separate package workspace root.
- Lan Mouse and `tv-multiview` currently have separate `Cargo.lock` files.
- Lan Mouse has an existing release workflow that builds the default GTK
  application and no-GTK Linux, Windows, and macOS artifacts.
- The current Lan Mouse release workflow runs on `main`, matching `v*` tags, and
  manual dispatch. The monorepo must separate ordinary CI from release
  publication so a docs-only commit does not build and publish every native
  release artifact.
- The 2026-07-24 tag-target audit found 22 selected Lan Mouse tags whose target
  commits contain a root Cachix workflow with broad
  `on: [push, pull_request, workflow_dispatch]`, mutable action references, and
  a `CACHIX_AUTH_TOKEN` input. Renaming those tags does not neutralize the
  retained workflow; initial remote ref imports require Actions quarantine.
- Deployment native-build mode currently assumes the Lan Mouse repository and
  `tv-multiview` are sibling source trees. It computes and vendors from the Lan
  Mouse lockfile and builds `tv-multiview` separately.
- Deployment GitHub-release mode currently downloads only Lan Mouse artifacts;
  `tv-multiview` is still built from sibling source.

### Publication-Sensitive State

- `osswitch/lan-mouse-deploy/inventory.ini` and
  `osswitch/lan-mouse-deploy/group_vars/all.yml` are tracked and contain
  machine-specific deployment data. Their historical blobs must not be public.
- The planning audit also found environment-specific network values in
  `osswitch/docs/fullscreenmultiviewswitchdesign.md`,
  `osswitch/docs/rustimplmovation.md`,
  `osswitch/docs/staletv_inputissue.md`, and
  `osswitch/tv-multiview/src/main.rs`. Machine-absolute home paths occur in
  `docs/clipboardplan.md`, selected clipboard/fullscreen documents, deployment
  inventory, and `osswitch/tla/README.md`. Removing only the two deployment
  files is therefore insufficient; selected blobs and commit/tag messages
  require rule-driven redaction across all retained refs.
- Pane transcripts, `quicksuggest.md`, `lan-mouse-deploy.zip`, generated output,
  summaries, the local Deskflow clone, and build trees are untracked and are not
  publication inputs.
- `osswitch/docs/WakeDisplaydesign.md`,
  `osswitch/docs/reviesionandclipboradexplore.md`, and this
  `osswitch/docs/publishplan.md` are currently untracked. They can be added as
  reviewed present-state documents but have no committed history to preserve.
- `desktopimprove/docs/clipboardplan.md` has 11 relevant commits. It is a
  history-selected input, not a post-import snapshot.
- The planning audit found 36 distinct Lan Mouse author/committer email values
  and 42 email-like historical-message matches. Identity exposure and license
  ownership require explicit approval before public visibility because public
  clones cannot be recalled.
- Lan Mouse declares `GPL-3.0-or-later` and carries the corresponding license.
  The current `tv-multiview` and deployment roots do not have independent
  license files.

## Goal Repository Layout

```text
$FINAL_CHECKOUT/
|-- .github/
|   `-- workflows/
|       |-- ci.yml
|       `-- release.yml
|-- Cargo.toml
|-- Cargo.lock
|-- LICENSE
|-- README.md
|-- lan-mouse/
|   |-- Cargo.toml
|   |-- LICENSE
|   |-- input-capture/
|   |-- input-emulation/
|   |-- input-event/
|   |-- lan-mouse-cli/
|   |-- lan-mouse-clipboard/
|   |-- lan-mouse-gtk/
|   |-- lan-mouse-ipc/
|   `-- lan-mouse-proto/
|-- tv-multiview/
|   |-- Cargo.toml
|   `-- src/
|-- deploy/
|   |-- README.md
|   |-- inventory.example.ini
|   |-- group_vars/
|   |   `-- all.example.yml
|   |-- playbook.yml
|   |-- tasks/
|   `-- templates/
|-- docs/
|   `-- history/
|       |-- lan-mouse-original-ref-map.txt
|       `-- desktopimprove-osswitch-commit-map.txt
`-- tla/
```

`deploy/`, `docs/`, and `tla/` are part of the Git repository but are not Cargo
workspace members. `$FINAL_CHECKOUT` is a private execution variable; no
machine-specific absolute checkout path is committed to the public repository.

## TLA Planning Frame

```tla
PublicationInputs ==
    {"lan-mouse", "tv-multiview", "deploy", "docs", "tla"}

InitState ==
    /\ LanMouseGitRoot = "lan-mouse"
    /\ OsswitchGitRoot = "desktopimprove"
    /\ CargoWorkspaceRoots = 2
    /\ CargoLockfiles = 2
    /\ DeploymentContainsTrackedLocalConfiguration
    /\ ReleaseAssetsOwnedByLanMouseWorkflow
    /\ TvMultiviewReleaseAsset = FALSE
    /\ FinalStandaloneCheckout = "absent"

GoalState ==
    /\ GitRoot = "osswitch"
    /\ SourceBackupsRestorable
    /\ SourceObjectsBoundToRecordedIds
    /\ ImportedLanMouseHistoryReachable
    /\ LanMouseCommitIdsAndSignaturesPreserved
    /\ ImportedOsswitchHistoryReachable
    /\ \A input \in PublicationInputs : InputHistoryVerified[input]
    /\ CrossInputCommitsRemainAtomic
    /\ CargoWorkspaceRoots = 1
    /\ CargoLockfiles = 1
    /\ NoPublicSensitiveObjectReachable
    /\ PublicIdentityAndLicenseApproved
    /\ PrivateNativeCIValidatedBeforePublic
    /\ OneTagDefinesSourceAndArtifacts
    /\ ReleasePublishedOnlyFromValidatedDraft
    /\ LanMouseNativeAssetsComplete
    /\ TvMultiviewLinuxAssetComplete
    /\ DeploymentSourcePublishedWithoutBinaryArchive
    /\ StandaloneCheckoutOutsideSourceTrees
    /\ OriginalRepositoriesUnchanged

Plan ==
    <<P0_FreezeAndInventory,
      P1_ImportLanMouseHistory,
      P2_ImportOsswitchHistory,
      P3_SanitizeHistory,
      P4_MergeHistories,
      P5_CreateSingleCargoWorkspace,
      P6_AdaptDeployment,
      P7_CreateCIAndRelease,
      P8_PreparePublicDocumentation,
      P9_VerifyOffline,
      P10_StagePrivateRepository,
      P11_AuthorizePublicVisibility,
      P12_CreateStandaloneCheckout,
      P13_PublishFirstRelease>>
```

No later phase may be used as evidence that an earlier gate passed. In
particular, a successful Cargo build does not prove history completeness, and a
successful secret scan does not prove artifact compatibility.

## Required Invariants

1. **OriginalRepositoriesUnchanged**: migration never changes refs, index,
   worktree files, remotes, tags, or configuration in either source repository.
2. **MigrationTargetIsDisposable**: every history-changing or cleanup command
   operates only in a newly created, canonical, non-symlink migration child with
   the current run marker; no source, source parent, home, root, or backup path
   can satisfy the guard.
3. **SourceBackupsRestorable**: verified source bundles and their checksums live
   outside the migration root, contain the recorded commits and approved tag
   objects, and pass a restore drill before any rewrite starts.
4. **ImmutableSourceBinding**: every migration operation uses recorded commit and
   tag object IDs. A moving branch name is never used as the effective input.
5. **PerInputHistoryCompleteness**: each member of `PublicationInputs` has a
   separately recorded source commit set, destination path, mapping or identity
   proof, representative log/blame checks, and a passing import exit gate.
6. **LanMouseHistoryExact**: every selected Lan Mouse historical commit remains
   reachable under its original object ID, with its original `gpgsig` header and
   signature validity where the verification key is available. Only one new
   layout commit moves the selected tip tree under `lan-mouse/`.
7. **HistoryReachable**: every selected `desktopimprove` commit has a non-zero
   mapping to a final commit reachable from `main`, except content explicitly
   removed by the approved publication-safety policy.
8. **CrossInputAtomicity**: a source commit touching TV, deployment, either docs
   selection, or TLA+ maps to one final commit containing all selected changes.
9. **TraceableRewrite**: the Lan Mouse identity/ref map and composed
   `desktopimprove` old-to-final commit map are retained. Historical SHA strings
   remain resolvable.
10. **NoSquash**: neither imported history is represented by a snapshot-only
   squash commit.
11. **OneWorkspace**: the published branch has one root `[workspace]`, no nested
   `[workspace]`, and every Rust package belongs to the root workspace.
12. **OneLockfile**: exactly one tracked `Cargo.lock` exists at the repository
   root on the published branch.
13. **RelativeDependencyIntegrity**: moving Lan Mouse under `lan-mouse/` does not
   alter the semantics of its internal relative path dependencies.
14. **BuildBehaviorPreserved**: the default Lan Mouse package still enables GTK;
   no-GTK builds still select their explicit feature sets; `tv-multiview`
   retains its intended release panic and optimization behavior.
15. **NoSecretReachability**: no public branch, tag, commit, tree, blob, commit or
    tag message, release archive, workflow artifact, example file, log, or
    commit-map file contains a credential, controller token, private key, local
    password, private inventory, host fingerprint, device identifier, or
    environment-specific endpoint.
16. **PublicIdentityApproved**: every reachable author, committer, tagger, and
    email-like message value is either approved for publication or rewritten by
    the recorded policy before private staging. Exact Lan Mouse commit/signature
    preservation and Lan Mouse identity rewriting are mutually exclusive; a
    conflict blocks publication for an explicit decision.
17. **PrivateStagingBeforePublic**: native GitHub CI and a complete manual
    prerelease pass in a private repository before any public visibility change.
18. **HistoricalWorkflowQuarantine**: exact imported ancestry may retain
    root-level historical GitHub workflows. GitHub Actions is disabled before
    the first remote ref is pushed, all archival and import refs are uploaded and
    audited while it remains disabled, and zero workflow runs from those refs
    are permitted. Actions is enabled only afterward; current root CI is then
    started explicitly against the frozen `main` commit.
19. **OneReleaseIdentity**: every binary uploaded for a release is built from the
    tagged monorepo commit. An artifact cannot be copied from an older workflow
    run or a local target directory.
20. **ReleaseAssetCompleteness**: release publication is all-or-nothing for the
    declared matrix. A missing required native asset prevents release completion.
21. **DraftBeforePublish**: assets are assembled and digest-checked in a draft;
    the release becomes public only after its exact manifest is verified.
22. **DeployModePreservation**: native-build mode remains supported and remains
    the default unless explicitly changed. GitHub-release mode is an additional
    selection, not a replacement.
23. **NoMixedReleaseIdentity**: one controller-side resolution supplies every
    host with the same immutable release tag, release ID, target commit, and
    manifest digests for Lan Mouse and `tv-multiview`.
24. **StandaloneCheckoutOutsideSources**: the final local checkout is created by
    cloning the verified public repository into a previously absent,
    non-symlink path outside source, migration, backup, and private-ledger trees.
    No source directory is moved, renamed, or deleted.
25. **RuntimeUnaffectedByPublication**: history migration, workspace checks,
    GitHub push, and release creation do not stop or restart deployed services.

## Non-Goals

- Maintaining a GitHub fork relationship with upstream Lan Mouse
- Importing every upstream remote feature branch
- Automating future upstream synchronization
- Preserving old `desktopimprove` commit SHA values after selected-path and
  content rewriting; those commits remain traceable through the composed map
- Preserving secret or machine-specific historical blob contents
- Combining all Rust packages into one package
- Rewriting Lan Mouse internal crate APIs solely for repository layout
- Adding Windows or macOS `tv-multiview` services or release assets
- Publishing deployment as a binary, container, package, or archive
- Adding installers, package-manager repositories, code notarization, SBOMs, or
  artifact signing in the first publication
- Deploying the first GitHub release to live hosts
- Running the full physical switch and failure matrix as a publication gate
- Moving, renaming, or deleting the original source directories as part of
  publication

## P0: Freeze Sources and Record the Migration Ledger

Status: pending.

### Run Roots and Destructive-Operation Guard

1. Create a unique run ID and three run-local paths: `$MIGRATION_ROOT`,
   `$BACKUP_ROOT`, and `$PRIVATE_LEDGER_ROOT`. Record `$FINAL_CHECKOUT` as a
   fourth, non-run path for the later standalone clone. All four must be outside
   both source trees; backup, private-ledger, and final-checkout paths must also
   be outside the migration root.
2. Prove `$FINAL_CHECKOUT` does not exist, resolve its existing parent
   canonically, and record the intended child name. Do not create the checkout in
   P0 and do not use a symlinked parent.
3. `$MIGRATION_ROOT` must not exist before the run. Create it empty, resolve its
   canonical path, prove it is not a symlink, then create a marker containing the
   run ID and the two recorded source commit IDs.
4. Apply separate path policies. Migration, backup, and private-ledger run roots
   cannot equal or sit below either source parent. `$FINAL_CHECKOUT` cannot equal,
   contain, or sit below either source root and cannot overlap any run root, but
   it may be a sibling of `desktopimprove` under the same project parent. Reject
   `/`, the user's home, and every symlink-resolved overlap. Each disposable
   migration child has its own marker.
5. Before `filter-repo`, ref deletion, clone replacement, or cleanup, re-resolve
   the target and assert both the expected canonical child path and current run
   marker. `--force` is allowed only in a verified disposable child clone.
6. There is no generic recursive cleanup command in this plan. By default retain
   the marked migration directory for audit. If cleanup is later authorized,
   remove only named child clones after repeating the guard; never remove the
   backup, private ledger, final checkout, or either source tree.

### Immutable Source Inventory

1. Record source canonical paths, Git version, worktree status, selected branch
   names, exact tip commit IDs, exact tree IDs, counts, and every approved tag
   object ID and peeled commit ID. Branch and tag names are labels in the ledger,
   not migration inputs after this point.
2. Confirm Lan Mouse is clean. Confirm the five selected `desktopimprove` paths
   have no uncommitted tracked changes. Ignore unrelated parent-repository
   changes; do not clean, reset, remove, or stage them.
3. Record one commit list for each publication input against the recorded object
   IDs, then record the parent-selector union. Do not use moving branch names:

   ```bash
   git -C "$LAN_MOUSE_SOURCE" rev-list --reverse "$LAN_MOUSE_SHA"
   git -C "$DESKTOPIMPROVE_SOURCE" rev-list --reverse \
       "$DESKTOPIMPROVE_SHA" -- osswitch/tv-multiview
   git -C "$DESKTOPIMPROVE_SOURCE" rev-list --reverse \
       "$DESKTOPIMPROVE_SHA" -- osswitch/lan-mouse-deploy
   git -C "$DESKTOPIMPROVE_SOURCE" rev-list --reverse \
       "$DESKTOPIMPROVE_SHA" -- osswitch/docs
   git -C "$DESKTOPIMPROVE_SOURCE" rev-list --reverse \
       "$DESKTOPIMPROVE_SHA" -- osswitch/tla
   git -C "$DESKTOPIMPROVE_SOURCE" rev-list --reverse \
       "$DESKTOPIMPROVE_SHA" -- docs/clipboardplan.md
   git -C "$DESKTOPIMPROVE_SOURCE" rev-list --reverse \
       "$DESKTOPIMPROVE_SHA" -- \
       osswitch/tv-multiview \
       osswitch/lan-mouse-deploy \
       osswitch/docs \
       osswitch/tla \
       docs/clipboardplan.md
   ```

4. For every input, record source path, destination path, commit count, exact
   commit-ID set, tip tree, commits unique to that input, and commits shared with
   another input. These records become the P1 and P2A-P2D acceptance ledgers.
5. Recompute and record every commit that touches two or more of the five parent
   selectors. This is the atomicity acceptance set; do not rely on the planning
   baseline count.
6. Record the 50-commit Lan Mouse divergence from configured upstream and prove
   all such commits are reachable from `$LAN_MOUSE_SHA`.
7. Inventory signed commits and signed or annotated tags. Save the signed-commit
   object-ID set and each raw `gpgsig` header for later byte-identity checks.
8. Inventory author, committer, and tagger names/emails and email-like commit/tag
   message content. Store values only in the private ledger. Classify explicitly
   public upstream identities separately from local/private identities requiring
   approval or rewrite.
9. Build a sensitive-value and path ledger covering credentials, hostnames,
   endpoints, IPv4/IPv6 values, MAC addresses, fingerprints, device IDs, local
   home paths, user names, inventory, and controller configuration. Store exact
   match and replacement rules outside every publishable repository and never
   print their contents in logs.
10. Record intended untracked documents by path and SHA-256. Logs, clones,
   archives, screenshots, generated output, and summaries are exclusions, not
   migration inputs.

### External Backup and Restore Drill

1. Before any history rewrite, create full Git bundles in `$BACKUP_ROOT` for the
   recorded Lan Mouse source branch and approved tags and for the recorded
   `desktopimprove` source branch and approved tags. Stop if either named source
   ref no longer resolves to its recorded object ID while the bundle is created.
2. Record bundle SHA-256 digests and `git bundle list-heads` output, then run
   `git bundle verify` on each bundle.
3. Clone each bundle into a separate scratch directory outside
   `$MIGRATION_ROOT`. Prove the recorded commit/tree objects and approved tag
   objects exist and compare representative files and commit counts with the
   sources.
4. Delete no backup as part of migration cleanup. The restore-drilled bundles
   remain the authoritative recovery path for the 50 local Lan Mouse commits and
   the original parent branch.

Exit gate:

- Both source tips, trees, selected commit sets, and approved tag objects are
  immutable inputs for the run.
- Selected tracked paths are clean.
- Unrelated dirty parent-repository state is documented and untouched.
- Canonical path and marker guards reject every source, backup, home, and root
  path.
- The final-checkout candidate is absent and cannot overlap any source or run
  path.
- Both external bundles pass verification and a restore drill.
- Every publication input has an exact, separately reproducible history ledger.
- The private ledger can reproduce exactly what was selected, classified,
  redacted, approved, and excluded without exposing those values publicly.

Rollback:

- No source state has changed. Stop and retain the marked migration root and
  verified external backups for inspection. Cleanup, when separately approved,
  is limited to guarded named disposable children.

## P1: Import Lan Mouse History Under `lan-mouse/`

Status: pending.

Use a fresh disposable clone restored from the verified Lan Mouse bundle. Bind
the clone to `$LAN_MOUSE_SHA` and do not rewrite its historical commits:

```bash
git clone "$LAN_MOUSE_BACKUP_BUNDLE" "$MIGRATION_ROOT/lan-history"
git -C "$MIGRATION_ROOT/lan-history" cat-file -e "$LAN_MOUSE_SHA^{commit}"
git -C "$MIGRATION_ROOT/lan-history" checkout --detach "$LAN_MOUSE_SHA"
```

Actions:

1. Revalidate the disposable-child marker before changing refs or the index.
   Remove the bundle remote after object verification so no later fetch can move
   the selected input.
2. Create one new `migration/lan-layout` commit whose parent is exactly
   `$LAN_MOUSE_SHA` and whose tree is exactly that parent's root tree prefixed by
   `lan-mouse/`. Construct the prefixed tree mechanically in the disposable
   index; do not run `filter-repo` or amend any historical commit.
3. Prove the layout commit has one parent, that the parent object ID is exact,
   and that stripping the `lan-mouse/` prefix from its tree yields the recorded
   source tree byte-for-byte.
4. Create `$PRIVATE_LEDGER_ROOT/lan-mouse-original-ref-map.private` recording
   every selected source tag object, peeled commit, tag kind, tagger, and
   message. Do not import original `v*` tag names into the product tag namespace.
5. Recreate approved public archival tags under `lan-mouse-*`, pointing to the
   same original commit IDs and retaining annotated metadata where possible.
   Re-check that no selected tag is signed; a signed tag requires an explicit
   keep-name or signature-loss decision because renaming changes its tag object.
   Treat these as data-preservation refs, not as executable release refs:
   namespacing does not neutralize root-level workflows retained by their target
   commits. Record the workflow files and event triggers reachable from every
   proposed archival tag in the private ref map.
6. Compare every selected commit ID and raw object with the backup. Run
   `git verify-commit` for the signed set where verification keys are available;
   otherwise still prove raw-object and `gpgsig` header identity.
7. Verify representative history across the initial commit, workspace split,
   native platform implementations, controller integration, clipboard work, and
   release workflow work.
8. Scan the exact Lan Mouse ancestry, archival tags, commit/tag messages, and
   identities against the private ledgers. A required Lan Mouse content or
   identity rewrite conflicts with exact commit/signature preservation and
   blocks publication for an explicit user decision.
9. Verify path-scoped log and blame cross the one layout commit into the original
   root paths using move/copy detection. Record that `--follow` may require the
   historical pre-move path at the layout boundary.
10. Write the Lan Mouse component acceptance record: selected count, identity
    count, layout commit ID, source and destination trees, signed-commit results,
    archival tag mapping, representative history checks, and zero missing
    commits.

Exit gate:

- The layout branch contains one new mechanical child above the complete exact
  Lan Mouse ancestry; its current tree lives under `lan-mouse/`.
- Every historical commit object ID, author, committer, timestamp, message,
  parent topology, and signature bytes are unchanged.
- Approved archival tags are namespaced and map to exact original commits;
  original tag metadata and object IDs remain recoverable from the backup and
  private ref map.
- No unresolved Lan Mouse secret or identity finding exists.
- The `lan-mouse` per-input history acceptance record passes independently of
  parent-history acceptance.
- No source repository ref changed.

## P2: Import Parent-Repository Inputs in One Atomic Rewrite

Status: pending.

P2A through P2D are separate history obligations, not separate rewrite passes.
P2E performs one physical transaction, then each obligation is checked against
that same result.

### P2A: Import `tv-multiview` History

1. Load the exact commit set for `osswitch/tv-multiview/` from the P0 ledger; the
   live planning baseline is 20 commits.
2. Require every selected commit to map through the P2 and P3 maps and remain
   reachable from final `main`.
3. Verify the pre-redaction destination tip tree at `tv-multiview/` equals the
   recorded source subtree after path-prefix removal.
4. Verify representative source-to-destination log, blame, and diff history,
   including each commit shared with deployment or docs.

### P2B: Import Deployment History

1. Load the exact commit set for `osswitch/lan-mouse-deploy/`; the live planning
   baseline is 46 commits.
2. Map the complete directory history to `deploy/`, including commits that later
   become empty when P3 removes private inventory paths.
3. Record private paths and literals as P3 redaction obligations rather than
   dropping their commit nodes or copying only the current sanitized tree.
4. Verify representative deployment, native-build, release-mode, service, and
   documentation history plus every cross-input commit.

### P2C: Import Documentation History

1. Load separate source sets for `osswitch/docs/` and
   `docs/clipboardplan.md`; the live baselines are 25 and 11 commits.
2. Map both selectors into final `docs/` and prove every source commit has one
   composed final mapping. Their shared destination does not merge or duplicate
   source commit nodes.
3. Verify clipboard-plan commits shared with deployment, docs, or TLA+ remain
   atomic, and verify representative design/plan history by source and final
   path.
4. Keep untracked present-state documents outside history accounting; P8 adds
   them later in one ordinary documentation commit.

### P2D: Import TLA+ History

1. Load the exact commit set for `osswitch/tla/`; the live planning baseline is
   6 commits.
2. Map it to `tla/` and prove all six commit nodes remain represented even though
   every baseline TLA+ commit also touches another selected input.
3. For each shared commit, compare the source selected diff and prove the model,
   config, README, and companion docs changes remain in one final commit.
4. Verify representative path history and current tip-tree equality before P3
   applies approved text redactions.

### P2E: Execute and Verify the Atomic Parent Import

Restore a disposable clone from the verified `desktopimprove` bundle, bind a
single local migration ref to `$DESKTOPIMPROVE_SHA`, remove all other disposable
refs, and extract all five selectors in one `filter-repo` run:

```bash
git clone "$DESKTOPIMPROVE_BACKUP_BUNDLE" "$MIGRATION_ROOT/osswitch-history"
git -C "$MIGRATION_ROOT/osswitch-history" cat-file -e \
    "$DESKTOPIMPROVE_SHA^{commit}"

git -C "$MIGRATION_ROOT/osswitch-history" filter-repo \
    --path osswitch/tv-multiview/ \
    --path osswitch/lan-mouse-deploy/ \
    --path osswitch/docs/ \
    --path osswitch/tla/ \
    --path docs/clipboardplan.md \
    --path-rename osswitch/tv-multiview/:tv-multiview/ \
    --path-rename osswitch/lan-mouse-deploy/:deploy/ \
    --path-rename osswitch/docs/:docs/ \
    --path-rename osswitch/tla/:tla/ \
    --path-rename docs/clipboardplan.md:docs/clipboardplan.md
```

Transaction actions:

1. Before filtering, prove the sole selected migration ref resolves to the
   recorded commit and revalidate the disposable-child marker. Exclude all
   unrelated `desktopimprove` paths and refs.
2. Rename `lan-mouse-deploy` to `deploy` across the selected history.
3. Save the first-pass commit map and derive a per-input mapping report for P2A
   through P2D.
4. Verify each selected source commit maps to one rewritten commit. A commit
   selected by several inputs appears once in the union and once in the output.
5. For every cross-input acceptance commit, compare the selected source diff to
   the rewritten diff and prove all selected path changes remain in one commit.
6. Verify all 11 `docs/clipboardplan.md` commits are represented, including the
   six that overlap osswitch content and the five additional union commits.
7. Recompute source and mapped counts for every selector and for the union. A
   planning-baseline difference requires an explained new P0 ledger, not an
   adjusted expected number during P2.
8. Verify the rewritten tree contains no pane transcript, local Deskflow clone,
   zip archive, generated output, build tree, or unrelated project.

P2 exit gate:

- The rewritten history contains only `tv-multiview/`, `deploy/`, `docs/`, and
  `tla/` content from the selected committed history, including the
  history-selected clipboard plan at `docs/clipboardplan.md`.
- P2A, P2B, P2C, and P2D each have a passing per-input acceptance record with no
  missing source commit.
- Cross-input commits remain atomic.
- The selected source commit set is fully represented in the mapping.

## P3: Remove Private History Without Removing Commit History

Status: pending.

The public repository cannot preserve private inventory blobs or merely edit
their current versions. Regenerate the disposable osswitch clone using
private-ledger rules that cover paths, all text blobs, and commit/tag messages.
The installed `git-filter-repo` supports the required `--replace-text` and
`--replace-message` inputs:

```bash
git -C "$MIGRATION_ROOT/osswitch-history" filter-repo --force \
    --path deploy/inventory.ini \
    --path deploy/group_vars/all.yml \
    --invert-paths \
    --replace-text "$PRIVATE_LEDGER_ROOT/blob-replacements.txt" \
    --replace-message "$PRIVATE_LEDGER_ROOT/message-replacements.txt" \
    --prune-empty never \
    --prune-degenerate never \
    --no-ff
```

Actions:

1. Revalidate the canonical child and marker immediately before the forced
   pass. Save every filter pass's commit map and compose one original-to-final
   `desktopimprove` map, including commits made empty by redaction.
2. Preserve commits that become empty after private paths are removed. Their
   messages, authorship, timestamps, and place in the history remain useful.
3. Rewrite every approved environment-specific literal and absolute local path
   in retained history, not just current files. The mandatory audit set is the
   concrete file set recorded under Publication-Sensitive State, including the
   fullscreen, implementation, stale-input, clipboard, inventory, TLA+ README,
   and `tv-multiview/src/main.rs` histories. Classify format-only examples and
   allowlist them narrowly; never allowlist an exact environment value.
4. Apply an approved private mailmap only if the identity policy requires
   rewriting `desktopimprove` author, committer, or tagger identities. Run it as
   part of deterministic regeneration and compose its mapping. Do not apply an
   identity rewrite to exact Lan Mouse commits.
5. Add sanitized examples only after the histories are merged:
   `deploy/inventory.example.ini` and
   `deploy/group_vars/all.example.yml`.
6. Add root and deployment ignore rules for the real local filenames, tokens,
   keys, certificates, logs, archives, editor files, and build output.
7. Search every retained ref's commits, tag objects, trees, blobs, and messages
   for exact known private values and private path forms using rules that are
   never committed or echoed.
8. Run the installed local secret scanner against all history without uploading
   candidate values to an external verifier:

   ```bash
   trufflehog git "file://$MIGRATION_ROOT/osswitch-history" \
       --no-verification \
       --results=verified,unknown,unverified \
       --fail
   ```

9. Inspect scanner findings individually. A scanner failure is not waived by
   calling a value a false positive; record the exact path, commit, classification,
   and disposition in the private migration ledger.
10. Scan the exact Lan Mouse branch and mapped archival tags separately without
    rewriting them. Any unresolved result blocks the exact-history design.
11. Repeat object, identity, exact-value, message, and scanner gates after all
    final refs exist and again against every locally built release archive.
12. Recompose and recheck each P2A-P2D acceptance record against the post-P3
    commit map. Redaction may change destination IDs and contents but may not
    erase a selected commit node or its per-input provenance.

Exit gate:

- All selected parent-history commit identities have final mappings, including
  commits emptied by redaction.
- No private inventory blob, environment value, absolute local path, or
  unapproved identity/message value is reachable from any retained ref.
- Secret scanning and the exact-value scan pass with zero unresolved findings.
- Sanitized examples contain placeholders only.
- Every per-input parent-history acceptance record still passes after redaction.

Rollback:

- Any uncertainty about a historical value blocks publication. Retain evidence,
  refine the private rules, and regenerate only the marked disposable clones
  from the verified bundles. Never patch or rewrite either source repository.

## P4: Merge the Imported Histories Without Squashing

Status: pending.

Use the sanitized osswitch history as the destination history, rename its branch
to `main`, fetch only the exact Lan Mouse layout branch without tags, then create
one explicit unrelated-history merge:

```bash
git -C "$MIGRATION_ROOT/osswitch-history" branch -M main
git -C "$MIGRATION_ROOT/osswitch-history" remote add lan-history \
    "$MIGRATION_ROOT/lan-history"
git -C "$MIGRATION_ROOT/osswitch-history" fetch --no-tags lan-history \
    refs/heads/migration/lan-layout:refs/remotes/lan-history/migration/lan-layout
git -C "$MIGRATION_ROOT/osswitch-history" merge \
    --allow-unrelated-histories \
    --no-ff \
    lan-history/migration/lan-layout \
    -m "chore: assemble osswitch monorepo history"
```

Before merging, create annotated import-tip tags in the migration repository:

- `history/lan-mouse-import`;
- `history/desktopimprove-osswitch-import`.

Actions:

1. Confirm the merge commit has exactly the sanitized parent-history tip and the
   one-commit Lan Mouse layout tip as parents.
2. Confirm both ancestry lines are reachable from `main` without temporary
   migration refs.
3. Confirm `git log --graph --all`, path-scoped logs, blame, and bisect traversal
   expose the expected histories.
4. Recreate only the approved `lan-mouse-*` archival tags from the private map.
   Do not fetch or push original `v*` names; reserve `v*` for coordinated product
   releases. Do not claim that a namespaced tag is safe merely because it does
   not match the current release workflow: historical target commits may contain
   other root-level workflows with broad `push` triggers. Remote Actions
   quarantine in P10 and P11, not tag naming, prevents their execution.
5. Derive a public `lan-mouse-original-ref-map.txt` containing only approved
   source-ref-to-archival-ref, tag-object, and peeled-commit mappings. Place it
   and the composed `desktopimprove-osswitch-commit-map.txt` under
   `docs/history/` only after proving they contain no path contents, unapproved
   identities, or secrets. The detailed private map never enters the repository.
6. Remove the temporary local history remote after the merge. The monorepo does
   not retain an upstream synchronization remote.
7. Re-run the five component history acceptance records from final `main` and
   record the final path and commit used for each proof.

Exit gate:

- Final `main` reaches the complete exact Lan Mouse commit graph and the complete
  selected, sanitized `desktopimprove` history.
- No imported history was squashed.
- All historical path, original-ID, signature, and mapped-SHA lookup checks pass.
- `lan-mouse`, `tv-multiview`, `deploy`, `docs`, and `tla` each have complete,
  independently reviewable import evidence.
- The only new structural events above imported history are the Lan Mouse layout
  commit and the explicit assembly merge.

## P5: Establish One Root Cargo Workspace

Status: pending.

Create a virtual root manifest with these members:

```toml
[workspace]
resolver = "2"
members = [
    "lan-mouse",
    "lan-mouse/input-capture",
    "lan-mouse/input-emulation",
    "lan-mouse/input-event",
    "lan-mouse/lan-mouse-cli",
    "lan-mouse/lan-mouse-clipboard",
    "lan-mouse/lan-mouse-gtk",
    "lan-mouse/lan-mouse-ipc",
    "lan-mouse/lan-mouse-proto",
    "tv-multiview",
]
default-members = ["lan-mouse", "tv-multiview"]
```

Actions:

1. Remove only the `[workspace]` declaration from
   `lan-mouse/Cargo.toml`. Keep its `[package]`, features, dependencies, bundle
   metadata, and internal relative dependency paths.
2. Keep `tv-multiview/Cargo.toml` as a package manifest and add its license and
   repository metadata after the root license decision.
3. Move profile ownership to the root. Keep the normal root `release` profile
   available to `tv-multiview`; do not silently apply Lan Mouse's current
   `panic = "abort"`, fat LTO, stripping, and single codegen unit to every
   workspace member.
4. Preserve Lan Mouse production optimization behavior with this root-owned
   custom profile, whose syntax and output-directory behavior are supported by
   the installed Cargo reference:

   ```toml
   [profile.lan-mouse-release]
   inherits = "release"
   codegen-units = 1
   lto = "fat"
   strip = true
   panic = "abort"
   ```

   Lan Mouse release commands use `--profile lan-mouse-release`; its outputs are
   read only from `target/lan-mouse-release/`. `tv-multiview` release commands
   use the standard root `--release` profile and `target/release/`.
5. Start the unified lock from the current Lan Mouse lockfile, add
   `tv-multiview` through root resolution, and inspect the lock diff. Existing
   Lan Mouse dependency versions must not drift without a demonstrated resolver
   conflict and focused verification.
6. Delete `lan-mouse/Cargo.lock` and `tv-multiview/Cargo.lock` only after the root
   lock is complete and both package sets build from it.
7. Update package repository URLs and root metadata to the final GitHub URL.
8. Verify Lan Mouse build metadata still reports the monorepo commit and does not
   assume its package directory is the Git root.
9. Update bundle resource paths only where root invocation changes resolution;
   do not reorganize application resources as part of this migration.

Required verification:

```bash
cargo metadata --locked --format-version 1
cargo check --locked --workspace
cargo test --locked --workspace
cargo test --locked --workspace --exclude lan-mouse-gtk --no-default-features
cargo test --locked -p tv-multiview
cargo build --locked -p lan-mouse
cargo build --locked --profile lan-mouse-release -p lan-mouse
cargo build --locked --release -p tv-multiview
```

Additional checks:

- `cargo metadata` reports the repository root as the only workspace root.
- The workspace has ten members: nine Lan Mouse packages and
  `tv-multiview`.
- A repository search finds one `[workspace]` and one tracked `Cargo.lock`.
- Default Lan Mouse build features still include GTK.
- Exact no-GTK production feature builds still pass.
- Linux may run root-wide workspace commands. Native macOS and Windows jobs must
  select the applicable Lan Mouse packages explicitly and must not use an
  unqualified `--workspace` command that also selects Linux-only
  `tv-multiview`.
- Every release command selects `-p lan-mouse` or `-p tv-multiview` explicitly;
  packaging reads from the profile-specific output directory rather than an
  assumed shared `target/release` path.
- `tv-multiview` tests produce the same passing test count as the pre-migration
  baseline or an explicitly explained increase.

Commit boundary:

- One mechanical commit establishes the root workspace, profile ownership,
  package metadata, and root lockfile. It contains no controller, clipboard,
  input, TV, or deployment behavior change.

Rollback:

- Revert the workspace commit inside the unpublished migration repository or
  regenerate the migration. Do not patch the source repositories.

## P6: Adapt Deployment to the Monorepo and Unified Release

Status: pending.

### Public Configuration

1. Replace real inventory and group variables with `.example` files containing
   documented placeholders.
2. Keep real `deploy/inventory.ini` and `deploy/group_vars/all.yml` ignored and
   local-only.
3. Do not publish a configured source revision, fixed commit, credential, local
   key path, real host identity, TV address, HDMI identifier, MAC address, TLS
   fingerprint, or controller token.
4. Do not retain a user-maintained hard-coded Cargo lock digest. If native build
   needs a lock digest as an internal archive/cache key, derive it from the root
   `Cargo.lock` during the run and do not treat it as compatibility evidence.

### Native-Build Mode

1. Preserve native build as a supported option and the initial default.
2. Build from the monorepo root and select packages explicitly.
3. Create one source bundle from the monorepo commit. The remote native build
   checkout must contain root `Cargo.toml`, root `Cargo.lock`, `lan-mouse/`, and
   `tv-multiview/`; an incomplete workspace checkout is invalid.
4. Vendor from the root lockfile. The vendor archive is a transport optimization,
   not a version pin or runtime compatibility mechanism.
5. Keep Linux, macOS, and Windows Lan Mouse native builds parallel under the
   current Ansible strategy. Do not serialize hosts merely because they share a
   source commit.
6. Build Lan Mouse with the existing target-specific no-GTK feature sets.
7. Build `tv-multiview` only on the Linux controller host.
8. Preserve current native macOS code-signing identity handling so publication
   changes do not invalidate Accessibility authorization.

### GitHub-Release Mode

1. Keep GitHub-release installation as a second explicit mode.
2. Keep one repository setting and one user-facing release selector for both Lan
   Mouse and `tv-multiview`. An explicit tag is preferred, but the existing
   `latest` selector remains accepted for operator convenience.
3. Resolve the selector exactly once on the Ansible controller before parallel
   host tasks. If it is `latest`, query the release API once and freeze the
   returned immutable tag name, release ID, and tag target commit for the whole
   play. Never let individual hosts resolve `latest` independently.
4. Fetch the workflow-generated checksums/asset manifest once from that release,
   validate its release identity, reject missing or duplicate expected names,
   and distribute the frozen asset names and SHA-256 digests to host tasks.
5. Build every URL with `releases/download/<resolved-tag>/...`; never use
   `latest/download` after resolution. Download the native Lan Mouse asset
   selected by host OS and architecture and verify its declared SHA-256 before
   extraction or installation.
6. Download `tv-multiview-linux-x86_64.tar.gz` only on the supported Linux
   controller host.
7. Record the resolved tag, release ID, target commit, manifest digest, and each
   installed asset digest in the run result and installation marker. This is a
   per-run immutable resolution, not a source-revision compatibility variable.
8. Preserve macOS local re-signing after digest verification and before install.
9. Do not download or invent a deployment archive; playbooks execute from the
   tagged source checkout.
10. Reject a release whose tag, release ID, target commit, manifest, asset set,
    or digest changes during the run. A new run must resolve a fresh identity.

### Service Behavior

1. Path and artifact selection changes may update service templates, but the
   publication migration does not execute them.
2. Existing Linux systemd, macOS LaunchAgent, and Windows scheduled-task
   supervision remain the deployment mechanisms.
3. Restart decisions remain based on installed runtime input changes, not merely
   on Git checkout movement or documentation changes.

Verification:

- Ansible syntax check passes with sanitized example inventory for native-build
  mode.
- Ansible syntax check passes with sanitized example inventory for
  GitHub-release mode.
- Task expansion proves all native hosts remain parallel.
- A no-change check does not require network access or restart a service.
- Artifact URL construction tests cover each supported OS/architecture and the
  Linux controller asset.
- Resolution tests prove `latest` is queried once, all hosts consume one frozen
  identity, no URL contains `latest/download`, and checksum failure occurs before
  extraction or service replacement.
- Duplicate, absent, extra, wrong-digest, and mixed-release assets are rejected.
- No live inventory or host is contacted during this phase.

Commit boundary:

- One deployment commit adapts paths, root lock ownership, build commands,
  source bundling, examples, and unified release asset selection. It does not
  include GitHub workflow implementation.

## P7: Create Root CI and Release Workflows

Status: pending.

### Workflow Trust and Permissions

1. Pin every third-party and GitHub-maintained action to a reviewed full commit
   SHA. A comment may record its human-readable release; mutable major or version
   tags are not accepted in release-critical or CI workflows.
2. Declare workflow-level permissions as read-only and grant the minimum
   additional permission per job. Only the final draft-release job receives
   `contents: write`, through a protected release environment with approval.
3. Pull-request jobs, especially jobs that execute untrusted code, receive no
   write token, release credential, environment secret, or privileged event
   context.
4. Record action owner, source repository, reviewed SHA, and purpose in the
   workflow review ledger. Verify those pins again in private staging before
   public visibility.

### CI Workflow

The root CI workflow runs for pull requests, ordinary branch pushes, and
explicit `workflow_dispatch`. It does not publish releases. The manual trigger
is required so the frozen candidate can be checked after the initial remote ref
import completes with GitHub Actions disabled.

Required jobs:

1. Root workspace metadata and lockfile validation.
2. Lan Mouse default GTK check/build on supported Linux CI.
3. Lan Mouse no-GTK workspace tests and target-feature checks.
4. `tv-multiview` check, tests, and lints on Linux.
5. Deployment YAML and Ansible syntax checks against examples.
6. Workflow syntax validation.

Linux CI may run root-wide workspace checks. Native Windows and macOS jobs select
only their applicable Lan Mouse package set; they explicitly exclude
`tv-multiview` instead of relying on an unqualified root `--workspace` command.

Use path filters or job-level change detection so a docs-only or deployment-only
commit does not launch every native binary build. Shared workspace manifest,
lockfile, workflow, or relevant source changes must still select every affected
job. Path selection is a CI-cost optimization only; it is not release
compatibility evidence.

### Release Workflow

Release publication runs only for an approved `v*` tag or explicit manual
prerelease dispatch. It does not run on every `main` push.

Required Lan Mouse assets:

- existing default GTK Linux artifacts;
- existing default Windows artifact;
- existing default macOS x86_64 and arm64 artifacts;
- `lan-mouse-no-gtk-linux-x86_64.tar.gz`;
- `lan-mouse-no-gtk-linux-aarch64.tar.gz`;
- `lan-mouse-no-gtk-windows-x86_64.zip`;
- `lan-mouse-no-gtk-macos-x86_64.zip`;
- `lan-mouse-no-gtk-macos-aarch64.zip`.

Required initial controller asset:

- `tv-multiview-linux-x86_64.tar.gz`.

Release rules:

1. Every build checks out and verifies the triggering tag's exact commit. The
   job refuses a tag that already has a public release or uploaded assets and
   never moves, deletes, recreates, or overwrites a public tag.
2. Every build uses the root lockfile with `--locked`.
3. Lan Mouse production jobs select
   `-p lan-mouse --profile lan-mouse-release`; `tv-multiview` selects
   `-p tv-multiview --release`.
   Packaging reads each profile's declared output directory.
4. Native jobs upload workflow artifacts. One final job downloads the complete
   declared matrix, rejects duplicate or unexpected names, computes SHA-256 for
   every archive, and creates a checksums/asset manifest bound to the repository,
   tag, tag commit, and declared asset set.
5. The final job creates a draft release, uploads the complete matrix and
   manifest, queries the draft through the API, and proves its exact names,
   sizes, and digests match the validated local set. Only then may the protected
   job publish the draft.
6. A missing job, archive, manifest entry, digest, or draft verification keeps
   the release unpublished. Failure may retain or separately remove only the
   draft; it must never expose a partial release as `latest`.
7. Each binary archive includes the applicable license and a short component
   README.
8. Publish one checksums/asset manifest covering every uploaded binary archive.
9. Deployment has no binary asset.
10. Do not add controller Windows/macOS jobs until those service integrations are
   implemented and added to the design.
11. Preserve the existing default GTK builds while adding monorepo package paths;
   do not replace them with no-GTK-only publication.
12. Release assets are treated as mutable remote objects despite their tag URL.
    The workflow refuses replacement, and deployment trusts the frozen manifest
    digest plus per-asset digest rather than tag text alone.
13. Rerunning the same public tag is allowed only for an infrastructure retry
    from the identical source when no public release or asset exists. A source
    fix always uses a new version tag.

Verification:

- Workflow YAML validates locally with the available validator.
- Exact Linux build and packaging commands pass locally.
- GitHub native Windows and macOS jobs pass on the pushed branch before the
  first release tag.
- Archive inspection verifies names, executable paths, licenses, and checksums.
- A forced missing/duplicate/wrong-digest asset leaves only an unpublished draft
  and never changes the latest public release.
- Workflow search finds no action reference that is not a full commit SHA, and
  permission review proves untrusted jobs cannot write repository content.
- Release workflow does not trigger from a docs-only `main` commit.

Commit boundary:

- One CI commit establishes root checks.
- One release commit migrates existing Lan Mouse release jobs and adds the Linux
  controller asset.

## P8: Prepare Public Documentation and Licensing

Status: pending.

Actions:

1. Add a root README that defines the product, component ownership, supported
   operating systems, one-workspace build commands, release artifacts, and
   deployment entry point.
2. Preserve Lan Mouse attribution, copyright notices, and
   `GPL-3.0-or-later` license material.
3. Select and record the root license before public push. If one root GPL license
   is selected, update original component manifests and documentation
   consistently; do not imply relicensing of third-party code.
4. Add a notices section explaining that `lan-mouse/` contains history derived
   from the Lan Mouse project.
5. Rewrite machine-absolute documentation links to repository-relative links.
6. Retain completed design and implementation plans as historical engineering
   records, but label stale source paths and revisions as historical rather than
   current instructions.
7. Add the three relevant present-state documents that lack selected Git
   history only in a dedicated documentation commit:
   `WakeDisplaydesign.md`, `reviesionandclipboradexplore.md`, and
   `publishplan.md`. Do not copy `clipboardplan.md`; P2 imports its full selected
   history.
8. Do not publish pane transcripts, copied chat histories, quick suggestions,
   screenshots containing private state, local clones, archives, or generated
   summaries.
9. Document how to create local real inventory from examples without committing
   it.
10. Document both deployment modes without making GitHub-release mode replace
    native-build mode.
11. Document that release creation does not deploy or restart hosts.
12. Review the private identity inventory and record a publication decision for
    every identity class and email-like historical message. Public upstream
    identities may remain with attribution; local/private identities require
    explicit approval or deterministic parent-history rewrite. Do not publish
    the inventory's raw values as review evidence.
13. Record ownership/license approval for original `tv-multiview`, deployment,
    docs, and TLA+ content. This is a publication gate, not an inference from the
    Lan Mouse license.
14. Produce a final exposure manifest listing the exact commit, refs, licenses,
    identity policy, archival tags, workflow pins, and artifact names proposed
    for public visibility. It contains classifications and digests, not secrets.

Verification:

- Every local Markdown link resolves within the monorepo or is an intentional
  external URL.
- Repository search finds no machine-specific absolute home path, local file
  URI, private inventory path, pane-log filename, or local source checkout
  instruction in public-facing current documentation. Historical records may
  retain old commit IDs but not machine secrets or unusable absolute links.
- Root and component license declarations are consistent.
- Every reachable author/committer/tagger identity and email-like message has a
  recorded publish-or-rewrite disposition, and the post-policy scan has no
  unknown identity.
- The exposure manifest has explicit owner approval before P11; approval cannot
  be inferred from private staging success.
- Example deployment documentation can be followed from a clean clone.

## P9: Offline Pre-Publication Verification

Status: pending.

Run all gates against the final local monorepo before adding any remote. Passing
P9 authorizes only private staging, not public exposure.

### Migration Safety Gate

- Revalidate migration-root canonical paths and markers and prove run roots
  reject both source trees, both source parents, home, `/`, and the recorded
  final-checkout target. Separately prove the final checkout does not overlap a
  source or run tree; a sibling under the source's project parent is valid.
- Verify both external bundle digests, run `git bundle verify`, and repeat a
  restore drill in a fresh scratch clone.
- Prove every migration commit was derived from the recorded object IDs and no
  source branch movement changed the inputs.

### History Gate

- Compare each source commit ledger with the Lan Mouse identity/ref record or
  the corresponding final composed parent-history map.
- Verify separate passing acceptance records for `lan-mouse`, `tv-multiview`,
  `deploy`, `docs`, and `tla`. Each record must identify its exact source set,
  destination path, map/identity proof, overlap set, and representative
  log/blame/diff checks.
- Verify every selected Lan Mouse commit remains reachable at its exact original
  object ID, and compare the complete signed set's raw objects and `gpgsig`
  headers. Run signature verification where keys are available.
- Verify every selected `desktopimprove` commit has a reachable mapped commit,
  including all 11 clipboard-plan commits.
- Recompute each parent selector and its union from `$DESKTOPIMPROVE_SHA`; compare
  those sets with the P2A-P2D and post-P3 records rather than validating only the
  aggregate 88-commit baseline.
- Verify representative path history, blame, merges, imported tags, and the two
  import-tip tags.
- Verify all cross-input acceptance commits remain one commit each.
- Run `git fsck --full`.
- Verify no temporary migration ref is required for ancestry reachability.
- Compare approved archival tag targets and metadata with the private original
  ref map. For every proposed archival or import tag, enumerate root-level
  `.github/workflows` files and their triggers at the target commit. Record every
  broad `push` trigger; do not rewrite exact Lan Mouse commits to suppress it.

### Secret Gate

- Run TruffleHog over all retained branches, archival tags, and import tags with
  external verification disabled.
- Search all reachable blobs and commit/tag messages for exact known private
  values, sensitive formats, absolute local paths, and environment identities.
- Inspect ignored and untracked files to prove they cannot enter the first push.
- Build release archives locally where possible and scan their extracted
  contents.
- Repeat after the final documentation and release-workflow commits.

### Identity and License Gate

- Compare all reachable author, committer, and tagger values plus email-like
  message content with the approved identity policy; no unknown value remains.
- Prove exact Lan Mouse commits needed no identity rewrite. If one is required,
  stop because the signed-history and identity policies conflict.
- Verify component/root license declarations and recorded ownership approval.
- Verify the exposure manifest identifies the exact candidate commit and refs.

### Workspace Gate

- `cargo metadata --locked` reports one workspace root and all ten packages.
- Exactly one tracked `Cargo.lock` exists.
- `cargo check` passes after every Rust/workspace change.
- Required workspace, Lan Mouse, no-GTK, and `tv-multiview` tests pass.
- Release package builds use the root lock and selected package/profile.
- Native macOS/Windows command definitions select Lan Mouse packages explicitly;
  only Linux uses root-wide workspace testing that includes `tv-multiview`.
- No nested workspace warning or ignored-profile warning occurs.

### Deployment Gate

- Ansible syntax passes for native-build and GitHub-release examples.
- Static task inspection proves native host parallelism remains.
- Source bundle inspection proves the root workspace is complete.
- No command contacts or restarts a live host.

### Workflow Gate

- Local workflow validation passes.
- The release job's declared needs cover all required asset jobs.
- Artifact names exactly match deployment download names.
- Main-branch CI and tag-release triggers are disjoint as designed.
- Every action is pinned to a reviewed full commit SHA and permissions are
  minimal per job.
- Static release-graph inspection proves draft creation, complete upload,
  manifest/API verification, protected approval, and publication occur in that
  order.
- The remote-import runbook disables GitHub Actions and reads the setting back
  before the first ref push, uploads only the frozen allowlist while disabled,
  queries zero workflow runs, and only then enables Actions and manually
  dispatches current root CI against the frozen `main` commit.
- The historical-workflow inventory covers every proposed archival and import
  tag target. No tag-name pattern or multi-tag event suppression is treated as a
  safety mechanism.

Exit gate:

- Every invariant has recorded evidence in the migration ledger.
- There are no unresolved scanner findings, missing histories, workspace
  warnings, failed tests, missing assets, unknown identities, or undocumented
  license decisions.
- Every publication input has independently reviewable import evidence, and all
  shared parent commits retain atomic selected diffs.
- The exact candidate is safe to copy to a private staging repository. Native
  GitHub execution and public-visibility approval are still pending.

## P10: Stage and Verify in a Private GitHub Repository

Status: pending and externally gated.

Preconditions:

- P0 through P9 complete.
- User supplies the GitHub owner and approves a temporary private staging
  repository name and initial branch.
- The candidate commit and allowed ref list match the P9 exposure manifest.

Actions:

1. Create an empty private staging repository with no generated README, license,
   or `.gitignore`.
2. Disable GitHub Actions through the repository setting before adding a remote
   or pushing a ref. Read the setting back through the API and stop unless it
   reports disabled. Repository creation defaults are not evidence that
   historical workflows are quarantined.
3. Add the repository as `staging` only in the migrated monorepo. While Actions
   remains disabled, push `main` and only the approved namespaced archival and
   import-tip tags by explicit refspec. Never use `git push --mirror`.
4. Query remote refs and workflow runs before enabling Actions. The refs must
   match the frozen allowlist and the workflow-run count must be zero. A run
   associated with any archival or import ref fails the quarantine gate.
5. Configure the reviewed action allowlist, minimum token permissions, and
   protected release environment, then enable Actions and read the setting back.
   Manually dispatch current root CI against the exact frozen `main` commit,
   including native GitHub macOS and Windows Lan Mouse jobs. Do not create a
   no-op commit merely to trigger CI.
6. Run the complete release workflow manually with a private-only
   `staging-<run-id>` prerelease identity. Build every asset natively, exercise
   draft assembly and verification, and do not reuse any staging artifact in the
   eventual public release.
7. Download the private prerelease assets and manifest into a clean directory;
   verify digests, archive contents, executable formats, licenses, attribution,
   and non-destructive help/version behavior where supported.
8. Clone the private remote into a fresh audit directory. Fetch only its explicit
   allowlisted refs, compare their object IDs and trees with the local candidate,
   run `git fsck`, and repeat history, secret, path, message, and identity scans.
9. Query remote branches, tags, Actions runs and artifacts, environments,
   packages, and releases. Record and resolve every unexpected object before
   proceeding.

Exit gate:

- Native Linux, macOS, and Windows CI passes on the exact candidate.
- The private prerelease contains the complete declared asset matrix and passes
  draft, manifest, archive, and scanner checks.
- A fresh remote clone matches the local exposure manifest exactly.
- No historical workflow ran while archival and import refs were uploaded.
- The repository remains private; this phase grants no public-push authority.

Recovery:

- A CI, workflow, scanner, or remote-ref failure blocks P11. Fix only in the
  local migration repository, create a new candidate commit when necessary,
  rerun P9, and stage the new candidate without reusing artifacts.
- If any historical workflow starts, immediately disable Actions, cancel all
  runs, record the triggering ref and workflow commit, and treat every secret
  available to that repository as potentially disclosed. Rotate affected
  credentials and use a fresh staging repository after the offline gates pass
  again.
- If a secret reached the private service, treat it as third-party exposure:
  rotate it and create a fresh private staging repository from regenerated
  history. Force-pushing the same staging repository is not sufficient proof of
  removal.
- Do not delete the private staging repository or its evidence as an implicit
  rollback. Any deletion is a separate, explicitly authorized cleanup action.

## P11: Authorize and Create the Public Repository

Status: pending and explicitly gated as irreversible disclosure.

Preconditions:

- P10 passes for the exact candidate commit, refs, workflow pins, and asset
  definitions.
- The user reviews and approves the final repository owner/name, root license,
  identity policy, attribution, exposure manifest, and initial branch.
- The user explicitly authorizes public exposure of that exact manifest. A prior
  request to create private staging is not public authorization.

Actions:

1. Freeze the candidate commit and ref allowlist. Re-run the exact-value,
   identity, license, and remote-artifact gates immediately before exposure; no
   commit, tag, workflow pin, or document may change afterward.
2. Create the final empty public repository with no generated files. Keep the
   private staging repository private so its staging-only tags, prereleases, and
   Actions artifacts can never become public accidentally. Immediately disable
   Actions and read the repository setting back before adding a remote or
   pushing any ref.
3. While Actions remains disabled, add the final repository as `origin` and push
   only `main`, approved `lan-mouse-*` archival tags, and the two import-tip tags
   by explicit refspec. Never push staging tags, temporary refs, private
   artifacts, or a mirror.
4. Query the public repository before enabling Actions. Its refs must equal the
   frozen allowlist and its workflow-run count must be zero. Then configure the
   reviewed branch protections, action allowlist, token permissions, and
   protected release environment.
5. Enable Actions, read the setting back, and manually dispatch current root CI
   against the frozen public `main` commit. Do not create a public release tag
   yet.
6. Clone the public repository into a clean audit directory and compare its
   allowlisted refs, graph, trees, files, and workflow SHAs with the frozen
   candidate. Repeat secret and identity scans and wait for public branch CI.

Exit gate:

- The public repository contains exactly the approved source graph and refs.
- Public branch CI passes on the same commit that passed private native CI.
- No historical workflow ran while archival and import refs were uploaded.
- No unintended branch, ref, release, package, artifact, staging identity, or
  secret is public.

Recovery:

- Before the first public ref is pushed, stop without exposure.
- If any historical workflow starts after a public ref push, immediately disable
  Actions and cancel all runs. Treat the workflow, token, and any available
  secret as a public exposure incident; namespaced tags are not a containment
  boundary.
- After the first public ref is pushed, assume every reachable object may have
  been cloned. Visibility reversal, ref deletion, or force-push cannot recall it;
  rotate exposed secrets and publish corrected history only as incident response.

## P12: Establish the Standalone Local Checkout

Status: pending.

Semantics:

- This phase implements "move out of `desktopimprove`" by cloning the verified
  public repository. It does not move files or reuse a source repository's
  `.git` directory.
- `$FINAL_CHECKOUT` is the private-ledger target recorded in P0. Its concrete
  machine path is not committed to public documentation.

Preconditions:

- P11 passes and public `main` resolves to the frozen candidate commit.
- `$FINAL_CHECKOUT` is still absent; its canonical parent is unchanged,
  non-symlinked, writable, and outside both source trees plus migration, backup,
  and private-ledger roots. A sibling of `desktopimprove` under the same project
  parent is allowed.
- No source repository or source directory has been moved, renamed, deleted, or
  repurposed as the final checkout.

Actions:

1. Re-run the P0 path-overlap checks immediately before cloning. Stop if the
   target now exists, even if it appears empty.
2. Clone the public repository directly into `$FINAL_CHECKOUT`; do not copy or
   rename the migration worktree into place.
3. Check out public `main` and prove its commit, tree, parents, allowlisted tags,
   import-tip tags, and component history records match the P11 audit clone.
4. Verify the clone has its own `.git` directory under `$FINAL_CHECKOUT`, has only
   the public repository as `origin`, and has no source, migration, staging, or
   filesystem remote.
5. Run `git fsck --full`, secret/path checks over fetched refs, and a clean
   worktree check. Re-run root Cargo metadata/check/tests, deployment syntax,
   workflow validation, and documentation-link checks from this standalone
   checkout.
6. Record the final checkout canonical path, public origin, `main` commit, tree,
   fetched refs, and verification results in the private ledger.
7. Designate `$FINAL_CHECKOUT` as the controlling local development checkout for
   future osswitch work. The migration clone remains evidence, not a development
   workspace.
8. Leave `$LAN_MOUSE_SOURCE`, `$DESKTOPIMPROVE_SOURCE`, and every selected source
   directory unchanged. Their later archival or removal is outside this plan and
   requires a separate verified-backup and explicit-cleanup decision.

Exit gate:

- One clean standalone checkout outside `desktopimprove` matches public `main`
  and all allowlisted refs exactly.
- All five component history acceptance records pass from the standalone clone.
- Root workspace, deployment, workflow, and documentation clean-clone checks
  pass.
- Both original source repositories and their directories still exist and match
  their P0 state.

Recovery:

- If the target exists or overlaps a source or run tree, stop and choose a new
  absent target; never merge into or overwrite the existing directory. Sharing
  only the project parent with `desktopimprove` is not an overlap.
- If cloning or verification fails after creating the target, retain the partial
  checkout as evidence. Retry only in another absent path or after a separately
  authorized, marker-guarded cleanup of that newly created failed clone.
- Never repair a failed standalone clone by moving files out of either source
  repository.

## P13: Publish the First Coordinated Release

Status: pending and externally gated.

Preconditions:

- P12 passes from the clean standalone checkout.
- Public branch CI passes.
- The initial version, exact source commit, and release notes are approved.
- The complete workflow passed in private staging from the same source commit;
  its artifacts are evidence only and will not be reused.
- The proposed public `v*` tag, release, and asset names do not already exist.

Actions:

1. From a clean `$FINAL_CHECKOUT`, create one annotated `v*` tag at the reviewed
   monorepo commit and verify its target and public `origin` before pushing only
   that tag.
2. Let GitHub Actions rebuild every required asset from the public tag. No local
   or private-staging artifact may enter the run.
3. Require the workflow to create an unpublished draft, upload the complete
   matrix and checksums/asset manifest, query and verify the draft's exact set,
   and publish only after protected approval.
4. Verify the public release contains the complete Lan Mouse matrix,
   `tv-multiview-linux-x86_64.tar.gz`, source archives generated by GitHub, and
   the checksums/asset manifest. Verify there is no deployment binary archive.
5. Download every release artifact into a clean directory, verify checksums,
   inspect archive paths and executable formats, and run non-destructive version
   or help smoke checks where supported.
6. Confirm deployment resolution freezes this release's tag, release ID, target
   commit, manifest digest, and asset digests without executing a live playbook.
7. Publish release notes describing supported platforms, native versus release
   deployment choices, known controller platform limits, and attribution.
8. Never move, delete, recreate, or overwrite the public tag or its release
   assets. An infrastructure-only retry may reuse the tag only if the source is
   identical and no public release or asset exists; any source fix uses a new
   version tag.

Exit gate:

- One immutable public tag identifies source, workspace lock, deployment source,
  manifest, and every binary artifact.
- The release was invisible until its complete draft passed exact verification,
  and it is consumable from a clean checkout.
- No live host or service changed as a side effect of publication.
- The final standalone checkout remains clean and points to the released public
  commit.

## Commit Sequence

The migration should produce reviewable commits after the imported histories:

1. `chore: assemble osswitch monorepo history`
2. `build: establish root Cargo workspace`
3. `deploy: adapt deployment to osswitch workspace`
4. `ci: validate osswitch workspace`
5. `ci: publish coordinated release assets`
6. `docs: prepare osswitch for public release`

Parent-history redaction happens before the assembly merge and is represented
through the composed map, not hidden in a later cleanup commit. Lan Mouse exact
history is never redacted silently; a finding blocks for an explicit policy
decision. Do not combine Rust logic changes with any migration commit.

Each code-affecting commit requires `cargo check`; any logic change discovered
as necessary during migration requires focused tests and a separate bug-fix
commit. The planned migration itself should not require Rust logic changes.

## Failure and Recovery Matrix

### Unsafe Migration or Cleanup Target

- State: canonical path, overlap, non-symlink, expected-child, or run-marker
  assertion failed.
- Action: execute nothing against that target; retain evidence and start a new
  uniquely marked migration root after correcting the path.
- Forbidden response: bypass the guard, broaden a recursive deletion, or use
  `--force` in a source, backup, or unverified directory.

### Backup or Restore Drill Fails

- State: history-changing work is blocked.
- Action: recreate the external bundle from unchanged recorded refs, verify its
  digest and heads, and pass a fresh restore drill before continuing.
- Forbidden response: treat the source worktree, reflog, or migration clone as
  the only backup.

### Source Ref Moved After Freeze

- State: a named source ref no longer matches the recorded object ID.
- Action: stop. Either continue from the already verified bundle and recorded ID
  or begin a new run with a new ledger and backups. Never silently consume the
  new tip.

### Lan Mouse Signature, Secret, or Identity Conflict

- State: exact Lan Mouse ancestry contains content or identity that policy says
  must be rewritten, or a selected signature/object changes.
- Action: block publication and ask for an explicit choice between retaining the
  exact signed/public history, excluding the affected selection where legally
  and historically valid, or accepting a separately reviewed rewrite with
  signature loss. Never claim both exact signatures and rewritten commits.

### Missing Per-Input History or Commit Mapping

- State: history gate failed.
- Action: stop; identify the failed component record, compare its exact source
  set with the union and composed maps, then regenerate the one atomic parent
  import if any parent selector is affected.
- Forbidden response: manually copy the missing current file and claim history
  preservation.

### Cross-Input Commit Split or Duplicated

- State: atomicity invariant failed.
- Action: regenerate the disposable parent clone and re-extract all five
  selections together.

### Final Standalone Checkout Target Is Unsafe

- State: `$FINAL_CHECKOUT` exists, is symlinked, overlaps a source or run tree,
  or its canonical parent changed after P0.
- Action: clone nothing and select another absent target outside every source
  and run tree; update the private ledger and rerun the P0 path gate. A sibling
  of `desktopimprove` under the same project parent remains valid.
- Forbidden response: merge into, overwrite, empty, or recursively remove the
  existing target.

### Standalone Clone Differs From Public Candidate

- State: checkout commit, tree, refs, component history record, workspace check,
  or clean-clone verification differs from P11.
- Action: stop before release. Retain the failed clone, compare public refs with
  the frozen manifest, and retry only in another absent target or after
  separately authorized cleanup of the newly created failed clone.
- Forbidden response: copy files from migration or source directories to patch
  the clone in place.

### Secret Found Before Any Remote Push

- State: publication blocked.
- Action: add exact redaction input, regenerate, and re-run all scans.

### Secret Found After Push

- State: public exposure incident.
- Action: rotate the value, remove public refs or repository access as needed,
  regenerate clean history, and treat force-push alone as insufficient.

### Historical Workflow Starts During Initial Ref Import

- State: the remote Actions setting was not disabled, changed unexpectedly, or
  an archival/import ref started a workflow from its historical target commit.
- Action: disable Actions immediately, cancel every run, record the triggering
  ref and exact workflow commit, and audit token and secret availability. In
  private staging, rotate every potentially exposed credential and restart P10
  with a fresh repository. After a public ref exists, apply the public-exposure
  incident rule.
- Forbidden response: rely on `lan-mouse-*` naming, current-workflow tag filters,
  a multi-tag push event limit, or the absence of a configured Cachix secret as
  proof that historical workflows cannot execute.

### Private Staging CI or Prerelease Fails

- State: public visibility remains blocked.
- Action: fix the exact local source/workflow defect, rerun all affected P9
  gates, and stage a new candidate. Do not reuse private prerelease artifacts.

### Identity or License Approval Missing

- State: irreversible public-visibility gate is blocked even if all tests pass.
- Action: retain the repository privately until ownership, license, attribution,
  and identity exposure are explicitly approved.

### Unified Lock Changes Existing Resolution

- State: workspace gate blocked.
- Action: compare both old locks with the root lock, constrain only the required
  packages, and run focused component tests. Do not accept broad lock churn as a
  formatting side effect.

### Native Asset Job Fails

- State: release incomplete.
- Action: keep the release draft unpublished. For private staging, fix and run a
  new candidate. For a public tag, retry only identical source with no public
  assets; otherwise fix source and use a new version tag. Never upload a local
  replacement.

### Deployment Asset Name Mismatch

- State: release deployment gate failed.
- Action: align workflow, manifest, and Ansible constants in source, test from a
  new tag, and do not rename or replace an existing uploaded asset manually.

### Release Identity Changes During Deployment

- State: resolved tag, release ID, target commit, manifest, or asset digest no
  longer matches the frozen controller facts.
- Action: abort before extraction or service replacement. Start a new deployment
  run and resolve one fresh immutable identity; never let hosts continue with a
  mixed set.

### Private or Public Repository Creation Fails

- State: local migration and external backups remain authoritative; originals
  remain untouched.
- Action: fix ownership or permissions without changing validated history. A
  private-stage failure returns to P10; a pre-push public failure returns to P11.
  After any public ref exists, apply the public-exposure recovery rule.

## Definition of Done

This plan is complete only when all of the following are true:

- One public GitHub repository contains `lan-mouse`, `tv-multiview`, `deploy`,
  `docs`, and `tla` in the approved layout.
- Both selected histories are reachable from `main`, unsquashed, mapped, and
  path-queryable.
- Each of the five final components has its own passing history acceptance
  record, while the five parent selectors were imported in one atomic rewrite.
- Every selected Lan Mouse historical commit retains its exact object ID and
  signature bytes; its tree moves under `lan-mouse/` in one new child commit.
- All 88 baseline-selected parent commits, including clipboard-plan history, are
  represented through the composed map; execution-time counts are recorded.
- Cross-input parent commits remain atomic.
- Verified external source bundles remain restorable outside the guarded
  migration root.
- No private deployment blob, environment literal, absolute local path,
  unapproved identity, or sensitive message is reachable; sanitized examples are
  usable.
- There is one root Cargo workspace with ten members and one root lockfile.
- Root Cargo checks and required logic tests pass.
- Existing default GTK Lan Mouse builds remain published.
- No-GTK Lan Mouse assets exist for every currently supported native target.
- The Linux `tv-multiview` tarball is present and matches current deployment
  support.
- Deployment source is published without a deployment binary archive.
- Native-build and GitHub-release deployment modes both validate.
- GitHub-release deployment resolves one immutable release identity and verifies
  every asset digest before installation.
- Private native CI and a complete private prerelease pass before explicit
  public authorization.
- Initial staging and public ref imports occur with GitHub Actions confirmed
  disabled; both remotes report zero historical workflow runs before current
  root CI is enabled and manually dispatched.
- One immutable public tag and verified manifest define the complete source and
  artifact set; no partial release becomes public.
- Public CI and the first draft-then-publish release workflow pass.
- A clean standalone checkout outside `desktopimprove` matches the public
  repository, passes clean-clone checks, and is the controlling local osswitch
  development checkout.
- Source repositories and running hosts remain unchanged by the migration and
  publication process.

## Execution Handoff

Execution begins only after this plan is reviewed and the user explicitly asks
to start it. The first implementation action is P0 evidence capture and external
backup/restore verification, not history rewriting. The first remote action is
the explicitly approved P10 creation of an empty private staging repository,
followed by confirmed Actions disablement before its allowlisted ref push. The
first irreversible public action is the P11 push of an allowlisted ref to the
empty public repository while Actions is confirmed disabled; it requires a
separate exact-manifest authorization. P12 then creates the standalone local
checkout without moving or deleting any source directory. P13 creates the first
coordinated release from that clean standalone checkout.
