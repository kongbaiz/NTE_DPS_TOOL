use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use eframe::egui;
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
#[cfg(not(feature = "no_debug"))]
use windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_F12;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_HOME;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetForegroundWindow, GetWindowThreadProcessId, KBDLLHOOKSTRUCT, MSG, PM_REMOVE,
    PeekMessageW, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP,
    WM_SYSKEYDOWN, WM_SYSKEYUP,
};

static HOTKEY_SENDER: OnceLock<Sender<HotkeyEvent>> = OnceLock::new();
static HOTKEY_CONTEXT: OnceLock<egui::Context> = OnceLock::new();
static HOME_DOWN: AtomicBool = AtomicBool::new(false);
#[cfg(not(feature = "no_debug"))]
static F12_DOWN: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy)]
pub enum HotkeyEvent {
    TogglePassthrough,
    #[cfg(not(feature = "no_debug"))]
    ToggleDebug,
    RegistrationFailed(&'static str),
}

fn send_hotkey(event: HotkeyEvent) {
    if let Some(sender) = HOTKEY_SENDER.get() {
        let _ = sender.send(event);
    }
    if let Some(context) = HOTKEY_CONTEXT.get() {
        context.request_repaint();
    }
}

unsafe extern "system" fn low_level_keyboard_proc(
    code: i32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    if code >= 0 {
        let foreground = unsafe { GetForegroundWindow() };
        let mut foreground_process_id = 0_u32;
        if !foreground.is_null() {
            unsafe {
                GetWindowThreadProcessId(foreground, &mut foreground_process_id);
            }
        }
        if foreground_process_id == std::process::id() {
            return unsafe { CallNextHookEx(std::ptr::null_mut(), code, w_param, l_param) };
        }
        let keyboard = unsafe { &*(l_param as *const KBDLLHOOKSTRUCT) };
        if keyboard.vkCode == VK_HOME as u32 {
            match w_param as u32 {
                WM_KEYDOWN | WM_SYSKEYDOWN if !HOME_DOWN.swap(true, Ordering::Relaxed) => {
                    send_hotkey(HotkeyEvent::TogglePassthrough);
                }
                WM_KEYUP | WM_SYSKEYUP => {
                    HOME_DOWN.store(false, Ordering::Relaxed);
                }
                _ => {}
            }
        }
        #[cfg(not(feature = "no_debug"))]
        if keyboard.vkCode == VK_F12 as u32 {
            match w_param as u32 {
                WM_KEYDOWN | WM_SYSKEYDOWN if !F12_DOWN.swap(true, Ordering::Relaxed) => {
                    send_hotkey(HotkeyEvent::ToggleDebug);
                }
                WM_KEYUP | WM_SYSKEYUP => {
                    F12_DOWN.store(false, Ordering::Relaxed);
                }
                _ => {}
            }
        }
    }
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, w_param, l_param) }
}

pub struct HotkeyHandle {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl HotkeyHandle {
    pub fn start(context: egui::Context) -> (Self, Receiver<HotkeyEvent>) {
        let (sender, receiver) = unbounded();
        let _ = HOTKEY_SENDER.set(sender.clone());
        let _ = HOTKEY_CONTEXT.set(context);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            let hook = unsafe {
                SetWindowsHookExW(
                    WH_KEYBOARD_LL,
                    Some(low_level_keyboard_proc),
                    std::ptr::null_mut(),
                    0,
                )
            };
            if hook.is_null() {
                let _ = sender.send(HotkeyEvent::RegistrationFailed(
                    "Home / F12（低级键盘钩子）",
                ));
                return;
            }

            let mut message = unsafe { std::mem::zeroed::<MSG>() };
            while !worker_stop.load(Ordering::Relaxed) {
                while unsafe { PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) }
                    != 0
                {}
                thread::sleep(Duration::from_millis(8));
            }

            unsafe {
                UnhookWindowsHookEx(hook);
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
