//! The collector's own footprint, sampled once per interval so observer
//! effect is measurable against the lane states in force at the time.

use crate::hostfacts::filetime;
use eidos_observe::ProcessResources;
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::ProcessStatus::{
    K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetProcessHandleCount, GetProcessIoCounters,
    GetProcessTimes, IO_COUNTERS,
};

pub fn sample_process() -> ProcessResources {
    // SAFETY: the pseudo handle is always valid and never closed.
    let process = unsafe { GetCurrentProcess() };
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut creation, mut exit, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: out-parameters only.
    unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) };
    let cpu_ms = (filetime(&kernel).saturating_add(filetime(&user))) / 10_000;

    // SAFETY: the counters struct is sized as declared.
    let mut memory: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
    memory.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    unsafe { K32GetProcessMemoryInfo(process, &mut memory as *mut _ as *mut _, memory.cb) };

    let mut handles = 0u32;
    // SAFETY: out-parameter only.
    unsafe { GetProcessHandleCount(process, &mut handles) };

    // SAFETY: out-parameter only.
    let mut io: IO_COUNTERS = unsafe { std::mem::zeroed() };
    unsafe { GetProcessIoCounters(process, &mut io) };

    ProcessResources {
        cpu_ms,
        working_set_bytes: memory.WorkingSetSize as u64,
        private_bytes: memory.PrivateUsage as u64,
        handles,
        threads: thread_count(),
        read_bytes: io.ReadTransferCount,
        write_bytes: io.WriteTransferCount,
        read_ops: io.ReadOperationCount,
        write_ops: io.WriteOperationCount,
    }
}

fn thread_count() -> u32 {
    // SAFETY: snapshot handle is closed below; entry struct sized as declared.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return 0;
        }
        let me = GetCurrentProcessId();
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut threads = 0;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                if entry.th32ProcessID == me {
                    threads = entry.cntThreads;
                    break;
                }
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
        threads
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn samples_this_process() {
        let sample = super::sample_process();
        assert!(sample.working_set_bytes > 0);
        assert!(sample.handles > 0);
        assert!(sample.threads > 0);
    }
}
