use anyhow::{Result, ensure};
use std::{collections::HashSet, ffi::c_void, path::PathBuf};
use windows_sys::Win32::{
    Foundation::{CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE},
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle, GetFinalPathNameByHandleW,
    },
    System::Threading::{
        GetCurrentProcess, GetProcessTimes, OpenProcess, PROCESS_DUP_HANDLE,
        PROCESS_QUERY_LIMITED_INFORMATION,
    },
};

#[repr(C)]
#[derive(Clone, Copy)]
struct Entry {
    object: *mut c_void,
    pid: usize,
    handle: usize,
    access: u32,
    trace: u16,
    object_type: u16,
    attributes: u32,
    reserved: u32,
}
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQuerySystemInformation(
        class: u32,
        buffer: *mut c_void,
        size: u32,
        needed: *mut u32,
    ) -> i32;
}
struct Owned(HANDLE);
impl Drop for Owned {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// Executed exclusively in a disposable helper. Duplicated handles are queried
/// for names and identity, never read or repositioned.
pub struct HeldRollout {
    pub path: PathBuf,
    pub metadata: Option<serde_json::Value>,
}

pub fn rollouts(pid: u32, started: u64) -> Result<Vec<HeldRollout>> {
    let process = Owned(unsafe {
        OpenProcess(
            PROCESS_DUP_HANDLE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        )
    });
    ensure!(
        !process.0.is_null(),
        "cannot inspect process {pid}: {}",
        std::io::Error::last_os_error()
    );
    let mut created = unsafe { std::mem::zeroed() };
    let mut exit = unsafe { std::mem::zeroed() };
    let mut kernel = unsafe { std::mem::zeroed() };
    let mut user = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { GetProcessTimes(process.0, &mut created, &mut exit, &mut kernel, &mut user) } != 0,
        "cannot query creation time"
    );
    let timestamp = ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64;
    ensure!(timestamp == started, "PID was reused");
    let mut buffer = vec![0usize; 131072];
    loop {
        let mut needed = 0;
        let status = unsafe {
            NtQuerySystemInformation(
                64,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * size_of::<usize>()) as u32,
                &mut needed,
            )
        };
        if status >= 0 {
            break;
        }
        ensure!(
            status == 0xc0000004u32 as i32 && needed < 128 * 1024 * 1024,
            "handle enumeration failed: {status:x}"
        );
        buffer.resize((needed as usize + 65536).div_ceil(size_of::<usize>()), 0);
    }
    let count = buffer[0];
    ensure!(
        count <= (buffer.len() * size_of::<usize>() - 2 * size_of::<usize>()) / size_of::<Entry>(),
        "invalid handle snapshot"
    );
    let entries =
        unsafe { std::slice::from_raw_parts(buffer.as_ptr().add(2).cast::<Entry>(), count) };
    let mut paths = HashSet::new();
    let mut result = Vec::new();
    for entry in entries.iter().filter(|e| e.pid == pid as usize) {
        let mut handle = std::ptr::null_mut();
        if unsafe {
            DuplicateHandle(
                process.0,
                entry.handle as HANDLE,
                GetCurrentProcess(),
                &mut handle,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        } == 0
        {
            continue;
        }
        let handle = Owned(handle);
        let mut name = vec![0u16; 32768];
        let n =
            unsafe { GetFinalPathNameByHandleW(handle.0, name.as_mut_ptr(), name.len() as u32, 0) }
                as usize;
        if n == 0 || n >= name.len() {
            continue;
        }
        let name = String::from_utf16_lossy(&name[..n]);
        if !name.ends_with(".jsonl") || !name.contains("rollout-") {
            continue;
        }
        let path = PathBuf::from(name);
        if let Ok(file) = std::fs::File::open(&path) {
            use std::os::windows::io::AsRawHandle;
            let mut held: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
            let mut opened: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
            if unsafe { GetFileInformationByHandle(handle.0, &mut held) } != 0
                && unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut opened) } != 0
                && held.dwVolumeSerialNumber == opened.dwVolumeSerialNumber
                && held.nFileIndexHigh == opened.nFileIndexHigh
                && held.nFileIndexLow == opened.nFileIndexLow
                && paths.insert(path.clone())
            {
                result.push(HeldRollout {
                    path,
                    metadata: super::rollout_metadata_file(file),
                });
            }
        }
    }
    Ok(result)
}

pub fn process_time(pid: u32) -> Result<u64> {
    let process = Owned(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) });
    ensure!(
        !process.0.is_null(),
        "cannot query process {pid}: {}",
        std::io::Error::last_os_error()
    );
    let mut created = unsafe { std::mem::zeroed() };
    let mut exit = unsafe { std::mem::zeroed() };
    let mut kernel = unsafe { std::mem::zeroed() };
    let mut user = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { GetProcessTimes(process.0, &mut created, &mut exit, &mut kernel, &mut user) } != 0,
        "creation time unavailable"
    );
    Ok(((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64)
}
