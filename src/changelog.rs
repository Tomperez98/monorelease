//! A strict, provider-neutral changelog model.
//!
//! This module deliberately knows nothing about Git, GitHub, package managers,
//! or release publication. It parses and renders a structured changelog file;
//! callers may supply bullets gathered by any provider-specific adapter.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

/// A strict `major.minor.patch` version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
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

    fn next_patch(self) -> Option<Self> {
        Some(Self {
            major: self.major,
            minor: self.minor,
            patch: self.patch.checked_add(1)?,
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

/// A changelog entry heading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Heading {
    Version(Version),
    Unreleased,
}

impl Heading {
    fn parse(text: &str) -> Option<Self> {
        if text == "(unreleased)" {
            return Some(Self::Unreleased);
        }
        Version::parse(text).map(Self::Version)
    }

    pub fn heading(&self) -> String {
        match self {
            Self::Version(version) => format!("## {version}"),
            Self::Unreleased => "## (unreleased)".to_owned(),
        }
    }
}

impl fmt::Display for Heading {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.heading())
    }
}

/// One changelog entry.
#[derive(Debug, PartialEq, Eq)]
pub struct Entry {
    pub heading: Heading,
    pub body: String,
}

/// A requested version, or an unreleased entry.
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

    pub fn heading(self) -> Heading {
        match self {
            Self::Version(version) => Heading::Version(version),
            Self::Unreleased => Heading::Unreleased,
        }
    }
}

/// The mutation performed by [`Changelog::scaffold`].
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Inserted,
    Renamed { from: Heading, to: Heading },
}

/// A parsed changelog, with entries ordered newest first.
#[derive(Debug, PartialEq, Eq)]
pub struct Changelog {
    preamble: String,
    entries: Vec<Entry>,
}

impl Changelog {
    pub fn parse(text: &str) -> Result<Self, String> {
        if !text.starts_with("# Changelog") {
            return Err("expected the file to start with `# Changelog`".to_owned());
        }

        let mut preamble = String::new();
        let mut entries = Vec::new();
        for line in text.split_inclusive('\n') {
            match line.strip_prefix("## ") {
                Some(rest) => {
                    let heading_text = rest.trim_end_matches(['\n', '\r']);
                    let heading = Heading::parse(heading_text).ok_or_else(|| {
                        format!(
                            "unrecognized entry heading `## {heading_text}`, expected `## <major>.<minor>.<patch>` or `## (unreleased)`"
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
        let mut previous_version = None;
        let mut unreleased_seen = false;

        for (index, entry) in self.entries.iter().enumerate() {
            match entry.heading {
                Heading::Version(version) => {
                    match entry.body.lines().find(|line| !line.trim().is_empty()) {
                        None => {
                            return Err(format!(
                                "entry `## {version}` is empty; it must start with `Released: <yyyy-mm-dd>`"
                            ));
                        }
                        Some(line) if !valid_release_line(line) => {
                            return Err(format!(
                                "entry `## {version}` must start with `Released: <yyyy-mm-dd>` with a valid date, found `{line}`"
                            ));
                        }
                        Some(_) => {}
                    }

                    if let Some(previous) = previous_version
                        && version >= previous
                    {
                        return Err(format!(
                            "entries must be newest first: `## {version}` appears after `## {previous}`"
                        ));
                    }
                    previous_version = Some(version);
                }
                Heading::Unreleased => {
                    if unreleased_seen {
                        return Err(
                            "the changelog may contain only one `## (unreleased)` entry".to_owned()
                        );
                    }
                    if index != 0 {
                        return Err("`## (unreleased)` must be the newest entry".to_owned());
                    }
                    unreleased_seen = true;
                }
            }
        }
        Ok(())
    }

    pub fn top(&self) -> &Entry {
        self.entries
            .first()
            .expect("a parsed changelog always has an entry")
    }

    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Return the next patch version after the newest versioned entry.
    ///
    /// A top `(unreleased)` entry must be promoted explicitly because the
    /// version it represents is a product decision, not a mechanical patch
    /// increment.
    pub fn next_version(&self) -> Result<Version, String> {
        match self.top().heading {
            Heading::Version(version) => version
                .next_patch()
                .ok_or_else(|| format!("cannot increment release version `{version}`")),
            Heading::Unreleased => Err(
                "the newest entry is `(unreleased)`; provide a version to promote it".to_owned(),
            ),
        }
    }

    pub fn entry(&self, heading: Heading) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.heading == heading)
    }

    pub fn replaces_unreleased(&self, request: Request) -> bool {
        request != Request::Unreleased
            && self
                .entries
                .first()
                .is_some_and(|entry| entry.heading == Heading::Unreleased)
    }

    /// Add a new entry, or rename the top `(unreleased)` entry.
    pub fn scaffold(
        &mut self,
        request: Request,
        date: &str,
        bullets: &[String],
    ) -> Result<Action, String> {
        if !is_valid_date(date) {
            return Err(format!(
                "invalid release date `{date}`, expected `<yyyy-mm-dd>`"
            ));
        }

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
                body: new_entry_body(date, bullets),
            },
        );
        Ok(Action::Inserted)
    }

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

fn valid_release_line(line: &str) -> bool {
    let Some(date) = line.strip_prefix("Released: ") else {
        return false;
    };
    is_valid_date(date)
}

/// Return whether `date` is a real ISO calendar date.
pub fn is_valid_date(date: &str) -> bool {
    let bytes = date.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
    {
        return false;
    }

    let year = date[0..4].parse::<u32>().ok();
    let month = date[5..7].parse::<u32>().ok();
    let day = date[8..10].parse::<u32>().ok();
    let (Some(year), Some(month), Some(day)) = (year, month, day) else {
        return false;
    };
    if !(1..=12).contains(&month) || day == 0 {
        return false;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => unreachable!("month was validated above"),
    };
    day <= days_in_month
}

fn new_entry_body(date: &str, bullets: &[String]) -> String {
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

/// Today's UTC date in `yyyy-mm-dd` form.
pub fn today() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    date_from_unix_seconds(seconds)
}

/// The UTC date `seconds` after the Unix epoch, in `yyyy-mm-dd` form.
///
/// Pure: the wall clock is read once, in `today`, so the calendar conversion
/// can be asserted without a clock.
fn date_from_unix_seconds(seconds: u64) -> String {
    let (year, month, day) = civil_from_days((seconds / 86_400) as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

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

    const SAMPLE: &str = "# Changelog\n\n## 1.2.0\nReleased: 2026-09-11\n\n### Features\n\n- New thing.\n\n## 1.1.0\nReleased: 2026-09-10\n\n- Old thing.\n";

    #[test]
    fn parse_then_render_is_lossless() {
        assert_eq!(Changelog::parse(SAMPLE).unwrap().render(), SAMPLE);
    }

    #[test]
    fn rejects_unrecognized_heading() {
        let error = Changelog::parse("# Changelog\n\n## beta\n").unwrap_err();
        assert!(error.contains("unrecognized entry heading"));
    }

    #[test]
    fn rejects_unreleased_below_a_version() {
        let error =
            Changelog::parse("# Changelog\n\n## 1.0.0\nReleased: 2026-01-01\n\n## (unreleased)\n")
                .unwrap_err();
        assert!(error.contains("must be the newest entry"));
    }

    #[test]
    fn promotes_unreleased_without_provider_data() {
        let mut changelog = Changelog::parse(
            "# Changelog\n\n## (unreleased)\n\n- Pending.\n\n## 1.0.0\nReleased: 2026-01-01\n",
        )
        .unwrap();
        let action = changelog
            .scaffold(Request::parse("1.1.0").unwrap(), "2026-09-11", &[])
            .unwrap();
        assert!(matches!(action, Action::Renamed { .. }));
        assert!(changelog.render().contains("## 1.1.0\n"));
    }

    #[test]
    fn infers_the_next_patch_version_from_the_top_entry() {
        let changelog = Changelog::parse(
            "# Changelog\n\n## 1.2.3\nReleased: 2026-01-01\n\n## 1.2.2\nReleased: 2025-12-01\n",
        )
        .unwrap();

        assert_eq!(changelog.next_version().unwrap().to_string(), "1.2.4");
    }

    #[test]
    fn does_not_infer_a_version_from_unreleased() {
        let changelog = Changelog::parse("# Changelog\n\n## (unreleased)\n").unwrap();

        assert!(changelog.next_version().is_err());
    }

    #[test]
    fn rejects_patch_version_overflow() {
        let changelog =
            Changelog::parse("# Changelog\n\n## 1.2.18446744073709551615\nReleased: 2026-01-01\n")
                .unwrap();

        assert!(changelog.next_version().is_err());
    }

    #[test]
    fn versions_are_strict() {
        assert!(Version::parse("1.2.3").is_some());
        assert!(Version::parse("01.2.3").is_none());
        assert!(Version::parse("1.2").is_none());
        assert!(Version::parse("1.2.x").is_none());
    }

    #[test]
    fn release_dates_are_real_iso_dates() {
        assert!(is_valid_date("2026-09-11"));
        assert!(is_valid_date("2024-02-29"));
        assert!(!is_valid_date("2023-02-29"));
        assert!(!is_valid_date("2026-04-31"));
        assert!(!is_valid_date("2026-9-11"));
    }

    #[test]
    fn rejects_duplicate_unreleased_entries() {
        let error = Changelog::parse(
            "# Changelog\n\n## (unreleased)\n\nPending.\n\n## (unreleased)\n\nMore pending.\n",
        )
        .unwrap_err();
        assert!(error.contains("only one"));
    }

    #[test]
    fn epoch_and_known_dates_convert_exactly() {
        // 0, 86_400, 951_782_400 (2000-02-29), and 1_789_084_800 (2026-09-11)
        // are checked against `date -u -r <seconds> +%Y-%m-%d`.
        assert_eq!(date_from_unix_seconds(0), "1970-01-01");
        assert_eq!(date_from_unix_seconds(86_400), "1970-01-02");
        assert_eq!(date_from_unix_seconds(951_782_400), "2000-02-29");
        assert_eq!(date_from_unix_seconds(1_789_084_800), "2026-09-11");
    }

    #[test]
    fn leap_day_rules_are_gregorian() {
        assert_eq!(date_from_unix_seconds(1_709_164_800), "2024-02-29");
        // 1900 is not a leap year under the Gregorian rule; 2000 is. Verify the
        // century rule through the pure days conversion.
        assert_eq!(civil_from_days(-25_508), (1900, 3, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }
}
