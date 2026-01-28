use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
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
    SendMessageW, GetClientRect,
};
use windows::core::{Interface, Result};

use crate::clipboard::get_selection_via_copy_preserving_clipboard;
use crate::{report_error_log, report_info_log};

type FocusedSelection = (Option<String>, Option<(i32, i32)>);

/// 从矩形数组中提取最佳的左上角坐标
/// 
/// @param rects: UIA 返回的 SafeArray，包含一组 double 类型的矩形数据 [l, t, w, h, ...]
/// @param container_rect: 控件本身的屏幕坐标边界，用于裁剪和过滤
unsafe fn extract_top_left_from_rects(
    rects: *mut SAFEARRAY,
    container_rect: Option<RECT>,
) -> Result<Option<(i32, i32)>> {
    if rects.is_null() {
        return Ok(None);
    }
    
    // 获取数组上下界
    let lbound = unsafe { SafeArrayGetLBound(rects, 1)? };
    let ubound = unsafe { SafeArrayGetUBound(rects, 1)? };
    if ubound < lbound {
        return Ok(None);
    }
    
    // 每一个矩形由 4 个 double 组成: Left, Top, Width, Height
    let total_len = (ubound - lbound + 1) as usize;
    if total_len < 4 {
        return Ok(None);
    }

    let mut data_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
    unsafe { SafeArrayAccessData(rects, &mut data_ptr)? };
    
    if data_ptr.is_null() {
        let _ = unsafe { SafeArrayUnaccessData(rects) };
        return Ok(None);
    }

    // 将裸指针转换为 slice 方便遍历
    let slice: &[f64] = unsafe { std::slice::from_raw_parts(data_ptr as *const f64, total_len) };
    
    let mut best_pos: Option<(i32, i32)> = None;

    // 遍历所有矩形 (步长为 4)
    // 逻辑：找到第一个与 Container 有交集（即可视）的矩形
    for chunk in slice.chunks(4) {
        if chunk.len() < 4 {
            break;
        }
        let r_left = chunk[0];
        let r_top = chunk[1];
        let r_width = chunk[2];
        let r_height = chunk[3];
        
        // 转换 text rect 为整数逻辑方便比较
        let r_right = r_left + r_width;
        let r_bottom = r_top + r_height;

        if let Some(c_rect) = container_rect {
            // Container 坐标
            let c_left = c_rect.left as f64;
            let c_top = c_rect.top as f64;
            let c_right = c_rect.right as f64;
            let c_bottom = c_rect.bottom as f64;

            // 1. 检查是否完全在可视区域上方或左方 (Scrolled out)
            if r_bottom < c_top || r_right < c_left {
                // 这个矩形已经被卷出去了，跳过，找下一行
                continue;
            }

            // 2. 检查是否完全在可视区域下方或右方
            if r_top > c_bottom || r_left > c_right {
                // 这个矩形还没出现或者已经超出范围，如果是顺序排列的文本，后面的一般也不会匹配了
                // 但为了保险，我们可以继续或者直接 break。这里选择 continue 以防万一。
                continue;
            }

            // 3. 找到了可视（或部分可视）的矩形
            // 执行 Clamping (钳制)，确保返回的坐标不会超出容器边界（变成负数或不可见）
            let final_x = r_left.max(c_left);
            let final_y = r_top.max(c_top);

            best_pos = Some((final_x.round() as i32, final_y.round() as i32));
            break; // 找到第一个可视的即可退出
        } else {
            report_info_log!("无法获取 Container 边界，回退到原始逻辑");
            // 如果无法获取 Container 边界，则回退到原来的逻辑：直接取第一个
            best_pos = Some((r_left.round() as i32, r_top.round() as i32));
            break;
        }
    }

    // 如果遍历完都没找到可视的（比如全选了但都不在视野内），
    // 理论上最好返回 None 让外部回退到鼠标位置，或者返回第一个计算出的坐标。
    // 这里如果 best_pos 还是 None，说明所有 rect 都被剔除了。
    // 作为一个兜底，如果原本有数据但都被剔除了，我们尝试取第一个数据的 Clamped 版本（如果容器存在），
    // 或者直接取第一个原始数据。
    if best_pos.is_none() && total_len >= 4 {
        let r_left = slice[0];
        let r_top = slice[1];
        if let Some(c_rect) = container_rect {
             // 强行钳制第一个
             let final_x = r_left.max(c_rect.left as f64);
             let final_y = r_top.max(c_rect.top as f64);
             best_pos = Some((final_x.round() as i32, final_y.round() as i32));
        } else {
             best_pos = Some((r_left.round() as i32, r_top.round() as i32));
        }
    }

    let _ = unsafe { SafeArrayUnaccessData(rects) };
    Ok(best_pos)
}

fn get_focused_selection() -> Result<FocusedSelection> {
    // 通过 UI Automation 获取当前控件的选中文本
    unsafe {
        let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
        let focused_element = uia.GetFocusedElement()?;

        // [新增] 获取当前控件的可视矩形边界 (Screen Coordinates)
        // 这对于过滤滚动导致的负坐标至关重要
        let container_rect = focused_element.CurrentBoundingRectangle().ok();
        report_info_log!("Container 边界: {:?}", container_rect);

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
                    top_left = extract_top_left_from_rects(rects, container_rect).unwrap_or(None);
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

        // [关键步骤] 获取控件可视区域并进行坐标钳制(Clamping)
        let mut client_rect = RECT::default();
        if GetClientRect(hwnd, &mut client_rect).is_ok() {
            // client_rect.left/top 永远是 0
            // 如果坐标小于 0 (卷出去了)，强行吸附到 0
            // 如果坐标大于宽高 (还没显示)，强行吸附到边缘
            pt.x = pt.x.clamp(client_rect.left, client_rect.right);
            pt.y = pt.y.clamp(client_rect.top, client_rect.bottom);
        }

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

        // [关键步骤] 坐标钳制
        let mut client_rect = RECT::default();
        if GetClientRect(hwnd, &mut client_rect).is_ok() {
            pt.x = pt.x.clamp(client_rect.left, client_rect.right);
            pt.y = pt.y.clamp(client_rect.top, client_rect.bottom);
        }

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
