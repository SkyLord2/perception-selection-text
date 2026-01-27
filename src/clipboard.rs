use std::thread;
use std::time::Duration;

use windows::Win32::Foundation::{ERROR_SUCCESS, GetLastError, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
    GetClipboardSequenceNumber, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GLOBAL_ALLOC_FLAGS, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{CLIPBOARD_FORMAT, OleDuplicateData};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
    VIRTUAL_KEY, VK_CONTROL,
};
use windows::core::Result;

use crate::global::CF_UNICODETEXT_U32;
use crate::report_info_log;

struct ClipboardSnapshot {
    items: Vec<(u32, HANDLE)>,
}

impl ClipboardSnapshot {
    unsafe fn capture() -> Result<Self> {
        // 复制剪贴板中所有格式的数据，供后续恢复
        unsafe { OpenClipboard(None)? };
        let mut items: Vec<(u32, HANDLE)> = Vec::new();

        let mut format: u32 = 0;
        loop {
            format = unsafe { EnumClipboardFormats(format) };
            if format == 0 {
                let err = unsafe { GetLastError() };
                if err == ERROR_SUCCESS {
                    break;
                }
                let _ = unsafe { CloseClipboard() };
                return Err(windows::core::Error::from_thread());
            }

            if let Ok(handle) = unsafe { GetClipboardData(format) } {
                let dup = unsafe {
                    OleDuplicateData(
                        handle,
                        CLIPBOARD_FORMAT(format as u16),
                        GLOBAL_ALLOC_FLAGS(0),
                    )
                };
                if !dup.0.is_null() {
                    items.push((format, dup));
                }
            }
        }

        let _ = unsafe { CloseClipboard() };
        Ok(Self { items })
    }

    unsafe fn restore(self) {
        // 尝试打开剪贴板，避免占用导致失败
        let mut opened = false;
        for _ in 0..5 {
            if unsafe { OpenClipboard(None) }.is_ok() {
                opened = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }

        if !opened {
            return;
        }

        let _ = unsafe { EmptyClipboard() };

        for (format, handle) in self.items {
            let _ = unsafe { SetClipboardData(format, Some(handle)) };
        }

        let _ = unsafe { CloseClipboard() };
    }
}

pub unsafe fn get_selection_via_copy_preserving_clipboard() -> Result<Option<String>> {
    report_info_log!("UIA 无法获取到选中文本，回退到剪贴板复制方案!");
    // 先保存原剪贴板内容，再触发复制，读取结果后恢复
    let snapshot = match unsafe { ClipboardSnapshot::capture() } {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };

    let original_seq = unsafe { GetClipboardSequenceNumber() };
    unsafe { send_ctrl_c()? };

    for _ in 0..30 {
        // 等待剪贴板内容变化，最多约 300ms
        if unsafe { GetClipboardSequenceNumber() } != original_seq {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    if unsafe { GetClipboardSequenceNumber() } == original_seq {
        unsafe { snapshot.restore() };
        return Ok(None);
    }

    let copied_text = unsafe { read_clipboard_unicode_text() }.unwrap_or_default();
    unsafe { snapshot.restore() };
    if copied_text.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(copied_text))
    }
}

unsafe fn send_ctrl_c() -> Result<()> {
    // 发送 Ctrl+C 触发系统复制
    let inputs = [
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_CONTROL,
                    wScan: 0,
                    dwFlags: KEYBD_EVENT_FLAGS(0),
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0x43),
                    wScan: 0,
                    dwFlags: KEYBD_EVENT_FLAGS(0),
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0x43),
                    wScan: 0,
                    dwFlags: KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_CONTROL,
                    wScan: 0,
                    dwFlags: KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
    ];

    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent != inputs.len() as u32 {
        return Err(windows::core::Error::from_thread());
    }
    Ok(())
}

unsafe fn read_clipboard_unicode_text() -> Result<String> {
    // 读取剪贴板中的 Unicode 文本
    unsafe { OpenClipboard(None)? };

    let mut text = String::new();
    let handle = unsafe { GetClipboardData(CF_UNICODETEXT_U32)? };
    if !handle.0.is_null() {
        let hglobal = HGLOBAL(handle.0);
        let locked = unsafe { GlobalLock(hglobal) };
        if !locked.is_null() {
            let mut len = 0usize;
            let mut ptr = locked as *const u16;
            while unsafe { *ptr } != 0 {
                len += 1;
                ptr = unsafe { ptr.add(1) };
            }
            let slice = unsafe { std::slice::from_raw_parts(locked as *const u16, len) };
            text = String::from_utf16_lossy(slice);
            let _ = unsafe { GlobalUnlock(hglobal) };
        }
    }

    let _ = unsafe { CloseClipboard() };
    Ok(text)
}
