# Release conventions

Mono's release commands use a provider-neutral release contract. Publishing remains an ordinary project task or CI step; Mono does not know registries or package managers.

## Changelog contract

- `CHANGELOG.md` is Markdown.
- The newest heading is `## (unreleased)` or `## X.Y.Z`.
- Version entries contain `Released: YYYY-MM-DD`.
- The newest versioned entry is the release being prepared.
- Release tags are `vX.Y.Z`, must match the newest entry, and must resolve to the checked-out source commit.
- The matching changelog entry becomes `RELEASE_NOTES.md`.
- Release directories contain an explicit, verified artifact inventory and SHA-256 metadata.

Use the changelog workflow to prepare, check, and extract a release. Before tagging,
run `cargo run -p xtask -- release-check` to verify that committed placeholders,
backup state, and the canonical target table are clean:

```bash
cargo run -p xtask -- release-check

# Infer the next patch release and create an editable entry.
mono changelog prepare

# Or explicitly skip this release cycle.
mono changelog prepare unreleased

# Optionally seed the entry from a read-only Git ref range.
mono changelog prepare --from origin/release --to HEAD \
  --pull-request-url 'https://github.com/org/repo/pull/{number}'

# Review the result before releasing.
mono changelog check
mono changelog release-notes
```

`prepare` never fetches remotes, switches branches, or changes Git state. It only
reads the requested refs and atomically updates the changelog. Generated entries
contain release metadata and harvested bullets, not empty category placeholders.
A range with no merge commits produces a warning and leaves the entry for manual
editing. Use `--pull-request-url` or `CHANGELOG_PR_URL` to link recognized PR
merge commits. `scaffold`,
`validate`, and `notes` remain compatibility aliases.

The newest changelog entry is the release being prepared. Release-note
extraction rejects an older version, requires substantive content beyond
`Released:`, and requires `RELEASE_TAG` to match the newest entry when CI
provides it.

## Repository releases

The repository's release files use pinned placeholders: the root package in `Cargo.toml`, the `mono` package in `Cargo.lock`, and the displayed Zensical site version are pinned at `0.0.0`. The release coordinator stamps all three from the tag before building and restores them before returning. The committed files never move. `release-prepare` owns source validation, changelog/tag agreement, CI, release gates, notes, and packaging checks; `release-build --target` owns native artifact builds and archive creation; `release-docs` builds the versioned Zensical site and writes `site/release.json`; `release-version-check` verifies every local version source; and `release-validate-published` owns the post-publication contract, provenance, reproducibility, and Pages checks. `release-docs` also renders the installers into the documentation it deploys, with the tag and the released `SHA256SUMS` digests baked in, and both the pre-deploy version check and the post-deploy validation refuse a published installer that names another release. Publication composes the release body from the checked changelog notes plus an install section generated from the tag, so the body never names a different version than the artifacts beside it.

To create and push an annotated release tag after the newest changelog entry
has been reviewed:

```console
cargo run -p xtask -- tag --tag v0.1.3
```

The release tag and newest changelog entry must agree. `xtask tag` refuses to
create or push a tag whose version differs from the newest changelog entry. The
tag is also the only version input for the repository's pinned release manifests;
do not edit `Cargo.toml` to prepare a release. To stamp a checkout manually:

```console
cargo run -p xtask -- release-stamp --version 0.1.5
cargo run -p xtask -- release-stamp --restore
```

`release-stamp` saves `.backup` copies while stamping and refuses incomplete
stamp states. Normal release builds should use the coordinator commands rather
than invoking it directly. The canonical artifacts are the four native archives
listed by `xtask`; their names, checksums, metadata, and version identity are
validated from one target table. The installers are not release assets: they are
rendered from the checked-in scripts into the documentation site, so the exact
artifact inventory, checksums, and provenance statements stay untouched. Scheduled validation runs the current validator
from the default branch, clones the immutable release source, rebuilds the Linux
artifact, verifies every platform artifact and attestation, and checks the
machine-readable marker deployed with the Pages site. Mono does not publish to npm, Cargo, Maven,
PyPI, Docker, or any other registry; those operations remain ordinary tasks or
CI workflow steps.
