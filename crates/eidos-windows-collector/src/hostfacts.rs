//! Host shape and lifecycle facts: OS build, physical/virtual, processor
//! and memory size, uptime, cumulative sleep, power source, and the
//! collector's own privilege facts. None of these identify the machine.

use eidos_observe::MachineKind;
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows_sys::Win32::Security::{
    GetTokenInformation, IsWellKnownSid, TokenElevation, TokenUser, WinLocalSystemSid,
    TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::System::Power::GetSystemPowerStatus;
use windows_sys::Win32::System::Registry::{
    RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
};
use windows_sys::Win32::System::SystemInformation::{
    GetSystemInfo, GetTickCount64, GlobalMemoryStatusEx, MEMORYSTATUSEX, SYSTEM_INFO,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetSystemTimes, OpenProcessToken};
use windows_sys::Win32::System::WindowsProgramming::{
    QueryInterruptTime, QueryUnbiasedInterruptTime,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostFacts {
    /// `major.minor.build.ubr` plus the installation type, e.g.
    /// `10.0.26100.1234 server`.
    pub os_build: String,
    pub machine: MachineKind,
    pub logical_processors: u32,
    pub memory_total: u64,
    pub elevated: bool,
    pub local_system: bool,
}

pub fn host_facts() -> HostFacts {
    let (elevated, local_system) = token_facts();
    HostFacts {
        os_build: os_build(),
        machine: machine_kind(),
        logical_processors: logical_processors(),
        memory_total: memory().0,
        elevated,
        local_system,
    }
}

const VERSION_KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";

pub fn os_build() -> String {
    let major = registry_dword(VERSION_KEY, "CurrentMajorVersionNumber").unwrap_or(0);
    let minor = registry_dword(VERSION_KEY, "CurrentMinorVersionNumber").unwrap_or(0);
    let build = registry_string(VERSION_KEY, "CurrentBuildNumber").unwrap_or_else(|| "0".into());
    let ubr = registry_dword(VERSION_KEY, "UBR").unwrap_or(0);
    let kind = registry_string(VERSION_KEY, "InstallationType")
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_else(|| "unknown".into());
    format!("{major}.{minor}.{build}.{ubr} {kind}")
}

/// CPUID leaf 1, ECX bit 31 is the hypervisor-present bit. It is set inside
/// every mainstream hypervisor and clear on bare metal (except when a
/// Hyper-V root partition is enabled, which the study treats as virtual
/// because its I/O path is virtualised too).
pub fn machine_kind() -> MachineKind {
    #[cfg(target_arch = "x86_64")]
    {
        let leaf = std::arch::x86_64::__cpuid(1);
        if leaf.ecx & (1 << 31) != 0 {
            MachineKind::Virtual
        } else {
            MachineKind::Physical
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        MachineKind::Unknown
    }
}

pub fn logical_processors() -> u32 {
    // SAFETY: plain out-parameter struct.
    let mut info: SYSTEM_INFO = unsafe { std::mem::zeroed() };
    unsafe { GetSystemInfo(&mut info) };
    info.dwNumberOfProcessors
}

/// `(total_bytes, used_percent)`.
pub fn memory() -> (u64, u8) {
    // SAFETY: the struct length is set before the call as documented.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        return (0, 0);
    }
    (status.ullTotalPhys, status.dwMemoryLoad.min(100) as u8)
}

pub fn uptime_ms() -> u64 {
    // SAFETY: trivially safe.
    unsafe { GetTickCount64() }
}

/// Interrupt time counts sleep; unbiased interrupt time does not. Their
/// difference is the time the machine has spent asleep since boot.
pub fn slept_ms() -> u64 {
    let mut biased = 0u64;
    let mut unbiased = 0u64;
    // SAFETY: out-parameters only.
    unsafe {
        QueryInterruptTime(&mut biased);
        QueryUnbiasedInterruptTime(&mut unbiased);
    }
    biased.saturating_sub(unbiased) / 10_000
}

pub fn on_battery() -> Option<bool> {
    // SAFETY: out-parameter struct.
    let mut status = unsafe { std::mem::zeroed() };
    if unsafe { GetSystemPowerStatus(&mut status) } == 0 {
        return None;
    }
    let status: windows_sys::Win32::System::Power::SYSTEM_POWER_STATUS = status;
    match status.ACLineStatus {
        0 => Some(true),
        1 => Some(false),
        _ => None,
    }
}

/// System-wide CPU busy percentage between two samples.
pub struct CpuSampler {
    last: Option<(u64, u64, u64)>,
}

impl Default for CpuSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl CpuSampler {
    pub fn new() -> Self {
        let mut sampler = Self { last: None };
        sampler.busy_percent();
        sampler
    }

    pub fn busy_percent(&mut self) -> u8 {
        let Some(now) = system_times() else {
            return 0;
        };
        let previous = self.last.replace(now);
        let Some((idle0, kernel0, user0)) = previous else {
            return 0;
        };
        let idle = now.0.saturating_sub(idle0);
        let total = now
            .1
            .saturating_sub(kernel0)
            .saturating_add(now.2.saturating_sub(user0));
        if total == 0 {
            return 0;
        }
        (100u64.saturating_sub(idle * 100 / total)).min(100) as u8
    }
}

fn system_times() -> Option<(u64, u64, u64)> {
    let mut idle = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut kernel = idle;
    let mut user = idle;
    // SAFETY: out-parameters only.
    if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } == 0 {
        return None;
    }
    Some((filetime(&idle), filetime(&kernel), filetime(&user)))
}

pub fn filetime(value: &FILETIME) -> u64 {
    ((value.dwHighDateTime as u64) << 32) | value.dwLowDateTime as u64
}

/// `(elevated, running as LocalSystem)`.
pub fn token_facts() -> (bool, bool) {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: the pseudo handle needs no closing; the token is closed below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return (false, false);
    }
    let elevated = {
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut returned = 0u32;
        // SAFETY: buffer is the struct the class documents.
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenElevation,
                &mut elevation as *mut _ as *mut _,
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut returned,
            )
        };
        ok != 0 && elevation.TokenIsElevated != 0
    };
    let local_system = {
        let mut buffer = vec![0u8; 256];
        let mut returned = 0u32;
        // SAFETY: TOKEN_USER plus its SID fits comfortably in 256 bytes.
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr() as *mut _,
                buffer.len() as u32,
                &mut returned,
            )
        };
        if ok != 0 {
            // SAFETY: the call succeeded, so the buffer starts with TOKEN_USER.
            let user = unsafe { &*(buffer.as_ptr() as *const TOKEN_USER) };
            unsafe { IsWellKnownSid(user.User.Sid, WinLocalSystemSid) != 0 }
        } else {
            false
        }
    };
    // SAFETY: token was opened above.
    unsafe { CloseHandle(token) };
    (elevated, local_system)
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn registry_dword(subkey: &str, value: &str) -> Option<u32> {
    let mut data = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: NUL-terminated names; buffer sized as declared.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            wide(subkey).as_ptr(),
            wide(value).as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut data as *mut _ as *mut _,
            &mut size,
        )
    };
    (status == 0).then_some(data)
}

fn registry_string(subkey: &str, value: &str) -> Option<String> {
    let mut buffer = vec![0u16; 256];
    let mut size = (buffer.len() * 2) as u32;
    // SAFETY: NUL-terminated names; buffer sized as declared.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            wide(subkey).as_ptr(),
            wide(value).as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buffer.as_mut_ptr() as *mut _,
            &mut size,
        )
    };
    if status != 0 {
        return None;
    }
    let length = (size as usize / 2).min(buffer.len());
    let text: Vec<u16> = buffer[..length]
        .iter()
        .copied()
        .take_while(|c| *c != 0)
        .collect();
    Some(String::from_utf16_lossy(&text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facts_are_populated_on_this_host() {
        let facts = host_facts();
        assert!(facts.os_build.starts_with("10."), "{}", facts.os_build);
        assert!(facts.logical_processors > 0);
        assert!(facts.memory_total > 0);
        assert!(uptime_ms() > 0);
        let _ = slept_ms();
        let _ = on_battery();
        let mut sampler = CpuSampler::new();
        assert!(sampler.busy_percent() <= 100);
    }
}
