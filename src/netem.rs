//! `tc netem` network-condition reproduction (Part 8: "tc netemによる
//! ネットワーク再現"), applied to the loopback interface since the PoC's
//! client and server both connect via `127.0.0.1`.
//!
//! Requires root (or `CAP_NET_ADMIN`) and the kernel's `sch_netem`
//! module. Some container/sandbox kernels ship without loadable network
//! qdisc modules at all (no `/lib/modules`, no `modprobe`); on such hosts
//! `tc qdisc add ... netem ...` fails with "Specified qdisc kind is
//! unknown" regardless of privilege. [`netem_available`] probes for this
//! up front so tests can skip cleanly instead of failing, the same
//! pattern `encoder::ffmpeg_available` uses for a missing `ffmpeg`.

use std::process::{Command, Stdio};

const DEFAULT_IFACE: &str = "lo";

#[derive(Debug)]
pub enum NetemError {
    Spawn(std::io::Error),
    /// `tc`'s exit status was non-zero; `stderr` is its error output.
    CommandFailed {
        stderr: String,
    },
}

fn run_tc(args: &[&str]) -> Result<(), NetemError> {
    let output = Command::new("tc")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(NetemError::Spawn)?;
    if !output.status.success() {
        return Err(NetemError::CommandFailed {
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(())
}

/// One netem network-condition profile (Part 8: LAN/WAN/loss profiles).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NetemProfile {
    pub delay_ms: u32,
    /// `None` for no jitter (a fixed delay).
    pub jitter_ms: Option<u32>,
    /// 0.0-100.0.
    pub loss_percent: f32,
}

impl NetemProfile {
    pub const LAN: Self = Self {
        delay_ms: 0,
        jitter_ms: None,
        loss_percent: 0.0,
    };

    /// The brief's example WAN profile: 80ms RTT contributed one-way
    /// (spec brief section 4: "既定プロファイルは要相談、例: 80ms RTT").
    pub const WAN_80MS_RTT: Self = Self {
        delay_ms: 80,
        jitter_ms: None,
        loss_percent: 0.0,
    };

    /// DR-029's target scenario: RTT high enough to expose an absolute
    /// threshold's false positive, but zero congestion/loss.
    pub const HIGH_RTT_NO_CONGESTION: Self = Self {
        delay_ms: 300,
        jitter_ms: None,
        loss_percent: 0.0,
    };

    fn netem_args(&self) -> Vec<String> {
        let mut args = vec!["delay".to_string(), format!("{}ms", self.delay_ms)];
        if let Some(jitter) = self.jitter_ms {
            args.push(format!("{jitter}ms"));
        }
        if self.loss_percent > 0.0 {
            args.push("loss".to_string());
            args.push(format!("{}%", self.loss_percent));
        }
        args
    }
}

/// `true` if `tc` is present and this host's kernel actually supports
/// `netem` (applying and then removing a trivial rule both succeed).
/// Tests that need netem should check this first and skip (rather than
/// fail) when it's absent.
pub fn netem_available() -> bool {
    if run_tc(&[
        "qdisc",
        "add",
        "dev",
        DEFAULT_IFACE,
        "root",
        "netem",
        "delay",
        "1ms",
    ])
    .is_err()
    {
        return false;
    }
    run_tc(&["qdisc", "del", "dev", DEFAULT_IFACE, "root"]).is_ok()
}

/// Applies `profile` to the loopback interface. Replaces any existing
/// root qdisc on it (`tc qdisc replace`, not `add`), so calling this
/// again with a different profile doesn't require clearing first.
pub fn apply_profile(profile: NetemProfile) -> Result<(), NetemError> {
    let mut args = vec![
        "qdisc".to_string(),
        "replace".to_string(),
        "dev".to_string(),
        DEFAULT_IFACE.to_string(),
        "root".to_string(),
        "netem".to_string(),
    ];
    args.extend(profile.netem_args());
    let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
    run_tc(&args_ref)
}

/// Removes any netem qdisc from the loopback interface, restoring normal
/// (unconditioned) delivery.
pub fn clear() -> Result<(), NetemError> {
    run_tc(&["qdisc", "del", "dev", DEFAULT_IFACE, "root"])
}

/// RAII guard: applies `profile` on construction, clears it (best-effort)
/// on drop, so a test leaves the loopback interface as it found it even
/// if it panics partway through.
pub struct NetemGuard;

impl NetemGuard {
    pub fn apply(profile: NetemProfile) -> Result<Self, NetemError> {
        apply_profile(profile)?;
        Ok(Self)
    }
}

impl Drop for NetemGuard {
    fn drop(&mut self) {
        let _ = clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lan_profile_has_no_delay_or_loss() {
        assert_eq!(NetemProfile::LAN.delay_ms, 0);
        assert_eq!(NetemProfile::LAN.loss_percent, 0.0);
    }

    #[test]
    fn netem_args_include_delay() {
        let profile = NetemProfile {
            delay_ms: 80,
            jitter_ms: None,
            loss_percent: 0.0,
        };
        assert_eq!(profile.netem_args(), vec!["delay", "80ms"]);
    }

    #[test]
    fn netem_args_include_jitter_when_present() {
        let profile = NetemProfile {
            delay_ms: 80,
            jitter_ms: Some(10),
            loss_percent: 0.0,
        };
        assert_eq!(profile.netem_args(), vec!["delay", "80ms", "10ms"]);
    }

    #[test]
    fn netem_args_include_loss_when_nonzero() {
        let profile = NetemProfile {
            delay_ms: 50,
            jitter_ms: None,
            loss_percent: 2.5,
        };
        assert_eq!(profile.netem_args(), vec!["delay", "50ms", "loss", "2.5%"]);
    }

    #[test]
    fn apply_and_clear_round_trip_if_netem_is_available() {
        if !netem_available() {
            eprintln!("skipping: tc netem not available on this host/kernel");
            return;
        }
        apply_profile(NetemProfile::WAN_80MS_RTT).expect("apply succeeds");
        clear().expect("clear succeeds");
    }
}
