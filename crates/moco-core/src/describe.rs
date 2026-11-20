// Turns `git describe` output into moco's version (issue archive#94). Plain std only: `build.rs` includes this file with
// `include!`, and the library compiles it as a module so the rules are unit-tested.
//
// The input is the output of
//     git describe --tags --match 'v[0-9]*' --long --dirty --abbrev=10 --always
// for example `v0.1.0-0-g1a2b3c4d5e`, `v0.1.0-12-g1a2b3c4d5e-dirty`, or just `1a2b3c4d5e` before any release tag.

/// What a build reports about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Described {
    /// `0.1.0` exactly at a release tag, `0.1.0-12-g1a2b3c4d5e` after it; `*` is appended when the working tree had
    /// uncommitted changes. `<release>-unknown` when the commit is not known.
    pub version: String,
    /// The abbreviated commit, or `unknown`.
    pub commit: String,
    /// `Some(true)` when tracked files had uncommitted changes; `None` when that is not known.
    pub dirty: Option<bool>,
    /// The release tag's version when the build is exactly at a tag (to compare with the Cargo version).
    pub exact_tag: Option<String>,
}

/// `release` is the Cargo package version, used when there is no release tag yet or nothing is known.
pub fn describe_version(describe: &str, release: &str) -> Described {
    let unknown = || Described { version: format!("{release}-unknown"), commit: "unknown".to_string(), dirty: None, exact_tag: None };
    let d = describe.trim();
    let (d, dirty) = match d.strip_suffix("-dirty") {
        Some(rest) => (rest, true),
        None => (d, false),
    };
    let mark = if dirty { "*" } else { "" };
    let is_hash = |h: &str| h.len() >= 4 && h.chars().all(|c| c.is_ascii_hexdigit());
    // `v<version>-<count>-g<hash>`; the version part may itself contain `-` (`v0.2.0-rc1`).
    let mut parts = d.rsplitn(3, '-');
    if let (Some(g), Some(count), Some(tag)) = (parts.next(), parts.next(), parts.next()) {
        if let (Some(hash), Some(tagged)) = (g.strip_prefix('g'), tag.strip_prefix('v')) {
            if is_hash(hash) && !tagged.is_empty() && !count.is_empty() && count.chars().all(|c| c.is_ascii_digit()) {
                let exact = count.trim_start_matches('0').is_empty();
                let base = if exact { tagged.to_string() } else { format!("{tagged}-{count}-g{hash}") };
                return Described { version: format!("{base}{mark}"), commit: hash.to_string(), dirty: Some(dirty), exact_tag: exact.then(|| tagged.to_string()) };
            }
        }
    }
    // No release tag reachable: `--always` printed just the commit.
    if is_hash(d) {
        return Described { version: format!("{release}-g{d}{mark}"), commit: d.to_string(), dirty: Some(dirty), exact_tag: None };
    }
    unknown()
}
