use std::fmt;
use std::sync::atomic::{AtomicI32, AtomicPtr, AtomicU32, AtomicU64};
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::Instant;

use chrono::Local;

use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi_derive::napi;

// 鼠标按下的时间
pub static MOUSE_DOWN_TIME: AtomicU64 = AtomicU64::new(0);
// 起始位置
pub static MOUSE_DOWN_X: AtomicI32 = AtomicI32::new(0);
pub static MOUSE_DOWN_Y: AtomicI32 = AtomicI32::new(0);
// 鼠标的实时位置
pub static MOUSE_LAST_X: AtomicI32 = AtomicI32::new(0);
pub static MOUSE_LAST_Y: AtomicI32 = AtomicI32::new(0);

pub static LAST_CLICK_UP_TIME: AtomicU64 = AtomicU64::new(0);
pub static LAST_CLICK_UP_X: AtomicI32 = AtomicI32::new(0);
pub static LAST_CLICK_UP_Y: AtomicI32 = AtomicI32::new(0);
pub static CLICK_UP_COUNT: AtomicU32 = AtomicU32::new(0);
pub static NONCLIENT_MOUSE_DOWN: AtomicU32 = AtomicU32::new(0);
// 钩子句柄
pub static HOOK_HANDLE: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
// 线程安全函数发送通道
pub static WORKER_TX: OnceLock<mpsc::Sender<TriggerEvent>> = OnceLock::new();

pub static SOME_EVENT: OnceLock<Mutex<(String, Instant)>> = OnceLock::new();

pub static GLOBAL_REPORT: OnceLock<ThreadsafeFunction<SelectionInfo>> = OnceLock::new();
pub static GLOBAL_LOG: OnceLock<ThreadsafeFunction<String>> = OnceLock::new();

// 用于记录后台监控线程的 ID
pub static MONITOR_THREAD_ID: AtomicU32 = AtomicU32::new(0);

pub const SELECTION_THRESHOLD_MS: u64 = 200;
pub const DRAG_THRESHOLD_PX: i32 = 4;
pub const CF_UNICODETEXT_U32: u32 = 13;

pub enum TriggerEvent {
    Drag(u64),
    ClickSequence(u32),
}

#[napi(object)]
#[derive(Clone)]
pub struct SelectionInfo {
    pub content: String,
    pub time: String,
    pub last_mouse_x: i32,
    pub last_mouse_y: i32,
    pub selection_left: i32,
    pub selection_top: i32,
}

pub fn report_func(info: SelectionInfo) {
    if let Some(tsfn) = GLOBAL_REPORT.get() {
        tsfn.call(Ok(info), ThreadsafeFunctionCallMode::NonBlocking);
    } else {
        println!("Warning: No report wnd listener registered yet!");
    }
}

fn report_log(msg: String) {
    if cfg!(debug_assertions) {
        println!("{}", msg);
    } else if let Some(tsfn) = GLOBAL_LOG.get() {
        tsfn.call(Ok(msg), ThreadsafeFunctionCallMode::NonBlocking);
    } else {
        println!("Warning: No report log listener registered yet!");
    }
}

#[doc(hidden)]
pub(crate) fn report_error(msg: fmt::Arguments, file: &'static str, line: u32, column: u32) {
    let curr_time = get_current_time();
    let log_msg = format!(
        "[selection_error]:{} - {}:{}:{} - {}",
        curr_time, file, line, column, msg
    );
    report_log(log_msg);
}

#[doc(hidden)]
pub(crate) fn report_info(msg: fmt::Arguments, file: &'static str, line: u32, column: u32) {
    let curr_time = get_current_time();
    let log_msg = format!(
        "[selection_info]:{} - {}:{}:{} - {}",
        curr_time, file, line, column, msg
    );
    report_log(log_msg);
}

#[macro_export]
macro_rules! report_error_log {
    ($($arg:tt)*) => {
        $crate::global::report_error(
            format_args!($($arg)*),
            file!(),
            line!(),
            column!(),
        )
    }
}

#[macro_export]
macro_rules! report_info_log {
    ($($arg:tt)*) => {
        $crate::global::report_info(
            format_args!($($arg)*),
            file!(),
            line!(),
            column!(),
        )
    }
}

pub fn get_current_time() -> String {
    Local::now().format("%Y-%m-%d %H:%M:%S.%3f").to_string()
}
