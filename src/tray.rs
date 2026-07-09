use crate::config::{get_active_llm_name, get_llm_names_cached, is_auto_select};
use std::path::PathBuf;
use tracing::{info, warn};

fn find_icon_path() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        for sub in &["assets/icon.ico", "icon.ico"] {
            let p = exe.parent().unwrap_or(std::path::Path::new(".")).join(sub);
            if p.exists() { return Some(p); }
        }
    }
    for p in &["assets/icon.ico", "icon.ico"] {
        let p = PathBuf::from(p);
        if p.exists() { return Some(p); }
    }
    None
}

fn load_icon() -> tray_icon::Icon {
    if let Some(path) = find_icon_path() {
        match tray_icon::Icon::from_path(&path, None) {
            Ok(icon) => return icon,
            Err(_) => warn!("icon load failed, using fallback"),
        }
    }
    let size = 32u32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let cx = (size / 2) as i32;
    let cy = (size / 2) as i32;
    let r = (size / 2 - 2) as i32;
    for y in 0..size as usize {
        for x in 0..size as usize {
            if (x as i32 - cx).pow(2) + (y as i32 - cy).pow(2) <= r * r {
                let idx = (y * size as usize + x) * 4;
                rgba[idx] = 37; rgba[idx + 1] = 99; rgba[idx + 2] = 235; rgba[idx + 3] = 255;
            }
        }
    }
    let l: [u8; 16] = [
        0b01100000, 0b01100000, 0b01100000, 0b01100000, 0b01100000, 0b01100000, 0b01111110,
        0b01100110, 0b01100110, 0b01100110, 0b01100110, 0b01100110, 0b01100110, 0b01100110,
        0b01100110, 0b01111110,
    ];
    let ox = ((32 - 7) / 2) as usize;
    let oy = ((32 - 16) / 2) as usize;
    for ly in 0..16 {
        let row = l[ly];
        for lx in 0..7 {
            if (row >> (7 - lx)) & 1 == 1 {
                let idx = ((oy + ly) * 32 + ox + lx) * 4;
                rgba[idx] = 255; rgba[idx + 1] = 255; rgba[idx + 2] = 255;
            }
        }
    }
    tray_icon::Icon::from_rgba(rgba, size, size).expect("icon failed")
}

fn build_menu() -> tray_icon::menu::Menu {
    use tray_icon::menu::{Menu, MenuId, MenuItem, PredefinedMenuItem};

    let auto = is_auto_select();
    let active = get_active_llm_name();
    let models = get_llm_names_cached();
    let menu = Menu::new();

    let auto_label = if auto {
        format!("✓ 自动选择 (当前: {})", active)
    } else {
        "  自动选择".into()
    };
    menu.append(&MenuItem::with_id(
        MenuId::new("__auto__".to_string()), auto_label, true, None,
    )).ok();

    menu.append(&PredefinedMenuItem::separator()).ok();

    for model in &models {
        let label = if !auto && model == &active {
            format!("✓ {}", model)
        } else {
            format!("  {}", model)
        };
        menu.append(&MenuItem::with_id(MenuId::new(model.clone()), label, true, None)).ok();
    }

    menu.append(&PredefinedMenuItem::separator()).ok();
    menu.append(&MenuItem::with_id(MenuId::new("__quit__".to_string()), "退出 holoProxy", true, None)).ok();
    menu
}

fn handle_menu_id(id_str: &str, tray: &mut tray_icon::TrayIcon) {
    if id_str == "__quit__" {
        info!("quit");
        std::process::exit(0);
    }

    if id_str == "__auto__" {
        let new_state = !is_auto_select();
        crate::config::switch_auto_select(new_state);
        let _ = tray.set_menu(Some(Box::new(build_menu())));
        let _ = tray.set_tooltip(Some(format!("holoProxy - {}", get_active_llm_name())));
        return;
    }

    let models = get_llm_names_cached();
    if models.contains(&id_str.to_string()) {
        let _ = crate::config::switch_active_llm(id_str);
        let _ = tray.set_menu(Some(Box::new(build_menu())));
        let _ = tray.set_tooltip(Some(format!("holoProxy - {}", get_active_llm_name())));
    }
}

struct TrayApp {
    menu_event_rx: crossbeam_channel::Receiver<tray_icon::menu::MenuEvent>,
    tray: Option<tray_icon::TrayIcon>,
}

impl winit::application::ApplicationHandler for TrayApp {
    fn resumed(&mut self, _event_loop: &winit::event_loop::ActiveEventLoop) {}

    fn window_event(
        &mut self,
        _event_loop: &winit::event_loop::ActiveEventLoop,
        _window_id: winit::window::WindowId,
        _event: winit::event::WindowEvent,
    ) {}

    fn about_to_wait(&mut self, _event_loop: &winit::event_loop::ActiveEventLoop) {
        while let Ok(event) = self.menu_event_rx.try_recv() {
            if let Some(ref mut tray) = self.tray {
                handle_menu_id(&event.id.0, tray);
            }
        }
    }
}

pub fn run_tray() {
    use tray_icon::{menu::MenuEvent, TrayIconBuilder};
    use winit::event_loop::EventLoop;

    let menu_event_rx = MenuEvent::receiver().clone();

    let event_loop = match EventLoop::new() {
        Ok(el) => el,
        Err(e) => { warn!("EventLoop failed: {:?}", e); return; }
    };

    let tray = match TrayIconBuilder::new()
        .with_menu(Box::new(build_menu()))
        .with_icon(load_icon())
        .with_tooltip(format!("holoProxy - {}", get_active_llm_name()))
        .build()
    {
        Ok(t) => t,
        Err(e) => { warn!("tray creation failed: {:?}", e); return; }
    };

    info!("holoProxy tray ready | auto_select={} | active={}", is_auto_select(), get_active_llm_name());

    let mut app = TrayApp { menu_event_rx, tray: Some(tray) };
    let _ = event_loop.run_app(&mut app);
}
