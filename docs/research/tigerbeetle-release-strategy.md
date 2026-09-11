# TigerBeetle release strategy: lessons for monorelease

**Inspected:** 2026-09-11  
**TigerBeetle source revision:** `47aeb2212a255273dda508288412e537d11e4b7c`  
**Scope:** TigerBeetle `.github/workflows/`, its release implementation and release-process documentation, compared with this repository's `.github/workflows/`.

## Executive conclusion

TigerBeetle's strongest lesson is not to copy its manual `release`-branch process. It is to make release publication a small, serialized, human-approved state machine:

1. prepare and review release content;
2. validate the exact source intended for release;
3. build all artifacts once;
4. publish only the validated build outputs;
5. keep the public release boundary last;
6. continuously test what users can download from external registries.

`monorelease` already implements most of the stronger version of this design. In particular, it has immutable tag/source identity checks, a validation → matrix-build → publish DAG, least-privilege permissions, artifact attestations, exact asset inventory checks, checksums, a protected environment, draft publication, and a safe resume path. The highest-value improvements are therefore operational and test-oriented—not wholesale workflow replacement.

## Primary sources

TigerBeetle was inspected at the pinned commit above:

- [Release workflow](https://github.com/tigerbeetle/tigerbeetle/blob/47aeb2212a255273dda508288412e537d11e4b7c/.github/workflows/release.yml)
- [Release validation workflow](https://github.com/tigerbeetle/tigerbeetle/blob/47aeb2212a255273dda508288412e537d11e4b7c/.github/workflows/release_validate.yml)
- [Release process](https://github.com/tigerbeetle/tigerbeetle/blob/47aeb2212a255273dda508288412e537d11e4b7c/docs/internals/releases.md)
- [Release build/publish script](https://github.com/tigerbeetle/tigerbeetle/blob/47aeb2212a255273dda508288412e537d11e4b7c/src/scripts/release.zig)
- [Published-release validator](https://github.com/tigerbeetle/tigerbeetle/blob/47aeb2212a255273dda508288412e537d11e4b7c/src/scripts/ci.zig)

Local evidence is in `.github/workflows/ci.yml`, `.github/workflows/release.yml`, `.github/workflows/release_validate.yml`, `monorepo.toml`, and the generic release/changelog commands under `src/`.

## 1. What TigerBeetle actually does

### 1.1 The workflow surface is intentionally small

At the inspected revision, `.github/workflows/` contains two release-related workflows:

- `release.yml`: manually dispatched only.
- `release_validate.yml`: manually dispatched, triggered after the `Release` workflow completes, and scheduled every six hours.

Both use `concurrency.group: release` with `cancel-in-progress: false`. This is a deliberate cross-workflow mutex: a scheduled/post-release validation cannot race with publication. See the two workflow files, and their comments at the top of each file.

### 1.2 Release is a controlled promotion, not a tag-push reaction

The release workflow checks out the `release` branch regardless of the manual dispatch ref. The documented human process first merges a changelog PR into `main`, pushes `main` to `release`, allows a weekend of VOPR/fuzzing and graph review, then manually starts the workflow and obtains a second person's approval through the protected `release` environment (`docs/internals/releases.md`, “Release Manager Algorithm”; `.github/workflows/release.yml`).

The top changelog entry is the version source of truth. The release script parses it, derives the release version, and receives the workflow commit SHA as `--sha=${{ github.sha }}`. The GitHub release is created with `gh release create --draft --target <sha>`, so the draft is tied to the selected source commit (`src/scripts/release.zig`, approximately lines 82–161 and 692–804).

This gives TigerBeetle a soak period and a human promotion checkpoint. The cost is operational complexity: the branch must be selected correctly, and the branch can diverge from `main` or be accidentally used as a mutable release input.

### 1.3 One release command owns the multi-ecosystem graph

The workflow installs every toolchain needed by the distribution—.NET, Go, Java, an old supported Rust, Node, Python, Ruby, and Zig—then calls one orchestrator:

```text
./zig/zig build scripts -- release --build --publish --sha=${{ github.sha }}
```

The script treats the binary and client packages as one lockstep distribution. It builds into a canonical `dist` tree and publishes the binary release plus NuGet, Go, Maven, npm, Python/Ruby/Rust where enabled, Docker, and documentation (`docs/internals/releases.md`, “What Is a Release?” and “Publishing”; `src/scripts/release.zig`).

The architectural lesson is useful for this repository: put release-specific policy in one release orchestrator, while keeping the GitHub workflow responsible for permissions, scheduling, and promotion boundaries. `monorelease` already follows this shape through the `release` pipeline and `xtask`.

### 1.4 Publication is staged around a draft release

TigerBeetle creates a draft GitHub release first, uploads explicitly enumerated artifacts, publishes external packages, and then changes the GitHub release from draft to latest. The explicit artifact list in `release.zig` is a useful completeness guard: publication does not silently upload whatever happens to be in a directory.

Package publication is made partially idempotent: each package checks whether its target version is already the latest published version and skips re-publishing when so (`is_already_published` in `release.zig`; the documented error-handling section). The documented recovery procedure is to fix the problem, delete the draft, and trigger the workflow again. A fix-forward release is preferred after a bad public release; moving an existing release is reserved for a special protocol-level hot fix.

One important nuance: the code marks the GitHub release non-draft before publishing documentation (`release.zig` around lines 836–843), while the prose says the release becomes non-draft after registries succeed. That means the source and process documentation are not perfectly aligned; this is a reason to verify behavior rather than copy the pattern blindly.

### 1.5 Validation is black-box validation of what users receive

`release_validate.yml` checks out `main`, not the release branch or tag. Its Zig validator then discovers the latest published GitHub release, clones that tag into a temporary directory, downloads the published assets, rebuilds production artifacts from the tag, compares release artifact checksums with local builds (for deterministic non-debug artifacts), runs the released binary, builds and validates published client packages, runs sample clients against the released binary, checks that package registries report the expected latest version, and compares Docker `latest` and versioned image digests (`src/scripts/ci.zig`, `validate_release`).

This is the most important TigerBeetle-specific lesson: validation is designed to catch drift in systems the project does not control—package registries, Docker tags, runners, and release downloads—not merely to rerun source tests.

The tradeoff is cost and flakiness. The validator depends on many external registries and moving tool versions. It also skips some checks outside Linux, so its coverage is not perfectly symmetric across runners.

### 1.6 Security and failure controls

Both workflows begin with `permissions: {}`. The release job grants `contents: write`, `packages: write`, and `id-token: write`, and runs in the protected `release` environment. Publishing credentials are environment secrets. Actions are pinned to full commit SHAs. A separate `alert_failure` job posts to Slack after failures.

These are good controls, but the exact workflow should be inspected at the pinned revision whenever implementing them. The validation job does not grant an explicit read permission despite using `GITHUB_TOKEN`; its validation primarily reads public releases and registries. Do not infer that its permission shape is universally appropriate.

## 2. What this repository already does

### CI: `.github/workflows/ci.yml`

- PRs, pushes to `main`, and manual dispatch.
- `contents: read` and cancelable per-ref concurrency.
- Pinned checkout/toolchain/cache actions.
- Runs the dogfooded `ci` pipeline with `--locked` and packages the crate.
- No release permissions, environment, artifact publication, or provenance.

### Release: `.github/workflows/release.yml`

- Starts on `v*` tag push, with manual resume requiring an existing tag.
- Defaults to no permissions and grants permissions per job.
- Serializes all release workflows with `group: release`, never canceling in progress.
- Checks out the release tag, validates strict tag syntax, verifies checked-out commit equals the tag target, records annotated tag identity, and checks the tag against the Cargo package version.
- Runs CI and release gates before building anything.
- Builds four native targets in a fail-fast-disabled matrix.
- Asserts every binary's `--version` before packaging.
- Generates GitHub artifact attestations.
- Uploads artifacts and release notes as an explicit handoff to `publish`.
- Writes `BUILD-METADATA.json`, generates `SHA256SUMS`, and checks exact expected asset inventory before publishing.
- Uses the protected `release` environment, creates/reuses a draft, uploads with `--clobber`, and only then marks the release latest.
- Refuses to overwrite an already published release.
- Has a non-privileged failure annotation job.

### Published-release validation: `.github/workflows/release_validate.yml`

- Runs manually, after successful release completion, and weekly.
- Shares the `release` concurrency mutex.
- Finds the latest release and checks out the released tag, not `main`.
- Downloads all release archives, checks metadata against source commit/tag/repository, rebuilds Linux from the tag, byte-compares the released Linux binary, verifies its artifact attestation, and runs the published binary's release pipeline against the downloaded assets.
- Falls back to checksum/version checks for pre-metadata legacy releases.

The published `monorelease` CLI now owns generic changelog, release-source, manifest, and checksum verification. `xtask` retains only this repository's project-specific checks: binary identity, repository manifests, example workloads, and the release plan.

## 3. Comparison: where TigerBeetle adds signal

| Concern | TigerBeetle | `monorelease` | Lesson |
|---|---|---|---|
| Release input | Manually promoted `release` branch | Immutable version tag plus commit equality | Keep the tag model; it is safer for this small CLI. |
| Human checkpoint | Protected release environment and second approval | Protected `release` environment is declared in YAML | Configure required reviewers and allowed refs in repository settings. |
| Build/publish separation | One orchestrator, draft first | Validate → build matrix → publish jobs | Strong alignment; do not collapse it. |
| Artifact handoff | Local release script/dist tree | Actions artifacts between jobs | Local handoff is more auditable and avoids rebuilds. |
| Artifact completeness | Explicit upload list | Exact inventory and checksum-file checks | Keep the local gate; it is one of the strongest controls. |
| Provenance | Registry/release verification and reproducible checks | GitHub attestations plus build metadata and checksums | Local GitHub provenance is stronger; add consumer-facing verification tests. |
| Published artifact testing | Broad external package/Docker/client smoke tests | Published binary, checksums, Linux rebuild, examples | Add only the relevant external/install-path tests, not TigerBeetle's entire matrix. |
| Reproducibility | Rebuilds all deterministic production binary artifacts | Byte-compares Linux artifact | Consider expanding only where Rust cross-builds are deterministic and affordable. |
| Release cadence | Manual weekly promotion with soak/fuzzing | Tag-driven on demand | Adopt a soak/release-candidate process only if monorelease becomes high-risk. |
| Retry | Delete draft; package checks skip already-published versions | Reuse same draft/tag; refuse published overwrite | Prefer the local behavior; it is less destructive and clearer. |
| Notifications | Slack failure alert | GitHub error annotation | Add Slack/issue notification only if a human release rotation needs it. |
| Permissions | Empty default, elevated release job | Same principle, more explicit validation/build split | Keep least privilege and re-audit when adding external checks. |

## 4. Recommendations for `.github/workflows/`

### Adopt now

1. **Configure the declared `release` environment.** Require at least one reviewer appropriate to the project, restrict deployment branches/tags, and document who may approve. The YAML declaration alone does not create those protections.
2. **Test the failure/resume state machine in a staging repository.** Exercise validation failure, matrix-build failure, failure after draft creation, failure after one asset upload, rerun with the same tag, and rerun after publication. Verify that the tag is never moved and a public release is never partially exposed.
3. **Add a small installed-artifact smoke test.** In a clean temporary directory, extract each archive on its native runner, run `--version`, verify the expected files, and run one representative command. The current matrix only runs `--version`; the post-release workflow executes the published Linux binary but cannot execute macOS/Windows artifacts on Linux.
4. **Keep release validation external-facing.** Continue downloading the release through GitHub rather than validating only the workspace build. Add checks for the final release asset set and each archive's checksum, not just the Linux path.
5. **Document the provenance verification contract.** The release notes already show `gh attestation verify`; state that consumers must verify both the repository and the expected artifact identity, and keep `BUILD-METADATA.json` as the human-readable link to source and workflow run.
6. **Keep action pins and least-privilege defaults.** TigerBeetle reinforces this existing local policy; do not replace SHA pins with floating action tags for convenience.

### Adapt later

1. **Add a release-candidate soak path** if release risk grows: a protected `release-candidate` tag/branch, scheduled real-world validation, then human promotion to a final immutable tag. Do not replace the current tag identity checks with a mutable branch.
2. **Increase scheduled validation frequency** from weekly to daily or every six hours only if releases have meaningful external consumers and the cost/flakiness is acceptable. TigerBeetle's six-hour schedule is a response to many external registries and a high-risk native database, not a universal default.
3. **Expand reproducible rebuild checks** to more targets only after measuring Rust linker/toolchain determinism. A failed comparison caused by platform toolchain noise is worse than a narrower reliable gate.
4. **Add registry publication only behind an explicit draft/promote boundary** if `monorelease` later publishes crates or other packages. Each registry needs an idempotency check and a recovery plan; never publish every target directly from build jobs.
5. **Add durable failure notification** (Slack, issue, or owner routing) if releases become scheduled team operations. Keep it separate from the release permission boundary.

### Do not copy

- Do not adopt TigerBeetle's mutable `release` branch as the release identity; the existing tag + commit verification is simpler and safer here.
- Do not copy its full multi-language toolchain setup or Zig commands.
- Do not make the publish job rebuild artifacts; publish exactly what the build jobs produced and attested.
- Do not use destructive tag movement or overwrite an existing public release. Fix forward with a new version.
- Do not mark a release public before all required assets and checks pass.
- Do not add six-hour external validation without deciding what external drift it detects and how failures are triaged.

## 5. Risks and open decisions

- `actions/attest` is useful only if maintainers and users actually verify attestations; decide whether release notes should prescribe a stricter repository/workflow predicate.
- The Linux byte comparison is valuable but does not establish bit-for-bit reproducibility for macOS or Windows artifacts.
- `ubuntu-latest`, `macos-latest`, and external registries are moving dependencies. Decide whether scheduled validation should detect runner/toolchain drift or whether release builds should use more fixed images.
- The manual resume input is safe because the workflow checks out and verifies the existing tag. Any future input that changes source, version, or assets must preserve that invariant.
- `--clobber` is safe only while an already-published release is refused and the tag is immutable by repository policy.
- The release environment must be configured outside this repository; review requirements and tag restrictions are not visible in YAML.
- If the project later grows multiple package registries, decide whether all packages must be lockstep or whether each publication needs an independent release state.

## Bottom line

The right direction is to **deepen the current `monorelease` workflow**, not imitate TigerBeetle's workflow file. TigerBeetle validates the final public ecosystem exceptionally broadly; `monorelease` already has a cleaner immutable-source, build-once, attested, exact-inventory publication boundary. Add native installed-artifact smoke tests, test the resume paths, configure the protected environment, and expand scheduled external validation only in proportion to the project's release risk.
