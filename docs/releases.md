# Release conventions

Mono's release commands use a provider-neutral release contract. Publishing remains an ordinary project task or CI step; Mono does not know registries or package managers.

## Changelog contract

- `CHANGELOG.md` is Markdown.
- The newest heading is `## (unreleased)` or `## X.Y.Z`.
- Version entries contain `Released: YYYY-MM-DD`.
- Release tags are `vX.Y.Z` and must resolve to the checked-out source commit.
- The matching changelog entry becomes `RELEASE_NOTES.md`.
- Release directories contain an explicit, verified artifact inventory and SHA-256 metadata.

Use the changelog commands to validate and prepare release notes:

```bash
mono changelog validate
mono changelog notes
```

## Repository releases

The repository's release files use pinned placeholders: the root package in `Cargo.toml`, the `mono` package in `Cargo.lock`, and the displayed Zensical site version are pinned at `0.0.0`. The release workflow stamps all three from the tag with the `xtask` release coordinator before building. The committed files never move. `release-prepare` owns source validation, CI, release gates, notes, and Cargo packaging; `release-docs` builds and restores the versioned Zensical site; and `release-publish` owns draft creation, artifact upload, retry behavior, and final publication.

To create and push an annotated release tag:

```console
cargo run -p xtask -- tag --tag v0.1.3
```

The tag is the only version input. To stamp a checkout manually:

```console
cargo run -p xtask -- release-stamp --version 0.1.5
cargo run -p xtask -- release-stamp --restore
```

`release-stamp` saves `.backup` copies while stamping. Mono does not publish to npm, Cargo, Maven, PyPI, Docker, or any other registry; those operations remain ordinary tasks or CI workflow steps.
