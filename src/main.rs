mod brew;
mod brew_preferences;
mod cache;
mod command;
mod config;
mod controller;
mod diagnostics;
mod elasticsearch;
mod instance;
mod language_tools;
mod login;
mod menu_bar;
mod php_extensions;
mod runtimes;
mod service;
mod settings;
mod setup;
mod sites;
mod ui;
mod updates;

use gpui::{
    App, AppContext as _, Application, Bounds, KeyBinding, Menu, MenuItem, TitlebarOptions,
    WindowBounds, WindowOptions, actions, point, px, size,
};

use crate::controller::Controller;
use crate::ui::Ui;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

actions!(usenv, [Quit, CloseWindow]);

fn main() {
    let controller = Controller::new();
    let app = Application::new();
    let reopened = controller.clone();
    app.on_reopen(move |cx| show_window(&reopened, cx));
    app.run(move |cx| {
        let quiet_login = controller.snapshot().settings.quiet_login && login::launched_at_login();
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-w", CloseWindow, None),
        ]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_action(|_: &CloseWindow, cx| {
            if let Some(window) = cx.active_window() {
                let _ = window.update(cx, |_, window, _| window.remove_window());
            }
        });
        cx.set_menus(vec![Menu {
            name: "Usenv".into(),
            items: vec![MenuItem::action("退出 Usenv", Quit)],
        }]);
        match menu_bar::install(controller.clone(), cx) {
            Ok(()) if quiet_login => {
                set_activation_policy(NSApplicationActivationPolicy::Accessory);
                return;
            }
            Err(error) => eprintln!("usenv 菜单栏启动失败：{error:#}"),
            Ok(()) => {}
        }
        show_window(&controller, cx);
    });
}

fn set_activation_policy(policy: NSApplicationActivationPolicy) {
    let app = NSApplication::sharedApplication(MainThreadMarker::new().expect("GPUI main thread"));
    if app.activationPolicy() != policy {
        app.setActivationPolicy(policy);
    }
}

fn show_window(controller: &Controller, cx: &mut App) {
    set_activation_policy(NSApplicationActivationPolicy::Regular);
    if let Some(handle) = cx.windows().first().copied() {
        let _ = handle.update(cx, |_, window, _| window.activate_window());
        cx.activate(true);
        return;
    }
    let bounds = Bounds::centered(None, size(px(980.0), px(680.0)), cx);
    let result = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(780.0), px(540.0))),
            titlebar: Some(TitlebarOptions {
                title: Some("Usenv".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(12.), px(15.))),
            }),
            ..Default::default()
        },
        |window, cx| cx.new(|cx| Ui::new(controller.clone(), window, cx)),
    );
    match result {
        Ok(_) => cx.activate(true),
        Err(error) => eprintln!("usenv 主窗口启动失败：{error:#}"),
    }
}
