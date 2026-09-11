//! Reading, validating, and extending `CHANGELOG.md`.
//!
//! The changelog is the source of truth for the version of a release: the
//! newest entry names it, and its body becomes the release notes. That makes a
//! malformed changelog a release blocker, so parsing is strict — an entry that
//! cannot be understood is reported, never silently skipped.
//!
//! ```
//! ## 0.1.1
//! Released: 2026-09-10
//!
//! ### Fixes
//!
//! - Something that changed for users.
//! ```

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

/// A `major.minor.patch` version, parsed strictly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
    /// Parse `1.2.3`. Leading zeroes are rejected so the heading, the git tag,
    /// and `Cargo.toml` cannot disagree by spelling.
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('.');
        let major = parse_component(parts.next()?)?;
        let minor = parse_component(parts.next()?)?;
        let patch = parse_component(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
        })
    }
}

fn parse_component(text: &str) -> Option<u64> {
    if text.is_empty() || (text.len() > 1 && text.starts_with('0')) {
        return None;
    }
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// What an entry is called: a released version, or work not yet released.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Heading {
    Version(Version),
    Unreleased,
}

impl Heading {
    /// Parse the text after `## `.
    fn parse(text: &str) -> Option<Self> {
        if text == "(unreleased)" {
            return Some(Self::Unreleased);
        }
        Version::parse(text).map(Self::Version)
    }

    /// The whole heading line.
    pub fn heading(&self) -> String {
        match self {
            Self::Version(version) => format!("## {version}"),
            Self::Unreleased => "## (unreleased)".to_owned(),
        }
    }
}

impl fmt::Display for Heading {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Version(version) => write!(formatter, "## {version}"),
            Self::Unreleased => formatter.write_str("## (unreleased)"),
        }
    }
}

/// One `## ` entry: its heading and the body below it.
#[derive(Debug)]
pub struct Entry {
    pub heading: Heading,
    /// Everything after the heading line, including the `Released:` line and the
    /// blank line that separates this entry from the next.
    pub body: String,
}

/// What is being prepared: a release, or an `(unreleased)` entry to fold into
/// the next one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    Version(Version),
    Unreleased,
}

impl Request {
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let version = text.strip_prefix('v').unwrap_or(text);
        if version == "unreleased" {
            return Ok(Self::Unreleased);
        }
        Version::parse(version)
            .map(Self::Version)
            .ok_or_else(|| format!("`{text}` is not `<major>.<minor>.<patch>` or `unreleased`"))
    }

    pub fn heading(&self) -> Heading {
        match self {
            Self::Version(version) => Heading::Version(*version),
            Self::Unreleased => Heading::Unreleased,
        }
    }
}

/// What [`Changelog::scaffold`] did.
#[derive(Debug)]
pub enum Action {
    Inserted,
    Renamed { from: Heading, to: Heading },
}

/// `CHANGELOG.md`, split into its preamble and its entries, newest first.
#[derive(Debug)]
pub struct Changelog {
    preamble: String,
    entries: Vec<Entry>,
}

impl Changelog {
    /// Parse and validate a changelog.
    ///
    /// The error is the sentence shown to the release engineer, so it names the
    /// offending entry and what was expected instead.
    pub fn parse(text: &str) -> Result<Self, String> {
        if !text.starts_with("# Changelog") {
            return Err("expected the file to start with `# Changelog`".to_owned());
        }

        let mut preamble = String::new();
        let mut entries: Vec<Entry> = Vec::new();
        for line in text.split_inclusive('\n') {
            match line.strip_prefix("## ") {
                Some(rest) => {
                    let text = rest.trim_end_matches(['\n', '\r']);
                    let heading = Heading::parse(text).ok_or_else(|| {
                        format!(
                            "unrecognized entry heading `## {text}`, expected `## <major>.<minor>.<patch>` or `## (unreleased)`"
                        )
                    })?;
                    entries.push(Entry {
                        heading,
                        body: String::new(),
                    });
                }
                None => match entries.last_mut() {
                    Some(entry) => entry.body.push_str(line),
                    None => preamble.push_str(line),
                },
            }
        }

        if entries.is_empty() {
            return Err("expected at least one `## ` entry".to_owned());
        }

        let changelog = Self { preamble, entries };
        changelog.validate()?;
        Ok(changelog)
    }

    fn validate(&self) -> Result<(), String> {
        for entry in &self.entries {
            let Heading::Version(version) = entry.heading else {
                continue;
            };
            let first = entry.body.lines().find(|line| !line.trim().is_empty());
            match first {
                None => {
                    return Err(format!(
                        "entry `## {version}` is empty; it must start with `Released: <yyyy-mm-dd>`"
                    ));
                }
                Some(line) if !line.starts_with("Released: ") => {
                    return Err(format!(
                        "entry `## {version}` must start with `Released: <yyyy-mm-dd>`, found `{line}`"
                    ));
                }
                Some(_) => {}
            }
        }

        // Newest first, and `(unreleased)` work only ever sits at the top.
        let mut previous: Option<Version> = None;
        for entry in &self.entries {
            match entry.heading {
                Heading::Version(version) => {
                    if let Some(previous) = previous
                        && version >= previous
                    {
                        return Err(format!(
                            "entries must be newest first: `## {version}` appears after `## {previous}`"
                        ));
                    }
                    previous = Some(version);
                }
                Heading::Unreleased => {
                    if previous.is_some() {
                        return Err("`## (unreleased)` must be the newest entry".to_owned());
                    }
                }
            }
        }

        Ok(())
    }

    pub fn top(&self) -> &Entry {
        self.entries
            .first()
            .expect("a parsed changelog has at least one entry")
    }

    /// True when scaffolding this request only renames the `(unreleased)` entry,
    /// so there is nothing to collect from the merge history.
    pub fn replaces_unreleased(&self, request: &Request) -> bool {
        *request != Request::Unreleased
            && self
                .entries
                .first()
                .is_some_and(|entry| entry.heading == Heading::Unreleased)
    }

    /// Add the newest entry, or rename `(unreleased)` when a real version
    /// replaces it.
    pub fn scaffold(
        &mut self,
        request: &Request,
        date: &str,
        bullets: &[String],
    ) -> Result<Action, String> {
        let heading = request.heading();
        if self.entries.iter().any(|entry| entry.heading == heading) {
            return Err(format!("`{}` already exists", heading.heading()));
        }

        if self.replaces_unreleased(request) {
            let from = self.entries[0].heading;
            self.entries[0].heading = heading;
            return Ok(Action::Renamed { from, to: heading });
        }

        self.entries.insert(
            0,
            Entry {
                heading,
                body: body(date, bullets),
            },
        );
        Ok(Action::Inserted)
    }

    /// The file, with whatever the last mutation produced.
    pub fn render(&self) -> String {
        let mut text = self.preamble.clone();
        for entry in &self.entries {
            text.push_str(&entry.heading.heading());
            text.push('\n');
            text.push_str(&entry.body);
        }
        text
    }
}

/// A new entry: the release date, then whatever pull requests were merged, then
/// the section skeleton a release engineer fills in.
fn body(date: &str, bullets: &[String]) -> String {
    let mut body = format!("Released: {date}\n");
    if !bullets.is_empty() {
        body.push('\n');
        for bullet in bullets {
            body.push_str(bullet);
            body.push('\n');
        }
    }
    body.push_str("\n### Features\n\n-\n\n### Fixes\n\n-\n\n### Internals\n\n-\n\n");
    body
}

/// Today, as `yyyy-mm-dd`, in UTC.
pub fn today() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (year, month, day) = civil_from_days((seconds / 86_400) as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Days since 1970-01-01 to a calendar date (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
# Changelog

Notable changes per release, newest first.

## 0.1.1
Released: 2026-09-10

### Features

- Something new.

## 0.1.0
Released: 2026-09-10

First release.
";

    fn parsed() -> Changelog {
        Changelog::parse(SAMPLE).expect("sample changelog parses")
    }

    #[test]
    fn parse_then_render_is_the_original_file() {
        assert_eq!(parsed().render(), SAMPLE);
    }

    #[test]
    fn top_entry_keeps_its_body() {
        let changelog = parsed();
        assert_eq!(
            changelog.top().heading,
            Heading::Version(Version::parse("0.1.1").unwrap())
        );
        assert!(changelog.top().body.starts_with("Released: 2026-09-10\n"));
        assert!(changelog.top().body.contains("### Features"));
    }

    #[test]
    fn rejects_an_unknown_heading() {
        let error =
            Changelog::parse("# Changelog\n\n## Release candidate\n").expect_err("rejected");
        assert!(error.contains("unrecognized entry heading"), "{error}");
    }

    #[test]
    fn rejects_versions_that_are_not_newest_first() {
        let text =
            "# Changelog\n\n## 0.1.0\nReleased: 2026-01-01\n\n## 0.1.1\nReleased: 2026-01-02\n";
        let error = Changelog::parse(text).expect_err("rejected");
        assert!(error.contains("newest first"), "{error}");
    }

    #[test]
    fn rejects_a_versioned_entry_without_a_release_date() {
        let text = "# Changelog\n\n## 0.1.1\n\n### Features\n\n- Nothing.\n";
        let error = Changelog::parse(text).expect_err("rejected");
        assert!(error.contains("Released: <yyyy-mm-dd>"), "{error}");
    }

    #[test]
    fn rejects_unreleased_below_a_version() {
        let text = "# Changelog\n\n## 0.1.1\nReleased: 2026-01-01\n\n## (unreleased)\n\n- Later.\n";
        let error = Changelog::parse(text).expect_err("rejected");
        assert!(error.contains("must be the newest entry"), "{error}");
    }

    #[test]
    fn version_parsing_is_strict() {
        assert_eq!(Version::parse("1.2.3").unwrap().to_string(), "1.2.3");
        assert!(Version::parse("1.2").is_none());
        assert!(Version::parse("1.2.3.4").is_none());
        assert!(Version::parse("01.2.3").is_none());
        assert!(Version::parse("1.2.x").is_none());
        assert!(Version::parse("").is_none());
    }

    #[test]
    fn request_parses_a_tag_or_unreleased() {
        assert_eq!(
            Request::parse("v0.1.2").unwrap().heading().heading(),
            "## 0.1.2"
        );
        assert_eq!(
            Request::parse("unreleased").unwrap().heading().heading(),
            "## (unreleased)"
        );
        assert!(Request::parse("0.1").is_err());
    }

    #[test]
    fn scaffold_inserts_a_newest_entry() {
        let mut changelog = parsed();
        let bullets = vec!["- [#7](https://github.com/o/r/pull/7)\n  Fix the thing".to_owned()];
        let action = changelog
            .scaffold(&Request::parse("0.1.2").unwrap(), "2026-09-11", &bullets)
            .unwrap();

        assert!(matches!(action, Action::Inserted));
        let rendered = changelog.render();
        assert!(
            rendered.contains("## 0.1.2\nReleased: 2026-09-11\n\n- [#7]"),
            "{rendered}"
        );
        assert!(
            rendered.contains("### Internals\n\n-\n\n## 0.1.1\n"),
            "{rendered}"
        );
        // The result must still be a changelog, not just a string that looks right.
        assert_eq!(Changelog::parse(&rendered).unwrap().render(), rendered);
    }

    #[test]
    fn scaffold_refuses_a_version_that_already_exists() {
        let mut changelog = parsed();
        let error = changelog
            .scaffold(&Request::parse("0.1.1").unwrap(), "2026-09-11", &[])
            .unwrap_err();
        assert!(error.contains("already exists"), "{error}");
    }

    #[test]
    fn scaffold_renames_an_unreleased_entry_instead_of_adding_one() {
        let text = "# Changelog\n\n## (unreleased)\nReleased: 2026-09-01\n\n- Pending work.\n\n## 0.1.1\nReleased: 2026-09-10\n\n- Shipped.\n";
        let mut changelog = Changelog::parse(text).unwrap();
        let request = Request::parse("0.1.2").unwrap();

        assert!(changelog.replaces_unreleased(&request));
        let action = changelog.scaffold(&request, "2026-09-11", &[]).unwrap();

        let Action::Renamed { from, to } = action else {
            panic!("expected a rename, got {action:?}");
        };
        assert_eq!(from.heading(), "## (unreleased)");
        assert_eq!(to.heading(), "## 0.1.2");
        let rendered = changelog.render();
        assert!(
            rendered.contains("## 0.1.2\nReleased: 2026-09-01\n\n- Pending work.\n"),
            "{rendered}"
        );
        assert_eq!(Changelog::parse(&rendered).unwrap().render(), rendered);
    }

    #[test]
    fn scaffolding_unreleased_does_not_rename_anything() {
        let mut changelog = parsed();
        let request = Request::parse("unreleased").unwrap();
        assert!(!changelog.replaces_unreleased(&request));
        let action = changelog.scaffold(&request, "2026-09-11", &[]).unwrap();

        assert!(matches!(action, Action::Inserted));
        assert_eq!(changelog.top().heading, Heading::Unreleased);
        assert_eq!(changelog.render().matches("## (unreleased)").count(), 1);
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(20_706), (2026, 9, 10));
    }
}
