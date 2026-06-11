use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, unbounded};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey, VK_HOME,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{MSG, PM_REMOVE, PeekMessageW, WM_HOTKEY};

const PASSTHROUGH_HOTKEY_ID: i32 = 0x4E54;

pub enum HotkeyEvent {
    TogglePassthrough,
    RegistrationFailed(&'static str),
}

pub struct HotkeyHandle {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl HotkeyHandle {
    pub fn start() -> (Self, Receiver<HotkeyEvent>) {
        let (sender, receiver) = unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            let passthrough_registered = unsafe {
                RegisterHotKey(
                    std::ptr::null_mut(),
                    PASSTHROUGH_HOTKEY_ID,
                    MOD_NOREPEAT,
                    VK_HOME as u32,
                )
            } != 0;
            if !passthrough_registered {
                let _ = sender.send(HotkeyEvent::RegistrationFailed("Home"));
                return;
            }

            let mut message = unsafe { std::mem::zeroed::<MSG>() };
            while !worker_stop.load(Ordering::Relaxed) {
                while unsafe {
                    PeekMessageW(
                        &mut message,
                        std::ptr::null_mut(),
                        WM_HOTKEY,
                        WM_HOTKEY,
                        PM_REMOVE,
                    )
                } != 0
                {
                    if message.wParam == PASSTHROUGH_HOTKEY_ID as usize {
                        let _ = sender.send(HotkeyEvent::TogglePassthrough);
                    }
                }
                thread::sleep(Duration::from_millis(25));
            }

            unsafe {
                if passthrough_registered {
                    UnregisterHotKey(std::ptr::null_mut(), PASSTHROUGH_HOTKEY_ID);
                }
            }
        });

        (
            Self {
                stop,
                thread: Some(thread),
            },
            receiver,
        )
    }
}

impl Drop for HotkeyHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
