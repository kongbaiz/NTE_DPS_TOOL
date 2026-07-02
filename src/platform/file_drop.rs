use std::path::PathBuf;

use crossbeam_channel::{Receiver, Sender, unbounded};
use eframe::Frame;

#[cfg(windows)]
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
#[cfg(windows)]
use std::ffi::OsString;
#[cfg(windows)]
use std::os::windows::ffi::OsStringExt;
#[cfg(windows)]
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
#[cfg(windows)]
use windows_sys::Win32::UI::Shell::{
    DefSubclassProc, DragAcceptFiles, DragFinish, DragQueryFileW, HDROP, RemoveWindowSubclass,
    SetWindowSubclass,
};
#[cfg(windows)]
use windows_sys::Win32::UI::WindowsAndMessaging::{
    ChangeWindowMessageFilter, ChangeWindowMessageFilterEx, MSGFLT_ADD, MSGFLT_ALLOW, WM_COPYDATA,
    WM_DROPFILES,
};

#[cfg(windows)]
const DROP_SUBCLASS_ID: usize = 0x4E54_4544;
#[cfg(windows)]
const WM_COPYGLOBALDATA: u32 = 0x0049;

pub struct NativeFileDrop {
    receiver: Receiver<PathBuf>,
    #[cfg(windows)]
    registration: Option<DropRegistration>,
}

#[cfg(windows)]
struct DropRegistration {
    hwnd: HWND,
    sender: Box<Sender<PathBuf>>,
}

impl NativeFileDrop {
    pub fn new() -> Self {
        let (_sender, receiver) = unbounded();
        Self {
            receiver,
            #[cfg(windows)]
            registration: None,
        }
    }

    pub fn install(&mut self, frame: &Frame) {
        #[cfg(windows)]
        {
            if self.registration.is_some() {
                return;
            }
            let Ok(window_handle) = frame.window_handle() else {
                return;
            };
            let RawWindowHandle::Win32(window_handle) = window_handle.as_raw() else {
                return;
            };
            let hwnd = window_handle.hwnd.get() as HWND;
            let (sender, receiver) = unbounded();
            self.receiver = receiver;
            let sender = Box::new(sender);
            let sender_pointer = std::ptr::from_ref(sender.as_ref()) as usize;
            // Elevated processes need both the legacy process filter and the per-window filter.
            // Some Explorer/Windows combinations do not complete the WM_DROPFILES message chain
            // when only ChangeWindowMessageFilterEx is used.
            allow_drop_message(hwnd, WM_DROPFILES);
            allow_drop_message(hwnd, WM_COPYDATA);
            allow_drop_message(hwnd, WM_COPYGLOBALDATA);

            // SAFETY: The callback data remains boxed until the subclass is removed in Drop.
            let installed = unsafe {
                DragAcceptFiles(hwnd, 1);
                SetWindowSubclass(
                    hwnd,
                    Some(drop_subclass_proc),
                    DROP_SUBCLASS_ID,
                    sender_pointer,
                ) != 0
            };
            if installed {
                self.registration = Some(DropRegistration { hwnd, sender });
            } else {
                // SAFETY: Reverses DragAcceptFiles above when subclass installation fails.
                unsafe { DragAcceptFiles(hwnd, 0) };
            }
        }
    }

    pub fn try_iter(&self) -> impl Iterator<Item = PathBuf> + '_ {
        self.receiver.try_iter()
    }
}

#[cfg(windows)]
fn allow_drop_message(hwnd: HWND, message: u32) {
    // SAFETY: Both functions only update the caller's own UIPI message filters.
    unsafe {
        let _ = ChangeWindowMessageFilter(message, MSGFLT_ADD);
        let _ = ChangeWindowMessageFilterEx(hwnd, message, MSGFLT_ALLOW, std::ptr::null_mut());
    }
}

#[cfg(windows)]
// SAFETY: Windows calls this subclass procedure with the SetWindowSubclass ABI.
// sender_pointer is the boxed Sender pointer provided during subclass installation.
unsafe extern "system" fn drop_subclass_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass_id: usize,
    sender_pointer: usize,
) -> LRESULT {
    if message == WM_DROPFILES {
        let drop_handle = wparam as HDROP;
        // SAFETY: sender_pointer was created from the boxed Sender during subclass installation
        // and remains alive until the subclass is removed in Drop.
        let sender = unsafe { &*(sender_pointer as *const Sender<PathBuf>) };
        // SAFETY: drop_handle is the HDROP supplied by WM_DROPFILES; null buffer queries count.
        let file_count = unsafe { DragQueryFileW(drop_handle, u32::MAX, std::ptr::null_mut(), 0) };
        for index in 0..file_count {
            // SAFETY: drop_handle is valid for this callback; null buffer queries UTF-16 length.
            let length =
                unsafe { DragQueryFileW(drop_handle, index, std::ptr::null_mut(), 0) } as usize;
            let mut buffer = vec![0_u16; length + 1];
            // SAFETY: buffer is writable and sized to include the terminating NUL.
            let written = unsafe {
                DragQueryFileW(drop_handle, index, buffer.as_mut_ptr(), buffer.len() as u32)
            } as usize;
            if written > 0 {
                let path = PathBuf::from(OsString::from_wide(&buffer[..written]));
                let _ = sender.send(path);
            }
        }
        // SAFETY: Completes ownership of the HDROP received in WM_DROPFILES.
        unsafe { DragFinish(drop_handle) };
        return 0;
    }
    // SAFETY: Messages not handled here are forwarded with the original callback parameters.
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

#[cfg(windows)]
impl Drop for NativeFileDrop {
    fn drop(&mut self) {
        if let Some(registration) = self.registration.take() {
            // SAFETY: The HWND and callback pair are the same values used during installation.
            let removed = unsafe {
                DragAcceptFiles(registration.hwnd, 0);
                RemoveWindowSubclass(
                    registration.hwnd,
                    Some(drop_subclass_proc),
                    DROP_SUBCLASS_ID,
                ) != 0
            };
            if removed {
                drop(registration.sender);
            } else {
                // The callback may still be registered. Leak the sender rather than leave it
                // pointing at freed memory during late window destruction messages.
                Box::leak(registration.sender);
            }
        }
    }
}
