# Mono release conventions

Mono intentionally uses one opinionated release pattern for every Mono project.
The pattern is inspired by TigerBeetle: release metadata is generated from the
source checkout, artifacts are inventoried explicitly, publication is resumable,
and a published release is validated again after it is uploaded.

These conventions are Mono's release policy. They are not language detection,
package discovery, or package-manager integration. A project may use any
language and any build or publishing tool; those operations remain ordinary
Mono tasks and CI steps.

## Source of truth

The version is the newest version heading in `CHANGELOG.md`. A release entry
has this shape:

```markdown
## 0.2.0
Released: 2026-09-30

### Features

- A user-visible change.

### Fixes

- A corrected behavior.

### Internals

- An implementation change worth recording.
```

The newest entry may instead be `## (unreleased)`. When a release is cut, the
release tag must be `v` followed by the version heading, and `Cargo.toml`'s
`mono` package version must match the tag for Mono's own repository.

Versions are strict `major.minor.patch` values. A skipped release is represented
by keeping changes under `(unreleased)` and promoting them into the next version
when there is something worth shipping.

Validate and prepare the changelog with:

```console
mono changelog validate
mono changelog scaffold --version 0.2.0
mono changelog notes --version 0.2.0
```

`mono changelog notes` writes `RELEASE_NOTES.md`. The body of the selected
changelog entry becomes the release notes shown to users.

## Artifact production

Build commands are project-defined. A release pipeline should produce all
artifacts into one explicit directory, normally `dist/`:

```toml
[pipelines.release]
tasks = ["release-verify"]

[tasks.release-verify]
command = ["./automation/release-verify"]
```

Do not rely on directory discovery to decide what is published. Write an
expected inventory containing one relative artifact path per line:

```text
mono-v0.2.0-x86_64-unknown-linux-gnu.tar.gz
mono-v0.2.0-aarch64-apple-darwin.tar.gz
mono-v0.2.0-x86_64-apple-darwin.tar.gz
mono-v0.2.0-x86_64-pc-windows-msvc.zip
```

Generate and verify the provider-neutral release metadata:

```console
mono release manifest \
  --directory dist \
  --expected release-expected.txt \
  --tag v0.2.0 \
  --commit "$GITHUB_SHA" \
  --repository "$GITHUB_REPOSITORY"

mono release verify \
  --directory dist \
  --expected release-expected.txt \
  --tag v0.2.0 \
  --commit "$GITHUB_SHA" \
  --repository "$GITHUB_REPOSITORY"
```

The commands write:

- `BUILD-METADATA.json`, containing release identity and artifact sizes/hashes;
- `SHA256SUMS`, containing the exact SHA-256 digest for every artifact.

The metadata and checksum files are not themselves included in the artifact
inventory. Symlinks, empty artifact directories, unexpected files, duplicate
entries, and changed artifacts are rejected.

## Source verification

The release source checkout must be exactly the commit named by the release
workflow and the tag must resolve to that checkout:

```console
mono release source \
  --tag "$RELEASE_TAG" \
  --commit "$(git rev-parse HEAD)"
```

This validates the Git ref and rejects a checkout that does not match the tag
or expected commit. Annotated tag objects may additionally be recorded in
`BUILD-METADATA.json`; lightweight tags intentionally have no tag-object value.

## Publishing and resuming

Publishing is deliberately outside Mono's core. Use the project's CI provider
and registry commands as tasks or workflow steps. Mono only guarantees the
execution, inventory, checksum, and verification primitives.

A release workflow should:

1. check out the tag;
2. verify the source with `mono release source`;
3. validate the changelog and run the normal CI pipeline;
4. run project-specific release gates;
5. build artifacts on the supported target matrix;
6. generate and verify release metadata;
7. verify the exact asset inventory and checksums;
8. create or reuse a draft release;
9. upload assets with replacement enabled so a failed run can resume;
10. publish the release only after every verification passes;
11. validate the published release from the released source checkout.

A published release must never be overwritten. A failed release may be resumed
against the same tag and draft after the underlying problem is fixed. The
release workflow should be serialized with a `release` concurrency group.

## Validation after publication

The validation workflow should run against the released tag, not the default
branch. At minimum it should:

- download the exact release assets;
- verify `BUILD-METADATA.json` and `SHA256SUMS`;
- compare release identity with the checked-out tag and commit;
- run the released binary's version command;
- rebuild at least one release artifact from the tagged source and compare it;
- execute the published binary against the repository's release gates.

Projects with published libraries should additionally install and exercise the
published packages. That logic belongs to the project release pipeline because
Mono does not know the project's registries or package managers.
