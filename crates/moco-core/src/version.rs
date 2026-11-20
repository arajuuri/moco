//! Which build this is (issue archive#94), recorded by `build.rs` from `git describe`. "Versions and releases" in
//! CONTRIBUTING.md says what the numbers mean and when to make a release.

/// What moco shows: `0.1.0` exactly at a release, `0.1.0-12-g1a2b3c4d5e` (12 commits after v0.1.0, at that commit)
/// otherwise, with a trailing `*` when built with uncommitted changes, or `0.1.0-unknown` without git information.
pub const VERSION: &str = env!("MOCO_VERSION");
/// The release number alone (`[workspace.package] version`), for places that want plain semver, such as MCP `clientInfo`.
pub const RELEASE: &str = env!("CARGO_PKG_VERSION");
/// The abbreviated commit, or `unknown`.
pub const COMMIT: &str = env!("MOCO_COMMIT");
/// `yes`, `no` or `unknown`: whether tracked files had uncommitted changes when this was built.
pub const UNCOMMITTED: &str = env!("MOCO_UNCOMMITTED");

/// The lines `moco --version` prints after `moco <VERSION>`: enough to identify a binary copied to another machine.
pub fn details() -> Vec<String> {
    let mut out = vec![format!("commit {COMMIT}, uncommitted changes: {UNCOMMITTED}")];
    let min = env!("MOCO_MIN_MACOS");
    let target = env!("MOCO_TARGET");
    out.push(if min.is_empty() { format!("built for {target}") } else { format!("built for {target}, macOS {min} or newer") });
    out
}

#[cfg(test)]
#[path = "describe.rs"]
mod describe;

#[cfg(test)]
mod tests {
    use super::describe::{describe_version, Described};

    fn v(describe: &str) -> Described {
        describe_version(describe, "0.1.0")
    }

    #[test]
    fn describe_output_becomes_the_version() {
        // Exactly at a release tag: the plain number.
        assert_eq!(v("v0.1.0-0-g1a2b3c4d5e"), Described { version: "0.1.0".into(), commit: "1a2b3c4d5e".into(), dirty: Some(false), exact_tag: Some("0.1.0".into()) });
        // After it: git describe style.
        assert_eq!(v("v0.1.0-12-g1a2b3c4d5e\n").version, "0.1.0-12-g1a2b3c4d5e");
        assert_eq!(v("v0.1.0-12-g1a2b3c4d5e").exact_tag, None);
        // Uncommitted changes: a quiet trailing `*`, also on a release.
        let d = v("v0.1.0-12-g1a2b3c4d5e-dirty");
        assert_eq!((d.version.as_str(), d.dirty), ("0.1.0-12-g1a2b3c4d5e*", Some(true)));
        assert_eq!(v("v0.2.0-0-gabcdef1234-dirty").version, "0.2.0*");
        // A tag with a pre-release part keeps it.
        assert_eq!(v("v0.2.0-rc1-3-gabcdef1234").version, "0.2.0-rc1-3-gabcdef1234");
        // No release tag yet: the Cargo version and the commit.
        assert_eq!(v("1a2b3c4d5e").version, "0.1.0-g1a2b3c4d5e");
        assert_eq!(v("1a2b3c4d5e-dirty").version, "0.1.0-g1a2b3c4d5e*");
    }

    #[test]
    fn nothing_usable_is_reported_as_unknown_rather_than_guessed() {
        for bad in ["", "   ", "fatal: not a git repository", "v0.1.0-x-g1a2b3c4d5e", "v0.1.0-3-gnothex", "release-3-g1a2b3c4d5e", "-dirty"] {
            let d = v(bad);
            assert_eq!((d.version.as_str(), d.commit.as_str(), d.dirty), ("0.1.0-unknown", "unknown", None), "{bad:?}");
        }
    }

    #[test]
    fn this_build_has_a_version_that_starts_with_its_release_line() {
        // Whatever the checkout: `x.y.z` first, then nothing, `-...` or `*`.
        let rest = super::VERSION.split_once(|c: char| c == '-' || c == '*').map_or("", |(_, r)| r);
        let head = super::VERSION.strip_suffix(rest).unwrap_or(super::VERSION).trim_end_matches(['-', '*']);
        assert_eq!(head.split('.').count(), 3, "{}", super::VERSION);
        assert!(head.split('.').all(|n| n.parse::<u32>().is_ok()), "{}", super::VERSION);
        assert!(!super::details().is_empty());
    }
}
