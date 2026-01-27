#![deny(clippy::all)]
mod global;

use napi_derive::napi;
use napi::threadsafe_function::{ThreadsafeFunction};
use napi::{ Env, Status };

use std::sync::atomic::Ordering;
use std::ffi::c_void;
use std::sync::{Mutex};
use std::time::{Instant, Duration};

use windows::{
    Win32::Foundation::{WPARAM, LPARAM},
    Win32::System::Threading::GetCurrentThreadId,
    Win32::UI::WindowsAndMessaging::{ 
        PostThreadMessageW, WM_QUIT
    },
};

use crate::global::{ARGS, GLOBAL_LOG, GLOBAL_REPORT, MONITOR_THREAD_ID, SOME_EVENT, SomeInfo, report_func};

// 【新增】定义清理回调函数
// 这个函数会在 Node.js 环境销毁（Electron 退出）时自动执行
unsafe extern "C" fn cleanup_monitor_thread(_arg: *mut c_void) {
    let thread_id = MONITOR_THREAD_ID.load(Ordering::SeqCst);
    if thread_id != 0 {
        // 向后台线程发送 WM_QUIT，打破它的死循环
        let _ = PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        println!("Cleanup hook triggered: Sent WM_QUIT to monitor thread.");
    }
}

#[napi]
pub fn do_initialize(args: u32, mut report: ThreadsafeFunction<Vec<SomeInfo>>, mut log: ThreadsafeFunction<String>, env: Env) -> napi::Result<()> {
    #[allow(deprecated)]
    report.unref(&env)?;
    #[allow(deprecated)]
    log.unref(&env)?;
    
    GLOBAL_REPORT.set(report).map_err(|_| napi::Error::new(Status::GenericFailure, "Global report listener already registered"))?;
    GLOBAL_LOG.set(log).map_err(|_| napi::Error::new(Status::GenericFailure, "Global log listener already registered"))?;

    SOME_EVENT.get_or_init(|| Mutex::new((String::from("Ready"), Instant::now() - Duration::from_secs(100))));

    ARGS.store(args, Ordering::SeqCst);

    if cfg!(debug_assertions) {
        report_info_log!("[Debug] 当前正处于开发模式运行，开启详细日志...");
    } else {
        report_info_log!("[Release] 生产模式运行");
    }
    report_error_log!("error occured.");

    env.add_env_cleanup_hook(
        std::ptr::null_mut(), 
        |arg| unsafe { cleanup_monitor_thread(arg) }
    )?;

    let thread_id = unsafe {
        GetCurrentThreadId()    
    };
    MONITOR_THREAD_ID.store(thread_id, Ordering::SeqCst);

    report_func(vec![SomeInfo {
        pname: "".to_string(),
        pid: 0,
        title: "".to_string()
    }]);

    Ok(())
}
