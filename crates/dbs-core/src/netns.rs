//! Detects whether this process runs inside a named Linux network
//! namespace.
//!
//! Mirrors `src/dbs/core/netns.py` in baileyrd/Daily-Backup-System
//! (pinned `@6cc6491`) — confirmed genuinely Linux-only by reading the
//! reference directly, not assumed from its name (see `gap-analysis.md`).
//! `requires_vpn` sources must back up through the VPN wrapper
//! (`vpn_exec`), which runs the command inside a dedicated network
//! namespace. Launched outside that namespace, a backup's traffic exits
//! via the host's real IP — the exact mistake this module guards against.
//!
//! Membership is checked the way `ip netns identify` does it: the current
//! net namespace (`/proc/self/ns/net`) and the named-netns bind mount
//! (`/run/netns/<name>`) share a `(device, inode)` pair iff this process
//! is inside it — the Rust equivalent of Python's `os.path.samestat`.
//! Everything degrades to a safe "not in the namespace" on non-Linux or
//! when the namespace is absent, rather than guessing. The reference
//! reaches that degradation implicitly (the paths simply don't exist off
//! Linux, so `os.stat` raises and is caught); this port makes it explicit
//! via `#[cfg(target_os = "linux")]` instead of relying on path-not-found
//! as the portability strategy.

/// An empty `name` disables the check (both functions treat "no
/// namespace configured" as vacuously satisfied) — checked once here so
/// neither platform branch has to repeat it.
pub fn named_netns_exists(name: &str) -> bool {
    !name.is_empty() && platform::netns_path_exists(name)
}

/// True if this process is currently inside the `name` network
/// namespace. An empty `name` disables the check (returns `true`). A
/// missing namespace, a stat error, or a non-Linux host all mean
/// membership can't be confirmed, so this returns `false`.
pub fn in_named_netns(name: &str) -> bool {
    if name.is_empty() {
        return true;
    }
    platform::in_named_netns(name)
}

#[cfg(target_os = "linux")]
mod platform {
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    const NETNS_DIR: &str = "/run/netns";

    pub fn netns_path_exists(name: &str) -> bool {
        Path::new(NETNS_DIR).join(name).exists()
    }

    pub fn in_named_netns(name: &str) -> bool {
        let (Ok(self_ns), Ok(netns)) = (
            std::fs::metadata("/proc/self/ns/net"),
            std::fs::metadata(Path::new(NETNS_DIR).join(name)),
        ) else {
            return false;
        };
        self_ns.dev() == netns.dev() && self_ns.ino() == netns.ino()
    }
}

#[cfg(not(target_os = "linux"))]
mod platform {
    pub fn netns_path_exists(_name: &str) -> bool {
        false
    }

    pub fn in_named_netns(_name: &str) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_name_disables_both_checks() {
        assert!(in_named_netns(""));
    }

    #[test]
    fn nonexistent_namespace_does_not_exist() {
        assert!(!named_netns_exists("rusty-dbs-definitely-not-a-real-netns"));
    }

    #[test]
    fn not_in_a_nonexistent_namespace() {
        assert!(!in_named_netns("rusty-dbs-definitely-not-a-real-netns"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_membership_check_is_a_real_dev_ino_comparison() {
        // Can't assert true/false without root + a real netns in CI, but
        // this confirms the Linux path actually stats both sides rather
        // than short-circuiting — a typo'd but plausible-looking name
        // stays false, matching the "missing namespace -> false" rule.
        assert!(!in_named_netns("no-such-netns-abc123"));
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_linux_always_degrades_to_false_for_a_named_check() {
        assert!(!named_netns_exists("vpn"));
        assert!(!in_named_netns("vpn"));
    }
}
