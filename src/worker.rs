use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};

use crate::global::{
    MOUSE_DOWN_X, MOUSE_DOWN_Y, MOUSE_LAST_X, MOUSE_LAST_Y, SelectionInfo, TriggerEvent,
    get_current_time, report_func,
};
use crate::report_info_log;
use crate::uia::get_focused_selection_with_fallback_copy;
use std::sync::atomic::Ordering;

pub fn worker_loop(rx: mpsc::Receiver<TriggerEvent>) {
    // 初始化 COM 以便 UIA 调用
    unsafe {
        if CoInitializeEx(None, COINIT_MULTITHREADED).is_err() {
            return;
        }
    }

    while let Ok(event) = rx.recv() {
        // 预留片刻等待系统选择状态稳定
        thread::sleep(Duration::from_millis(50));
        perform_uia_detection(event);
    }

    // 退出前释放 COM 资源
    unsafe {
        CoUninitialize();
    }
}

fn perform_uia_detection(event: TriggerEvent) {
    // 优先 UIA 获取选中文本，失败再回退到复制方案
    if let Ok((text, top_left_initial)) = get_focused_selection_with_fallback_copy()
        && text.is_some()
        && !text.as_ref().unwrap().trim().is_empty()
    {
        let content = text.unwrap_or_default();
        let mut top_left = top_left_initial;
        if top_left.is_none() {
            match event {
                TriggerEvent::Drag(_) => {
                    // 拖拽选中时使用起点/终点构成矩形左上角作为兜底坐标
                    // 该坐标来自鼠标轨迹，适用于 UIA 与控件消息均不可用的窗口
                    let down_x = MOUSE_DOWN_X.load(Ordering::SeqCst);
                    let down_y = MOUSE_DOWN_Y.load(Ordering::SeqCst);
                    let last_x = MOUSE_LAST_X.load(Ordering::SeqCst);
                    let last_y = MOUSE_LAST_Y.load(Ordering::SeqCst);
                    top_left = Some((down_x.min(last_x), down_y.min(last_y)));
                }
                TriggerEvent::ClickSequence(_) => {
                    let last_x = MOUSE_LAST_X.load(Ordering::SeqCst);
                    let last_y = MOUSE_LAST_Y.load(Ordering::SeqCst);
                    top_left = Some((last_x, last_y));
                }
            }
        }
        report_info_log!("--------------------------------------------------");
        match event {
            TriggerEvent::Drag(duration_ms) => {
                report_info_log!("检测到长按/拖拽 ({}ms) 结束，捕获文本:", duration_ms);
            }
            TriggerEvent::ClickSequence(count) => {
                report_info_log!("检测到鼠标连击({}次)选中，捕获文本:", count);
            }
        }
        let curr_time = get_current_time();
        report_info_log!(">>> {} - {}", content, curr_time);
        // 无法获取高亮坐标时，使用 -1 标识
        let (selection_left, selection_top) = top_left.unwrap_or((-1, -1));
        report_func(SelectionInfo {
            content,
            time: curr_time,
            last_mouse_x: MOUSE_LAST_X.load(Ordering::SeqCst),
            last_mouse_y: MOUSE_LAST_Y.load(Ordering::SeqCst),
            selection_left,
            selection_top,
        });
        report_info_log!("--------------------------------------------------");
    }
}
