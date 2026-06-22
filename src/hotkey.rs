use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use eframe::egui;
use windows_sys::Win32::Foundation::{GetLastError, LPARAM, LRESULT, WPARAM};
#[cfg(not(feature = "no_debug"))]
use windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_F12;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_HOME;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetForegroundWindow, GetWindowThreadProcessId, KBDLLHOOKSTRUCT, MSG, PM_REMOVE,
    PeekMessageW, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP,
    WM_SYSKEYDOWN, WM_SYSKEYUP,
};

#[derive(Default)]
struct HotkeyState {
    sender: Option<Sender<HotkeyEvent>>,
    context: Option<egui::Context>,
    instance_id: u64,
}

static HOTKEY_STATE: OnceLock<Mutex<HotkeyState>> = OnceLock::new();
static HOTKEY_INSTANCE_COUNTER: AtomicU64 = AtomicU64::new(1);
static HOME_DOWN: AtomicBool = AtomicBool::new(false);
#[cfg(not(feature = "no_debug"))]
static F12_DOWN: AtomicBool = AtomicBool::new(false);

#[derive(Clone)]
pub enum HotkeyEvent {
    TogglePassthrough,
    #[cfg(not(feature = "no_debug"))]
    ToggleDebug,
    RegistrationFailed(String),
}

fn send_hotkey(event: HotkeyEvent) {
    let (sender, context) = HOTKEY_STATE
        .get()
        .map(|state| match state.lock() {
            Ok(state) => (state.sender.clone(), state.context.clone()),
            Err(poisoned) => {
                let state = poisoned.into_inner();
                (state.sender.clone(), state.context.clone())
            }
        })
        .unwrap_or((None, None));
    if let Some(sender) = sender {
        let _ = sender.send(event);
    }
    if let Some(context) = context {
        context.request_repaint();
    }
}

// SAFETY: Windows calls this function with the WH_KEYBOARD_LL hook ABI and hook-owned
// parameters. The body forwards unhandled events and only dereferences l_param for code >= 0.
unsafe extern "system" fn low_level_keyboard_proc(
    code: i32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    if code >= 0 {
        // SAFETY: The low-level keyboard hook is called on a Windows hook thread; querying
        // the current foreground window does not require ownership of the returned HWND.
        let foreground = unsafe { GetForegroundWindow() };
        let mut foreground_process_id = 0_u32;
        if !foreground.is_null() {
            // SAFETY: foreground was returned by GetForegroundWindow and the out pointer is valid.
            unsafe {
                GetWindowThreadProcessId(foreground, &mut foreground_process_id);
            }
        }
        if foreground_process_id == std::process::id() {
            // SAFETY: Forwarding the hook parameters exactly as received is required by the API.
            return unsafe { CallNextHookEx(std::ptr::null_mut(), code, w_param, l_param) };
        }
        // SAFETY: For WH_KEYBOARD_LL with code >= 0, l_param points to a KBDLLHOOKSTRUCT
        // that is valid for the duration of this callback.
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
    // SAFETY: Forwarding the hook parameters exactly as received is required by the API.
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, w_param, l_param) }
}

pub struct HotkeyHandle {
    instance_id: u64,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl HotkeyHandle {
    pub fn start(context: egui::Context) -> (Self, Receiver<HotkeyEvent>) {
        let instance_id = HOTKEY_INSTANCE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = unbounded();
        {
            let state = HOTKEY_STATE.get_or_init(|| Mutex::new(HotkeyState::default()));
            let mut state = match state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            state.sender = Some(sender.clone());
            state.context = Some(context);
            state.instance_id = instance_id;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            // SAFETY: Installs a process-local low-level keyboard hook with a static callback.
            let hook = unsafe {
                SetWindowsHookExW(
                    WH_KEYBOARD_LL,
                    Some(low_level_keyboard_proc),
                    std::ptr::null_mut(),
                    0,
                )
            };
            if hook.is_null() {
                // SAFETY: GetLastError reads the calling thread's last Windows error code.
                let error = unsafe { GetLastError() };
                #[cfg(not(feature = "no_debug"))]
                let shortcut = "Home / F12";
                #[cfg(feature = "no_debug")]
                let shortcut = "Home";
                let _ = sender.send(HotkeyEvent::RegistrationFailed(format!(
                    "{shortcut} 注册失败，GetLastError={error}"
                )));
                return;
            }

            // SAFETY: MSG is a plain old data Win32 struct and zero is a valid initial state.
            let mut message = unsafe { std::mem::zeroed::<MSG>() };
            while !worker_stop.load(Ordering::Relaxed) {
                // SAFETY: message points to valid storage and PM_REMOVE drains this thread's queue.
                while unsafe { PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) }
                    != 0
                {}
                thread::sleep(Duration::from_millis(8));
            }

            // SAFETY: hook was returned by SetWindowsHookExW in this thread and is unhooked once.
            unsafe {
                UnhookWindowsHookEx(hook);
            }
        });

        (
            Self {
                instance_id,
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
        let Some(state) = HOTKEY_STATE.get() else {
            return;
        };
        let mut state = match state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        if state.instance_id == self.instance_id {
            state.sender = None;
            state.context = None;
        }
    }
}
