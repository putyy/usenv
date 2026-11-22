use std::time::Duration;

use anyhow::Result;
use gpui::App;
use objc2::{AnyThread, runtime::AnyObject};
use objc2_app_kit::{
    NSBaselineOffsetAttributeName, NSColor, NSFont, NSFontAttributeName,
    NSForegroundColorAttributeName, NSKernAttributeName, NSMenu, NSStringDrawing,
};
use objc2_foundation::{NSDictionary, NSMutableAttributedString, NSNumber, NSRange, NSString};
use tray_icon::menu::{ContextMenu, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIconBuilder};

use crate::service::{Service, ServiceAction};
use crate::controller::{Action, Controller, Snapshot};
use crate::show_window;

const OPEN: &str = "open";
const REFRESH: &str = "refresh";
const QUIT: &str = "quit";
const SHOW_ERROR: &str = "show-error";

pub fn install(controller: Controller, cx: &mut App) -> Result<()> {
    let snapshot = controller.snapshot();
    let tray = TrayIconBuilder::new()
        .with_icon(menu_icon()?)
        .with_icon_as_template(true)
        .with_tooltip("Usenv")
        .with_menu(Box::new(build_menu(&snapshot)?))
        .build()?;
    let executor = cx.background_executor().clone();
    cx.spawn(async move |cx| {
        let mut displayed = snapshot;
        loop {
            executor.timer(Duration::from_millis(200)).await;
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                let id = event.id.0.as_str();
                match id {
                    OPEN | SHOW_ERROR => {
                        if cx.update(|cx| show_window(&controller, cx)).is_err() {
                            return;
                        }
                    }
                    REFRESH => controller.submit(Action::Refresh),
                    QUIT => {
                        let _ = cx.update(|cx| cx.quit());
                        return;
                    }
                    _ => {
                        if let Some((formula, action)) = service_action(id) {
                            controller.submit(Action::Service(formula.to_owned(), action));
                        }
                    }
                }
            }
            if let Some(current) = controller.snapshot_if_changed(displayed.generation) {
                if menu_changed(&displayed, &current) {
                    match build_menu(&current) {
                        Ok(menu) => tray.set_menu(Some(Box::new(menu))),
                        Err(error) => eprintln!("usenv 菜单栏刷新失败：{error:#}"),
                    }
                }
                displayed = current;
            }
        }
    })
    .detach();
    Ok(())
}

fn service_action(id: &str) -> Option<(&str, ServiceAction)> {
    let (verb, formula) = id.split_once(':')?;
    let action = match verb {
        "start" => ServiceAction::Start,
        "stop" => ServiceAction::Stop,
        "restart" => ServiceAction::Restart,
        _ => return None,
    };
    Some((formula, action))
}

fn menu_changed(before: &Snapshot, after: &Snapshot) -> bool {
    let same_services = before.services.len() == after.services.len()
        && before
            .services
            .iter()
            .zip(&after.services)
            .all(|(old, new)| {
                let action = Action::Service(old.formula.clone(), old.toggle_action());
                old.formula == new.formula
                    && old.backend == new.backend
                    && old.environment == new.environment
                    && old.running == new.running
                    && old.loaded == new.loaded
                    && old.manageable == new.manageable
                    && before.action_available(&action) == after.action_available(&action)
            });
    before.busy != after.busy
        || before.error.is_some() != after.error.is_some()
        || before.installation_counts() != after.installation_counts()
        || !same_services
}

fn build_menu(snapshot: &Snapshot) -> Result<Menu> {
    let menu = Menu::new();
    let idle = snapshot.busy.is_none();
    menu.append(&MenuItem::with_id(OPEN, "打开 Usenv", true, None))?;
    menu.append(&MenuItem::with_id(REFRESH, "刷新环境", idle, None))?;
    menu.append(&PredefinedMenuItem::separator())?;

    if let Some(busy) = &snapshot.busy {
        menu.append(&MenuItem::new(busy, false, None))?;
    }
    let (running, queued) = snapshot.installation_counts();
    if running + queued > 0 {
        menu.append(&MenuItem::new(
            format!("安装任务 · {running} 项执行中 · {queued} 项排队"), false, None,
        ))?;
    }
    if snapshot.error.is_some() {
        menu.append(&MenuItem::with_id(SHOW_ERROR, "查看操作错误…", true, None))?;
    }
    if snapshot.services.is_empty() && idle {
        menu.append(&MenuItem::new("没有已安装的服务", false, None))?;
    }
    let first_service = menu.items().len();
    for service in &snapshot.services {
        let label = service
            .environment
            .service_label()
            .unwrap_or(service.environment.label());
        let title = match service.formula.split_once('@') {
            Some((_, version)) => format!("{label} {version}"),
            None => label.to_owned(),
        };
        let action = service.toggle_action();
        let (key, title_for_action) = if action == ServiceAction::Stop {
            ("stop", "停止")
        } else {
            ("start", "启动")
        };
        let sub = Submenu::new(title, snapshot.action_available(
            &Action::Service(service.formula.clone(), action),
        ));
        sub.append(&MenuItem::with_id(
            format!("{key}:{}", service.formula),
            title_for_action,
            true,
            None,
        ))?;
        if service.can_perform(ServiceAction::Restart) {
            sub.append(&MenuItem::with_id(
                format!("restart:{}", service.formula),
                "重启",
                true,
                None,
            ))?;
        }
        menu.append(&sub)?;
    }

    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&MenuItem::with_id(QUIT, "退出 Usenv", idle && running + queued == 0, None))?;
    if !snapshot.services.is_empty() {
        align_status_dots(&menu, first_service, &snapshot.services);
    }
    Ok(menu)
}

fn align_status_dots(menu: &Menu, first_service: usize, services: &[Service]) {
    // The tray uses this same native context menu, which stays owned by `menu`.
    let native = unsafe { &*menu.ns_menu().cast::<NSMenu>() };
    let font = NSFont::menuFontOfSize(0.0);
    let dot_font = NSFont::systemFontOfSize(8.0);
    let green = NSColor::systemGreenColor();
    let red = NSColor::systemRedColor();
    let baseline = NSNumber::new_f64(2.0);
    // AppKit text attributes pair each key with the corresponding object type.
    unsafe {
        let attributes = NSDictionary::<NSString, AnyObject>::from_slices(
            &[NSFontAttributeName],
            &[font.as_ref()],
        );
        let rows: Vec<_> = (0..native.numberOfItems())
            .filter_map(|index| native.itemAtIndex(index))
            .map(|item| {
                let title = item.title();
                let width = title.sizeWithAttributes(Some(&attributes)).width;
                (item, title, width)
            })
            .collect();
        let widest = rows.iter().map(|(_, _, width)| *width).fold(0.0, f64::max);
        for ((item, title, width), service) in rows.into_iter().skip(first_service).zip(services) {
            let text = NSString::from_str(&format!("{}\u{a0}●", &*title));
            let styled = NSMutableAttributedString::initWithString_attributes(
                NSMutableAttributedString::alloc(),
                &text,
                Some(&attributes),
            );
            let gap = NSRange::new(title.length(), 1);
            let dot = NSRange::new(title.length() + 1, 1);
            let padding = NSNumber::new_f64(widest - width + 12.0);
            let color = if service.running { &green } else { &red };
            styled.addAttribute_value_range(NSKernAttributeName, padding.as_ref(), gap);
            styled.addAttribute_value_range(NSFontAttributeName, dot_font.as_ref(), dot);
            styled.addAttribute_value_range(NSForegroundColorAttributeName, color.as_ref(), dot);
            styled.addAttribute_value_range(NSBaselineOffsetAttributeName, baseline.as_ref(), dot);
            item.setAttributedTitle(Some(&styled));
        }
    }
}

fn menu_icon() -> Result<Icon> {
    // Draw the app logo's u_ silhouette as a 2× macOS template image.
    const WIDTH: usize = 40;
    const HEIGHT: usize = 36;
    let mut pixels = vec![0; WIDTH * HEIGHT * 4];
    let samples = [(0.25, 0.25), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75)];
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let covered = samples
                .iter()
                .filter(|&&(dx, dy)| {
                    let x = x as f32 + dx;
                    let y = y as f32 + dy;
                    let left_top = 10.5 - (x - 4.).max(0.) * 0.5;
                    let left = (4. ..9.5).contains(&x) && (left_top..18.).contains(&y);
                    let right = (19. ..24.).contains(&x) && (8. ..27.5).contains(&y);
                    let radius_squared = (x - 14.).powi(2) + (y - 18.).powi(2);
                    let curve = y >= 18. && (20.25..=100.).contains(&radius_squared);
                    let underscore = (27. ..37.).contains(&x) && (23.5..27.5).contains(&y);
                    left || right || curve || underscore
                })
                .count();
            pixels[(y * WIDTH + x) * 4 + 3] = (covered * 255 / samples.len()) as u8;
        }
    }
    Ok(Icon::from_rgba(pixels, WIDTH as u32, HEIGHT as u32)?)
}
