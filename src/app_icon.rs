//! Keeps the window, taskbar and tray icons showing today's date.
//!
//! On Windows, winit's `set_window_icon` (behind `ViewportCommand::Icon`) only
//! sets `ICON_SMALL`, so the taskbar would keep the startup icon. We set both
//! `ICON_BIG` and `ICON_SMALL` ourselves at the sizes Windows wants for the
//! window's DPI, and also show a tray icon that opens the calendar.

use chrono::NaiveDate;
use eframe::egui;

#[cfg(not(windows))]
use crate::date_icon;

pub struct AppIcon {
    shown: Option<NaiveDate>,
    #[cfg(windows)]
    win: windows::State,
}

impl AppIcon {
    pub fn new(ctx: &egui::Context) -> Self {
        #[cfg(not(windows))]
        let _ = ctx;
        AppIcon {
            shown: None,
            #[cfg(windows)]
            win: windows::State::new(ctx),
        }
    }

    /// Call every frame: redraws the icons when the date changes and handles
    /// tray clicks.
    pub fn update(&mut self, ctx: &egui::Context, frame: &eframe::Frame, today: NaiveDate) {
        #[cfg(windows)]
        {
            self.win.handle_tray_events(ctx);
            if self.shown != Some(today) && self.win.set_icons(frame, today) {
                self.shown = Some(today);
            }
        }
        #[cfg(not(windows))]
        {
            let _ = frame;
            if self.shown != Some(today) {
                let icon = std::sync::Arc::new(date_icon::icon_data(today, 128));
                ctx.send_viewport_cmd(egui::ViewportCommand::Icon(Some(icon)));
                self.shown = Some(today);
            }
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::sync::mpsc::{self, Receiver};

    use chrono::NaiveDate;
    use eframe::egui;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
    use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow, GetSystemMetricsForDpi};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateIcon, DestroyIcon, SendMessageW, HICON, ICON_BIG, ICON_SMALL, SM_CXICON, SM_CXSMICON, WM_SETICON,
    };

    use crate::date_icon;

    enum TrayAction {
        Show,
        Quit,
    }

    pub struct State {
        /// Icons we created and handed to the window; destroyed on replace.
        big: HICON,
        small: HICON,
        tray: Option<Tray>,
    }

    struct Tray {
        icon: TrayIcon,
        actions: Receiver<TrayAction>,
    }

    impl State {
        pub fn new(ctx: &egui::Context) -> Self {
            let tray = Tray::new(ctx).map_err(|e| eprintln!("tray icon unavailable: {e}")).ok();
            State { big: std::ptr::null_mut(), small: std::ptr::null_mut(), tray }
        }

        /// Sets the window/taskbar and tray icons for `date`. Returns false
        /// if the window isn't available yet, so the caller retries.
        pub fn set_icons(&mut self, frame: &eframe::Frame, date: NaiveDate) -> bool {
            let Ok(handle) = frame.window_handle() else { return false };
            let RawWindowHandle::Win32(h) = handle.as_raw() else { return false };
            let hwnd = h.hwnd.get() as HWND;

            // SAFETY: plain Win32 calls on our own live window; icons we
            // replace were created by us and are no longer referenced.
            unsafe {
                let dpi = GetDpiForWindow(hwnd).max(96);
                let big = make_hicon(date, GetSystemMetricsForDpi(SM_CXICON, dpi) as u32);
                let small = make_hicon(date, GetSystemMetricsForDpi(SM_CXSMICON, dpi) as u32);
                if big.is_null() || small.is_null() {
                    return false;
                }
                SendMessageW(hwnd, WM_SETICON, ICON_BIG as usize, big as isize);
                SendMessageW(hwnd, WM_SETICON, ICON_SMALL as usize, small as isize);
                for old in [std::mem::replace(&mut self.big, big), std::mem::replace(&mut self.small, small)] {
                    if !old.is_null() {
                        DestroyIcon(old);
                    }
                }
            }

            if let Some(tray) = &self.tray {
                tray.set_date(date);
            }
            true
        }

        pub fn handle_tray_events(&mut self, ctx: &egui::Context) {
            let Some(tray) = &self.tray else { return };
            while let Ok(action) = tray.actions.try_recv() {
                match action {
                    TrayAction::Show => {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    }
                    TrayAction::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                }
            }
        }
    }

    impl Tray {
        fn new(ctx: &egui::Context) -> anyhow::Result<Self> {
            let open = MenuItem::new("Open Calendar", true, None);
            let quit = MenuItem::new("Quit Calendar", true, None);
            let menu = Menu::with_items(&[&open, &PredefinedMenuItem::separator(), &quit])?;
            let (open_id, quit_id): (MenuId, MenuId) = (open.id().clone(), quit.id().clone());

            let (tx, actions) = mpsc::channel();
            let (tx_menu, ctx_menu) = (tx.clone(), ctx.clone());
            MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
                let action = if e.id == open_id {
                    TrayAction::Show
                } else if e.id == quit_id {
                    TrayAction::Quit
                } else {
                    return;
                };
                let _ = tx_menu.send(action);
                ctx_menu.request_repaint();
            }));
            let ctx_tray = ctx.clone();
            TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
                if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                    let _ = tx.send(TrayAction::Show);
                    ctx_tray.request_repaint();
                }
            }));

            let today = chrono::Local::now().date_naive();
            let icon = TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_menu_on_left_click(false)
                .with_icon(tray_image(today)?)
                .with_tooltip(tooltip(today))
                .build()?;
            Ok(Tray { icon, actions })
        }

        fn set_date(&self, date: NaiveDate) {
            if let Ok(image) = tray_image(date) {
                let _ = self.icon.set_icon(Some(image));
            }
            let _ = self.icon.set_tooltip(Some(tooltip(date)));
        }
    }

    fn tray_image(date: NaiveDate) -> anyhow::Result<tray_icon::Icon> {
        // The notification area uses small icons at the system DPI.
        let size = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForSystem().max(96)) }.max(16) as u32;
        tray_icon::Icon::from_rgba(date_icon::render(date, size), size, size).map_err(anyhow::Error::from)
    }

    fn tooltip(date: NaiveDate) -> String {
        date.format("%A, %B %-d, %Y").to_string()
    }

    /// Builds a 32-bit alpha HICON from our RGBA rendering.
    unsafe fn make_hicon(date: NaiveDate, size: u32) -> HICON {
        let size = size.max(16);
        let rgba = date_icon::render(date, size);
        // Win32 wants BGRA; the AND mask is ignored for 32-bit icons but must
        // be present (1 bpp, rows padded to 16 bits).
        let bgra: Vec<u8> = rgba.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], p[3]]).collect();
        let mask = vec![0u8; size.div_ceil(16) as usize * 2 * size as usize];
        CreateIcon(std::ptr::null_mut(), size as i32, size as i32, 1, 32, mask.as_ptr(), bgra.as_ptr())
    }
}
