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

# Run the complete pinned-state preflight before tagging.
cargo run --locked --quiet -- run release-preflight --no-cache --ui stream

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

The repository's release files use pinned placeholders: the root package in `Cargo.toml`, the `mono` package in `Cargo.lock`, and the displayed Zensical site version are pinned at `0.0.0`. The release coordinator stamps all three from the tag before building and restores them before returning. The committed files never move. Release commands receive their tag and release identity explicitly; GitHub environment variables are workflow adapters, not part of the release-domain interface. `release-prepare` owns source validation, changelog/tag agreement, CI, release gates, notes, and packaging checks; `release-build --tag TAG --target TARGET` owns native artifact builds and archive creation; `release-docs --tag TAG --repository OWNER/REPO` builds the versioned Zensical site and renders the tag-pinned installers into the release directory; `release-contract` verifies the four archives plus both installers; `release-version-check --tag TAG` verifies every local version source; and `release-validate-published --tag TAG --repository OWNER/REPO` owns the post-publication contract, provenance, reproducibility, release-asset installer checks, and Pages checks. The installers carry the released archive digests and are published as first-class GitHub Release assets. Publication composes the release body from the checked changelog notes plus tag-scoped installer URLs, so the body never names a different version than the artifacts beside it.

To create and push an annotated release tag after the newest changelog entry
has been reviewed:

```console
cargo run --locked --quiet -p xtask -- tag --tag v0.1.3
```

The release tag and newest changelog entry must agree. `xtask tag` refuses to
create or push a tag whose version differs from the newest changelog entry. The
tag is also the only version input for the repository's pinned release manifests;
do not edit `Cargo.toml` to prepare a release. To stamp a checkout manually:

```console
cargo run --locked --quiet -p xtask -- release-stamp --version 0.1.5
cargo run --locked --quiet -p xtask -- release-stamp --restore
```

`release-stamp` saves `.backup` copies while stamping and refuses incomplete
stamp states. Normal release builds should use the coordinator commands rather
than invoking it directly. The canonical release assets are the four native archives and the two rendered
installers listed by `xtask`; their names, checksums, metadata, and version identity
are validated from one inventory. The documentation site contains release metadata
and versioned docs, while GitHub Release assets are the canonical installer source.
Scheduled validation runs the current validator from the default branch, clones the
immutable release source, rebuilds the Linux artifact, verifies every platform
artifact and attestation, checks both release installers, executes the published
installer on supported runners, and checks the machine-readable marker deployed
with the Pages site. Mono does not publish to npm, Cargo, Maven,
PyPI, Docker, or any other registry; those operations remain ordinary tasks or
CI workflow steps.
