use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, SAFEARRAY};
use windows::Win32::System::Ole::{
    SafeArrayAccessData, SafeArrayGetLBound, SafeArrayGetUBound, SafeArrayUnaccessData,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationTextPattern, UIA_TextPatternId,
};
use windows::Win32::UI::Controls::{EM_GETSEL, EM_POSFROMCHAR};
use windows::Win32::UI::WindowsAndMessaging::{
    GUITHREADINFO, GetClassNameW, GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId,
    SendMessageW,
};
use windows::core::{Interface, Result};

use crate::clipboard::get_selection_via_copy_preserving_clipboard;
use crate::{report_error_log, report_info_log};

type FocusedSelection = (Option<String>, Option<(i32, i32)>);

unsafe fn extract_top_left_from_rects(rects: *mut SAFEARRAY) -> Result<Option<(i32, i32)>> {
    // 从矩形数组中取首个矩形的左上角
    if rects.is_null() {
        return Ok(None);
    }
    let lbound = unsafe { SafeArrayGetLBound(rects, 1)? };
    let ubound = unsafe { SafeArrayGetUBound(rects, 1)? };
    if ubound < lbound {
        return Ok(None);
    }
    let len = (ubound - lbound + 1) as usize;
    if len < 4 {
        return Ok(None);
    }
    let mut data_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
    unsafe { SafeArrayAccessData(rects, &mut data_ptr)? };
    if data_ptr.is_null() {
        let _ = unsafe { SafeArrayUnaccessData(rects) };
        return Ok(None);
    }
    let slice: &[f64] = unsafe { std::slice::from_raw_parts(data_ptr as *const f64, len) };
    let (left, top) = (slice[0], slice[1]);
    let _ = unsafe { SafeArrayUnaccessData(rects) };
    Ok(Some((left.round() as i32, top.round() as i32)))
}

fn get_focused_selection() -> Result<FocusedSelection> {
    // 通过 UI Automation 获取当前控件的选中文本
    unsafe {
        let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
        let focused_element = uia.GetFocusedElement()?;

        let pattern_obj = focused_element.GetCurrentPattern(UIA_TextPatternId)?;
        let text_pattern: IUIAutomationTextPattern = match pattern_obj.cast() {
            Ok(p) => p,
            Err(_) => return Ok((None, None)),
        };

        let selection_ranges = text_pattern.GetSelection()?;
        let count = selection_ranges.Length()?;

        if count == 0 {
            return Ok((None, None));
        }

        let mut full_text = String::new();
        let mut top_left: Option<(i32, i32)> = None;
        for i in 0..count {
            let range = selection_ranges.GetElement(i)?;
            let text_bstr = range.GetText(-1)?;
            full_text.push_str(&text_bstr.to_string());
            if top_left.is_none() {
                // 读取选中文字高亮矩形，优先取第一个矩形
                if let Ok(rects) = range.GetBoundingRectangles() {
                    top_left = extract_top_left_from_rects(rects).unwrap_or(None);
                }
            }
        }

        if full_text.trim().is_empty() {
            Ok((None, top_left))
        } else {
            Ok((Some(full_text), top_left))
        }
    }
}

fn get_focus_hwnd() -> Option<HWND> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }
        let thread_id = GetWindowThreadProcessId(hwnd, None);
        if thread_id == 0 {
            return Some(hwnd);
        }
        let mut info = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        if GetGUIThreadInfo(thread_id, &mut info).is_ok() && !info.hwndFocus.0.is_null() {
            return Some(info.hwndFocus);
        }
        Some(hwnd)
    }
}

fn get_class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) } as usize;
    String::from_utf16_lossy(&buf[..len])
}

fn pos_from_edit(hwnd: HWND) -> Option<(i32, i32)> {
    let mut start = 0u32;
    let mut end = 0u32;
    unsafe {
        let _ = SendMessageW(
            hwnd,
            EM_GETSEL,
            Some(WPARAM(&mut start as *mut u32 as usize)),
            Some(LPARAM(&mut end as *mut u32 as isize)),
        );
        let pos = SendMessageW(
            hwnd,
            EM_POSFROMCHAR,
            Some(WPARAM(start as usize)),
            Some(LPARAM(0)),
        );
        let pos_val = pos.0 as u32;
        let mut pt = POINT {
            x: (pos_val & 0xFFFF) as i16 as i32,
            y: ((pos_val >> 16) & 0xFFFF) as i16 as i32,
        };
        if ClientToScreen(hwnd, &mut pt).as_bool() {
            report_info_log!("Edit坐标: {:?}", pt);
            return Some((pt.x, pt.y));
        }
    }
    None
}

fn pos_from_scintilla(hwnd: HWND) -> Option<(i32, i32)> {
    const SCI_GETSELECTIONSTART: u32 = 2143;
    const SCI_POINTXFROMPOSITION: u32 = 2164;
    const SCI_POINTYFROMPOSITION: u32 = 2165;
    unsafe {
        let start = SendMessageW(
            hwnd,
            SCI_GETSELECTIONSTART,
            Some(WPARAM(0)),
            Some(LPARAM(0)),
        )
        .0;
        let x = SendMessageW(
            hwnd,
            SCI_POINTXFROMPOSITION,
            Some(WPARAM(0)),
            Some(LPARAM(start)),
        )
        .0 as i32;
        let y = SendMessageW(
            hwnd,
            SCI_POINTYFROMPOSITION,
            Some(WPARAM(0)),
            Some(LPARAM(start)),
        )
        .0 as i32;
        let mut pt = POINT { x, y };
        if ClientToScreen(hwnd, &mut pt).as_bool() {
            report_info_log!("Scintilla坐标: {:?}", pt);
            return Some((pt.x, pt.y));
        }
    }
    None
}

fn get_selection_top_left_fallback() -> Option<(i32, i32)> {
    let hwnd = get_focus_hwnd()?;
    let class_name = get_class_name(hwnd);
    if class_name.eq_ignore_ascii_case("scintilla") {
        // Notepad++ 等 Scintilla 控件通过 SCI_* 消息换算选区起点坐标
        return pos_from_scintilla(hwnd);
    }
    if class_name.eq_ignore_ascii_case("edit")
        || class_name.to_ascii_lowercase().contains("richedit")
    {
        // 记事本 / RichEdit 通过 EM_GETSEL + EM_POSFROMCHAR 获取选区起点坐标
        return pos_from_edit(hwnd);
    }
    unsafe {
        let mut target_hwnd = hwnd;
        let mut tid = GetWindowThreadProcessId(target_hwnd, None);
        if tid == 0 {
            let fg = GetForegroundWindow();
            if fg.0.is_null() {
                report_error_log!("获取前台窗口句柄失败");
                return None;
            }
            target_hwnd = fg;
            tid = GetWindowThreadProcessId(target_hwnd, None);
        }
        let mut info = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        if GetGUIThreadInfo(tid, &mut info).is_ok() {
            let mut pt = POINT {
                x: info.rcCaret.left,
                y: info.rcCaret.top,
            };
            let caret_hwnd = if info.hwndCaret.0.is_null() {
                target_hwnd
            } else {
                info.hwndCaret
            };
            let caret_is_valid = info.rcCaret.left != 0
                || info.rcCaret.top != 0
                || info.rcCaret.right != 0
                || info.rcCaret.bottom != 0;
            if caret_is_valid && ClientToScreen(caret_hwnd, &mut pt).as_bool() {
                report_info_log!("插入符坐标: {:?}", pt);
                return Some((pt.x, pt.y));
            }
        } else {
            report_error_log!("获取Thread info失败");
        }
    }
    None
}

pub fn get_focused_selection_with_fallback_copy() -> Result<FocusedSelection> {
    // UIA 无结果时回退到剪贴板复制
    let (text_uia, top_left_uia) = get_focused_selection().unwrap_or((None, None));
    let text_opt = text_uia
        .or_else(|| unsafe { get_selection_via_copy_preserving_clipboard().unwrap_or(None) });
    if text_opt.is_some() && top_left_uia.is_none() {
        let top_left = top_left_uia.or_else(get_selection_top_left_fallback);
        Ok((text_opt, top_left))
    } else {
        Ok((text_opt, top_left_uia))
    }
}
