use std::sync::{OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
use windows::Win32::UI::WindowsAndMessaging::{
    CURSORINFO, CallNextHookEx, GetCursorInfo, GetSystemMetrics,
    HHOOK, IDC_IBEAM, LoadCursorW, MSLLHOOKSTRUCT,
    SM_CXDOUBLECLK, SM_CYDOUBLECLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
};

use crate::global::{
    CLICK_UP_COUNT, DRAG_THRESHOLD_PX, HOOK_HANDLE, LAST_CLICK_UP_TIME,
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

/// 判断当前光标是否为文本选择光标 (I-Beam)
/// 
/// 原理解析：
/// Windows 系统中，当鼠标悬停在可进行文本选择的区域（如输入框、文档内容区）时，
/// 光标形状通常会自动切换为 `IDC_IBEAM` (工字型光标)。
/// 通过检测鼠标按下时的光标形状，我们可以精确区分用户的操作意图：
/// - 如果是 `IDC_IBEAM`，则极大概率是进行文本选择操作。
/// - 如果是 `IDC_ARROW` (箭头)、`IDC_SIZE` (调整大小)、`IDC_HAND` (链接) 等其他光标，
///   则通常意味着点击按钮、拖动窗口、调整边框等非文本选择操作。
/// 
/// 这种基于光标形状的“白名单”过滤策略，比传统的“黑名单”排除法（排除标题栏、滚动条等）
/// 更加精准和健壮，能够有效避免在窗口空白区域、工具栏等无关区域拖动时误触发文字检测。
fn is_text_select_cursor() -> bool {
    unsafe {
        let mut info = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        if GetCursorInfo(&mut info).is_ok() {
            let hcursor = info.hCursor.0;
            // 获取系统标准的 I-Beam 光标句柄
            let ibeam = LoadCursorW(None, IDC_IBEAM)
                .map(|c| c.0)
                .unwrap_or_default();
            // 只有当前光标是 I-Beam 时，才认为是有效的文本选择意图
            hcursor == ibeam
        } else {
            // 获取光标信息失败，保守处理，视为非文本选择
            false
        }
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
                
                // 采用精确的光标形态过滤策略：
                // 仅当光标呈现为 I-Beam (文本选择样式) 时，才记录鼠标按下事件并后续触发选区检测。
                // 这样可以完美过滤掉：
                // 1. 窗口标题栏拖动 (Arrow 光标)
                // 2. 滚动条拖动 (Arrow 光标)
                // 3. 窗口边框调整 (Size 光标)
                // 4. 分割线调整 (Size 光标)
                // 5. 窗口内空白区域、按钮、图片等非文本区域的拖动 (Arrow/Hand 光标)
                // 
                // 之前的逻辑是枚举所有“非客户区”进行排除 (is_title_bar_hit || is_scrollbar_hit ...)，
                // 这种“黑名单”方式难以覆盖所有无关区域（如窗口内的空白面板），导致误触发。
                // 现在改为“白名单”方式，只认准 I-Beam 光标，逻辑更收敛、更准确。
                if !is_text_select_cursor() {
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
