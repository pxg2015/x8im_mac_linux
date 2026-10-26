//! 原生 Windows 升级 UI（2026-10-26 老8 诉求）
//!
//! 背景：远程域页面 → 客户端的 IPC 命令通道是**断的**（`invoke` 永久 pending），
//! 前端那个漂亮的 Vue 确认框/进度条在 Windows 上一个都等不到。
//! 所以升级这种「必须让用户看见、必须让用户点」的交互只能在客户端内部用原生 Win32 做：
//!
//! 1. [`confirm_new_version`] —— 系统消息框，问「是否现在下载并安装」；
//! 2. [`NativeProgress`] —— 一个小窗 + 系统进度条，下载时实时跑百分比和 MB。
//!
//! 两者都不依赖任何 IPC，跑在后台调度线程里，各自带消息循环。

mod imp {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{
        CreateFontW, COLOR_BTNFACE, DEFAULT_CHARSET, FW_NORMAL, HFONT,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetSystemMetrics,
        LoadCursorW, MessageBoxW, PeekMessageW, RegisterClassExW, SendMessageW, SetWindowPos,
        ShowWindow,
        TranslateMessage, CS_HREDRAW, CS_VREDRAW, IDYES, MB_ICONQUESTION, MB_SETFOREGROUND,
        MB_TOPMOST, MB_YESNO, MSG, PM_REMOVE, SW_SHOWNOACTIVATE, WM_SETFONT, WNDCLASSEXW,
        WS_CAPTION, WS_CHILD, WS_POPUP, WS_SYSMENU, WS_VISIBLE,
    };

    // 进度条控件消息（comctl32）
    const PBM_SETPOS: u32 = 0x0402;
    // 窗口消息
    const WM_CLOSE_MSG: u32 = 0x0010;
    const SM_CXSCREEN: i32 = 0;
    const SM_CYSCREEN: i32 = 1;
    const WM_CTLCOLORSTATIC: u32 = 0x0138;
    const SWP_NOSIZE: u32 = 0x0001;
    const SWP_NOMOVE: u32 = 0x0002;
    const SWP_NOACTIVATE: u32 = 0x0010;
    const PBT_CLASS: &str = "msctls_progress32";
    const STATIC_CLASS: &str = "STATIC";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0u16)).collect()
    }

    /// 原生「发现新版本，是否现在下载并安装」提示框。
    /// 返回 true = 用户点「是」；false = 点「否」或直接关掉。
    pub fn confirm_new_version(ver: &str, cur: &str) -> bool {
        let text = format!(
            "发现新版本 v{ver}（当前 v{cur}）。\n\n\
             是否现在下载并安装？\n\n\
             · 点「是」：开始下载，窗口里会显示下载进度，下载完自动弹出安装向导\n\
             · 点「否」：稍后再说（6 小时内不再打扰；想立刻升级可点左下角版本号）"
        );
        let caption = "邦信 · 升级提示";
        let (t, c) = (wide(&text), wide(&caption));
        unsafe {
            let r = MessageBoxW(
                std::ptr::null_mut(),
                t.as_ptr(),
                c.as_ptr(),
                MB_YESNO | MB_ICONQUESTION | MB_TOPMOST | MB_SETFOREGROUND,
            );
            r == IDYES
        }
    }

    struct State {
        pct: AtomicU32,
        text: Mutex<String>,
        done: AtomicBool,
        title: Mutex<String>,
    }

    /// 原生下载进度窗。`open` 立即返回（窗口在自己的线程里建），
    /// `update` 由下载线程随时调，`finish` 关窗并等线程退出。
    pub struct NativeProgress {
        st: Arc<State>,
        join: Option<std::thread::JoinHandle<()>>,
    }

    impl NativeProgress {
        pub fn open(title: &str, detail: &str) -> Self {
            let st = Arc::new(State {
                pct: AtomicU32::new(0),
                text: Mutex::new(detail.to_string()),
                done: AtomicBool::new(false),
                title: Mutex::new(title.to_string()),
            });
            let st2 = st.clone();
            let join = std::thread::spawn(move || unsafe { run_window(st2) });
            NativeProgress { st, join: Some(join) }
        }

        pub fn update(&self, pct: u32, detail: &str) {
            self.st.pct.store(pct.min(100), Ordering::Relaxed);
            if let Ok(mut t) = self.st.text.lock() {
                *t = detail.to_string();
            }
        }

        pub fn finish(mut self) {
            self.st.done.store(true, Ordering::SeqCst);
            if let Some(j) = self.join.take() {
                let _ = j.join();
            }
        }
    }

    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: u32,
        wp: WPARAM,
        lp: LPARAM,
    ) -> LRESULT {
        // 点右上角 × ：只隐藏窗口，不中断下载（下载本就在另一线程跑）
        if msg == WM_CLOSE_MSG {
            ShowWindow(hwnd, 0);
            return 0;
        }
        // 文字控件与窗体统一用系统对话框底色（灰），避免文字背后出现一块异色带
        if msg == WM_CTLCOLORSTATIC {
            return (COLOR_BTNFACE + 1) as usize as LRESULT;
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }

    /// 窗口线程主体：建窗 → 消息循环 → 按需刷新 → 收到 done 后销毁。
    unsafe fn run_window(st: Arc<State>) {
        let hinst = GetModuleHandleW(std::ptr::null());
        let class = wide("BangXinUpdProgress");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: std::ptr::null_mut(),
            hCursor: LoadCursorW(std::ptr::null_mut(), 32512 as *const u16), // IDC_ARROW
            hbrBackground: (COLOR_BTNFACE + 1) as usize as *mut c_void,
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
            hIconSm: std::ptr::null_mut(),
        };
        RegisterClassExW(&wc);

        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        let (w, h) = (520i32, 150i32);
        let (x, y) = ((sw - w) / 2, (sh - h) / 3);

        let title = st.title.lock().map(|t| t.clone()).unwrap_or_default();
        let title_w = wide(&title);
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            title_w.as_ptr(),
            WS_POPUP | WS_CAPTION | WS_SYSMENU,
            x,
            y,
            w,
            h,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return;
        }

        let font: HFONT = CreateFontW(
            -16, 0, 0, 0, FW_NORMAL as i32, 0, 0, 0,
            DEFAULT_CHARSET as u32,
            0, 0, 5, // CLEARTYPE_QUALITY
            0,
            wide("Microsoft YaHei UI").as_ptr(),
        );

        let label = CreateWindowExW(
            0,
            wide(STATIC_CLASS).as_ptr(),
            wide(&st.text.lock().map(|t| t.clone()).unwrap_or_default()).as_ptr(),
            WS_CHILD | WS_VISIBLE,
            22,
            16,
            w - 64,
            40,
            hwnd,
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        );
        let bar = CreateWindowExW(
            0,
            wide(PBT_CLASS).as_ptr(),
            std::ptr::null(),
            WS_CHILD | WS_VISIBLE,
            22,
            68,
            w - 64,
            22,
            hwnd,
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        );

        if !font.is_null() {
            SendMessageW(label, WM_SETFONT, font as usize, 1);
            SendMessageW(bar, WM_SETFONT, font as usize, 1);
        }
        // 置顶但**不抢焦点**（用户可能正在别处打字），只让他余光能看到进度
        SetWindowPos(
            hwnd,
            -1isize as HWND,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);

        let mut msg: MSG = std::mem::zeroed();
        let (mut last_pct, mut last_text) = (u32::MAX, String::new());
        loop {
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if st.done.load(Ordering::SeqCst) {
                break;
            }
            let pct = st.pct.load(Ordering::Relaxed);
            if pct != last_pct {
                SendMessageW(bar, PBM_SETPOS, pct as usize, 0);
                last_pct = pct;
            }
            let t = st.text.lock().map(|x| x.clone()).unwrap_or_default();
            if t != last_text {
                let tw = wide(&t);
                windows_sys::Win32::UI::WindowsAndMessaging::SetWindowTextW(label, tw.as_ptr());
                last_text = t;
            }
            std::thread::sleep(std::time::Duration::from_millis(60));
        }

        DestroyWindow(hwnd);
    }
}

pub use imp::{confirm_new_version, NativeProgress};
