use crate::{Result, invalid};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MIB: usize = 1024 * 1024;
const GIB: usize = 1024 * MIB;
const RUNTIME_FLOOR: usize = 64 * MIB;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Limits {
    pub target: usize,
    pub high: usize,
    pub max: usize,
}

impl Limits {
    pub fn fixed(max: usize) -> Self {
        Self { target: max / 4 * 3, high: max / 8 * 7, max }
    }

    pub fn root_limit(self) -> usize {
        (self.target / (384 * MIB)).clamp(1, 24)
    }

    fn page_aligned(self, page: usize) -> Result<Self> {
        if page == 0 { return Err(invalid("invalid kernel memory page size").into()); }
        Ok(Self {
            target:self.target / page * page,
            high:self.high / page * page,
            max:self.max / page * page,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Policy {
    pub fixed_max: Option<usize>,
    pub reserve: Option<usize>,
}

fn mib_setting(name: &str) -> Result<Option<usize>> {
    match std::env::var(name) {
        Ok(value) => {
            let mib = value.parse::<usize>().map_err(|_| invalid(format!("{name} must be a positive MiB integer")))?;
            if mib == 0 {
                return Err(invalid(format!("{name} must be positive")).into());
            }
            Ok(Some(mib.checked_mul(MIB).ok_or_else(|| invalid(format!("{name} is too large")))?))
        }
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

impl Policy {
    pub fn from_env() -> Result<Self> {
        let fixed_max = mib_setting("CCTX_MEMORY_MAX_MIB")?;
        if fixed_max.is_some_and(|max| max < 256 * MIB) {
            return Err(invalid("CCTX_MEMORY_MAX_MIB must be at least256MiB").into());
        }
        Ok(Self { fixed_max, reserve: mib_setting("CCTX_MEMORY_RESERVE_MIB")? })
    }

    pub fn calculate(&self, total: usize, available: usize, own: usize, parent_room: usize) -> Limits {
        let reserve = self.reserve.unwrap_or((2 * GIB).max(total / 5));
        let safe = available.saturating_add(own).min(total)
            .saturating_sub(reserve).min(parent_room);
        let maximum = self.fixed_max.map_or(safe, |fixed| fixed.min(safe));
        Limits::fixed(maximum.max(RUNTIME_FLOOR))
    }
}

fn field(text: &str, name: &str) -> Result<usize> {
    text.lines().find_map(|line| line.strip_prefix(name))
        .and_then(|value| value.split_whitespace().next())
        .ok_or_else(|| invalid(format!("memory metadata missing {name}")))?
        .parse::<usize>()?.checked_mul(1024)
        .ok_or_else(|| invalid("memory metadata overflow").into())
}

fn parent_room(group: &Path, own: usize, managed: bool) -> Result<usize> {
    let hierarchy = Path::new("/sys/fs/cgroup");
    let mut room = usize::MAX;
    for parent in group.ancestors().skip(usize::from(managed)).take_while(|path| path.starts_with(hierarchy)) {
        let limit = match std::fs::read_to_string(parent.join("memory.max")) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if limit.trim() != "max" {
            let max = limit.trim().parse::<usize>()?;
            let current = std::fs::read_to_string(parent.join("memory.current"))?
                .trim().parse::<usize>()?;
            room = room.min(max.saturating_sub(max / 5).saturating_sub(current.saturating_sub(own)));
        }
    }
    Ok(room)
}

pub struct Controller {
    pub limits: Limits,
    policy: Option<Policy>,
    group: Option<PathBuf>,
    managed: bool,
    wsl: bool,
    windows_host: Option<(usize, usize, Instant)>,
    last_sample: Instant,
    last_growth: Instant,
    pub last_error: Option<String>,
}

impl Default for Controller {
    fn default() -> Self {
        Self {
            limits: Limits::fixed(4 * GIB), policy: None, group: None, managed: false,
            wsl: false, windows_host: None,
            last_sample: Instant::now(), last_growth: Instant::now(), last_error: None,
        }
    }
}

impl Controller {
    pub fn start(group: PathBuf, managed: bool) -> Result<Self> {
        if managed && group.file_name().is_none_or(|name| name != "code-context.service") {
            return Err(invalid("memory guard requires the dedicated code-context.service cgroup").into());
        }
        let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")?;
        let mut value = Self {
            policy: Some(Policy::from_env()?), group: Some(group), managed,
            wsl: release.to_ascii_lowercase().contains("microsoft"), ..Self::default()
        };
        value.limits = value.sample()?;
        value.apply_kernel_limits(true)?;
        Ok(value)
    }

    fn sample(&self) -> Result<Limits> {
        let text = std::fs::read_to_string("/proc/meminfo")?;
        let total = field(&text, "MemTotal:")?;
        let available = field(&text, "MemAvailable:")?;
        let group = self.group.as_ref().ok_or_else(|| invalid("memory controller has no cgroup"))?;
        let (own, anon) = if self.managed {
            let own = std::fs::read_to_string(group.join("memory.current"))?.trim().parse::<usize>()?;
            let stats = std::fs::read_to_string(group.join("memory.stat"))?;
            let anon = stats.lines().find_map(|line| line.strip_prefix("anon "))
                .ok_or_else(|| invalid("cgroup memory.stat has no anon counter"))?.parse::<usize>()?;
            (own, anon)
        } else {
            let status = std::fs::read_to_string("/proc/self/status")?;
            (field(&status, "VmRSS:")?, field(&status, "RssAnon:")?)
        };
        let policy = self.policy.as_ref().ok_or_else(|| invalid("memory policy is absent"))?;
        let local = policy.calculate(total, available, anon, parent_room(group, own, self.managed)?);
        let host_max = self.host_ceiling(policy, anon);
        let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })?;
        Limits::fixed(local.max.min(host_max)).page_aligned(page)
    }

    fn host_ceiling(&self, policy: &Policy, own: usize) -> usize {
        if !self.wsl { return usize::MAX; }
        if let Some((total, available, at)) = self.windows_host
            && at.elapsed() < Duration::from_secs(120)
        {
            return policy.calculate(total, available, own, usize::MAX).max;
        }
        policy.fixed_max.unwrap_or(4 * GIB)
    }

    pub fn observe_windows_host(&mut self, total: usize, available: usize) -> Result<()> {
        if total == 0 || available > total {
            return Err(invalid("invalid Windows host memory report").into());
        }
        self.windows_host = Some((total, available, Instant::now()));
        Ok(())
    }

    pub fn refresh(&mut self, worker_active: bool) -> Result<()> {
        if self.policy.is_none() || self.last_sample.elapsed() < Duration::from_secs(5) {
            return Ok(());
        }
        self.last_sample = Instant::now();
        let next = self.sample()?;
        self.consider(next);
        self.apply_kernel_limits(!worker_active)?;
        self.last_error = None;
        Ok(())
    }

    fn consider(&mut self, next: Limits) {
        let change = next.max.abs_diff(self.limits.max);
        let threshold = (64 * MIB).max(self.limits.max / 10);
        if next.max < self.limits.max && change >= 16 * MIB
            || next.max > self.limits.max && change >= threshold
                && self.last_growth.elapsed() >= Duration::from_secs(30)
        {
            if next.max > self.limits.max { self.last_growth = Instant::now(); }
            self.limits = next;
        }
    }

    fn apply_kernel_limits(&self, can_shrink: bool) -> Result<()> {
        if !self.managed { return Ok(()); }
        let group = self.group.as_ref().ok_or_else(|| invalid("memory guard has no cgroup"))?;
        let current = std::fs::read_to_string(group.join("memory.current"))?.trim().parse::<usize>()?;
        let old = std::fs::read_to_string(group.join("memory.max"))?;
        let old = if old.trim() == "max" { usize::MAX } else { old.trim().parse()? };
        if self.limits.max < old && !can_shrink {
            return Ok(());
        }
        let mut changes = vec![("memory.high", self.limits.high)];
        if self.limits.max >= old {
            changes.insert(0, ("memory.max", self.limits.max));
        } else if current < self.limits.high {
            changes.push(("memory.max", self.limits.max));
        }
        for (name, expected) in changes {
            let path = group.join(name);
            let value = std::fs::read_to_string(&path)?;
            if value.trim() != expected.to_string() {
                std::fs::write(&path, expected.to_string())?;
                if std::fs::read_to_string(&path)?.trim() != expected.to_string() {
                    return Err(invalid(format!("{name} kernel verification failed")).into());
                }
                eprintln!("CCTX_MEMORY_GUARD {name}={expected}");
            }
        }
        Ok(())
    }

    pub fn status(&self) -> serde_json::Value {
        serde_json::json!({
            "mode":match &self.policy {Some(policy) if policy.fixed_max.is_none()=>"auto",Some(_)=>"capped_auto",None=>"test_default"},
            "limits":self.limits,"kernel_managed":self.managed,"last_error":self.last_error,
            "resident_root_limit":self.limits.root_limit(),
            "wsl":self.wsl,
            "windows_host_report_age_seconds":self.windows_host.map(|(_,_,at)|at.elapsed().as_secs()),
            "windows_host_fresh":self.windows_host.is_some_and(|(_,_,at)|at.elapsed()<Duration::from_secs(120)),
            "wsl_without_host_report_ceiling_bytes":4 * GIB,
            "reserve_bytes":self.policy.as_ref().and_then(|policy|policy.reserve),
            "reserve_default":"max(2GiB,20% of MemTotal)","sample_interval_seconds":5,"growth_interval_seconds":30,
        })
    }

    pub fn is_managed(&self) -> bool { self.managed }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_uses_spare_memory_beyond_four_gib_and_leaves_reserve() {
        let policy = Policy { fixed_max: None, reserve: None };
        let limits = policy.calculate(64 * GIB, 48 * GIB, 2 * GIB, usize::MAX);
        assert_eq!(limits.max, 50 * GIB - 64 * GIB / 5);
        assert!(limits.target > 4 * GIB);
        assert_eq!(limits.root_limit(), 24);
        assert_eq!(Limits::fixed(4 * GIB).root_limit(), 8);
        assert!(limits.target < limits.high && limits.high < limits.max);
    }

    #[test]
    fn parent_room_manual_ceiling_and_low_memory_remain_bounded() {
        let auto = Policy { fixed_max: None, reserve: None };
        assert_eq!(auto.calculate(16 * GIB, 12 * GIB, GIB, GIB).max, GIB);
        assert_eq!(auto.calculate(4 * GIB, 128 * MIB, 0, usize::MAX).max, RUNTIME_FLOOR);
        let fixed = Policy { fixed_max: Some(4 * GIB), reserve: None };
        assert_eq!(fixed.calculate(64 * GIB, 48 * GIB, 0, usize::MAX), Limits::fixed(4 * GIB));
        assert_eq!(fixed.calculate(64 * GIB, 48 * GIB, 0, GIB).max, GIB);
        assert_eq!(auto.calculate(usize::MAX, usize::MAX, usize::MAX, usize::MAX).max,
            usize::MAX - usize::MAX / 5);
    }

    #[test]
    fn invalid_meminfo_is_an_error_not_a_large_success_shaped_budget() {
        assert!(field("MemTotal: invalid kB", "MemTotal:").is_err());
        assert!(field("MemTotal: 1 kB", "MemAvailable:").is_err());
        assert_eq!(field("MemAvailable: 1024 kB", "MemAvailable:").unwrap(), MIB);
    }

    #[test]
    fn shrink_is_prompt_but_growth_has_hysteresis_and_cooldown() {
        let mut controller = Controller::default();
        controller.consider(Limits::fixed(8 * GIB));
        assert_eq!(controller.limits.max, 4 * GIB);
        controller.last_growth = Instant::now() - Duration::from_secs(31);
        controller.consider(Limits::fixed(8 * GIB));
        assert_eq!(controller.limits.max, 8 * GIB);
        controller.consider(Limits::fixed(6 * GIB));
        assert_eq!(controller.limits.max, 6 * GIB);
        controller.consider(Limits::fixed(6 * GIB + MIB));
        assert_eq!(controller.limits.max, 6 * GIB);
        controller.consider(Limits::fixed(6 * GIB - 32 * MIB));
        assert_eq!(controller.limits.max, 6 * GIB - 32 * MIB);
    }

    #[test]
    fn memory_guard_does_not_overwrite_shared_or_busy_cgroups() {
        assert!(Controller::start(PathBuf::from("/tmp/not-a-dedicated-service"), true).is_err());
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("memory.max"), (8 * GIB).to_string()).unwrap();
        std::fs::write(directory.path().join("memory.high"), (7 * GIB).to_string()).unwrap();
        std::fs::write(directory.path().join("memory.current"), (5 * GIB).to_string()).unwrap();
        let controller = Controller {
            limits: Limits::fixed(4 * GIB), group: Some(directory.path().to_path_buf()),
            managed: true, ..Controller::default()
        };
        controller.apply_kernel_limits(false).unwrap();
        assert_eq!(std::fs::read_to_string(directory.path().join("memory.max")).unwrap(), (8 * GIB).to_string());
        controller.apply_kernel_limits(true).unwrap();
        assert_eq!(std::fs::read_to_string(directory.path().join("memory.max")).unwrap(), (8 * GIB).to_string());
        assert_eq!(std::fs::read_to_string(directory.path().join("memory.high")).unwrap(), controller.limits.high.to_string());
        std::fs::write(directory.path().join("memory.current"), GIB.to_string()).unwrap();
        controller.apply_kernel_limits(true).unwrap();
        assert_eq!(std::fs::read_to_string(directory.path().join("memory.max")).unwrap(), (4 * GIB).to_string());
    }

    #[test]
    fn wsl_respects_outer_windows_memory_and_stale_report_fallback() {
        let policy = Policy { fixed_max: None, reserve: None };
        let mut controller = Controller { wsl: true, ..Controller::default() };
        assert_eq!(controller.host_ceiling(&policy, GIB), 4 * GIB);
        controller.observe_windows_host(32 * GIB, 24 * GIB).unwrap();
        assert!(controller.host_ceiling(&policy, GIB) > 4 * GIB);
        controller.observe_windows_host(32 * GIB, GIB).unwrap();
        assert_eq!(controller.host_ceiling(&policy, GIB), RUNTIME_FLOOR);
        controller.windows_host = Some((32 * GIB, 24 * GIB, Instant::now() - Duration::from_secs(121)));
        assert_eq!(controller.host_ceiling(&policy, GIB), 4 * GIB);
        assert!(controller.observe_windows_host(0, 0).is_err());
        assert!(controller.observe_windows_host(GIB, 2 * GIB).is_err());
    }

    #[test]
    fn kernel_memory_limits_are_rounded_down_to_native_page_boundaries() {
        let raw = Limits::fixed(15_264_404_276);
        for page in [4096, 65536] {
            let aligned = raw.page_aligned(page).unwrap();
            assert!(aligned.target < aligned.high && aligned.high < aligned.max);
            for (before, after) in [(raw.target, aligned.target), (raw.high, aligned.high), (raw.max, aligned.max)] {
                assert_eq!(after % page, 0);
                assert!(after <= before && before - after < page);
            }
        }
        assert!(raw.page_aligned(0).is_err());
    }
}
