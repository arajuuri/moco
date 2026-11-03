//! The machine moco runs on: what the system prompt and the log header say about it.
//!
//! The model needs this to know where its shell commands and tests run. Without it, it says "I have not tested
//! this on macOS 10.14" while it is running on exactly that.
use std::sync::OnceLock;

/// `uname` fields: (system name, kernel release, hardware machine).
fn uname() -> Option<(String, String, String)> {
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut u) } != 0 {
        return None;
    }
    let s = |f: &[libc::c_char]| unsafe { std::ffi::CStr::from_ptr(f.as_ptr()) }.to_string_lossy().into_owned();
    Some((s(&u.sysname), s(&u.release), s(&u.machine)))
}

/// The kernel as `uname` reports it, "Darwin 18.7.0".
pub fn kernel() -> String {
    uname().map(|(sys, rel, _)| format!("{sys} {rel}")).unwrap_or_else(|| "unknown".into())
}

/// The product name and version: "macOS 10.14.6", or the distribution's `PRETTY_NAME` on Linux.
pub fn product() -> Option<String> {
    if cfg!(target_os = "macos") {
        let out = std::process::Command::new("sw_vers").arg("-productVersion").output().ok().filter(|o| o.status.success())?;
        let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
        return (!v.is_empty()).then(|| format!("macOS {v}"));
    }
    if cfg!(target_os = "linux") {
        return pretty_name(&std::fs::read_to_string("/etc/os-release").or_else(|_| std::fs::read_to_string("/usr/lib/os-release")).ok()?);
    }
    None
}

/// `PRETTY_NAME` of an os-release file, without its quotes.
fn pretty_name(os_release: &str) -> Option<String> {
    let v = os_release.lines().find_map(|l| l.strip_prefix("PRETTY_NAME="))?.trim().trim_matches(|c| c == '"' || c == '\'');
    (!v.is_empty()).then(|| v.to_string())
}

/// The CPU architecture of the machine, "x86_64" or "arm64" (what `uname -m` says).
pub fn arch() -> String {
    uname().map(|(_, _, machine)| machine).filter(|m| !m.is_empty()).unwrap_or_else(|| std::env::consts::ARCH.to_string())
}

fn describe_with(product: Option<&str>, kernel: &str, arch: &str, os: &str) -> String {
    match product {
        // The kernel version tells Linux machines apart; on macOS the product version already says everything.
        Some(p) if os == "linux" => format!("{p} (kernel {kernel}), {arch}"),
        Some(p) => format!("{p}, {arch}"),
        None => format!("{os} (kernel {kernel}), {arch}"),
    }
}

/// One line for the system prompt, "macOS 10.14.6, x86_64". Computed once per process, so every request of a
/// session carries the same bytes (the prompt prefix is cached by the provider).
pub fn describe() -> &'static str {
    static DESCRIPTION: OnceLock<String> = OnceLock::new();
    DESCRIPTION.get_or_init(|| describe_with(product().as_deref(), &kernel(), &arch(), std::env::consts::OS))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_release_pretty_name_is_unquoted() {
        assert_eq!(pretty_name("NAME=\"Ubuntu\"\nPRETTY_NAME=\"Ubuntu 22.04.4 LTS\"\nID=ubuntu\n").as_deref(), Some("Ubuntu 22.04.4 LTS"));
        assert_eq!(pretty_name("PRETTY_NAME=Alpine\n").as_deref(), Some("Alpine"));
        assert_eq!(pretty_name("NAME=x\n"), None);
        assert_eq!(pretty_name("PRETTY_NAME=\"\"\n"), None);
    }

    #[test]
    fn description_names_version_and_architecture() {
        assert_eq!(describe_with(Some("macOS 10.14.6"), "Darwin 18.7.0", "x86_64", "macos"), "macOS 10.14.6, x86_64");
        assert_eq!(describe_with(Some("Ubuntu 22.04.4 LTS"), "Linux 5.15.0", "aarch64", "linux"), "Ubuntu 22.04.4 LTS (kernel Linux 5.15.0), aarch64");
        assert_eq!(describe_with(None, "FreeBSD 14.0", "amd64", "freebsd"), "freebsd (kernel FreeBSD 14.0), amd64");
    }

    #[test]
    fn the_running_machine_is_described_and_stable() {
        let d = describe();
        assert!(!d.is_empty() && d.contains(&arch()), "{d}");
        assert!(std::ptr::eq(d, describe()));
        if cfg!(target_os = "macos") {
            assert!(d.starts_with("macOS "), "{d}");
        }
    }
}
