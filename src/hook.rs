use std::sync::{OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
use windows::Win32::UI::WindowsAndMessaging::{
    CURSORINFO, CallNextHookEx, GA_ROOT, GetAncestor, GetCursorInfo, GetSystemMetrics,
    GetWindowRect, GetWindowTextLengthW, GetWindowTextW, HHOOK, HTBOTTOM, HTBOTTOMLEFT,
    HTBOTTOMRIGHT, HTCAPTION, HTHSCROLL, HTLEFT, HTRIGHT, HTTOP, HTTOPLEFT, HTTOPRIGHT, HTVSCROLL,
    IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, LoadCursorW, MSLLHOOKSTRUCT,
    SM_CXDOUBLECLK, SM_CXPADDEDBORDER, SM_CYCAPTION, SM_CYDOUBLECLK, SM_CYFRAME, SendMessageW,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCHITTEST, WindowFromPoint,
};

use crate::global::{
    CHERRY_WND_TOP_HEIGHT, CLICK_UP_COUNT, DRAG_THRESHOLD_PX, HOOK_HANDLE, LAST_CLICK_UP_TIME,
    LAST_CLICK_UP_X, LAST_CLICK_UP_Y, MOUSE_DOWN_TIME, MOUSE_DOWN_X, MOUSE_DOWN_Y, MOUSE_LAST_X,
    MOUSE_LAST_Y, NONCLIENT_MOUSE_DOWN, SELECTION_THRESHOLD_MS, TriggerEvent, WORKER_TX,
};
use std::sync::atomic::Ordering;

enum DebounceMsg {
    Schedule {
        count: u32,
        last_time: u64,
        debounce_ms: u64,
    },
}

static CLICK_DEBOUNCE_TX: OnceLock<mpsc::Sender<DebounceMsg>> = OnceLock::new();

fn schedule_click_sequence(count: u32, last_time: u64, debounce_ms: u64) {
    let tx = CLICK_DEBOUNCE_TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || debounce_loop(rx));
        tx
    });
    let _ = tx.send(DebounceMsg::Schedule {
        count,
        last_time,
        debounce_ms,
    });
}

fn debounce_loop(rx: mpsc::Receiver<DebounceMsg>) {
    let mut pending: Option<(u32, u64, Instant)> = None;
    loop {
        if let Some((count, last_time, deadline)) = pending {
            let now = Instant::now();
            if now >= deadline {
                if LAST_CLICK_UP_TIME.load(Ordering::SeqCst) == last_time
                    && CLICK_UP_COUNT.load(Ordering::SeqCst) == count
                {
                    if let Some(tx) = WORKER_TX.get() {
                        let _ = tx.send(TriggerEvent::ClickSequence(count));
                    }
                    CLICK_UP_COUNT.store(0, Ordering::SeqCst);
                    LAST_CLICK_UP_TIME.store(0, Ordering::SeqCst);
                }
                pending = None;
                continue;
            }
            let timeout = deadline.saturating_duration_since(now);
            match rx.recv_timeout(timeout) {
                Ok(DebounceMsg::Schedule {
                    count,
                    last_time,
                    debounce_ms,
                }) => {
                    pending = Some((
                        count,
                        last_time,
                        Instant::now() + Duration::from_millis(debounce_ms),
                    ));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match rx.recv() {
                Ok(DebounceMsg::Schedule {
                    count,
                    last_time,
                    debounce_ms,
                }) => {
                    pending = Some((
                        count,
                        last_time,
                        Instant::now() + Duration::from_millis(debounce_ms),
                    ));
                }
                Err(_) => break,
            }
        }
    }
}

fn is_title_bar_hit(pt: POINT) -> bool {
    unsafe {
        let hwnd = WindowFromPoint(pt);
        if hwnd.0.is_null() {
            return false;
        }
        let lparam_val = ((pt.y as u32 & 0xFFFF) << 16) | (pt.x as u32 & 0xFFFF);
        let hit = SendMessageW(
            hwnd,
            WM_NCHITTEST,
            Some(WPARAM(0)),
            Some(LPARAM(lparam_val as isize)),
        );
        hit.0 == HTCAPTION as isize
    }
}

fn is_resize_border_hit(pt: POINT) -> bool {
    unsafe {
        let hwnd = WindowFromPoint(pt);
        if hwnd.0.is_null() {
            return false;
        }
        let lparam_val = ((pt.y as u32 & 0xFFFF) << 16) | (pt.x as u32 & 0xFFFF);
        let hit = SendMessageW(
            hwnd,
            WM_NCHITTEST,
            Some(WPARAM(0)),
            Some(LPARAM(lparam_val as isize)),
        );
        let code = hit.0 as i32;
        code == HTLEFT as i32
            || code == HTRIGHT as i32
            || code == HTTOP as i32
            || code == HTBOTTOM as i32
            || code == HTTOPLEFT as i32
            || code == HTTOPRIGHT as i32
            || code == HTBOTTOMLEFT as i32
            || code == HTBOTTOMRIGHT as i32
    }
}

fn is_scrollbar_hit(pt: POINT) -> bool {
    unsafe {
        let hwnd = WindowFromPoint(pt);
        if hwnd.0.is_null() {
            return false;
        }
        let lparam_val = ((pt.y as u32 & 0xFFFF) << 16) | (pt.x as u32 & 0xFFFF);
        let hit = SendMessageW(
            hwnd,
            WM_NCHITTEST,
            Some(WPARAM(0)),
            Some(LPARAM(lparam_val as isize)),
        );
        let code = hit.0 as i32;
        code == HTVSCROLL as i32 || code == HTHSCROLL as i32
    }
}

fn is_splitter_drag_by_cursor() -> bool {
    unsafe {
        let mut info = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        if GetCursorInfo(&mut info).is_ok() {
            let hcursor = info.hCursor.0;
            let size_ns = LoadCursorW(None, IDC_SIZENS)
                .map(|c| c.0)
                .unwrap_or_default();
            let size_we = LoadCursorW(None, IDC_SIZEWE)
                .map(|c| c.0)
                .unwrap_or_default();
            let size_nwse = LoadCursorW(None, IDC_SIZENWSE)
                .map(|c| c.0)
                .unwrap_or_default();
            let size_nesw = LoadCursorW(None, IDC_SIZENESW)
                .map(|c| c.0)
                .unwrap_or_default();
            hcursor == size_ns || hcursor == size_we || hcursor == size_nwse || hcursor == size_nesw
        } else {
            false
        }
    }
}

fn get_window_text(hwnd: HWND) -> String {
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; (len as usize) + 1];
    let written = unsafe { GetWindowTextW(hwnd, &mut buf) } as usize;
    String::from_utf16_lossy(&buf[..written])
}

fn is_cherry_window(hwnd: HWND) -> bool {
    let title = get_window_text(hwnd);
    title.to_lowercase().contains("cherry")
}

fn is_client_top_drag_region(pt: POINT) -> bool {
    unsafe {
        let hwnd = WindowFromPoint(pt);
        if hwnd.0.is_null() {
            return false;
        }
        let root = GetAncestor(hwnd, GA_ROOT);
        let target = if root.0.is_null() { hwnd } else { root };
        let mut rect = RECT::default();
        if GetWindowRect(target, &mut rect).is_err() {
            return false;
        }
        let caption = GetSystemMetrics(SM_CYCAPTION);
        let frame = GetSystemMetrics(SM_CYFRAME);
        let padded = GetSystemMetrics(SM_CXPADDEDBORDER);
        let mut top_limit = rect.top + caption + frame + padded;
        if is_cherry_window(target) {
            let cherry_limit = rect.top + CHERRY_WND_TOP_HEIGHT;
            if cherry_limit > top_limit {
                top_limit = cherry_limit;
            }
        }
        pt.y >= rect.top && pt.y <= top_limit
    }
}

pub unsafe extern "system" fn mouse_hook_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // 仅处理钩子链中的有效消息
    if code >= 0 {
        let msg = wparam.0 as u32;

        match msg {
            WM_LBUTTONDOWN => {
                // 记录按下起点与时间
                let hook_struct = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
                if is_title_bar_hit(hook_struct.pt)
                    || is_client_top_drag_region(hook_struct.pt)
                    || is_resize_border_hit(hook_struct.pt)
                    || is_scrollbar_hit(hook_struct.pt)
                    || is_splitter_drag_by_cursor()
                {
                    NONCLIENT_MOUSE_DOWN.store(1, Ordering::SeqCst);
                    MOUSE_DOWN_TIME.store(0, Ordering::SeqCst);
                    return unsafe { CallNextHookEx(None, code, wparam, lparam) };
                }
                let x = hook_struct.pt.x;
                let y = hook_struct.pt.y;
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                MOUSE_DOWN_TIME.store(now, Ordering::SeqCst);
                MOUSE_DOWN_X.store(x, Ordering::SeqCst);
                MOUSE_DOWN_Y.store(y, Ordering::SeqCst);
                MOUSE_LAST_X.store(x, Ordering::SeqCst);
                MOUSE_LAST_Y.store(y, Ordering::SeqCst);
            }
            WM_MOUSEMOVE => {
                // 按下期间更新最后位置
                if MOUSE_DOWN_TIME.load(Ordering::SeqCst) != 0 {
                    let hook_struct = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
                    MOUSE_LAST_X.store(hook_struct.pt.x, Ordering::SeqCst);
                    MOUSE_LAST_Y.store(hook_struct.pt.y, Ordering::SeqCst);
                }
            }
            WM_LBUTTONUP => {
                if NONCLIENT_MOUSE_DOWN.swap(0, Ordering::SeqCst) != 0 {
                    MOUSE_DOWN_TIME.store(0, Ordering::SeqCst);
                    return unsafe { CallNextHookEx(None, code, wparam, lparam) };
                }
                // 释放时计算拖拽与连击
                let hook_struct = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
                let up_x = hook_struct.pt.x;
                let up_y = hook_struct.pt.y;
                MOUSE_LAST_X.store(up_x, Ordering::SeqCst);
                MOUSE_LAST_Y.store(up_y, Ordering::SeqCst);

                let start_time = MOUSE_DOWN_TIME.swap(0, Ordering::SeqCst);
                if start_time > 0 {
                    // 判断是否为拖拽选中
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    let duration = now.saturating_sub(start_time);

                    let down_x = MOUSE_DOWN_X.load(Ordering::SeqCst) as i64;
                    let down_y = MOUSE_DOWN_Y.load(Ordering::SeqCst) as i64;
                    let last_x = MOUSE_LAST_X.load(Ordering::SeqCst) as i64;
                    let last_y = MOUSE_LAST_Y.load(Ordering::SeqCst) as i64;
                    let dx = last_x - down_x;
                    let dy = last_y - down_y;
                    let moved_enough = dx * dx + dy * dy
                        >= (DRAG_THRESHOLD_PX as i64) * (DRAG_THRESHOLD_PX as i64);

                    if duration >= SELECTION_THRESHOLD_MS
                        && moved_enough
                        && let Some(tx) = WORKER_TX.get()
                    {
                        let _ = tx.send(TriggerEvent::Drag(duration));
                    }

                    if moved_enough {
                        // 拖拽视为一次独立选择，重置连击计数
                        LAST_CLICK_UP_TIME.store(0, Ordering::SeqCst);
                        CLICK_UP_COUNT.store(0, Ordering::SeqCst);
                    } else if duration < SELECTION_THRESHOLD_MS {
                        // 仅短按才参与双击/三击判定
                        let max_ms = unsafe { GetDoubleClickTime() } as u64;
                        let cx = unsafe { GetSystemMetrics(SM_CXDOUBLECLK) };
                        let cy = unsafe { GetSystemMetrics(SM_CYDOUBLECLK) };
                        let half_cx = (cx / 2).max(1);
                        let half_cy = (cy / 2).max(1);

                        let last_time = LAST_CLICK_UP_TIME.load(Ordering::SeqCst);
                        let last_x = LAST_CLICK_UP_X.load(Ordering::SeqCst);
                        let last_y = LAST_CLICK_UP_Y.load(Ordering::SeqCst);
                        let last_count = CLICK_UP_COUNT.load(Ordering::SeqCst);

                        let within_time = last_time != 0 && now.saturating_sub(last_time) <= max_ms;
                        let within_rect =
                            (up_x - last_x).abs() <= half_cx && (up_y - last_y).abs() <= half_cy;

                        let new_count = if within_time && within_rect {
                            last_count.saturating_add(1)
                        } else {
                            1
                        };

                        LAST_CLICK_UP_TIME.store(now, Ordering::SeqCst);
                        LAST_CLICK_UP_X.store(up_x, Ordering::SeqCst);
                        LAST_CLICK_UP_Y.store(up_y, Ordering::SeqCst);
                        CLICK_UP_COUNT.store(new_count, Ordering::SeqCst);

                        if new_count >= 2 {
                            schedule_click_sequence(new_count, now, max_ms);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let hook = HOOK_HANDLE.load(Ordering::SeqCst);
    let hook = if hook.is_null() {
        None
    } else {
        Some(HHOOK(hook))
    };
    unsafe { CallNextHookEx(hook, code, wparam, lparam) }
}
