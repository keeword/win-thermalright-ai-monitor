use anyhow::{Result, ensure};
use std::{collections::HashSet, ffi::c_void, path::PathBuf};
use windows_sys::Win32::{
    Foundation::{CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE},
    Networking::WinSock::{WSACleanup, WSAIoctl, WSAStartup},
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
struct Winsock;
impl Winsock {
    fn new() -> Result<Self> {
        let mut data = unsafe { std::mem::zeroed() };
        ensure!(
            unsafe { WSAStartup(0x0202, &mut data) } == 0,
            "Winsock initialization failed"
        );
        Ok(Self)
    }
}
impl Drop for Winsock {
    fn drop(&mut self) {
        unsafe { WSACleanup() };
    }
}

/// Executed exclusively in a disposable helper. Duplicated handles are queried
/// for names and identity, never read or repositioned.
pub struct HeldRollout {
    pub path: PathBuf,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Default)]
pub struct CodexHandles {
    pub rollouts: Vec<HeldRollout>,
    pub unix_peer_pids: HashSet<u32>,
}

pub fn codex_handles(pid: u32, started: u64) -> Result<CodexHandles> {
    let _winsock = Winsock::new()?;
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
    let mut result = CodexHandles::default();
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
        let mut peer = 0u32;
        let mut returned = 0u32;
        // SIO_AF_UNIX_GETPEERPID (_WSAIOR(IOC_VENDOR, 256), afunix.h).
        // This only queries a connected socket; it never connects or sends data.
        // Windows can leave bytes-returned at zero even when it supplies the PID.
        if unsafe {
            WSAIoctl(
                handle.0 as usize,
                0x58000100,
                std::ptr::null(),
                0,
                (&mut peer as *mut u32).cast(),
                size_of::<u32>() as u32,
                &mut returned,
                std::ptr::null_mut(),
                None,
            )
        } == 0
            && peer != 0
        {
            result.unix_peer_pids.insert(peer);
        }
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
                result.rollouts.push(HeldRollout {
                    path,
                    metadata: super::rollout_metadata_file(file),
                });
            }
        }
    }
    ensure!(process_time(pid)? == started, "process identity changed");
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

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Networking::WinSock::{
        AF_UNIX, INVALID_SOCKET, SOCK_STREAM, SOCKADDR_UN, SOCKET, accept, bind, closesocket,
        connect, listen, recv, send, socket,
    };

    struct Socket(SOCKET);
    impl Drop for Socket {
        fn drop(&mut self) {
            unsafe { closesocket(self.0) };
        }
    }
    fn unix_socket() -> Socket {
        let value = unsafe { socket(AF_UNIX as i32, SOCK_STREAM, 0) };
        assert_ne!(value, INVALID_SOCKET);
        Socket(value)
    }

    #[test]
    fn held_unix_socket_peer_query_preserves_the_connection_and_rollout_cursor() {
        use std::io::{Seek, Write};
        let _winsock = Winsock::new().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let socket_path = directory.path().join("peer.sock");
        let mut address: SOCKADDR_UN = unsafe { std::mem::zeroed() };
        address.sun_family = AF_UNIX;
        let path = socket_path.to_str().unwrap().as_bytes();
        assert!(path.len() < address.sun_path.len());
        for (target, byte) in address.sun_path.iter_mut().zip(path) {
            *target = *byte as i8;
        }
        let pointer = (&address as *const SOCKADDR_UN).cast();
        let length = size_of::<SOCKADDR_UN>() as i32;
        let listener = unix_socket();
        assert_eq!(unsafe { bind(listener.0, pointer, length) }, 0);
        assert_eq!(unsafe { listen(listener.0, 1) }, 0);
        let client = unix_socket();
        assert_eq!(unsafe { connect(client.0, pointer, length) }, 0);
        let accepted = unsafe { accept(listener.0, std::ptr::null_mut(), std::ptr::null_mut()) };
        assert_ne!(accepted, INVALID_SOCKET);
        let accepted = Socket(accepted);

        let rollout_path = directory.path().join("rollout-test.jsonl");
        let mut rollout = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&rollout_path)
            .unwrap();
        rollout
            .write_all(b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"test\"}}\n")
            .unwrap();
        let cursor = rollout.stream_position().unwrap();
        let pid = std::process::id();
        let started = process_time(pid).unwrap();
        let found = codex_handles(pid, started).unwrap();
        assert!(found.unix_peer_pids.contains(&pid));
        assert!(found.rollouts.iter().any(|held| {
            held.metadata
                .as_ref()
                .is_some_and(|v| v["payload"]["id"] == "test")
        }));
        assert_eq!(rollout.stream_position().unwrap(), cursor);
        assert!(codex_handles(pid, started + 1).is_err());
        assert_eq!(unsafe { send(client.0, b"x".as_ptr(), 1, 0) }, 1);
        let mut byte = 0u8;
        assert_eq!(unsafe { recv(accepted.0, &mut byte, 1, 0) }, 1);
        assert_eq!(byte, b'x');
    }
}
