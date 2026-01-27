#![deny(clippy::all)]
mod clipboard;
mod global;
mod hook;
mod uia;
mod worker;

use napi::threadsafe_function::ThreadsafeFunction;
use napi::{Env, Status};
use napi_derive::napi;

use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use windows::{
    Win32::Foundation::{HINSTANCE, LPARAM, WPARAM},
    Win32::System::LibraryLoader::GetModuleHandleW,
    Win32::System::Threading::GetCurrentThreadId,
    Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, MSG, PostThreadMessageW, SetWindowsHookExW,
        TranslateMessage, UnhookWindowsHookEx, WH_MOUSE_LL, WM_QUIT,
    },
};

use crate::global::{
    GLOBAL_LOG, GLOBAL_REPORT, HOOK_HANDLE, MONITOR_THREAD_ID, SOME_EVENT, SelectionInfo,
    TriggerEvent, WORKER_TX,
};
use crate::hook::mouse_hook_proc;
use crate::worker::worker_loop;

// 【新增】定义清理回调函数
// 这个函数会在 Node.js 环境销毁（Electron 退出）时自动执行
unsafe extern "C" fn cleanup_monitor_thread(_arg: *mut c_void) {
    let thread_id = MONITOR_THREAD_ID.load(Ordering::SeqCst);
    if thread_id != 0 {
        // 向后台线程发送 WM_QUIT，打破它的死循环
        let _ = unsafe { PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        println!("Cleanup hook triggered: Sent WM_QUIT to monitor thread.");
    }
}

#[napi]
pub fn do_initialize(
    mut report: ThreadsafeFunction<SelectionInfo>,
    mut log: ThreadsafeFunction<String>,
    env: Env,
) -> napi::Result<()> {
    #[allow(deprecated)]
    report.unref(&env)?;
    #[allow(deprecated)]
    log.unref(&env)?;

    GLOBAL_REPORT.set(report).map_err(|_| {
        napi::Error::new(
            Status::GenericFailure,
            "Global report listener already registered",
        )
    })?;
    GLOBAL_LOG.set(log).map_err(|_| {
        napi::Error::new(
            Status::GenericFailure,
            "Global log listener already registered",
        )
    })?;

    SOME_EVENT.get_or_init(|| {
        Mutex::new((
            String::from("Ready"),
            Instant::now() - Duration::from_secs(100),
        ))
    });

    if cfg!(debug_assertions) {
        report_info_log!("[Debug] 当前正处于开发模式运行，开启详细日志...");
    } else {
        report_info_log!("[Release] 生产模式运行");
    }

    env.add_env_cleanup_hook(std::ptr::null_mut(), |arg| unsafe {
        cleanup_monitor_thread(arg)
    })?;

    let (tx, rx) = mpsc::channel::<TriggerEvent>();
    let _ = WORKER_TX.set(tx);
    let _ = thread::Builder::new()
        .name("uia-worker".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || worker_loop(rx));

    thread::spawn(move || {
        let thread_id = unsafe { GetCurrentThreadId() };
        MONITOR_THREAD_ID.store(thread_id, Ordering::SeqCst);

        let result = (|| -> windows::core::Result<()> {
            unsafe {
                // 1. 设置全局鼠标钩子
                let instance = GetModuleHandleW(None)?;
                let instance_handle = HINSTANCE(instance.0);
                let hook_id = SetWindowsHookExW(
                    WH_MOUSE_LL,
                    Some(mouse_hook_proc),
                    Some(instance_handle),
                    0,
                )?;

                if hook_id.is_invalid() {
                    report_error_log!("无法安装鼠标钩子！");
                    return Ok(());
                }
                HOOK_HANDLE.store(hook_id.0, Ordering::SeqCst);

                report_info_log!("系统监控已启动...");
                report_info_log!("请尝试：");
                report_info_log!("- 按住鼠标左键 -> 拖拽选中文字 -> 松开鼠标");
                report_info_log!("- 鼠标左键双击选中文字");
                report_info_log!("- 鼠标左键三击选中一行/段落文字");

                // 2. 开启 Windows 消息循环 (必须，否则钩子不生效)
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).into() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }

                // 退出前卸载钩子
                HOOK_HANDLE.store(std::ptr::null_mut(), Ordering::SeqCst);
                let _ = UnhookWindowsHookEx(hook_id);
                Ok(())
            }
        })();

        if let Err(err) = result {
            report_error_log!("系统监控线程异常退出: {:?}", err);
        }
    });
    Ok(())
}
