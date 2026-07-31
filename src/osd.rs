//! A small on-screen readout of the last action, on the monitor holding the
//! cursor. Modelled on Raycast's HUD: a dark pill near the bottom that shows
//! up, sits there briefly, and fades out without ever taking focus.
//!
//! Runs its own window and message loop on a dedicated thread. A window's
//! messages have to be pumped by the thread that created it, and the hotkey
//! loop must never block, so the two cannot share one.

use std::time::Duration;

/// Time the fade takes once the hold has elapsed. Callers that need the popup
/// to outlive them (`send`) have to wait this out on top of the hold.
pub const FADE_TAIL: Duration = Duration::from_millis(200);

#[cfg(windows)]
pub use imp::show;

/// Nothing to draw on: the daemon is headless everywhere else.
#[cfg(not(windows))]
pub fn show(_text: &str, _hold: Duration) {}

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
    use std::sync::mpsc;
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;
    use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
    use windows_sys::Win32::Graphics::Gdi::{
        BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect,
        GetDC, GetMonitorInfoW, InvalidateRect, MonitorFromPoint, ReleaseDC, SelectObject,
        SetBkMode, SetTextColor, DT_CALCRECT, DT_CENTER, DT_SINGLELINE, DT_VCENTER, FW_SEMIBOLD,
        HFONT, MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, TRANSPARENT,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::HiDpi::{
        GetDpiForMonitor, SetProcessDpiAwarenessContext,
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, MDT_EFFECTIVE_DPI,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetCursorPos,
        GetMessageW, KillTimer, PostMessageW, RegisterClassW, SetLayeredWindowAttributes, SetTimer,
        SetWindowPos, ShowWindow, TranslateMessage, HWND_TOPMOST, LWA_ALPHA, MSG, SWP_NOACTIVATE,
        SWP_SHOWWINDOW, SW_HIDE, SW_SHOWNA, WM_APP, WM_PAINT, WM_TIMER, WNDCLASSW, WS_EX_LAYERED,
        WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
    };

    const WM_OSD_SHOW: u32 = WM_APP + 1;
    const HIDE_TIMER: usize = 1;
    const FADE_TIMER: usize = 2;

    /// Roughly one frame at 60Hz, for ~8 steps of fade.
    const FADE_INTERVAL_MS: u32 = 16;
    const FADE_STEP: u8 = 30;

    /// Design sizes at 96 DPI; everything is scaled to the monitor's DPI.
    const FONT_PX: i32 = 13;
    const PAD_X: i32 = 14;
    const PAD_Y: i32 = 8;

    /// Distance from the bottom of the monitor, as a percentage of its height.
    const BOTTOM_OFFSET_PCT: i32 = 15;

    const BG: COLORREF = 0x0020_2020; // 0x00BBGGRR
    const FG: COLORREF = 0x00FF_FFFF;
    const ALPHA: u8 = 235;

    static TEXT: Mutex<String> = Mutex::new(String::new());
    static HOLD_MS: AtomicU32 = AtomicU32::new(0);
    static FADE_ALPHA: AtomicU8 = AtomicU8::new(ALPHA);

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Replace whatever is on screen. Volume arrives in bursts while a key is
    /// held, so this deliberately overwrites rather than queues: the newest
    /// reading wins, the popup never stacks, and the hold restarts each time.
    pub fn show(text: &str, hold: Duration) {
        let Some(hwnd) = window() else { return };
        if let Ok(mut current) = TEXT.lock() {
            current.clear();
            current.push_str(text);
        }
        HOLD_MS.store(hold.as_millis() as u32, Ordering::Relaxed);
        // The window thread owns every GDI call; this only nudges it.
        unsafe { PostMessageW(hwnd, WM_OSD_SHOW, 0, 0) };
    }

    /// The popup window, created on first use. `None` if it could not be made,
    /// in which case the daemon simply carries on without visual feedback.
    fn window() -> Option<HWND> {
        static WINDOW: OnceLock<isize> = OnceLock::new();

        let raw = *WINDOW.get_or_init(|| {
            let (tx, rx) = mpsc::channel();
            let spawned = std::thread::Builder::new()
                .name("spotify-osd".into())
                .spawn(move || {
                    let hwnd = create_window();
                    let _ = tx.send(hwnd as isize);
                    if !hwnd.is_null() {
                        pump_messages();
                    }
                });

            if spawned.is_err() {
                return 0;
            }
            rx.recv_timeout(Duration::from_secs(5)).unwrap_or(0)
        });

        (raw != 0).then_some(raw as HWND)
    }

    fn create_window() -> HWND {
        let class = wide("SpotifyControlOsd");

        unsafe {
            // Per-monitor aware so the popup is sharp on scaled displays. If
            // this fails the process stays DPI-unaware, GetDpiForMonitor then
            // reports 96, and the scaling below becomes a no-op — consistent
            // either way.
            SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

            let instance = GetModuleHandleW(std::ptr::null());

            let mut wc: WNDCLASSW = std::mem::zeroed();
            wc.lpfnWndProc = Some(wndproc);
            wc.hInstance = instance;
            wc.lpszClassName = class.as_ptr();
            RegisterClassW(&wc);

            let hwnd = CreateWindowExW(
                // Layered for opacity, transparent so clicks pass straight
                // through, no-activate so it never steals focus mid-keypress,
                // toolwindow to stay out of Alt+Tab.
                WS_EX_LAYERED
                    | WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE
                    | WS_EX_TRANSPARENT,
                class.as_ptr(),
                std::ptr::null(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                std::ptr::null(),
            );

            if !hwnd.is_null() {
                SetLayeredWindowAttributes(hwnd, 0, ALPHA, LWA_ALPHA);
                round_corners(hwnd);
            }
            hwnd
        }
    }

    /// Windows 11 rounds the corners for us; older builds ignore this.
    fn round_corners(hwnd: HWND) {
        const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
        const DWMWCP_ROUND: u32 = 2;
        unsafe {
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                (&DWMWCP_ROUND as *const u32).cast(),
                std::mem::size_of::<u32>() as u32,
            );
        }
    }

    fn pump_messages() {
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_OSD_SHOW => {
                place_and_show(hwnd);
                0
            }
            WM_TIMER if wparam == HIDE_TIMER => {
                unsafe {
                    KillTimer(hwnd, HIDE_TIMER);
                    SetTimer(hwnd, FADE_TIMER, FADE_INTERVAL_MS, None);
                }
                0
            }
            WM_TIMER if wparam == FADE_TIMER => {
                fade_step(hwnd);
                0
            }
            WM_PAINT => {
                paint(hwnd);
                0
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    fn fade_step(hwnd: HWND) {
        let next = FADE_ALPHA.load(Ordering::Relaxed).saturating_sub(FADE_STEP);
        FADE_ALPHA.store(next, Ordering::Relaxed);

        unsafe {
            if next == 0 {
                KillTimer(hwnd, FADE_TIMER);
                ShowWindow(hwnd, SW_HIDE);
                // Back to full for the next popup, while hidden.
                FADE_ALPHA.store(ALPHA, Ordering::Relaxed);
                SetLayeredWindowAttributes(hwnd, 0, ALPHA, LWA_ALPHA);
            } else {
                SetLayeredWindowAttributes(hwnd, 0, next, LWA_ALPHA);
            }
        }
    }

    /// DPI of the monitor under the cursor, and that monitor's bounds.
    fn cursor_monitor() -> (u32, RECT) {
        unsafe {
            let mut point = POINT { x: 0, y: 0 };
            GetCursorPos(&mut point);
            let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);

            let mut info: MONITORINFO = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            GetMonitorInfoW(monitor, &mut info);

            let (mut dpi_x, mut dpi_y) = (96, 96);
            GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);

            (dpi_x.max(96), info.rcMonitor)
        }
    }

    fn scaled(value: i32, dpi: u32) -> i32 {
        value * dpi as i32 / 96
    }

    fn make_font(dpi: u32) -> HFONT {
        let face = wide("Segoe UI");
        unsafe {
            CreateFontW(
                -scaled(FONT_PX, dpi),
                0,
                0,
                0,
                FW_SEMIBOLD as i32,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                face.as_ptr(),
            )
        }
    }

    fn current_text() -> Vec<u16> {
        TEXT.lock()
            .map(|text| wide(&text))
            .unwrap_or_else(|_| wide(""))
    }

    fn place_and_show(hwnd: HWND) {
        let mut label = current_text();
        let (dpi, monitor) = cursor_monitor();

        unsafe {
            // Cancel any fade in flight and go back to full opacity, so a burst
            // of volume presses reads as one steady popup rather than a flicker.
            KillTimer(hwnd, FADE_TIMER);
            KillTimer(hwnd, HIDE_TIMER);
            if FADE_ALPHA.swap(ALPHA, Ordering::Relaxed) != ALPHA {
                SetLayeredWindowAttributes(hwnd, 0, ALPHA, LWA_ALPHA);
            }

            // Measure first, so the popup is only as wide as it needs to be.
            let dc = GetDC(hwnd);
            let font = make_font(dpi);
            let previous = SelectObject(dc, font);
            let mut bounds = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            DrawTextW(
                dc,
                label.as_mut_ptr(),
                -1,
                &mut bounds,
                DT_CALCRECT | DT_SINGLELINE,
            );
            SelectObject(dc, previous);
            DeleteObject(font);
            ReleaseDC(hwnd, dc);

            let width = bounds.right - bounds.left + 2 * scaled(PAD_X, dpi);
            let height = bounds.bottom - bounds.top + 2 * scaled(PAD_Y, dpi);

            let monitor_width = monitor.right - monitor.left;
            let monitor_height = monitor.bottom - monitor.top;
            let x = monitor.left + (monitor_width - width) / 2;
            let y = monitor.bottom - monitor_height * BOTTOM_OFFSET_PCT / 100 - height;

            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                x,
                y,
                width,
                height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            ShowWindow(hwnd, SW_SHOWNA);
            InvalidateRect(hwnd, std::ptr::null(), 1);
            SetTimer(hwnd, HIDE_TIMER, HOLD_MS.load(Ordering::Relaxed), None);
        }
    }

    fn paint(hwnd: HWND) {
        let mut label = current_text();
        let (dpi, _) = cursor_monitor();

        unsafe {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let dc = BeginPaint(hwnd, &mut ps);

            let brush = CreateSolidBrush(BG);
            FillRect(dc, &ps.rcPaint, brush);
            DeleteObject(brush);

            let font = make_font(dpi);
            let previous = SelectObject(dc, font);
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, FG);

            let mut client = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            GetClientRect(hwnd, &mut client);
            DrawTextW(
                dc,
                label.as_mut_ptr(),
                -1,
                &mut client,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );

            SelectObject(dc, previous);
            DeleteObject(font);
            EndPaint(hwnd, &ps);
        }
    }
}
