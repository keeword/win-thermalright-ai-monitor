#[cfg(windows)]
use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, AtomicUsize};
#[cfg(windows)]
use std::sync::{Arc, atomic::Ordering};
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::{
        Power::{
            HPOWERNOTIFY, POWERBROADCAST_SETTING, RegisterPowerSettingNotification,
            UnregisterPowerSettingNotification,
        },
        SystemServices::GUID_CONSOLE_DISPLAY_STATE,
    },
    UI::{
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{
            DEVICE_NOTIFY_WINDOW_HANDLE, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMECRITICAL,
            PBT_APMRESUMESUSPEND, PBT_APMSUSPEND, PBT_POWERSETTINGCHANGE, WM_NCDESTROY,
            WM_POWERBROADCAST,
        },
    },
};

#[derive(Default)]
pub struct DisplayState {
    pub off: AtomicBool,
    pub suspended: AtomicBool,
    pub generation: AtomicUsize,
}

#[cfg(windows)]
const SUBCLASS_ID: usize = 0x54484d50;

/// Owned by the UI thread, like the preview HWND. Notifications also arrive
/// while the window is hidden; the USB worker reads the shared state directly.
#[cfg(windows)]
pub struct DisplayPower {
    window: HWND,
    notification: HPOWERNOTIFY,
    state: Arc<DisplayState>,
}

#[cfg(windows)]
impl DisplayPower {
    pub fn new(window: HWND, state: Arc<DisplayState>) -> Result<Self> {
        let mut monitor = Self {
            window,
            notification: 0,
            state,
        };
        // The Arc allocation stays alive until Drop has removed the subclass.
        let installed = unsafe {
            SetWindowSubclass(
                window,
                Some(power_callback),
                SUBCLASS_ID,
                Arc::as_ptr(&monitor.state) as usize,
            )
        };
        if installed == 0 {
            return Err(std::io::Error::last_os_error())
                .context("Installing display power listener");
        }
        // Windows sends the current display state after registration, including
        // when the application starts with the console display already off.
        monitor.notification = unsafe {
            RegisterPowerSettingNotification(
                window,
                &GUID_CONSOLE_DISPLAY_STATE,
                DEVICE_NOTIFY_WINDOW_HANDLE,
            )
        };
        if monitor.notification == 0 {
            return Err(std::io::Error::last_os_error())
                .context("Registering display power notification");
        }
        Ok(monitor)
    }
}

#[cfg(windows)]
impl Drop for DisplayPower {
    fn drop(&mut self) {
        unsafe {
            if self.notification != 0 {
                UnregisterPowerSettingNotification(self.notification);
            }
            RemoveWindowSubclass(self.window, Some(power_callback), SUBCLASS_ID);
        }
    }
}

#[cfg(windows)]
unsafe extern "system" fn power_callback(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    subclass_id: usize,
    data: usize,
) -> LRESULT {
    let state = unsafe { &*(data as *const DisplayState) };
    if message == WM_POWERBROADCAST && wparam == PBT_APMSUSPEND as usize {
        state.suspended.store(true, Ordering::Relaxed);
    } else if message == WM_POWERBROADCAST
        && matches!(
            wparam as u32,
            PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND | PBT_APMRESUMECRITICAL
        )
    {
        // Automatic wake may be unattended; only an interactive resume clears
        // a stale off state. Every resume invalidates the old USB connection.
        if wparam != PBT_APMRESUMEAUTOMATIC as usize {
            state.off.store(false, Ordering::Relaxed);
        }
        state.suspended.store(false, Ordering::Relaxed);
        state.generation.fetch_add(1, Ordering::Relaxed);
    } else if message == WM_POWERBROADCAST
        && wparam == PBT_POWERSETTINGCHANGE as usize
        && lparam != 0
    {
        // Windows owns this variable-length payload for the duration of the
        // callback. Read the DWORD only after checking its advertised length.
        let setting = lparam as *const POWERBROADCAST_SETTING;
        let guid = unsafe { std::ptr::addr_of!((*setting).PowerSetting).read_unaligned() };
        let length = unsafe { std::ptr::addr_of!((*setting).DataLength).read_unaligned() };
        if guid.data1 == GUID_CONSOLE_DISPLAY_STATE.data1
            && guid.data2 == GUID_CONSOLE_DISPLAY_STATE.data2
            && guid.data3 == GUID_CONSOLE_DISPLAY_STATE.data3
            && guid.data4 == GUID_CONSOLE_DISPLAY_STATE.data4
            && length == size_of::<u32>() as u32
        {
            let value = unsafe {
                std::ptr::addr_of!((*setting).Data)
                    .cast::<u32>()
                    .read_unaligned()
            };
            // 0 = off, 1 = on, 2 = dimmed. Dimming keeps the LCD running.
            if value <= 2 && state.off.swap(value == 0, Ordering::Relaxed) && value != 0 {
                state.generation.fetch_add(1, Ordering::Relaxed);
            }
        }
    } else if message == WM_NCDESTROY {
        unsafe { RemoveWindowSubclass(window, Some(power_callback), subclass_id) };
    }
    unsafe { DefSubclassProc(window, message, wparam, lparam) }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use windows_sys::{
        Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, HWND_MESSAGE, IsWindowVisible, SendMessageW,
        },
        core::{GUID, w},
    };

    struct TestWindow(HWND);
    impl Drop for TestWindow {
        fn drop(&mut self) {
            unsafe { DestroyWindow(self.0) };
        }
    }

    #[repr(C)]
    struct Notification {
        guid: GUID,
        length: u32,
        value: u32,
    }

    fn notify(window: HWND, guid: GUID, length: u32, value: u32) {
        let payload = Notification {
            guid,
            length,
            value,
        };
        unsafe {
            SendMessageW(
                window,
                WM_POWERBROADCAST,
                PBT_POWERSETTINGCHANGE as usize,
                &payload as *const Notification as isize,
            );
        }
    }

    #[test]
    fn hidden_window_tracks_power_events_and_detaches_on_drop() {
        let window = TestWindow(unsafe {
            CreateWindowExW(
                0,
                w!("STATIC"),
                std::ptr::null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        });
        assert!(!window.0.is_null());
        assert_eq!(unsafe { IsWindowVisible(window.0) }, 0);
        let state = Arc::new(DisplayState::default());
        let off = &state.off;
        let monitor = DisplayPower::new(window.0, state.clone()).unwrap();

        for (value, expected) in [(0, true), (1, false), (0, true), (2, false)] {
            notify(window.0, GUID_CONSOLE_DISPLAY_STATE, 4, value);
            assert_eq!(off.load(Ordering::Relaxed), expected);
        }
        notify(window.0, GUID_CONSOLE_DISPLAY_STATE, 4, 0);
        // Unrelated settings, malformed lengths, and unknown values must not
        // accidentally wake the LCD.
        notify(window.0, GUID::from_u128(0), 4, 1);
        notify(window.0, GUID_CONSOLE_DISPLAY_STATE, 1, 1);
        notify(window.0, GUID_CONSOLE_DISPLAY_STATE, 4, 3);
        unsafe { SendMessageW(window.0, WM_POWERBROADCAST, 0, 0) };
        unsafe {
            SendMessageW(
                window.0,
                WM_POWERBROADCAST,
                PBT_POWERSETTINGCHANGE as usize,
                0,
            )
        };
        assert!(off.load(Ordering::Relaxed));

        let generation = state.generation.load(Ordering::Relaxed);
        unsafe { SendMessageW(window.0, WM_POWERBROADCAST, PBT_APMSUSPEND as usize, 0) };
        assert!(state.suspended.load(Ordering::Relaxed));
        unsafe {
            SendMessageW(
                window.0,
                WM_POWERBROADCAST,
                PBT_APMRESUMEAUTOMATIC as usize,
                0,
            )
        };
        assert!(!state.suspended.load(Ordering::Relaxed));
        assert!(
            off.load(Ordering::Relaxed),
            "unattended wake must keep the display off"
        );
        assert_eq!(state.generation.load(Ordering::Relaxed), generation + 1);
        unsafe {
            SendMessageW(
                window.0,
                WM_POWERBROADCAST,
                PBT_APMRESUMESUSPEND as usize,
                0,
            )
        };
        assert!(
            !off.load(Ordering::Relaxed),
            "interactive resume must clear stale off state"
        );
        assert_eq!(state.generation.load(Ordering::Relaxed), generation + 2);
        // Off/on between worker ticks still invalidates the previous handle.
        notify(window.0, GUID_CONSOLE_DISPLAY_STATE, 4, 0);
        notify(window.0, GUID_CONSOLE_DISPLAY_STATE, 4, 1);
        assert_eq!(state.generation.load(Ordering::Relaxed), generation + 3);
        notify(window.0, GUID_CONSOLE_DISPLAY_STATE, 4, 0);

        drop(monitor);
        notify(window.0, GUID_CONSOLE_DISPLAY_STATE, 4, 1);
        assert!(off.load(Ordering::Relaxed));
    }
}
