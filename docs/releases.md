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

Use the changelog workflow to prepare, check, and extract a release:

```bash
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
reads the requested refs and atomically updates the changelog. Use
`--pull-request-url` or `CHANGELOG_PR_URL` to link recognized PR merge commits.
`scaffold`,
`validate`, and `notes` remain compatibility aliases.

The newest changelog entry is the release being prepared. Release-note
extraction rejects an older version and requires `RELEASE_TAG` to match the
newest entry when CI provides it.

## Repository releases

The repository's release files use pinned placeholders: the root package in `Cargo.toml`, the `mono` package in `Cargo.lock`, and the displayed Zensical site version are pinned at `0.0.0`. The release workflow stamps all three from the tag with the `xtask` release coordinator before building. The committed files never move. `release-prepare` owns source validation, CI, release gates, notes, and Cargo packaging; `release-docs` builds and restores the versioned Zensical site; and `release-publish` owns draft creation, artifact upload, retry behavior, and final publication.

To create and push an annotated release tag after the newest changelog entry
has been reviewed:

```console
cargo run -p xtask -- tag --tag v0.1.3
```

The release tag and newest changelog entry must agree. The tag is also the only
version input for the repository's pinned release manifests; do not edit
`Cargo.toml` to prepare a release. To stamp a checkout manually:

```console
cargo run -p xtask -- release-stamp --version 0.1.5
cargo run -p xtask -- release-stamp --restore
```

`release-stamp` saves `.backup` copies while stamping. Mono does not publish to npm, Cargo, Maven, PyPI, Docker, or any other registry; those operations remain ordinary tasks or CI workflow steps.
