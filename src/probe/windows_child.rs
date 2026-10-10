//! GUI probes need STARTF_FORCEOFFFEEDBACK, unavailable in stable CommandExt.
use std::{
    fs::File,
    io,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::ExitStatusExt,
    },
    path::Path,
    process::ExitStatus,
    ptr,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::Threading::{
        CREATE_NO_WINDOW, CreateProcessW, DeleteProcThreadAttributeList,
        EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetExitCodeProcess,
        InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION, STARTF_FORCEOFFFEEDBACK,
        STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
        WaitForSingleObject,
    },
};

struct Attributes(Vec<usize>);
impl Attributes {
    fn new(handles: &mut [*mut std::ffi::c_void]) -> io::Result<Self> {
        let mut size = 0;
        unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut size) };
        let mut buffer = vec![0usize; size.div_ceil(size_of::<usize>())];
        if unsafe { InitializeProcThreadAttributeList(buffer.as_mut_ptr().cast(), 1, 0, &mut size) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut attributes = Self(buffer);
        if unsafe {
            UpdateProcThreadAttribute(
                attributes.as_ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_mut_ptr().cast(),
                size_of_val(handles),
                ptr::null_mut(),
                ptr::null(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(attributes)
    }
    fn as_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.0.as_mut_ptr().cast()
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.as_ptr()) };
    }
}

fn inheritable(handle: &impl AsRawHandle) -> io::Result<OwnedHandle> {
    let mut duplicate = ptr::null_mut();
    let process = unsafe { GetCurrentProcess() };
    if unsafe {
        DuplicateHandle(
            process,
            handle.as_raw_handle(),
            process,
            &mut duplicate,
            0,
            1,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(duplicate) })
}

pub(super) fn run(program: &Path, arguments: &str, stop: &AtomicBool) -> anyhow::Result<Vec<u8>> {
    let executable: Vec<u16> = program.as_os_str().encode_wide().chain(Some(0)).collect();
    // Only fixed internal arguments are accepted; argv[0] must retain Unicode paths and spaces.
    let mut command: Vec<u16> = std::iter::once(b'"' as u16)
        .chain(program.as_os_str().encode_wide())
        .chain("\" ".encode_utf16())
        .chain(arguments.encode_utf16())
        .chain(Some(0))
        .collect();
    let (stdout, stdout_writer) = io::pipe()?;
    let (stderr, stderr_writer) = io::pipe()?;
    // Standard Command launches must not inherit our temporary pipe duplicates.
    let lock = super::SPAWN_LOCK.lock().unwrap();
    let input = inheritable(&File::open("NUL")?)?;
    let output = inheritable(&stdout_writer)?;
    let error = inheritable(&stderr_writer)?;
    let mut handles = [
        input.as_raw_handle(),
        output.as_raw_handle(),
        error.as_raw_handle(),
    ];
    // Restrict inheritance so concurrent WSL/preview launches cannot keep these pipes open.
    let mut attributes = Attributes::new(&mut handles)?;
    let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES | STARTF_FORCEOFFFEEDBACK;
    startup.StartupInfo.hStdInput = handles[0];
    startup.StartupInfo.hStdOutput = handles[1];
    startup.StartupInfo.hStdError = handles[2];
    startup.lpAttributeList = attributes.as_ptr();
    let mut information: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe {
        CreateProcessW(
            executable.as_ptr(),
            command.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            1,
            CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT,
            ptr::null(),
            ptr::null(),
            &startup.StartupInfo,
            &mut information,
        )
    } == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    let process = unsafe { OwnedHandle::from_raw_handle(information.hProcess) };
    drop(unsafe { OwnedHandle::from_raw_handle(information.hThread) });
    drop((
        attributes,
        input,
        output,
        error,
        stdout_writer,
        stderr_writer,
    ));
    drop(lock);
    super::read_output(stdout, stderr, &program.to_string_lossy(), None, || {
        wait(&process, stop)
    })
}

fn wait(process: &OwnedHandle, stop: &AtomicBool) -> anyhow::Result<ExitStatus> {
    let started = Instant::now();
    loop {
        match unsafe { WaitForSingleObject(process.as_raw_handle(), 20) } {
            WAIT_OBJECT_0 => {
                let mut code = 0;
                if unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut code) } == 0 {
                    return Err(io::Error::last_os_error().into());
                }
                return Ok(ExitStatus::from_raw(code));
            }
            WAIT_TIMEOUT => {}
            _ => return Err(io::Error::last_os_error().into()),
        }
        if stop.load(Ordering::Relaxed) || started.elapsed() > Duration::from_secs(2) {
            if unsafe { TerminateProcess(process.as_raw_handle(), 1) } == 0 {
                return Err(io::Error::last_os_error().into());
            }
            unsafe { WaitForSingleObject(process.as_raw_handle(), 150) };
            anyhow::bail!("probe timed out or cancelled");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "helper executed by the native launcher regression test"]
    fn child_reports_startup_flags() {
        let mut startup = unsafe { std::mem::zeroed() };
        unsafe { windows_sys::Win32::System::Threading::GetStartupInfoW(&mut startup) };
        println!("probe-startup-flags={}", startup.dwFlags);
        println!("{}", "x".repeat(100_000));
        eprintln!("probe-stderr");
    }

    #[test]
    #[ignore = "helper executed by the native launcher regression test"]
    fn child_waits() {
        std::thread::sleep(Duration::from_secs(10));
    }

    #[test]
    fn native_probe_suppresses_feedback_and_drains_output() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("探测 with spaces.exe");
        std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
        let output = run(
            &executable,
            "--ignored --exact probe::windows_child::tests::child_reports_startup_flags --nocapture",
            &AtomicBool::new(false),
        ).unwrap();
        let output = String::from_utf8(output).unwrap();
        let flags: u32 = output
            .lines()
            .find_map(|line| line.strip_prefix("probe-startup-flags="))
            .unwrap()
            .parse()
            .unwrap();
        assert_ne!(flags & STARTF_FORCEOFFFEEDBACK, 0);
        assert_ne!(flags & STARTF_USESTDHANDLES, 0);
        assert!(output.contains(&"x".repeat(100_000)));
        assert!(output.contains("test result: ok"));

        let cmd = std::env::var_os("COMSPEC").unwrap();
        let error = run(
            Path::new(&cmd),
            "/c echo native-probe-error 1>&2 & exit /b 7",
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(error.to_string().contains("native-probe-error"));
        assert!(error.to_string().contains("7"));
        let started = Instant::now();
        let cancelled = run(
            &executable,
            "--ignored --exact probe::windows_child::tests::child_waits",
            &AtomicBool::new(true),
        );
        assert!(cancelled.unwrap_err().to_string().contains("cancelled"));
        assert!(started.elapsed() < Duration::from_secs(3));
        let started = Instant::now();
        let timeout = run(
            &executable,
            "--ignored --exact probe::windows_child::tests::child_waits",
            &AtomicBool::new(false),
        );
        assert!(timeout.unwrap_err().to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
