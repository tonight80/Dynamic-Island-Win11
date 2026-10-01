//! Tray icon with the app menu (the island itself has no taskbar button).

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

#[derive(Debug, Clone, Copy)]
pub enum Action {
    OpenConfig,
    ReloadConfig,
    ClearLyricsCache,
    ToggleAutostart,
    Quit,
}

/// Draws a small black pill with a white dot — no icon files needed.
fn icon() -> Icon {
    const S: u32 = 32;
    let mut rgba = vec![0u8; (S * S * 4) as usize];
    let (x0, x1, y0, y1, r) = (2.0f32, 30.0f32, 9.0f32, 23.0f32, 7.0f32);
    for y in 0..S {
        for x in 0..S {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            // Signed distance to a rounded rectangle, used for anti-aliasing.
            let cx = px.clamp(x0 + r, x1 - r);
            let cy = py.clamp(y0 + r, y1 - r);
            let d = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt() - r;
            let a = (0.5 - d).clamp(0.0, 1.0);
            let dot = (((px - 23.0).powi(2) + (py - 16.0).powi(2)).sqrt() - 3.0).clamp(-0.5, 0.5);
            let white = (0.5 - dot) * a;
            let i = ((y * S + x) * 4) as usize;
            let v = (white * 255.0) as u8;
            // Light outline so the pill is visible on a dark taskbar.
            let edge = if (-1.2..0.0).contains(&d) { 90u8 } else { 0 };
            let c = v.max(edge);
            rgba[i..i + 4].copy_from_slice(&[c, c, c, (a * 255.0) as u8]);
        }
    }
    Icon::from_rgba(rgba, S, S).expect("valid icon")
}

pub struct Tray {
    _icon: TrayIcon,
    pub autostart: CheckMenuItem,
}

pub fn create(autostart_on: bool, on_action: impl Fn(Action) + Send + Sync + 'static) -> Tray {
    let open = MenuItem::new("Открыть настройки", true, None);
    let reload = MenuItem::new("Применить настройки", true, None);
    let clear = MenuItem::new("Очистить кэш текстов", true, None);
    let autostart = CheckMenuItem::new("Запускать вместе с Windows", true, autostart_on, None);
    let quit = MenuItem::new("Выход", true, None);

    let menu = Menu::new();
    let _ = menu.append_items(&[
        &open,
        &reload,
        &clear,
        &PredefinedMenuItem::separator(),
        &autostart,
        &PredefinedMenuItem::separator(),
        &quit,
    ]);

    let ids = [
        (open.id().clone(), Action::OpenConfig),
        (reload.id().clone(), Action::ReloadConfig),
        (clear.id().clone(), Action::ClearLyricsCache),
        (autostart.id().clone(), Action::ToggleAutostart),
        (quit.id().clone(), Action::Quit),
    ];
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        if let Some((_, action)) = ids.iter().find(|(id, _)| *id == e.id) {
            on_action(*action);
        }
    }));

    let icon = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Dynamic Island")
        .with_icon(icon())
        .build()
        .expect("create tray icon");

    Tray { _icon: icon, autostart }
}
