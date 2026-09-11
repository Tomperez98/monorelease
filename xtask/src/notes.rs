//! `RELEASE_NOTES.md`: what the GitHub release page shows.
//!
//! The notes a reviewer approves in a pull request are exactly the notes users
//! read, because the changelog body is used verbatim.

use std::path::Path;

use crate::changelog::{Changelog, Heading};
use crate::{Error, Tag};

const PREFIX: &str = "release-notes";

/// Everything a user needs before the changelog: what is attached and how to
/// verify it.
const HEADER: &str = "\
Prebuilt binaries for Linux x86_64, macOS arm64, macOS x86_64, and Windows
x86_64 are attached to this release, together with `SHA256SUMS`:

```bash
sha256sum -c SHA256SUMS   # macOS: shasum -a 256 -c SHA256SUMS
```

`BUILD-METADATA.json` records the source commit and workflow run that produced
this release. Verify an archive's GitHub Actions provenance with:

```bash
gh attestation verify monorelease-<tag>-<target>.<archive> \\
  --repo Tomperez98/monorelease
```

Install from source with `cargo install --path .` (Rust 1.85+, edition 2024).
";

pub fn run(tag: &Tag, changelog_path: &Path, notes_path: &Path) -> Result<(), Error> {
    let text = crate::read_to_string(changelog_path)?;
    let changelog =
        Changelog::parse(&text).map_err(|message| crate::invalid(changelog_path, message))?;

    let expected = Heading::Version(tag.version);
    let top = changelog.top();
    if top.heading != expected {
        return Err(Error::Invalid(format!(
            "the top entry of {} is `{}`, expected `{}`\n  add `{}` as the newest entry, or rename the existing one.",
            changelog_path.display(),
            top.heading.heading(),
            expected.heading(),
            expected.heading()
        )));
    }

    let body = top.body.trim();
    if body.is_empty() {
        return Err(crate::invalid(
            changelog_path,
            format!("the `{}` entry is empty", expected.heading()),
        ));
    }

    let notes = format!("# {}\n\n{HEADER}\n## Changelog\n\n{body}\n", tag.name);
    crate::write(notes_path, &notes)?;

    println!("{PREFIX}: wrote {} for {}", notes_path.display(), tag.name);
    Ok(())
}
