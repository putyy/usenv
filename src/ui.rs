use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    Bounds, ClipboardItem, Context, Corner, DispatchPhase, Div, FocusHandle, FontWeight, KeyDownEvent,
    ListHorizontalSizingBehavior, MouseDownEvent, MouseMoveEvent, PathBuilder, PathPromptOptions,
    Pixels, PromptButton, PromptLevel, Render,
    Rgba, ScrollHandle, ScrollStrategy,
    ScrollWheelEvent, SharedString, Stateful, Subscription, Task, UniformListScrollHandle, Window,
    WindowAppearance, anchored, canvas, deferred, div, point, prelude::*, px, rgb, size, uniform_list,
};

use crate::brew::{Environment, Formula, PackageChange};
use crate::service::{Service, ServiceAction, ServiceBackend};
use crate::brew_preferences::BrewPreferences;
use crate::command::shell_quote;
use crate::config::{ConfigKind, ServicePaths};
use crate::controller::{
    Action, Controller, CurrentJavaStatus, DeveloperTool, Preference, RuntimeDetails, RuntimeTarget,
    Snapshot, TaskStatus,
};
use crate::language_tools::{NpmInfo, RustComponent, RustComponents, ToolRuntime};
use crate::login::LoginStatus;
use crate::php_extensions::{ExtensionInfo, ExtensionState, Operation as ExtensionOperation};
use crate::runtimes::{Mise, MiseTool};
use crate::settings::{MAX_PARALLEL_INSTALLS, MIN_PARALLEL_INSTALLS, SettingsStore, ShellFile, Theme};
use crate::setup;
use crate::updates::{self, UpdatePhase, UpdateStatus};

const INPUT_HEIGHT: Pixels = px(35.);
const RECENT_PROJECTS: SharedString = SharedString::new_static("recent-projects");

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Environment(Environment),
    Settings,
}

impl Page {
    fn title(self) -> &'static str {
        match self {
            Self::Environment(environment) => environment.label(),
            Self::Settings => "设置",
        }
    }

    fn description(self) -> Option<&'static str> {
        match self {
            Self::Environment(Environment::Node) => Some("包含 npm 和 npx"),
            Self::Environment(Environment::Rust) => Some("rustc 和 Cargo"),
            Self::Environment(Environment::Flutter) => Some("Flutter SDK 和 Dart"),
            Self::Environment(Environment::Php) => Some("PHP CLI、PHP-FPM 和 Composer"),
            Self::Environment(Environment::Python) => Some("Python 解释器和 pip"),
            Self::Environment(Environment::Java) => Some("使用 Mise 设置终端默认 JDK"),
            Self::Environment(Environment::Mariadb) => Some("Homebrew 默认数据目录与 MySQL 共用"),
            Self::Environment(Environment::Elasticsearch) => Some("Mise 管理版本，本地单节点服务"),
            _ => None,
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Environment(environment) => environment.key(),
            Self::Settings => "settings",
        }
    }
}

fn first_page(snapshot: &Snapshot) -> Page {
    Environment::SIDEBAR
        .into_iter()
        .find(|environment| snapshot.environment_visible(*environment))
        .map_or(Page::Settings, Page::Environment)
}

#[derive(Clone, Copy)]
struct Palette {
    background: Rgba,
    sidebar: Rgba,
    sidebar_ink: Rgba,
    sidebar_selected: Rgba,
    sidebar_selected_ink: Rgba,
    sidebar_hover: Rgba,
    surface: Rgba,
    control: Rgba,
    control_hover: Rgba,
    ink: Rgba,
    muted: Rgba,
    placeholder: Rgba,
    accent: Rgba,
    accent_hover: Rgba,
    accent_text: Rgba,
    accent_soft: Rgba,
    success: Rgba,
    danger: Rgba,
}

impl Palette {
    fn for_theme(theme: Theme, appearance: WindowAppearance) -> Self {
        let dark = match theme {
            Theme::System => matches!(
                appearance,
                WindowAppearance::Dark | WindowAppearance::VibrantDark
            ),
            Theme::Light => false,
            Theme::Dark => true,
        };
        if dark {
            Self {
                background: rgb(0x141414),
                sidebar: rgb(0x1f1f1f),
                sidebar_ink: rgb(0xb3b3b3),
                sidebar_selected: rgb(0x3a3a3a),
                sidebar_selected_ink: rgb(0xdadada),
                sidebar_hover: rgb(0x2e2e2e),
                surface: rgb(0x202020),
                control: rgb(0x2e2e2e),
                control_hover: rgb(0x3d3d3d),
                ink: rgb(0xe2e2e2),
                muted: rgb(0xa7a7a7),
                placeholder: rgb(0xa7a7a7),
                accent: rgb(0xa6c0ff),
                accent_hover: rgb(0xbbceff),
                accent_text: rgb(0x18243b),
                accent_soft: rgb(0x28344c),
                success: rgb(0x70d3ac),
                danger: rgb(0xff9ca9),
            }
        } else {
            let panel_background = rgb(0xededef);
            Self {
                background: rgb(0xf6f6f7),
                sidebar: panel_background,
                sidebar_ink: rgb(0x595a5a),
                sidebar_selected: rgb(0xd9dadd),
                sidebar_selected_ink: rgb(0x262626),
                sidebar_hover: rgb(0xe3e4e7),
                surface: panel_background,
                control: rgb(0xe3e4e7),
                control_hover: rgb(0xd7d9de),
                ink: rgb(0x4a4a4a),
                muted: rgb(0x686868),
                placeholder: rgb(0x707276),
                accent: rgb(0x385fa6),
                accent_hover: rgb(0x2e508e),
                accent_text: rgb(0xffffff),
                accent_soft: rgb(0xdbe4f3),
                success: rgb(0x167657),
                danger: rgb(0xa83450),
            }
        }
    }
}

#[derive(Clone, Copy)]
enum ButtonStyle {
    Plain,
    Primary,
    Selected,
    Sidebar,
}

enum MiseCatalog {
    Loading,
    Ready(Vec<String>),
    Failed(String),
}

#[derive(Clone, Copy)]
enum Manager {
    Homebrew,
    Mise,
}

struct UpdateDialog {
    previous_focus: Option<FocusHandle>,
    github: FocusHandle,
    cancel: FocusHandle,
    install: FocusHandle,
}

pub struct Ui {
    controller: Controller,
    snapshot: Snapshot,
    page: Page,
    sidebar_collapsed: bool,
    update_dialog: Option<UpdateDialog>,
    expanded_services: Option<Environment>,
    open_actions: Option<SharedString>,
    log_open: bool,
    log_selected: Option<u64>,
    log_follow: bool,
    log_scroll: UniformListScrollHandle,
    log_tabs_scroll: ScrollHandle,
    terminal_report_collapsed: bool,
    copy_feedback: Option<Bounds<Pixels>>,
    _copy_feedback_timer: Option<Task<()>>,
    runtime_target: Option<RuntimeTarget>,
    runtime_input: LineEditor,
    runtime_focus: FocusHandle,
    site_php: Option<String>,
    site_root: Option<PathBuf>,
    site_port: LineEditor,
    site_port_focus: FocusHandle,
    proxy: LineEditor,
    proxy_focus: FocusHandle,
    brew_api_mirror: LineEditor,
    brew_api_focus: FocusHandle,
    brew_bottle_mirror: LineEditor,
    brew_bottle_focus: FocusHandle,
    brew_disable_auto_update: bool,
    mise_version: LineEditor,
    mise_focus: FocusHandle,
    mise_catalogs: BTreeMap<Environment, MiseCatalog>,
    mise_list_open: bool,
    mise_search: LineEditor,
    mise_search_focus: FocusHandle,
    mise_list_scroll: UniformListScrollHandle,
    _refresh: Task<()>,
    _appearance: Subscription,
}

impl Ui {
    pub fn new(controller: Controller, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let snapshot = controller.snapshot();
        let proxy = LineEditor::new(snapshot.settings.proxy.clone());
        let brew_preferences = snapshot.brew_preferences().cloned().unwrap_or_default();
        let executor = cx.background_executor().clone();
        let refresh = cx.spawn(async move |view, cx| {
            loop {
                executor.timer(Duration::from_millis(250)).await;
                if view.update(cx, |view, cx| view.sync(cx)).is_err() {
                    break;
                }
            }
        });
        Self {
            controller,
            page: first_page(&snapshot),
            sidebar_collapsed: false,
            update_dialog: None,
            expanded_services: None,
            open_actions: None,
            log_open: false,
            log_selected: snapshot.output_tasks.back().map(|task| task.id),
            log_follow: true,
            log_scroll: UniformListScrollHandle::new(),
            log_tabs_scroll: ScrollHandle::new(),
            terminal_report_collapsed: false,
            copy_feedback: None,
            _copy_feedback_timer: None,
            runtime_target: None,
            runtime_input: LineEditor::default(),
            runtime_focus: cx.focus_handle().tab_stop(true),
            site_php: None,
            site_root: None,
            site_port: LineEditor::new("8081".into()),
            site_port_focus: cx.focus_handle().tab_stop(true),
            snapshot,
            proxy,
            proxy_focus: cx.focus_handle().tab_stop(true),
            brew_api_mirror: LineEditor::new(brew_preferences.api_mirror),
            brew_api_focus: cx.focus_handle().tab_stop(true),
            brew_bottle_mirror: LineEditor::new(brew_preferences.bottle_mirror),
            brew_bottle_focus: cx.focus_handle().tab_stop(true),
            brew_disable_auto_update: brew_preferences.disable_auto_update,
            mise_version: LineEditor::default(),
            mise_focus: cx.focus_handle().tab_stop(true),
            mise_catalogs: BTreeMap::new(),
            mise_list_open: false,
            mise_search: LineEditor::default(),
            mise_search_focus: cx.focus_handle().tab_stop(true),
            mise_list_scroll: UniformListScrollHandle::new(),
            _refresh: refresh,
            _appearance: cx.observe_window_appearance(window, |_, _, cx| cx.notify()),
        }
    }

    fn sync(&mut self, cx: &mut Context<Self>) {
        let generation = self.snapshot.generation;
        let mut updated = self.controller.snapshot_if_changed(generation);
        let current = updated.as_ref().unwrap_or(&self.snapshot);
        if self.page == Page::Environment(Environment::Java)
            && matches!(current.current_java, CurrentJavaStatus::Idle)
        {
            self.controller.submit(Action::ReadCurrentJava);
            updated = self.controller.snapshot_if_changed(generation);
        }
        let Some(snapshot) = updated else {
            return;
        };
        if snapshot.settings.proxy != self.snapshot.settings.proxy
            && (self.proxy.text == self.snapshot.settings.proxy
                || self.proxy.text.trim() == snapshot.settings.proxy)
        {
            self.proxy.sync_text(&snapshot.settings.proxy);
        }
        let saved = self.snapshot.brew_preferences();
        if let Some(preferences) = snapshot.brew_preferences()
            && Some(preferences) != saved
        {
            let draft = self.brew_preferences_draft();
            if saved == Some(&draft) || Some(preferences) == Some(&draft)
                || (saved.is_none() && draft == BrewPreferences::default())
            {
                self.brew_api_mirror.sync_text(&preferences.api_mirror);
                self.brew_bottle_mirror.sync_text(&preferences.bottle_mirror);
                self.brew_disable_auto_update = preferences.disable_auto_update;
            }
        }
        if let Page::Environment(environment) = self.page
            && !snapshot.environment_visible(environment)
        {
            self.navigate(first_page(&snapshot));
        }
        self.snapshot = snapshot;
        if self.log_selected.is_none_or(|id| !self.snapshot.output_tasks.iter().any(|task| task.id == id)) {
            self.select_install_log(self.snapshot.output_tasks.back().map(|task| task.id));
        }
        cx.notify();
    }

    fn select_install_log(&mut self, id: Option<u64>) {
        if self.log_selected != id {
            self.log_selected = id;
            self.log_follow = true;
            self.log_scroll = UniformListScrollHandle::new();
        }
        if let Some(index) = self.snapshot.output_tasks.iter().position(|task| Some(task.id) == id) {
            self.log_tabs_scroll.scroll_to_item(index);
        }
    }

    fn submit(&mut self, action: Action, cx: &mut Context<Self>) {
        self.close_actions(cx);
        let checking_terminal = matches!(&action, Action::CheckTerminal(..));
        let select_new_task = !self.log_open || checking_terminal
            || matches!(&action, Action::OpenTerminalInstall(_));
        let last_task = self.snapshot.output_tasks.back().map(|task| task.id);
        self.controller.submit(action);
        self.sync(cx);
        let latest_task = self.snapshot.output_tasks.back().map(|task| task.id);
        if select_new_task && latest_task != last_task {
            if checking_terminal {
                self.terminal_report_collapsed = false;
            }
            self.select_install_log(latest_task);
            cx.notify();
        }
    }

    fn confirm_package(
        &self,
        id: impl Into<SharedString>,
        label: &'static str,
        target: String,
        action: Action,
        colors: Palette,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        button(id, label, ButtonStyle::Plain, self.snapshot.action_available(&action), colors)
            .on_click(cx.listener(move |view, _, window, cx| {
                if !view.snapshot.action_available(&action) {
                    return;
                }
                view.close_actions(cx);
                let action = action.clone();
                let title = format!("{label} {target}？");
                let detail = if label == "升级" {
                    "Homebrew 将更新这条公式。服务需要先停止并关闭登录启动。"
                } else if matches!(&action, Action::UninstallMise(env, ..)
                    if env.service_label().is_some() && *env != Environment::Elasticsearch)
                {
                    "Mise 会移除该版本的安装目录和其中的文件。请先停止使用此版本的服务，并备份保存在安装目录中的配置和数据。"
                } else if matches!(&action, Action::UninstallMise(..)) {
                    "Mise 会移除所选版本的安装目录。"
                } else {
                    "卸载会移除选中的程序版本。服务的配置和数据目录会保留。"
                };
                let answer = window.prompt(
                    PromptLevel::Warning,
                    &title,
                    Some(detail),
                    &[PromptButton::cancel("取消"), PromptButton::ok(label)],
                    cx,
                );
                cx.spawn(async move |view, cx| {
                    if matches!(answer.await, Ok(1)) {
                        let _ = view.update(cx, |view, cx| view.submit(action, cx));
                    }
                }).detach();
            }))
    }

    fn navigate(&mut self, page: Page) {
        if self.page != page {
            self.page = page;
            self.open_actions = None;
            self.copy_feedback = None;
            self.mise_version = LineEditor::default();
            self.mise_list_open = false;
            self.runtime_target = None;
        } else if self.runtime_target.take().is_some() {
            self.open_actions = None;
        }
    }

    fn runtime_button(
        &self,
        id: String,
        label: &'static str,
        target: RuntimeTarget,
        colors: Palette,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        button(
            id,
            label,
            ButtonStyle::Plain,
            self.snapshot.busy.is_none(),
            colors,
        )
        .on_click(cx.listener(move |view, _, _, cx| {
            view.runtime_target = Some(target.clone());
            view.runtime_input = LineEditor::default();
            view.submit(Action::InspectRuntime(target.clone()), cx);
        }))
    }

    fn action_button(
        &self,
        id: impl Into<SharedString>,
        label: &str,
        style: ButtonStyle,
        enabled: bool,
        action: Action,
        colors: Palette,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let label = match self.snapshot.active_install(&action) {
            Some(TaskStatus::Queued) => "排队中",
            Some(TaskStatus::Running) => "执行中",
            _ => label,
        };
        let enabled = enabled && self.snapshot.action_available(&action);
        let element = button(id, label, style, enabled, colors);
        if enabled {
            element.on_click(cx.listener(move |view, _, _, cx| view.submit(action.clone(), cx)))
        } else {
            element
        }
    }

    fn close_actions(&mut self, cx: &mut Context<Self>) {
        if self.open_actions.take().is_some() {
            cx.notify();
        }
    }

    fn show_copy_feedback(&mut self, bounds: Bounds<Pixels>, cx: &mut Context<Self>) {
        self.copy_feedback = Some(bounds);
        let timer = cx.background_executor().timer(Duration::from_millis(1400));
        self._copy_feedback_timer = Some(cx.spawn(async move |view, cx| {
            timer.await;
            let _ = view.update(cx, |view, cx| {
                view.copy_feedback = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn copy_feedback(&self, window: &Window, colors: Palette) -> Option<Div> {
        let bounds = self.copy_feedback.as_ref()?;
        let width = px(60.);
        let height = px(25.);
        let gap = px(4.);
        let below = bounds.bottom() + gap;
        let (anchor, y) = if below + height + gap <= window.viewport_size().height {
            (Corner::TopLeft, below)
        } else {
            (Corner::BottomLeft, bounds.top() - gap)
        };
        Some(div().absolute().top_0().left_0().size(px(0.)).child(
            deferred(anchored().anchor(anchor)
                .position(point(bounds.center().x - width / 2., y))
                .snap_to_window_with_margin(gap)
                .child(
                    div().w(width).h(height).flex().items_center().justify_center()
                        .rounded_md().bg(colors.ink).text_color(colors.background)
                        .text_size(px(11.)).shadow_md().child("已复制"),
                ))
            .with_priority(1),
        ))
    }

    // The first button is the primary action when the row needs an overflow menu.
    fn row_actions(
        &self,
        id: impl Into<SharedString>,
        buttons: Vec<Stateful<Div>>,
        colors: Palette,
        cx: &Context<Self>,
    ) -> Div {
        let row = div().flex().flex_shrink_0().items_center().gap_2();
        if buttons.len() <= 2 {
            return row.children(buttons);
        }

        let id = id.into();
        let is_open = self.open_actions.as_ref() == Some(&id);
        let hover_id = id.clone();
        let click_id = id.clone();
        let trigger = button(
            id,
            "⋯",
            if is_open {
                ButtonStyle::Selected
            } else {
                ButtonStyle::Plain
            },
            true,
            colors,
        )
        .w(px(30.))
        .px_0()
        .text_size(px(16.))
        .on_mouse_move(cx.listener(move |view, _, _, cx| {
            if view.open_actions.as_ref() != Some(&hover_id) {
                view.open_actions = Some(hover_id.clone());
                cx.notify();
            }
        }))
        .on_click(cx.listener(move |view, event: &gpui::ClickEvent, _, cx| {
            if event.mouse_position().is_none() && view.open_actions.as_ref() == Some(&click_id) {
                view.close_actions(cx);
            } else if view.open_actions.as_ref() != Some(&click_id) {
                view.open_actions = Some(click_id.clone());
                cx.notify();
            }
        }));

        let mut buttons = buttons.into_iter();
        let result = row.children(buttons.next());
        let trigger_slot = div().relative().flex_shrink_0().child(trigger);
        if !is_open {
            return result.child(trigger_slot);
        }

        let card_bounds = Rc::new(Cell::new(None));
        let track_card = card_bounds.clone();
        let card = div()
            .relative()
            .flex()
            .flex_col()
            .gap_1()
            .min_w(px(140.))
            .p_1()
            .rounded_lg()
            .bg(colors.background)
            .shadow_md()
            .block_mouse_except_scroll()
            .child(
                canvas(
                    move |bounds, _, _| track_card.set(Some(bounds)),
                    |_, (), _, _| {},
                )
                .absolute()
                .inset_0(),
            )
            .children(buttons.map(|entry| {
                entry
                    .justify_start()
                    .whitespace_nowrap()
                    .bg(colors.background)
            }));
        let dismiss = Rc::new(cx.listener(|view, _: &(), _, cx| view.close_actions(cx)));
        let watch_pointer = canvas(
            |_, _, _| (),
            move |trigger, (), window, _| {
                // Deferred prepaint has already placed the card, including any flip at the window edge.
                let Some(card) = card_bounds.take() else {
                    return;
                };
                let between = trigger.union(&card);
                let over_menu = Rc::new(move |position: &gpui::Point<Pixels>| {
                    let in_gap = between.contains(position)
                        && position.x >= trigger.left()
                        && position.x <= trigger.right();
                    trigger.contains(position) || card.contains(position) || in_gap
                });
                let over_menu_on_move = over_menu.clone();
                let dismiss_outside = dismiss.clone();
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                    if phase == DispatchPhase::Capture && !over_menu_on_move(&event.position) {
                        dismiss_outside(&(), window, cx);
                    }
                });
                let dismiss_press = dismiss.clone();
                window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                    if phase == DispatchPhase::Capture && !over_menu(&event.position) {
                        dismiss_press(&(), window, cx);
                    }
                });
                window.on_mouse_event(move |_: &ScrollWheelEvent, phase, window, cx| {
                    if phase == DispatchPhase::Capture {
                        dismiss(&(), window, cx);
                    }
                });
            },
        )
        .absolute()
        .inset_0();

        result.child(trigger_slot.child(watch_pointer).child(
            div()
                .absolute()
                .right_0()
                .top_0()
                .size(px(0.))
                .child(deferred(
                    anchored().anchor(Corner::TopRight).child(
                        // Clear the 30 px trigger below the anchor, or sit just above it when flipped.
                        div().pt(px(34.)).pb_1().child(card),
                    ),
                )),
        ))
    }

    fn sidebar(&self, width: Pixels, fullscreen: bool, colors: Palette, cx: &Context<Self>) -> Div {
        let collapsed = self.sidebar_collapsed;
        let groups = [("语言与工具", true), ("服务器与数据库", false)]
            .into_iter()
            .filter_map(|(title, runtime)| {
                let environments: Vec<_> = Environment::SIDEBAR
                    .into_iter()
                    .filter(|environment| {
                        environment.runtime() == runtime
                            && self.snapshot.environment_visible(*environment)
                    })
                    .collect();
                (!environments.is_empty()).then(|| {
                    let items = div().flex().flex_col().gap_1().children(
                        environments
                            .into_iter()
                            .map(|environment| self.sidebar_environment(environment, colors, cx)),
                    );
                    if collapsed {
                        items
                    } else {
                        section(title, colors.sidebar_ink).child(items)
                    }
                })
            });
        let show_empty = !collapsed
            && Environment::SIDEBAR
                .into_iter()
                .all(|environment| !self.snapshot.environment_visible(environment));
        let has_brew = self.snapshot.brew_available();
        let brew_status = if !self.snapshot.managers_checked {
            "正在检测 Homebrew"
        } else if self.snapshot.brew_detection_failed {
            "Homebrew 检测失败"
        } else if has_brew {
            "Homebrew 已连接"
        } else {
            "未检测到 Homebrew"
        };

        div()
            .w(width)
            .flex_shrink_0()
            .flex()
            .flex_col()
            .pt(px(if fullscreen { 16. } else { 44. }))
            .bg(colors.sidebar)
            .text_color(colors.sidebar_ink)
            .child(
                div()
                    .id("environment-sidebar")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_3()
                    .pt_2()
                    .pb_3()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .children(groups)
                            .children(show_empty.then(|| div().px_2().child("暂无显示的环境"))),
                    ),
            )
            .child(
                div()
                    .mx_3()
                    .pt_3()
                    .pb_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children((!collapsed).then(|| {
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .text_size(px(11.))
                            .child(dot(if has_brew {
                                colors.success
                            } else {
                                colors.sidebar_ink
                            }))
                            .child(brew_status)
                    }))
                    .child(
                        div()
                            .flex()
                            .when(collapsed, |row| row.flex_col())
                            .gap_1()
                            .child(
                                self.sidebar_item(Page::Settings, colors, cx)
                                    .when(!collapsed, |item| item.flex_1()),
                            )
                            .child(
                                button(
                                    "sidebar-github",
                                    if collapsed { "GH" } else { "GitHub" },
                                    ButtonStyle::Sidebar,
                                    true,
                                    colors,
                                )
                                .h(px(35.))
                                .px_2()
                                .when(collapsed, |button| button.size(px(34.)).mx_auto().px_0())
                                .tooltip(move |_, cx| {
                                    cx.new(|_| SidebarTooltip("打开 GitHub 仓库", colors))
                                        .into()
                                })
                                .on_click(|_, _, cx| cx.open_url(updates::REPOSITORY)),
                            ),
                    ),
            )
    }

    fn sidebar_toggle(&self, sidebar_width: Pixels, colors: Palette, cx: &Context<Self>) -> Div {
        let tip_offset = px(if self.sidebar_collapsed { 2.5 } else { -2.5 });
        let hint = if self.sidebar_collapsed { "展开侧边栏" } else { "收起侧边栏" };
        let arrow = canvas(
            |_, _, _| (),
            move |bounds, (), window, _| {
                let center = bounds.center();
                let outer_x = center.x - tip_offset;
                let mut path = PathBuilder::stroke(px(1.6));
                path.move_to(point(outer_x, center.y - px(4.5)));
                path.line_to(point(center.x + tip_offset, center.y));
                path.line_to(point(outer_x, center.y + px(4.5)));
                if let Ok(path) = path.build() {
                    window.paint_path(path, window.text_style().color);
                }
            },
        )
        .size(px(12.));
        let diameter = px(24.);
        div()
            .absolute()
            .left(sidebar_width - diameter / 2.)
            .top_0()
            .bottom_0()
            .w(diameter)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .child(
                keyboard_focus(div().id("sidebar-collapse"))
                    .size(diameter)
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .border_1()
                    .border_color(colors.control_hover)
                    .bg(colors.sidebar)
                    .text_color(colors.sidebar_ink)
                    .hover(move |style| style.bg(colors.sidebar_hover).text_color(colors.accent))
                    .focus(move |style| style.border_color(colors.accent).text_color(colors.accent))
                    .cursor_pointer()
                    .block_mouse_except_scroll()
                    .tooltip(move |_, cx| {
                        cx.new(|_| SidebarTooltip(hint, colors))
                            .into()
                    })
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.sidebar_collapsed = !view.sidebar_collapsed;
                        cx.notify();
                    }))
                    .child(arrow),
            )
    }

    fn sidebar_environment(
        &self,
        environment: Environment,
        colors: Palette,
        cx: &Context<Self>,
    ) -> Div {
        let services: Vec<_> = self
            .snapshot
            .services
            .iter()
            .filter(|service| service.environment == environment && service.manageable)
            .collect();
        let row = self.sidebar_item(Page::Environment(environment), colors, cx);
        if self.sidebar_collapsed {
            let active = services
                .iter()
                .any(|service| service.toggle_action() == ServiceAction::Stop);
            return div().child(
                row.relative()
                    .children(active.then(|| dot(colors.success).absolute().top_1().right_1())),
            );
        }
        let expanded = self.expanded_services == Some(environment);
        match services.as_slice() {
            [] => div().child(row),
            [service] => div().child(row.child(self.sidebar_switch(service, colors, cx))),
            _ => {
                let active = services
                    .iter()
                    .any(|service| service.toggle_action() == ServiceAction::Stop);
                let expander = keyboard_focus(div().id(SharedString::from(format!(
                    "sidebar-services-{}",
                    environment.key()
                ))))
                .flex()
                .items_center()
                .justify_center()
                .size(px(22.))
                .rounded_md()
                .text_color(if active {
                    colors.success
                } else {
                    colors.sidebar_ink
                })
                .hover(move |style| style.bg(colors.control_hover))
                .focus(move |style| style.bg(colors.control_hover))
                .cursor_pointer()
                .child(if expanded { "⌄" } else { "›" })
                .on_click(cx.listener(move |view, _, _, cx| {
                    cx.stop_propagation();
                    view.expanded_services = (!expanded).then_some(environment);
                    cx.notify();
                }));
                div().child(row.child(expander)).children(expanded.then(|| {
                    div()
                        .pt_1()
                        .pl_4()
                        .pr_2()
                        .children(services.into_iter().map(|service| {
                            div()
                                .h(px(27.))
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(px(11.))
                                        .child(service.formula.clone()),
                                )
                                .child(self.sidebar_switch(service, colors, cx))
                        }))
                }))
            }
        }
    }

    fn sidebar_switch(
        &self,
        service: &Service,
        colors: Palette,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let action = service.toggle_action();
        let on = action == ServiceAction::Stop;
        let pending = self.service_operation(service).is_some();
        let formula = service.formula.clone();
        let request = Action::Service(formula.clone(), action);
        let enabled = self.snapshot.action_available(&request);
        div()
            .id(SharedString::from(format!("sidebar-toggle-{formula}")))
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .w(px(32.))
            .h(px(26.))
            .rounded_md()
            .when(enabled, |toggle| {
                keyboard_focus(toggle)
                    .cursor_pointer()
                    .hover(move |style| style.bg(colors.control_hover))
                    .focus(move |style| style.bg(colors.control_hover))
            })
            .child(
                div()
                    .w(px(29.))
                    .h(px(16.))
                    .flex()
                    .items_center()
                    .px(px(3.))
                    .when(on, |track| track.justify_end())
                    .rounded_full()
                    .bg(if pending {
                        colors.accent
                    } else if on {
                        colors.success
                    } else {
                        colors.control_hover
                    })
                    .child(div().size(px(10.)).rounded_full().bg(if pending {
                        colors.accent_text
                    } else if on {
                        colors.background
                    } else {
                        colors.sidebar_ink
                    })),
            )
            .on_click(cx.listener(move |view, _, _, cx| {
                cx.stop_propagation();
                if enabled {
                    view.submit(request.clone(), cx);
                }
            }))
    }

    fn sidebar_item(&self, page: Page, colors: Palette, cx: &Context<Self>) -> Stateful<Div> {
        let selected = self.page == page;
        let collapsed = self.sidebar_collapsed;
        let background = if selected {
            colors.sidebar_selected
        } else {
            colors.sidebar
        };
        let hovered = if selected {
            background
        } else {
            colors.sidebar_hover
        };
        let settings_icon = || {
            div()
                .size(px(23.))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(15.))
                .child("⚙")
        };
        let item = if collapsed {
            match page {
                Page::Environment(environment) => environment_icon(environment, 34.),
                Page::Settings => settings_icon().size(px(34.)),
            }
        } else {
            div().when(page == Page::Settings, |row| row.child(settings_icon()))
        };
        let item = keyboard_focus(item.id(page.key()))
            .rounded_md()
            .border_1()
            .border_color(gpui::rgba(0))
            .bg(background)
            .text_color(if selected {
                colors.sidebar_selected_ink
            } else {
                colors.sidebar_ink
            })
            .hover(move |style| style.bg(hovered))
            .focus(move |style| style.border_color(colors.accent))
            .cursor_pointer()
            .on_click(cx.listener(move |view, _, _, cx| {
                view.navigate(page);
                cx.notify();
            }));
        if collapsed {
            item.mx_auto()
                .tooltip(move |_, cx| cx.new(|_| SidebarTooltip(page.title(), colors)).into())
        } else {
            item.flex()
                .h(px(35.))
                .items_center()
                .gap_2()
                .px_2()
                .font_weight(if selected {
                    FontWeight::SEMIBOLD
                } else {
                    FontWeight::NORMAL
                })
                .child(div().min_w_0().flex_1().truncate().child(page.title()))
        }
    }

    fn header(&self, colors: Palette, cx: &Context<Self>) -> Div {
        let missing_manager = !self.snapshot.brew_available() || self.snapshot.mise.is_none();
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(px(29.))
                            .line_height(px(36.))
                            .font_weight(FontWeight::BOLD)
                            .child(self.page.title()),
                    )
                    .children(
                        self.page
                            .description()
                            .map(|description| div().text_color(colors.muted).child(description)),
                    ),
            )
            .child(
                div().flex().items_center().gap_2()
                    .children(match self.page {
                        Page::Environment(environment) if environment.runtime() => {
                            Some(div().flex().items_center().gap_2()
                                .child(self.action_button(
                                    "check-terminal",
                                    "检查终端",
                                    ButtonStyle::Plain,
                                    true,
                                    Action::CheckTerminal(environment, None),
                                    colors,
                                    cx,
                                ))
                                .child(self.project_check_button(environment, colors, cx)))
                        }
                        _ => None,
                    })
                    .child(self.action_button(
                        "refresh",
                        if missing_manager || self.page == Page::Settings { "重新检测" } else { "刷新" },
                        ButtonStyle::Plain,
                        self.snapshot.busy.is_none(),
                        Action::Refresh,
                        colors,
                        cx,
                    )),
            )
    }

    fn feedback(&self, colors: Palette, cx: &Context<Self>) -> Div {
        let (running, queued) = self.snapshot.installation_counts();
        let status = self
            .snapshot
            .busy
            .as_ref()
            .map(|text| (text.clone(), colors.accent))
            .or_else(|| {
                self.snapshot
                    .error
                    .as_ref()
                    .map(|text| (text.clone(), colors.danger))
            })
            .or_else(|| (running + queued > 0).then(|| (
                format!("{running} 项执行中 · {queued} 项排队"), colors.accent,
            )))
            .or_else(|| (!self.snapshot.managers_checked).then(|| {
                ("正在后台检测本机环境…".into(), colors.accent)
            }))
            .or_else(|| {
                self.snapshot
                    .notice
                    .as_ref()
                    .map(|text| (text.clone(), colors.success))
            })
            .or_else(|| {
                self.snapshot.output_tasks.back().map(|task| {
                    let (label, color) = task_status(task.status, colors);
                    (format!("{} · {label}", task.title), color)
                })
            });
        let phase = self.snapshot.update.phase();
        let status = phase.map(|phase| (phase.label(), colors.accent)).or(status);
        let has_status = status.is_some();
        let has_output = !self.snapshot.output_tasks.is_empty();
        let status_area = div().id("open-output-status").flex_1().min_w_0().h_full()
            .flex().items_center().gap_2().text_color(colors.muted)
            .children(status.map(|(message, color)| {
                div().flex().items_center().gap_2().min_w_0()
                    .child(dot(color))
                    .child(div().min_w_0().truncate().text_size(px(12.))
                        .child(message.lines().next().unwrap_or_default().to_owned()))
            }));
        let status_area = if phase.is_some() {
            keyboard_focus(status_area).cursor_pointer().child("›")
                .on_click(cx.listener(|view, _, window, cx| view.open_update_dialog(window, cx)))
        } else if has_output {
            keyboard_focus(status_area).cursor_pointer()
                .hover(move |style| style.text_color(colors.ink))
                .on_click(cx.listener(|view, _, _, cx| view.open_task_output(cx)))
        } else {
            status_area
        };
        div()
            .h(px(38.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .px_7()
            .when(has_status, |bar| bar.bg(colors.surface))
            .child(status_area)
            .when(phase.is_some() && has_output, |bar| bar.child(
                button("open-task-output", "任务日志", ButtonStyle::Plain, true, colors)
                    .on_click(cx.listener(|view, _, _, cx| view.open_task_output(cx))),
            ))
            .children(self.snapshot.error.as_ref().map(|error| {
                copy_button("copy-error", "复制详情", error.clone(), colors, cx)
            }))
    }

    fn open_task_output(&mut self, cx: &mut Context<Self>) {
        let task_id = self.snapshot.output_tasks.iter().rev().find(|task| {
            matches!(task.status, TaskStatus::Running | TaskStatus::Queued)
        }).or_else(|| self.snapshot.output_tasks.back()).map(|task| task.id);
        self.log_open = true;
        self.select_install_log(task_id);
        cx.notify();
    }

    fn task_output(&self, colors: Palette, cx: &Context<Self>) -> Option<Div> {
        let selected = self.log_selected.filter(|_| self.log_open)?;
        let log = self.snapshot.output_tasks.iter().find(|task| task.id == selected)?;
        let tabs = div()
            .id("install-output-tabs")
            .flex()
            .flex_1()
            .gap_1()
            .min_w_0()
            .overflow_x_scroll()
            .track_scroll(&self.log_tabs_scroll)
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .children(self.snapshot.output_tasks.iter().map(|task| {
                let id = task.id;
                let label = task.title.char_indices().nth(30).map_or_else(
                    || task.title.clone(), |(index, _)| format!("{}…", &task.title[..index]),
                );
                let (status, color) = task_status(task.status, colors);
                button(
                    format!("install-output-tab-{id}"),
                    &label,
                    if id == selected { ButtonStyle::Selected } else { ButtonStyle::Plain },
                    true,
                    colors,
                )
                .flex_shrink_0()
                .gap_2()
                .child(div().text_color(color).child(status))
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.select_install_log(Some(id));
                    cx.notify();
                }))
            }));
        let lines = log.lines.clone();
        if self.log_follow && !lines.is_empty() {
            self.log_scroll
                .scroll_to_item_strict(lines.len() - 1, ScrollStrategy::Bottom);
        }
        let widest = lines
            .iter()
            .enumerate()
            .max_by_key(|(_, line)| line.len())
            .map(|(ix, _)| ix);
        let count = lines.len();
        let mut header = div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(log.title.clone()),
            )
            .children((log.status == TaskStatus::Queued).then(|| {
                button("cancel-install", "取消排队", ButtonStyle::Plain, true, colors)
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.controller.cancel_install(selected);
                        view.sync(cx);
                    }))
            }))
            .children(matches!(log.status, TaskStatus::Failed | TaskStatus::RefreshFailed).then(|| {
                let label = if log.status == TaskStatus::RefreshFailed { "重新读取" } else { "重试" };
                button("retry-install", label, ButtonStyle::Plain, true, colors)
                    .on_click(cx.listener(move |view, _, _, cx| {
                        let before = view.snapshot.output_tasks.back().map(|task| task.id);
                        view.controller.retry_task(selected);
                        view.sync(cx);
                        let after = view.snapshot.output_tasks.back().map(|task| task.id);
                        if after != before {
                            view.select_install_log(after);
                            cx.notify();
                        }
                    }))
            }))
            .children(log.terminal_install.as_ref().map(|install| {
                self.action_button(
                    "terminal-install",
                    "在终端继续",
                    ButtonStyle::Primary,
                    true,
                    Action::OpenTerminalInstall(install.clone()),
                    colors,
                    cx,
                )
            }))
            .children(log.command_line_tools_required.then(|| {
                self.action_button(
                    "install-command-line-tools",
                    "安装命令行工具",
                    ButtonStyle::Primary,
                    true,
                    Action::InstallCommandLineTools(log.id),
                    colors,
                    cx,
                )
            }));
        if log.omitted_lines > 0 {
            header = header.child(
                div()
                    .text_color(colors.muted)
                    .child(format!("较早的 {} 行已省略", log.omitted_lines)),
            );
        }
        if !self.log_follow {
            header = header.child(button(
                "follow-install-output",
                "跳到最新",
                ButtonStyle::Plain,
                true,
                colors,
            )
                .on_click(cx.listener(|view, _, _, cx| {
                    view.log_follow = true;
                    cx.notify();
                })));
        }
        let output = lines.clone();
        header = header.child(copy_control(
            "copy-install-output",
            "复制输出",
            count != 0,
            move || output.iter().map(|line| line.as_ref()).collect::<Vec<_>>().join("\n"),
            colors,
            cx,
        ));

        let body = div()
            .h(px(160.))
            .min_w_0()
            .rounded_md()
            .bg(colors.background)
            .overflow_hidden();
        let body = if count == 0 {
            body.px_3()
                .py_2()
                .text_color(colors.muted)
                .child(if log.status == TaskStatus::Queued {
                    "正在排队，空出安装位置后会自动开始。"
                } else {
                    "等待命令输出…"
                })
        } else {
            body.child(
                uniform_list(
                    SharedString::from(format!("install-output-{}", log.id)),
                    count,
                    move |range, _, _| {
                        range
                            .map(|ix| {
                                div()
                                    .h(px(17.))
                                    .px_3()
                                    .whitespace_nowrap()
                                    .font_family("Menlo")
                                    .text_size(px(11.))
                                    .line_height(px(17.))
                                    .child(SharedString::new(lines[ix].clone()))
                            })
                            .collect()
                    },
                )
                .with_width_from_item(widest)
                .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
                .track_scroll(self.log_scroll.clone())
                .on_scroll_wheel(cx.listener(|view, _, _, cx| {
                    if view.log_follow {
                        view.log_follow = false;
                        cx.notify();
                    }
                    cx.stop_propagation();
                }))
                .size_full(),
            )
        };
        Some(
            div()
                .flex_none()
                .flex()
                .flex_col()
                .gap_2()
                .px_7()
                .pt_3()
                .pb_2()
                .bg(colors.surface)
                .child(div().flex().items_center().gap_2()
                    .child(tabs)
                    .child(button("close-install-output", "×", ButtonStyle::Plain, true, colors)
                        .size(px(28.)).px_0().text_size(px(18.))
                        .tooltip(move |_, cx| cx.new(|_| SidebarTooltip("关闭输出", colors)).into())
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.log_open = false;
                            cx.notify();
                        }))))
                .child(header)
                .child(body),
        )
    }

    fn runtime_page(
        &self,
        target: &RuntimeTarget,
        window: &Window,
        colors: Palette,
        cx: &Context<Self>,
    ) -> Div {
        let enabled = self.snapshot.busy.is_none();
        let (name, version) = match target {
            RuntimeTarget::Php(formula) => {
                let version = self.snapshot.formulae.iter()
                    .find(|entry| &entry.name == formula)
                    .and_then(|entry| entry.installed_versions.last())
                    .map_or_else(|| formula.clone(), |version| format!("PHP {version} · Homebrew"));
                ("PHP 扩展", version)
            }
            RuntimeTarget::Npm(tool) => ("npm 工具", runtime_title(tool)),
            RuntimeTarget::Rust(tool) => ("Rust 组件", runtime_title(tool)),
        };
        let header = div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().flex_1().min_w_0().child(
                div().text_size(px(29.)).line_height(px(36.)).font_weight(FontWeight::BOLD).child(name),
            ).child(div().text_color(colors.muted).truncate().child(version)))
            .child(self.action_button(
                "runtime-refresh", "刷新", ButtonStyle::Plain, enabled,
                Action::InspectRuntime(target.clone()), colors, cx,
            ))
            .child(button("runtime-back", "返回版本列表", ButtonStyle::Plain, true, colors)
                .on_click(cx.listener(|view, _, _, cx| {
                    view.runtime_target = None;
                    view.close_actions(cx);
                    cx.notify();
                })));
        let details = self.snapshot.runtime_details.as_ref()
            .filter(|(selected, _)| selected == target)
            .map(|(_, details)| details);
        let content = match (target, details) {
            (RuntimeTarget::Php(formula), Some(RuntimeDetails::Php(extensions))) => {
                self.php_extension_list(formula, extensions, window, colors, cx)
            }
            (RuntimeTarget::Npm(tool), Some(RuntimeDetails::Npm(info))) => {
                self.npm_tools(tool, info, window, colors, cx)
            }
            (RuntimeTarget::Rust(tool), Some(RuntimeDetails::Rust(components))) => {
                self.rust_tools(tool, components, colors, cx)
            }
            _ => panel(colors).p_4().text_color(colors.muted).child(
                if enabled { "读取失败，可刷新重试。" } else { "正在读取…" },
            ),
        };
        div().flex().flex_col().gap_4().child(header).child(content)
    }

    fn php_extension_list(
        &self,
        formula: &str,
        extensions: &[ExtensionInfo],
        window: &Window,
        colors: Palette,
        cx: &Context<Self>,
    ) -> Div {
        let enabled = self.snapshot.busy.is_none();
        let requested = self.runtime_input.text.trim();
        let requested = (!requested.is_empty()).then(|| requested.to_owned());
        let can_install = extensions.iter().any(|entry| {
            !entry.extension.is_bundled()
                && matches!(entry.state, ExtensionState::Missing | ExtensionState::Broken)
        });
        let mut content = card_list();
        if can_install {
            content = content.child(
                panel(colors).px_4().py_2().flex().items_center().gap_3()
                    .child("扩展安装版本")
                    .child(self.runtime_input.field(
                        "php-extension-version", &self.runtime_focus, window,
                        "最新兼容稳定版", 24, colors,
                    ).on_key_down(cx.listener(|view, event, _, cx| view.runtime_input.key(event, cx)))),
            );
        }
        content.children(extensions.iter().map(|entry| {
            let name = entry.extension.label();
            let bundled = entry.extension.is_bundled();
            let status = match entry.state {
                ExtensionState::Enabled if bundled => "PHP 自带 · 缓存已开启",
                ExtensionState::Disabled if bundled => "PHP 自带 · 缓存已关闭",
                ExtensionState::Missing if bundled => "当前 PHP 未提供",
                ExtensionState::External if entry.loaded => "外部管理 · 已加载",
                ExtensionState::External => "外部管理 · 未加载",
                state => state.label(),
            };
            let label = [Some(status), entry.version.as_deref(), entry.detail]
                .into_iter().flatten()
                .collect::<Vec<_>>().join(" · ");
            let main = match entry.state {
                ExtensionState::Missing | ExtensionState::Broken if bundled => None,
                ExtensionState::Missing => Some(("安装", ExtensionOperation::Install(requested.clone()))),
                ExtensionState::Enabled => Some(("停用", ExtensionOperation::Disable)),
                ExtensionState::Disabled => Some(("启用", ExtensionOperation::Enable)),
                ExtensionState::Broken => Some(("重新安装", ExtensionOperation::Install(requested.clone()))),
                ExtensionState::External => None,
            };
            let mut actions = Vec::new();
            if let Some((text, operation)) = main {
                let style = if matches!(&operation, ExtensionOperation::Install(_)) {
                    ButtonStyle::Primary
                } else {
                    ButtonStyle::Plain
                };
                actions.push(self.action_button(
                    format!("php-extension-main-{name}"), text, style, enabled,
                    Action::PhpExtension(formula.to_owned(), entry.extension, operation), colors, cx,
                ));
            }
            if entry.config.is_some() {
                actions.push(self.action_button(
                    format!("php-extension-config-{name}"), "打开配置", ButtonStyle::Plain, enabled,
                    Action::OpenPhpExtensionConfig(formula.to_owned(), entry.extension), colors, cx,
                ));
            }
            if !bundled && matches!(entry.state, ExtensionState::Enabled | ExtensionState::Disabled | ExtensionState::Broken) {
                actions.push(self.action_button(
                    format!("php-extension-uninstall-{name}"), "卸载", ButtonStyle::Plain, enabled,
                    Action::PhpExtension(formula.to_owned(), entry.extension, ExtensionOperation::Uninstall), colors, cx,
                ));
            }
            self.tool_row(name, label, actions, format!("php-extension-{name}"), colors, cx)
        }))
    }

    fn npm_tools(
        &self,
        target: &ToolRuntime,
        info: &NpmInfo,
        window: &Window,
        colors: Palette,
        cx: &Context<Self>,
    ) -> Div {
        let enabled = self.snapshot.busy.is_none();
        let package = self.runtime_input.text.trim().to_owned();
        card_list()
            .child(panel(colors).px_4().py_3().flex().flex_col().gap_2()
                .child(div().font_weight(FontWeight::SEMIBOLD).child(format!("npm {}", info.version)))
                .child(div().truncate().text_color(colors.muted).child(format!("全局安装路径  {}", info.prefix.display())))
                .child(div().flex().items_center().gap_2()
                    .child(self.runtime_input.field(
                        "npm-package", &self.runtime_focus, window, "包名或包名@版本", 48, colors,
                    ).on_key_down(cx.listener(|view, event, _, cx| view.runtime_input.key(event, cx))))
                    .child(self.action_button(
                        "npm-install", "全局安装", ButtonStyle::Primary, enabled && !package.is_empty(),
                        Action::NpmChange(target.clone(), package, true), colors, cx,
                    )),
                ),
            )
            .children(info.packages.is_empty().then(|| {
                panel(colors).p_4().text_color(colors.muted).child("没有已安装的全局工具")
            }))
            .children(info.packages.iter().map(|package| {
                let remove = self.action_button(
                    format!("npm-remove-{}", package.name), "卸载", ButtonStyle::Plain, enabled,
                    Action::NpmChange(target.clone(), package.name.clone(), false), colors, cx,
                );
                self.tool_row(package.name.clone(), package.version.clone(), vec![remove],
                    format!("npm-tool-{}", package.name), colors, cx)
            }))
    }

    fn rust_tools(
        &self,
        target: &ToolRuntime,
        components: &RustComponents,
        colors: Palette,
        cx: &Context<Self>,
    ) -> Div {
        let enabled = self.snapshot.busy.is_none();
        card_list().children([
            (RustComponent::Rustfmt, components.rustfmt),
            (RustComponent::Clippy, components.clippy),
        ].into_iter().map(|(component, installed)| {
            let name = component.label();
            let actions = (!installed && components.installable).then(|| self.action_button(
                format!("rust-install-{name}"), "安装", ButtonStyle::Primary, enabled,
                Action::InstallRustComponent(target.clone(), component), colors, cx,
            )).into_iter().collect();
            let status = if installed { "已安装" } else if components.installable { "未安装" } else { "当前工具链未提供" };
            self.tool_row(name, status, actions, format!("rust-component-{name}"), colors, cx)
        }))
    }

    fn developer_tools(&self, environment: Environment, colors: Palette, cx: &Context<Self>) -> Option<Div> {
        let tools = DeveloperTool::for_environment(environment);
        if tools.is_empty() || !self.snapshot.brew_available() {
            return None;
        }
        Some(section("开发工具", colors.muted).child(card_list().children(tools.iter().map(|tool| {
            let version = self.snapshot.developer_tools.get(tool.formula());
            let actions = version.is_none().then(|| self.action_button(
                format!("install-tool-{}", tool.formula()), "安装", ButtonStyle::Primary,
                self.snapshot.busy.is_none(), Action::InstallDeveloperTool(*tool), colors, cx,
            )).into_iter().collect();
            let status = version.map_or_else(|| "Homebrew · 未安装".into(), |version| format!("Homebrew · {version}"));
            self.tool_row(tool.label(), status, actions, format!("developer-tool-{}", tool.formula()), colors, cx)
        }))))
    }

    fn tool_row(
        &self,
        name: impl Into<SharedString>,
        summary: impl Into<SharedString>,
        actions: Vec<Stateful<Div>>,
        id: String,
        colors: Palette,
        cx: &Context<Self>,
    ) -> Div {
        let name: SharedString = name.into();
        let summary: SharedString = summary.into();
        panel(colors).flex().items_center().gap_3().px_4().py_2().min_h(px(44.))
            .child(div().flex_1().min_w_0().flex().items_center().gap_3()
                .child(div().truncate().font_weight(FontWeight::SEMIBOLD).child(name))
                .child(div().truncate().text_color(colors.muted).text_size(px(12.)).child(summary)),
            )
            .child(self.row_actions(id, actions, colors, cx))
    }

    fn service_operation(&self, service: &Service) -> Option<ServiceAction> {
        self.snapshot
            .service_operation
            .as_ref()
            .filter(|(formula, _)| *formula == service.formula)
            .map(|(_, operation)| *operation)
    }

    fn environment(
        &self,
        environment: Environment,
        window: &Window,
        colors: Palette,
        cx: &Context<Self>,
    ) -> Div {
        let mise_tools: Vec<_> = self
            .snapshot
            .mise_tools
            .iter()
            .filter(|tool| tool.environment == environment)
            .collect();
        let (installed, available): (Vec<_>, Vec<_>) = self
            .snapshot
            .formulae
            .iter()
            .filter(|formula| formula.environment == environment)
            .partition(|formula| !formula.installed_versions.is_empty());
        let missing_default = self
            .snapshot
            .brew_path
            .as_ref()
            .and_then(|_| self.snapshot.settings.defaults.get(environment.key()))
            .filter(|selected| self.snapshot.brew_inventory_error.is_none()
                && !installed.iter().any(|formula| &formula.name == *selected));

        let installed_list = (!installed.is_empty()
            || !mise_tools.is_empty()
            || missing_default.is_some())
        .then(|| {
            section("已安装版本", colors.muted).child(
                card_list()
                    .children(missing_default.map(|formula| {
                        panel(colors)
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .px_4()
                            .py_3()
                            .child(
                                div()
                                    .text_color(colors.muted)
                                    .child(format!("默认公式 {formula} 当前未安装")),
                            )
                            .child(self.action_button(
                                "clear-missing-default",
                                "清除默认",
                                ButtonStyle::Plain,
                                self.snapshot.busy.is_none(),
                                Action::ClearDefault(environment.key().into()),
                                colors,
                                cx,
                            ))
                    }))
                    .children(
                        installed
                            .iter()
                            .map(|formula| self.version(formula, colors, cx)),
                    )
                    .children(
                        mise_tools
                            .iter()
                            .map(|tool| self.mise_version_row(tool, colors, cx)),
                    ),
            )
        });
        let has_mise = self.snapshot.mise.is_some() && Mise::supports(environment);
        let available_list = (self.snapshot.brew_available()
            && environment != Environment::Elasticsearch
            && (!available.is_empty() || (installed_list.is_none() && !has_mise)))
            .then(|| {
                section("Homebrew 可安装版本", colors.muted).child(
                    card_list()
                        .children(available.is_empty().then(|| {
                            panel(colors)
                                .px_4()
                                .py_3()
                                .text_color(colors.muted)
                                .child("暂无可用版本")
                        }))
                        .children(
                            available
                                .iter()
                                .map(|formula| self.version(formula, colors, cx)),
                        ),
                )
            });
        div()
            .flex()
            .flex_col()
            .gap_5()
            .children((self.open_actions.as_ref() == Some(&RECENT_PROJECTS))
                .then(|| self.recent_projects(environment, colors, cx)))
            .children(self.manager_setup(Some(environment), window, colors, cx))
            .children([
                ("Homebrew", self.snapshot.brew_inventory_error.as_ref()
                    .filter(|_| environment != Environment::Elasticsearch)),
                ("Mise", self.snapshot.mise_inventory_errors.get(&environment)),
            ].into_iter().filter_map(|(source, error)| {
                error.map(|error| panel(colors).p_4().text_color(colors.muted)
                    .child(format!("{source} 安装清单待确认，请刷新。\n{error}")))
            }))
            .children((environment == Environment::Java).then(|| self.current_java(colors, cx)))
            .children(self.terminal_report(environment, colors, cx))
            .children(
                environment
                    .service_label()
                    .map(|label| self.services(environment, label, colors, cx)),
            )
            .children(installed_list)
            .children(has_mise.then(|| self.mise_install(environment, window, colors, cx)))
            .children((environment == Environment::Elasticsearch).then(|| {
                div().flex().items_center().gap_2()
                    .child(div().text_size(px(12.)).text_color(colors.muted).child("首次连接和账号配置可查看 Elastic 文档。"))
                    .child(button("elasticsearch-docs", "官方指南", ButtonStyle::Plain, true, colors)
                        .on_click(|_, _, cx| cx.open_url("https://www.elastic.co/docs/deploy-manage/deploy/self-managed/install-elasticsearch-from-archive-on-linux-macos")))
            }))
            .children(available_list)
            .children(self.developer_tools(environment, colors, cx))
            .children((environment == Environment::Nginx)
                .then(|| self.site_builder(window, colors, cx)))
            .children((environment == Environment::Php && self.snapshot.brew_available())
                .then(|| self.composer(colors, cx)))
    }

    fn current_java(&self, colors: Palette, cx: &Context<Self>) -> Div {
        let card = panel(colors).p_4().flex().flex_col().gap_2();
        let card = match &self.snapshot.current_java {
            CurrentJavaStatus::Idle | CurrentJavaStatus::Checking(_) => {
                card.child(div().text_color(colors.muted).child("正在读取 zsh 中的 Java…"))
            }
            CurrentJavaStatus::Ready(Some(java)) => {
                card.child(div().font_weight(FontWeight::SEMIBOLD)
                    .child(java.version.clone().unwrap_or_else(|| "Java".into())))
                    .children([
                        ("java", Some(java.executable.as_path())),
                        ("JAVA_HOME", java.java_home.as_deref()),
                    ].into_iter().filter_map(|(label, path)| {
                        let path = path?.display().to_string();
                        Some(div().flex().items_center().gap_2()
                            .child(div().flex_1().min_w_0().flex().flex_col()
                                .child(div().text_size(px(12.)).text_color(colors.muted).child(label))
                                .child(div().truncate().child(path.clone())))
                            .child(copy_button(format!("current-java-{label}"), "复制", path, colors, cx)))
                    }))
            }
            CurrentJavaStatus::Ready(None) => {
                card.child(div().text_color(colors.muted).child("zsh 未找到可用的 Java 命令"))
            }
            CurrentJavaStatus::Failed(error) => {
                card.child(div().flex().items_center().gap_2()
                    .child(div().flex_1().min_w_0().text_color(colors.muted)
                        .child(error.lines().next().unwrap_or("读取 Java 失败").to_owned()))
                    .child(self.action_button(
                        "retry-current-java", "重试", ButtonStyle::Plain, true,
                        Action::ReadCurrentJava, colors, cx,
                    )))
            }
        };
        section("终端当前使用", colors.muted).child(card)
    }

    fn project_check_button(
        &self,
        environment: Environment,
        colors: Palette,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        if self.snapshot.settings.recent_projects.is_empty() {
            return self.choose_project_button("check-project", "检查项目…", environment, colors, cx);
        }
        let open = self.open_actions.as_ref() == Some(&RECENT_PROJECTS);
        let style = if open { ButtonStyle::Selected } else { ButtonStyle::Plain };
        button("check-project", "检查项目…", style, true, colors)
            .on_click(cx.listener(move |view, _, _, cx| {
                view.open_actions = (!open).then_some(RECENT_PROJECTS);
                cx.notify();
            }))
    }

    fn choose_project_button(
        &self,
        id: &'static str,
        label: &str,
        environment: Environment,
        colors: Palette,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let enabled = self.snapshot.action_available(&Action::CheckTerminal(environment, None));
        let control = button(id, label, ButtonStyle::Plain, enabled, colors);
        if !enabled {
            return control;
        }
        control.on_click(cx.listener(move |_, _, _, cx| {
            let directory = cx.prompt_for_paths(PathPromptOptions {
                files: false, directories: true, multiple: false,
                prompt: Some("选择要检查终端版本的项目目录".into()),
            });
            cx.spawn(async move |view, cx| {
                if let Ok(Ok(Some(paths))) = directory.await
                    && let Some(path) = paths.into_iter().next()
                {
                    let _ = view.update(cx, |view, cx| {
                        if view.page == Page::Environment(environment) {
                            view.submit(Action::CheckTerminal(environment, Some(path)), cx);
                        }
                    });
                }
            }).detach();
        }))
    }

    fn recent_projects(&self, environment: Environment, colors: Palette, cx: &Context<Self>) -> Div {
        panel(colors).p_4().flex().flex_col().gap_2()
            .child(div().flex().items_center().justify_between()
                .child(div().font_weight(FontWeight::SEMIBOLD).child("最近检查的项目"))
                .child(self.choose_project_button("choose-project", "选择目录…", environment, colors, cx)))
            .children(self.snapshot.settings.recent_projects.iter().enumerate().map(|(index, directory)| {
                div().flex().items_center().gap_2()
                    .child(div().flex_1().min_w_0().truncate().child(directory.display().to_string()))
                    .child(self.action_button(format!("check-recent-{index}"), "检查", ButtonStyle::Plain, true,
                        Action::CheckTerminal(environment, Some(directory.clone())), colors, cx))
            }))
    }

    fn terminal_report(&self, environment: Environment, colors: Palette, cx: &Context<Self>) -> Option<Div> {
        let (_, report) = self.snapshot.terminal_report.as_ref().filter(|(env, _)| *env == environment)?;
        let collapsed = self.terminal_report_collapsed;
        let mut header = div().flex().items_center().gap_2()
            .child(div().flex_1().min_w_0().font_weight(FontWeight::SEMIBOLD).child("终端版本"));
        if !collapsed {
            header = header
                .child(copy_button("copy-terminal-report", "复制结果", report.clone(), colors, cx))
                .child(self.choose_project_button("diagnose-project", "检查项目目录…", environment, colors, cx));
        }
        Some(
            panel(colors).p_4().flex().flex_col().gap_2()
                .child(header.child(
                    button(
                        "toggle-terminal-report",
                        if collapsed { "展开" } else { "收起" },
                        ButtonStyle::Plain,
                        true,
                        colors,
                    )
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.terminal_report_collapsed = !view.terminal_report_collapsed;
                        cx.notify();
                    })),
                ))
                .children((!collapsed).then(|| report_text("terminal-report-text", report, colors))),
        )
    }

    fn site_builder(&self, window: &Window, colors: Palette, cx: &Context<Self>) -> Div {
        let (nginx, php) = (
            self.snapshot.formulae.iter().find(|formula| {
                formula.environment == Environment::Nginx && !formula.installed_versions.is_empty()
            }),
            self.snapshot.formulae.iter().filter(|formula| {
                formula.environment == Environment::Php && !formula.installed_versions.is_empty()
            }).collect::<Vec<_>>(),
        );
        let mut body = panel(colors).p_4().flex().flex_col().gap_2();
        if let Some(nginx) = nginx
            && let Some(php_formula) = php.iter().copied()
                .find(|formula| self.site_php.as_deref() == Some(formula.name.as_str()))
                .or_else(|| php.first().copied())
        {
            let port = self.site_port.text.trim().parse::<u16>().ok().filter(|port| *port >= 1024);
            let root_label = self.site_root.as_ref()
                .map(|root| root.display().to_string())
                .unwrap_or_else(|| "选择公开目录，例如项目的 public 文件夹".into());
            let form_action = self.site_root.clone().zip(port).map(|(root, port)| Action::CreateSite {
                nginx: nginx.name.clone(), php: php_formula.name.clone(), root, port,
            });
            body = body
                .child(div().flex().items_center().gap_2()
                    .child(div().flex_1().min_w_0().truncate().text_color(colors.muted).child(root_label))
                    .child(button("site-root", "选择目录…", ButtonStyle::Plain, true, colors)
                        .on_click(cx.listener(|_, _, _, cx| {
                            let directory = cx.prompt_for_paths(PathPromptOptions {
                                files: false, directories: true, multiple: false,
                                prompt: Some("选择站点要公开的目录".into()),
                            });
                            cx.spawn(async move |view, cx| {
                                if let Ok(Ok(Some(paths))) = directory.await
                                    && let Some(path) = paths.into_iter().next()
                                {
                                    let _ = view.update(cx, |view, cx| {
                                        view.site_root = Some(path);
                                        cx.notify();
                                    });
                                }
                            }).detach();
                        }))))
                .child(div().flex().items_center().gap_2()
                    .child(div().text_color(colors.muted).child("PHP-FPM"))
                    .child(div().flex().flex_wrap().gap_2().children(php.iter().map(|formula| {
                        let name = formula.name.clone();
                        button(
                            format!("site-{name}"), &name,
                            if name == php_formula.name { ButtonStyle::Selected } else { ButtonStyle::Plain },
                            true, colors,
                        ).on_click(cx.listener(move |view, _, _, cx| {
                            view.site_php = Some(name.clone());
                            cx.notify();
                        }))
                    }))))
                .child(div().flex().items_center().gap_2()
                    .child(div().text_color(colors.muted).child("本地端口"))
                    .child(self.site_port.field("site-port", &self.site_port_focus, window, "8081", 7, colors)
                        .on_key_down(cx.listener(|view, event, _, cx| view.site_port.key(event, cx))))
                    .child(match form_action {
                        Some(action) => self.action_button(
                            "create-site", "生成配置", ButtonStyle::Primary, true, action, colors, cx,
                        ),
                        None => button("create-site", "生成配置", ButtonStyle::Primary, false, colors),
                    }));
        } else {
            body = body.child(div().text_color(colors.muted)
                .child("通过 Homebrew 安装 Nginx 和 PHP 后，可为本地项目生成站点配置。"));
        }
        if let Some(site) = &self.snapshot.site {
            body = body
                .child(div().text_color(colors.muted).text_size(px(12.))
                    .child(format!("{}   {}", site.url, site.config.display())))
                .child(div().text_color(colors.muted).text_size(px(12.))
                    .child(format!("PHP-FPM 监听地址 {}", site.php_endpoint)))
                .child(div().text_color(colors.muted).text_size(px(12.))
                    .child("把 include 加到 Nginx 主配置的 http 块，检查通过后重启 Nginx，并启动所选的 PHP-FPM。"))
                .child(div().flex().flex_wrap().gap_2()
                    .child(self.action_button(
                        "open-site-config", "打开生成的配置", ButtonStyle::Plain, true, Action::OpenSite, colors, cx,
                    ))
                    .child(copy_button("copy-site-include", "复制 include", site.include.clone(), colors, cx))
                    .child(copy_button("copy-site-test", "复制检查命令", site.test_command.clone(), colors, cx))
                    .child(copy_button("copy-site-url", "复制地址", site.url.clone(), colors, cx)));
        }
        section("本地 PHP 站点", colors.muted).child(body)
    }

    fn mise_version_row(&self, tool: &MiseTool, colors: Palette, cx: &Context<Self>) -> Div {
        let path = tool
            .executable
            .as_ref()
            .or(tool.install_path.as_ref())
            .map(|path| path.display().to_string());
        let is_rust = tool.environment == Environment::Rust;
        let cargo = tool
            .cargo_executable
            .as_ref()
            .map(|path| path.display().to_string());
        let java_home = tool
            .executable
            .as_deref()
            .filter(|_| tool.environment == Environment::Java)
            .and_then(|path| path.parent()?.parent());
        let mut actions = Vec::new();
        if tool.environment != Environment::Elasticsearch
            && tool.active != Some(true)
            && tool.executable.is_some()
        {
            actions.push(self.action_button(
                format!("mise-global-{}-{}", tool.name, tool.version),
                "设为 Mise 全局",
                ButtonStyle::Plain,
                self.snapshot.busy.is_none(),
                Action::SetMiseGlobal(tool.environment, tool.name.clone(), tool.version.clone()),
                colors,
                cx,
            ));
        }
        if tool.executable.is_some() {
            let runtime = ToolRuntime::Mise {
                name: tool.name.clone(),
                version: tool.version.clone(),
            };
            let action = match tool.environment {
                Environment::Node => Some(("管理 npm 工具", RuntimeTarget::Npm(runtime))),
                Environment::Rust => Some(("管理组件", RuntimeTarget::Rust(runtime))),
                _ => None,
            };
            if let Some((label, target)) = action {
                actions.push(self.runtime_button(
                    format!("mise-tools-{}-{}", tool.name, tool.version),
                    label,
                    target,
                    colors,
                    cx,
                ));
            }
        }
        if let Some(path) = &path {
            actions.push(copy_button(
                format!("runtime-path-{}-{}", tool.name, tool.version),
                if is_rust && tool.executable.is_some() {
                    "复制 rustc"
                } else {
                    "复制路径"
                },
                path.clone(),
                colors,
                cx,
            ));
        }
        if let Some(path) = &cargo {
            actions.push(copy_button(
                format!("runtime-cargo-{}-{}", tool.name, tool.version),
                "复制 cargo",
                path.clone(),
                colors,
                cx,
            ));
        }
        if tool.active != Some(true) {
            actions.push(self.confirm_package(
                format!("mise-uninstall-{}-{}", tool.name, tool.version),
                "卸载",
                format!("{} {}（Mise）", tool.name, tool.version),
                Action::UninstallMise(tool.environment, tool.name.clone(), tool.version.clone()),
                colors,
                cx,
            ));
        }
        panel(colors)
            .flex()
            .items_center()
            .gap_3()
            .px_4()
            .py_2()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(tool.version.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(colors.muted)
                                    .child("Mise"),
                            )
                            .children((tool.active == Some(true)).then(|| {
                                div()
                                    .text_size(px(11.))
                                    .rounded_sm()
                                    .px_2()
                                    .bg(colors.accent_soft)
                                    .text_color(colors.accent)
                                    .child("Mise 全局")
                            }))
                            .children(tool.active.is_none().then(|| {
                                div().text_size(px(11.)).text_color(colors.muted).child("全局状态待确认")
                            })),
                    )
                    .children(path.as_ref().map(|path| {
                        div()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(colors.muted)
                            .child(if tool.executable.is_none() {
                                format!("安装目录  {path}")
                            } else if is_rust {
                                format!("rustc  {path}")
                            } else {
                                path.clone()
                            })
                    }))
                    .children(cargo.as_ref().map(|path| {
                        div()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(colors.muted)
                            .child(format!("cargo  {path}"))
                    }))
                    .children(java_home.map(|home| {
                        div()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(colors.muted)
                            .child(format!("JAVA_HOME  {}", home.display()))
                    })),
            )
            .child(self.row_actions(
                format!("mise-actions-{}-{}", tool.name, tool.version),
                actions,
                colors,
                cx,
            ))
    }

    fn mise_install(
        &self,
        environment: Environment,
        window: &Window,
        colors: Palette,
        cx: &Context<Self>,
    ) -> Div {
        section("Mise 安装", colors.muted).child(
            panel(colors)
                .flex()
                .flex_col()
                .gap_2()
                .p_4()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            self.mise_version
                                .field(
                                    "mise-version",
                                    &self.mise_focus,
                                    window,
                                    "latest",
                                    48,
                                    colors,
                                )
                                .on_key_down(cx.listener(Self::mise_key)),
                        )
                        .child(
                            button(
                                "mise-browse",
                                if self.mise_list_open {
                                    "收起版本"
                                } else {
                                    "选择版本"
                                },
                                if self.mise_list_open {
                                    ButtonStyle::Selected
                                } else {
                                    ButtonStyle::Plain
                                },
                                true,
                                colors,
                            )
                            .h(INPUT_HEIGHT)
                            .on_click(cx.listener(
                                move |view, _, window, cx| {
                                    view.mise_list_open = !view.mise_list_open;
                                    if view.mise_list_open {
                                        view.mise_search = LineEditor::default();
                                        view.mise_list_scroll = UniformListScrollHandle::new();
                                        view.mise_search_focus.focus(window);
                                        if !matches!(
                                            view.mise_catalogs.get(&environment),
                                            Some(MiseCatalog::Loading)
                                        ) {
                                            view.load_mise_catalog(environment, cx);
                                        }
                                    }
                                    cx.notify();
                                },
                            )),
                        )
                        .child(self.action_button(
                            "mise-install",
                            "安装",
                            ButtonStyle::Primary,
                            self.snapshot.busy.is_none(),
                            Action::InstallMise(environment, self.mise_version.version()),
                            colors,
                            cx,
                        ).h(INPUT_HEIGHT)),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(colors.muted)
                        .child(match environment {
                            Environment::Elasticsearch => "选择单节点服务版本，留空安装 latest。",
                            Environment::Nginx => "Mise 通过 conda:nginx 提供多个版本。全局版本用于终端，页面上的启停按钮管理 Homebrew 服务。",
                            env if env.service_label().is_some() => "Mise 全局版本用于终端，页面上的启停按钮管理 Homebrew 服务。留空安装 latest。",
                            _ => "升级可选择新版本安装，再切换全局版本。输入版本号或渠道，留空安装 latest。",
                        }),
                )
                .children(
                    self.mise_list_open
                        .then(|| self.mise_catalog(environment, window, colors, cx)),
                ),
        )
    }

    fn load_mise_catalog(&mut self, environment: Environment, cx: &mut Context<Self>) {
        let Some(mise) = self.snapshot.mise.clone() else {
            return;
        };
        self.mise_catalogs.insert(environment, MiseCatalog::Loading);
        let proxy = self.snapshot.settings.proxy.clone();
        let lookup = cx
            .background_executor()
            .spawn(async move { mise.available_versions(environment, Some(&proxy)) });
        cx.spawn(async move |view, cx| {
            let catalog = match lookup.await {
                Ok(versions) => MiseCatalog::Ready(versions),
                Err(error) => MiseCatalog::Failed(format!("{error:#}")),
            };
            let _ = view.update(cx, |view, cx| {
                view.mise_catalogs.insert(environment, catalog);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn mise_catalog(
        &self,
        environment: Environment,
        window: &Window,
        colors: Palette,
        cx: &Context<Self>,
    ) -> Div {
        let content = panel(colors)
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .bg(colors.background);
        let versions = match self.mise_catalogs.get(&environment) {
            Some(MiseCatalog::Ready(versions)) => versions,
            Some(MiseCatalog::Failed(error)) => {
                let details = error.clone();
                return content.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(colors.danger)
                                .child(error.lines().next().unwrap_or_default().to_owned()),
                        )
                        .child(copy_button("mise-copy-error", "复制详情", details, colors, cx))
                        .child(
                            button("mise-retry", "重试", ButtonStyle::Plain, true, colors)
                                .on_click(cx.listener(move |view, _, _, cx| {
                                    view.load_mise_catalog(environment, cx)
                                })),
                        ),
                );
            }
            _ => return content.child(div().text_color(colors.muted).child("正在读取 Mise 版本…")),
        };
        let query = self.mise_search.text.trim();
        let versions: Vec<_> = versions
            .iter()
            .filter(|version| version.contains(query))
            .cloned()
            .collect();
        let content = content.child(
            self.mise_search
                .field(
                    "mise-search",
                    &self.mise_search_focus,
                    window,
                    "搜索版本号",
                    48,
                    colors,
                )
                .on_key_down(cx.listener(Self::mise_search_key)),
        );
        if versions.is_empty() {
            return content.child(
                div()
                    .py_2()
                    .text_color(colors.muted)
                    .child("没有匹配的版本"),
            );
        }

        let installed: BTreeSet<_> = self
            .snapshot
            .mise_tools
            .iter()
            .filter(|tool| tool.environment == environment)
            .map(|tool| tool.version.clone())
            .collect();
        let visible_rows = versions.len().min(6) as f32;
        content.child(
            uniform_list(
                "mise-versions",
                versions.len(),
                cx.processor(move |_, range: std::ops::Range<usize>, _, cx| {
                    versions[range]
                        .iter()
                        .map(|version| {
                            let is_installed = installed.contains(version);
                            let row = div()
                                .id(SharedString::from(version.clone()))
                                .h(px(32.))
                                .px_3()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(version.clone());
                            if is_installed {
                                row.child(
                                    div()
                                        .text_color(colors.muted)
                                        .text_size(px(12.))
                                        .child("已安装"),
                                )
                            } else {
                                let selected = version.clone();
                                row.cursor_pointer()
                                    .hover(move |style| style.bg(colors.control))
                                    .on_click(cx.listener(move |view, _, window, cx| {
                                        view.mise_version.replace_all(&selected);
                                        view.mise_list_open = false;
                                        view.mise_focus.focus(window);
                                        cx.notify();
                                    }))
                            }
                        })
                        .collect()
                }),
            )
            .track_scroll(self.mise_list_scroll.clone())
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .h(px(32. * visible_rows))
            .rounded_md()
            .bg(colors.surface),
        )
    }

    fn mise_search_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key == "escape" {
            self.mise_list_open = false;
            self.mise_focus.focus(window);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        let previous = self.mise_search.text.clone();
        self.mise_search.key(event, cx);
        if self.mise_search.text != previous {
            self.mise_list_scroll
                .scroll_to_item_strict(0, ScrollStrategy::Top);
        }
    }

    fn composer(&self, colors: Palette, cx: &Context<Self>) -> Div {
        let formulas: Vec<_> = self
            .snapshot
            .formulae
            .iter()
            .filter(|formula| formula.environment == Environment::Composer)
            .collect();
        let missing_default =
            self.snapshot
                .settings
                .defaults
                .get("composer")
                .is_some_and(|selected| {
                    !formulas.iter().any(|formula| {
                        &formula.name == selected && !formula.installed_versions.is_empty()
                    })
                });
        section("Composer", colors.muted).child(
            card_list()
                .children(missing_default.then(|| {
                    panel(colors)
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_3()
                        .px_4()
                        .py_2()
                        .child(
                            div()
                                .text_color(colors.muted)
                                .child("默认 Composer 当前未安装"),
                        )
                        .child(self.action_button(
                            "clear-missing-composer",
                            "清除默认",
                            ButtonStyle::Plain,
                            self.snapshot.busy.is_none(),
                            Action::ClearDefault("composer".into()),
                            colors,
                            cx,
                        ))
                }))
                .children((formulas.is_empty() && !missing_default).then(|| {
                    panel(colors)
                        .px_4()
                        .py_3()
                        .text_color(colors.muted)
                        .child("暂无可用版本")
                }))
                .children(
                    formulas
                        .iter()
                        .map(|formula| self.version(formula, colors, cx)),
                ),
        )
    }

    fn version(&self, formula: &Formula, colors: Palette, cx: &Context<Self>) -> Div {
        let enabled = self.snapshot.busy.is_none() || self.snapshot.service_operation.is_some();
        let installed = !formula.installed_versions.is_empty();
        let install_path = formula
            .install_path
            .as_ref()
            .map(|path| path.display().to_string());
        let versions = if installed {
            let local = formula.installed_versions.join(" · ");
            match &formula.version {
                Some(catalog) if formula.installable && !formula.installed_versions.contains(catalog) => {
                    format!("已安装 {local} · 目录版本 {catalog}")
                }
                _ => local,
            }
        } else {
            formula.version.clone().unwrap_or_else(|| "版本未知".into())
        };
        let key = formula.environment.key();
        let is_default =
            installed && self.snapshot.settings.defaults.get(key) == Some(&formula.name);
        let action = if installed && formula.environment.supports_default() {
            Some(if is_default {
                (
                    "清除默认",
                    ButtonStyle::Plain,
                    Action::ClearDefault(key.into()),
                )
            } else {
                (
                    "设为默认",
                    ButtonStyle::Plain,
                    Action::SetDefault(formula.name.clone()),
                )
            })
        } else if !installed && formula.installable {
            Some((
                "安装",
                ButtonStyle::Primary,
                Action::Install(formula.name.clone()),
            ))
        } else {
            None
        };
        let mut actions = Vec::new();
        if let Some((label, style, action)) = action {
            actions.push(self.action_button(
                format!("version-action-{}", formula.name),
                label,
                style,
                enabled,
                action,
                colors,
                cx,
            ));
        }
        if installed {
            let action = match formula.environment {
                Environment::Php => Some(("管理扩展", RuntimeTarget::Php(formula.name.clone()))),
                Environment::Node => Some((
                    "管理 npm 工具",
                    RuntimeTarget::Npm(ToolRuntime::Brew(formula.name.clone())),
                )),
                Environment::Rust => Some((
                    "管理组件",
                    RuntimeTarget::Rust(ToolRuntime::Brew(formula.name.clone())),
                )),
                _ => None,
            };
            if let Some((label, target)) = action {
                actions.push(self.runtime_button(
                    format!("brew-tools-{}", formula.name),
                    label,
                    target,
                    colors,
                    cx,
                ));
            }
        }
        if let Some(path) = &install_path {
            actions.push(copy_button(
                format!("brew-path-{}", formula.name),
                "复制路径",
                path.clone(),
                colors,
                cx,
            ));
        }
        if installed && formula.environment == Environment::Php {
            actions.push(self.action_button(
                format!("ini-{}", formula.name),
                "打开 php.ini",
                ButtonStyle::Plain,
                enabled,
                Action::OpenConfig(formula.name.clone(), ConfigKind::PhpIni),
                colors,
                cx,
            ));
        }
        if installed && formula.installable {
            actions.push(self.confirm_package(
                format!("brew-upgrade-{}", formula.name), "升级", formula.name.clone(),
                Action::ChangeBrew(formula.name.clone(), PackageChange::Upgrade), colors, cx,
            ));
        }
        if installed && !is_default {
            actions.push(self.confirm_package(
                format!("brew-uninstall-{}", formula.name), "卸载", formula.name.clone(),
                Action::ChangeBrew(formula.name.clone(), PackageChange::Uninstall), colors, cx,
            ));
        }
        panel(colors)
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .px_4()
            .py_2()
            .min_h(px(44.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(formula.name.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(colors.muted)
                                    .child(versions),
                            )
                            .children(installed.then(|| {
                                div()
                                    .text_size(px(12.))
                                    .text_color(colors.muted)
                                    .child("Homebrew")
                            }))
                            .children(is_default.then(|| {
                                div()
                                    .rounded_sm()
                                    .bg(colors.accent_soft)
                                    .text_color(colors.accent)
                                    .text_size(px(11.))
                                    .px_2()
                                    .child("终端默认")
                            })),
                    )
                    .children(install_path.as_ref().map(|path| {
                        div()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(colors.muted)
                            .child(format!("安装目录  {path}"))
                    })),
            )
            .child(self.row_actions(
                format!("brew-actions-{}", formula.name),
                actions,
                colors,
                cx,
            ))
    }

    fn shell_preferences(&self, colors: Palette, cx: &Context<Self>) -> Div {
        let store = SettingsStore::discover().ok();
        section("终端配置", colors.muted).child(card_list().children(
            ShellFile::ALL.into_iter().map(|file| {
                let path = store.as_ref().map(|store| store.shell_file(file));
                let purpose = match file {
                    ShellFile::Env => "所有 zsh 会话的环境变量",
                    ShellFile::Profile => "登录时加载的 PATH 和环境变量",
                    ShellFile::Rc => "交互式终端的别名和工具初始化",
                };
                panel(colors)
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .py_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(file.name()))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(colors.muted)
                                    .child(purpose),
                            ),
                    )
                    .child(self.action_button(
                        format!("edit-zsh-{}", file.name()),
                        if path.as_ref().is_some_and(|path| path.exists()) {
                            "编辑"
                        } else {
                            "新建并编辑"
                        },
                        ButtonStyle::Plain,
                        self.snapshot.busy.is_none() && path.is_some(),
                        Action::EditShellFile(file),
                        colors,
                        cx,
                    ))
            }),
        ))
    }

    fn services(
        &self,
        environment: Environment,
        label: &'static str,
        colors: Palette,
        cx: &Context<Self>,
    ) -> Div {
        let mut services = self
            .snapshot
            .services
            .iter()
            .filter(|service| service.environment == environment)
            .peekable();
        let empty = services.peek().is_none();
        let title = match environment {
            Environment::Elasticsearch => "本地服务",
            Environment::Php => "Homebrew · PHP-FPM",
            _ => "Homebrew 服务",
        };
        section(title, colors.muted).child(
            card_list()
                .children(empty.then(|| {
                    panel(colors)
                        .px_4()
                        .py_3()
                        .text_color(colors.muted)
                        .child(format!("未检测到已安装的 {label} 服务"))
                }))
                .children(services.map(|service| self.service(service, colors, cx))),
        )
    }

    fn service(&self, service: &Service, colors: Palette, cx: &Context<Self>) -> Div {
        let formula = &service.formula;
        let failed = service.status.starts_with("error") || service.status == "failed";
        let (status, status_color) = if let Some(operation) = self.service_operation(service) {
            let label = match operation {
                ServiceAction::Start => "正在启动",
                ServiceAction::Stop => "正在停止",
                ServiceAction::Restart => "正在重启",
                ServiceAction::SetAutoStart(_) => "正在更新",
            };
            (label, colors.accent)
        } else if service.running {
            ("运行中", colors.success)
        } else if failed {
            ("异常", colors.danger)
        } else if service.status == "unknown" {
            ("状态未知", colors.muted)
        } else if service.loaded {
            ("已载入", colors.muted)
        } else {
            ("已停止", colors.muted)
        };
        let action = service.toggle_action();
        let (verb, style) = if action == ServiceAction::Stop {
            ("停止", ButtonStyle::Plain)
        } else {
            ("启动", ButtonStyle::Primary)
        };
        let mut actions = Vec::new();
        if service.manageable {
            actions.push(self.action_button(
                format!("toggle-{formula}"),
                verb,
                style,
                true,
                Action::Service(formula.clone(), action),
                colors,
                cx,
            ));
            actions.push(self.action_button(
                format!("restart-{formula}"),
                "重启",
                ButtonStyle::Plain,
                true,
                Action::Service(formula.clone(), ServiceAction::Restart),
                colors,
                cx,
            ));
        }
        actions.push(self.action_button(
            format!("config-{formula}"),
            if service.environment == Environment::Php {
                "打开 FPM 配置"
            } else {
                "打开配置"
            },
            ButtonStyle::Plain,
            true,
            Action::OpenConfig(formula.clone(), ConfigKind::Service),
            colors,
            cx,
        ));
        actions.push(self.action_button(
            format!("diagnose-{formula}"), "日志与端口", ButtonStyle::Plain, true,
            Action::DiagnoseService(formula.clone()), colors, cx,
        ));
        if service.can_perform(ServiceAction::SetAutoStart(!service.auto_start)) {
            actions.push(self.action_button(
                format!("auto-{formula}"),
                if service.auto_start {
                    "登录启动 · 开"
                } else {
                    "登录启动 · 关"
                },
                if service.auto_start {
                    ButtonStyle::Selected
                } else {
                    ButtonStyle::Plain
                },
                true,
                Action::Service(formula.clone(), ServiceAction::SetAutoStart(!service.auto_start)),
                colors,
                cx,
            ));
        }
        let controls = div()
            .flex()
            .items_center()
            .gap_2()
            .px_4()
            .py_2()
            .min_h(px(48.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(formula.clone()),
            )
            .child(
                div()
                    .flex()
                    .w(px(66.))
                    .flex_shrink_0()
                    .items_center()
                    .gap_1()
                    .text_size(px(12.))
                    .text_color(status_color)
                    .child(dot(status_color))
                    .child(status),
            )
            .child(self.row_actions(format!("service-actions-{formula}"), actions, colors, cx))
            .children((!service.manageable).then(|| {
                div()
                    .text_size(px(12.))
                    .text_color(colors.muted)
                    .child("系统级服务")
            }));
        panel(colors)
            .child(controls)
            .child(self.service_instance(service, colors, cx))
            .children(self.snapshot.service_report.as_ref()
                .filter(|(selected, _)| selected == formula)
                .map(|(_, report)| {
                    div().mx_4().mb_3().p_3().rounded_md().bg(colors.background).flex().flex_col().gap_2()
                        .child(div().flex().justify_end().gap_2()
                            .children(report.connection.as_ref().map(|connection| copy_button(
                                format!("connection-{formula}"), "复制连接命令", connection.clone(), colors, cx,
                            )))
                            .child(copy_button(
                                format!("copy-report-{formula}"), "复制结果", report.details.clone(), colors, cx,
                            )))
                        .child(report_text(format!("diagnostics-{formula}"), &report.details, colors))
                }))
            .children(
                self.snapshot
                    .service_paths
                    .as_ref()
                    .filter(|(shown, _)| shown == formula)
                    .map(|(_, paths)| self.service_paths(service, paths, colors, cx)),
            )
    }

    fn service_instance(&self, service: &Service, colors: Palette, cx: &Context<Self>) -> Div {
        let formula = &service.formula;
        let managed = self.snapshot.settings.managed_services.contains(formula);
        let can_switch = !service.running && !service.loaded && !service.auto_start;
        let paths_open = self
            .snapshot
            .service_paths
            .as_ref()
            .is_some_and(|(shown, _)| shown == formula);
        let mut actions = vec![self.action_button(
            format!("instance-paths-{formula}"),
            if paths_open { "收起路径" } else { "查看路径" },
            ButtonStyle::Plain,
            true,
            Action::ShowPaths(formula.clone()),
            colors,
            cx,
        )];
        if managed {
            actions.push(self.action_button(
                format!("instance-folder-{formula}"),
                "打开目录",
                ButtonStyle::Plain,
                true,
                Action::OpenInstance(formula.clone()),
                colors,
                cx,
            ));
        }
        if service.supports_instance_mode() {
            actions.push(self.action_button(
                format!("instance-mode-{formula}"),
                if !can_switch {
                    "停服并关闭登录启动后可切换"
                } else if managed {
                    "切回 Homebrew 配置"
                } else {
                    "启用独立配置"
                },
                ButtonStyle::Plain,
                can_switch,
                Action::SetManaged(formula.clone(), !managed),
                colors,
                cx,
            ));
        }
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_4()
            .pb_2()
            .child(
                div()
                    .flex_1()
                    .text_size(px(12.))
                    .text_color(colors.muted)
                    .child(
                        if service.backend == ServiceBackend::MiseLaunchd || managed {
                            "独立实例"
                        } else {
                            "Homebrew 配置"
                        },
                    ),
            )
            .child(self.row_actions(format!("instance-actions-{formula}"), actions, colors, cx))
    }

    fn service_paths(
        &self,
        service: &Service,
        paths: &ServicePaths,
        colors: Palette,
        cx: &Context<Self>,
    ) -> Div {
        let items = [
            ("程序", Some(paths.executable.as_path())),
            ("配置文件", paths.config.as_deref()),
            (
                if service.environment == Environment::Nginx {
                    "站点目录"
                } else {
                    "数据目录"
                },
                paths.data.as_deref(),
            ),
            ("FPM 的 php.ini", paths.php_ini.as_deref()),
        ];
        div()
            .mx_4()
            .mb_3()
            .p_3()
            .rounded_md()
            .bg(colors.background)
            .flex()
            .flex_col()
            .gap_2()
            .children(
                items
                    .into_iter()
                    .enumerate()
                    .filter_map(|(index, (title, path))| {
                        let path = path?.display().to_string();
                        let copy = path.clone();
                        Some(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_size(px(12.))
                                        .text_color(colors.muted)
                                        .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
                                        .child(div().text_color(colors.ink).child(path)),
                                )
                                .children((index == 3).then(|| {
                                    self.action_button(
                                        format!("fpm-ini-{}", service.formula),
                                        "打开",
                                        ButtonStyle::Plain,
                                        self.snapshot.busy.is_none(),
                                        Action::OpenConfig(
                                            service.formula.clone(),
                                            ConfigKind::FpmIni,
                                        ),
                                        colors,
                                        cx,
                                    )
                                }))
                                .child(copy_button(
                                    format!("copy-service-path-{}-{index}", service.formula),
                                    "复制", copy, colors, cx,
                                )),
                        )
                    }),
            )
            .children(paths.initial_credentials.then(|| {
                div().child(self.action_button(
                    format!("instance-password-{}", service.formula),
                    "初始凭据",
                    ButtonStyle::Plain,
                    self.snapshot.busy.is_none(),
                    Action::OpenCredentials(service.formula.clone()),
                    colors,
                    cx,
                ))
            }))
    }

    fn sidebar_preferences(&self, colors: Palette, cx: &Context<Self>) -> Div {
        let rows = Environment::SIDEBAR.chunks(2).map(|row| {
            div()
                .flex()
                .gap_1()
                .children(row.iter().map(|&environment| {
                    let visible = self.snapshot.environment_visible(environment);
                    panel(colors)
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_4()
                        .py_2()
                        .child(environment_icon(environment, 23.).bg(colors.control))
                        .child(div().flex_1().truncate().child(environment.label()))
                        .child(
                            button(
                                format!("visibility-{}", environment.key()),
                                if visible { "已显示" } else { "已隐藏" },
                                if visible {
                                    ButtonStyle::Selected
                                } else {
                                    ButtonStyle::Plain
                                },
                                true,
                                colors,
                            )
                            .on_click(cx.listener(
                                move |view, _, _, cx| {
                                    let visible = view.snapshot.environment_visible(environment);
                                    view.controller.set_preference(Preference::Visibility(
                                        environment,
                                        !visible,
                                    ));
                                    view.sync(cx);
                                },
                            )),
                        )
                }))
        });
        section("侧栏显示", colors.muted).child(card_list().children(rows))
    }

    fn theme_preferences(&self, colors: Palette, cx: &Context<Self>) -> Div {
        let selected = self.snapshot.theme();
        section("外观", colors.muted).child(
            panel(colors)
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .p_4()
                .child(div().font_weight(FontWeight::SEMIBOLD).child("主题"))
                .child(
                    div().flex().gap_1().children(
                        [
                            (Theme::System, "theme-system", "跟随系统"),
                            (Theme::Light, "theme-light", "浅色"),
                            (Theme::Dark, "theme-dark", "深色"),
                        ]
                        .into_iter()
                        .map(|(theme, id, label)| {
                            button(
                                id,
                                label,
                                if selected == theme {
                                    ButtonStyle::Selected
                                } else {
                                    ButtonStyle::Plain
                                },
                                true,
                                colors,
                            )
                            .on_click(cx.listener(
                                move |view, _, _, cx| {
                                    if theme != view.snapshot.theme() {
                                        view.controller.set_preference(Preference::Theme(theme));
                                        view.sync(cx);
                                    }
                                },
                            ))
                        }),
                    ),
                ),
        )
    }

    fn manager_setup(
        &self,
        environment: Option<Environment>,
        window: &Window,
        colors: Palette,
        cx: &Context<Self>,
    ) -> Option<Div> {
        if !self.snapshot.managers_checked {
            return (!self.snapshot.brew_available() && self.snapshot.mise.is_none())
                .then(|| panel(colors).p_4().child("正在后台检测 Homebrew 和 Mise…"));
        }
        let brew_missing = !self.snapshot.brew_available();
        let mise_missing = self.snapshot.mise.is_none();
        let show_both = environment.is_none() || (brew_missing && mise_missing);
        let cards = [
            (
                Manager::Homebrew,
                show_both || (brew_missing && environment != Some(Environment::Elasticsearch)),
            ),
            (
                Manager::Mise,
                show_both || (mise_missing && environment.is_some_and(Mise::supports)),
            ),
        ];
        cards.iter().any(|(_, show)| *show).then(|| {
            section("安装工具", colors.muted).child(
                card_list().gap_2().children(
                    cards
                        .into_iter()
                        .filter(|(_, show)| *show)
                        .map(|(manager, _)| {
                            self.manager_card(manager, environment.is_some(), window, colors, cx)
                        }),
                ),
            )
        })
    }

    fn manager_card(
        &self,
        manager: Manager,
        compact_actions: bool,
        window: &Window,
        colors: Palette,
        cx: &Context<Self>,
    ) -> Div {
        let (name, path, description, label, action, manual, docs) = match manager {
            Manager::Homebrew => (
                "Homebrew",
                self.snapshot.brew_path.as_deref(),
                "在系统终端按提示安装，可能需要管理员密码。完成后会自动检测。",
                "在终端安装",
                Action::OpenHomebrewInstaller,
                setup::HOMEBREW_MANUAL,
                setup::HOMEBREW_DOCS,
            ),
            Manager::Mise => (
                "Mise",
                self.snapshot.mise.as_ref().map(Mise::path),
                "可独立安装，用于管理语言运行时和 Nginx、MySQL 等工具的版本。",
                "安装 Mise",
                Action::InstallMiseManager,
                setup::MISE_MANUAL,
                setup::MISE_DOCS,
            ),
        };
        let broken = matches!(manager, Manager::Homebrew) && self.snapshot.brew_detection_failed;
        let card = panel(colors)
            .flex()
            .flex_col()
            .items_start()
            .gap_2()
            .p_4()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(name))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(if path.is_some() && !broken {
                                colors.success
                            } else {
                                colors.muted
                            })
                            .child(if broken {
                                "检测失败"
                            } else if path.is_some() {
                                "已就绪"
                            } else {
                                "未检测到"
                            }),
                    ),
            );
        if let Some(path) = path.filter(|_| !broken) {
            let path = path.to_string_lossy();
            let version = match manager {
                Manager::Homebrew => self.snapshot.brew_details.as_ref()
                    .and_then(|details| details.version.as_deref()),
                Manager::Mise => self.snapshot.mise_version.as_deref(),
            };
            return card
                .children(version.map(|version| {
                    div().text_size(px(12.)).text_color(colors.muted).child(version.to_owned())
                }))
                .child(
                    div()
                        .max_w_full()
                        .truncate()
                        .text_size(px(12.))
                        .text_color(colors.muted)
                        .child(path.to_string()),
                )
                .children((!compact_actions).then(|| match manager {
                    Manager::Homebrew => self.brew_configuration(window, colors, cx),
                    Manager::Mise => self.mise_configuration(colors, cx),
                }));
        }
        let mut actions = Vec::new();
        if !broken {
            actions.push(self.action_button(
                format!("install-manager-{name}"),
                label,
                ButtonStyle::Primary,
                self.snapshot.busy.is_none(),
                action,
                colors,
                cx,
            ));
            actions.push(copy_button(
                format!("copy-manager-{name}"),
                "复制安装命令",
                manual.to_owned(),
                colors,
                cx,
            ));
        }
        actions.push(
            button(
                format!("manager-docs-{name}"),
                "官方步骤",
                ButtonStyle::Plain,
                true,
                colors,
            )
            .on_click(cx.listener(move |view, _, _, cx| {
                view.close_actions(cx);
                cx.open_url(docs);
            })),
        );
        let actions = if compact_actions {
            self.row_actions(format!("manager-actions-{name}"), actions, colors, cx)
        } else {
            div().flex().flex_wrap().gap_2().children(actions)
        };
        card.child(div().text_color(colors.muted).child(if broken {
            "已找到 Homebrew，但无法读取安装路径。查看底部错误详情后重新检测。"
        } else {
            description
        }))
        .child(actions)
    }

    fn brew_preferences_draft(&self) -> BrewPreferences {
        BrewPreferences {
            api_mirror: self.brew_api_mirror.text.trim().to_owned(),
            bottle_mirror: self.brew_bottle_mirror.text.trim().to_owned(),
            disable_auto_update: self.brew_disable_auto_update,
        }
    }

    fn brew_configuration(&self, window: &Window, colors: Palette, cx: &Context<Self>) -> Div {
        let Some(details) = &self.snapshot.brew_details else {
            return div();
        };
        let body = div().w_full().flex().flex_col().gap_2()
            .child(div().text_size(px(12.)).text_color(colors.muted)
                .child(format!("安装前缀  {}", details.prefix.display())))
            .child(div().flex().flex_wrap().gap_2()
                .child(self.action_button(
                    "open-brew-config", "打开 brew.env", ButtonStyle::Plain, details.config.is_ok(),
                    Action::OpenBrewConfig, colors, cx,
                ))
                .child(self.action_button(
                    "update-homebrew", "更新 Homebrew", ButtonStyle::Plain, true,
                    Action::UpdateHomebrew, colors, cx,
                )));
        let config = match &details.config {
            Ok(config) => config,
            Err(error) => return body.child(div().text_size(px(12.)).text_color(colors.danger).child(error.clone())),
        };
        let body = body.child(div().truncate().text_size(px(12.)).text_color(colors.muted)
            .child(config.path.display().to_string()));
        let saved = match &config.preferences {
            Ok(saved) => saved,
            Err(error) => return body.child(div().text_size(px(12.)).text_color(colors.danger).child(error.clone())),
        };
        let draft = self.brew_preferences_draft();
        let changed = draft != *saved;
        let columns = ((window.viewport_size().width / px(1.) - 450.) / 7.8).clamp(18., 75.) as usize;
        body
            .child(div().flex().items_center().gap_2()
                .child(div().w(px(108.)).flex_shrink_0().child("软件索引镜像"))
                .child(self.brew_api_mirror.field(
                    "brew-api-mirror", &self.brew_api_focus, window, "沿用 Homebrew 配置", columns, colors,
                ).on_key_down(cx.listener(|view, event, _, cx| view.brew_api_mirror.key(event, cx)))))
            .child(div().flex().items_center().gap_2()
                .child(div().w(px(108.)).flex_shrink_0().child("预编译包镜像"))
                .child(self.brew_bottle_mirror.field(
                    "brew-bottle-mirror", &self.brew_bottle_focus, window, "沿用 Homebrew 配置", columns, colors,
                ).on_key_down(cx.listener(|view, event, _, cx| view.brew_bottle_mirror.key(event, cx)))))
            .child(div().flex().items_center().gap_2()
                .child(div().w(px(108.)).flex_shrink_0().child("安装前自动更新"))
                .child(button("brew-auto-default", "沿用配置",
                    if draft.disable_auto_update { ButtonStyle::Plain } else { ButtonStyle::Selected },
                    true, colors,
                ).on_click(cx.listener(|view, _, _, cx| {
                    view.brew_disable_auto_update = false;
                    cx.notify();
                })))
                .child(button("brew-auto-disabled", "关闭",
                    if draft.disable_auto_update { ButtonStyle::Selected } else { ButtonStyle::Plain },
                    true, colors,
                ).on_click(cx.listener(|view, _, _, cx| {
                    view.brew_disable_auto_update = true;
                    cx.notify();
                })))
                .child(div().flex_1())
                .child(self.action_button(
                    "save-brew-preferences", "保存", ButtonStyle::Primary, changed,
                    Action::SaveBrewPreferences(draft), colors, cx,
                )))
            .child(div().text_size(px(12.)).text_color(colors.muted)
                .child("保存到用户 brew.env，终端里的 brew 也会使用这些设置。镜像留空时沿用已有配置。"))
    }

    fn mise_configuration(&self, colors: Palette, cx: &Context<Self>) -> Div {
        let Some(mise) = &self.snapshot.mise else {
            return div();
        };
        let open = self.snapshot.mise_configs.is_some();
        let activate = format!("eval \"$({} activate zsh)\"", shell_quote(&mise.path().to_string_lossy()));
        div().w_full().flex().flex_col().gap_2()
            .child(div().text_size(px(12.)).text_color(colors.muted)
                .child(format!("工具安装目录  {}", mise.installs_directory().display())))
            .child(div().text_size(px(12.)).text_color(colors.muted)
                .child(format!("全局配置  {}", mise.global_config_path().display())))
            .child(div().flex().flex_wrap().gap_2()
                .child(self.action_button(
                    "open-mise-global", "打开全局配置", ButtonStyle::Plain, true,
                    Action::OpenMiseGlobalConfig, colors, cx,
                ))
                .child(self.action_button(
                    "toggle-mise-configs", if open { "收起已加载配置" } else { "查看已加载配置" },
                    if open { ButtonStyle::Selected } else { ButtonStyle::Plain }, true,
                    Action::ToggleMiseConfigs, colors, cx,
                ))
                .child(copy_button("copy-mise-activate", "复制 zsh 启用命令", activate, colors, cx)))
            .children(self.snapshot.mise_configs.as_ref().map(|paths| {
                div().flex().flex_col().gap_1()
                    .children(paths.is_empty().then(|| {
                        div().text_size(px(12.)).text_color(colors.muted).child("Mise 当前没有加载配置文件")
                    }))
                    .children(paths.iter().enumerate().map(|(index, path)| {
                        div().flex().items_center().gap_2()
                            .child(div().flex_1().min_w_0().truncate().text_size(px(12.)).text_color(colors.muted)
                                .child(path.display().to_string()))
                            .child(self.action_button(
                                format!("open-mise-config-{index}"), "打开", ButtonStyle::Plain, true,
                                Action::OpenMiseConfig(path.clone()), colors, cx,
                            ))
                    }))
            }))
    }

    fn settings(&self, window: &Window, colors: Palette, cx: &Context<Self>) -> Div {
        let enabled = self.snapshot.busy.is_none();
        let login = self.snapshot.app_login;
        let login_registered = matches!(login, LoginStatus::Enabled | LoginStatus::PendingApproval);
        let quiet_login = self.snapshot.settings.quiet_login;
        let visible_columns =
            ((window.viewport_size().width / px(1.) - 390.) / 7.8).clamp(18., 75.) as usize;
        let saved = self.snapshot.settings.proxy.as_str();
        let changed = self.proxy.text.trim() != saved;

        div()
            .flex()
            .flex_col()
            .gap_5()
            .child(self.theme_preferences(colors, cx))
            .children(self.manager_setup(None, window, colors, cx))
            .child(self.sidebar_preferences(colors, cx))
            .child(self.shell_preferences(colors, cx))
            .child(
                section("下载", colors.muted).child(
                    panel(colors)
                        .flex()
                        .flex_col()
                        .gap_3()
                        .p_4()
                        .child(div().font_weight(FontWeight::SEMIBOLD).child("安装代理"))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    self.proxy
                                        .field(
                                            "proxy-input",
                                            &self.proxy_focus,
                                            window,
                                            "http://127.0.0.1:7890",
                                            visible_columns,
                                            colors,
                                        )
                                        .on_key_down(cx.listener(Self::proxy_key)),
                                )
                                .child(self.action_button(
                                    "save-proxy",
                                    "保存",
                                    ButtonStyle::Primary,
                                    changed && enabled,
                                    Action::SetProxy(self.proxy.text.trim().to_owned()),
                                    colors,
                                    cx,
                                )),
                        )
                        .child(
                            div().text_size(px(12.)).text_color(colors.muted).child(
                                "用于应用更新检查，以及 Homebrew 和 Mise 的安装和版本查询。留空后保存可清除代理。",
                            ),
                        )
                        .child(self.install_parallelism(colors, cx)),
                ),
            )
            .child(
                section("应用", colors.muted).child(
                    panel(colors)
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_3()
                        .p_4()
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child("登陆时启动"),
                                )
                                .children(match login {
                                    LoginStatus::Unavailable => Some("使用 Usenv.app 启动后可设置。"),
                                    LoginStatus::PendingApproval => Some("请在系统设置的「登录项与扩展」中允许 Usenv。"),
                                    _ => None,
                                }.map(|hint| div().text_size(px(12.)).text_color(colors.muted).child(hint))),
                        )
                        .child(self.action_button(
                            "login-app",
                            match login {
                                LoginStatus::Enabled => "已开启",
                                LoginStatus::PendingApproval => "待允许",
                                _ => "已关闭",
                            },
                            if login_registered {
                                ButtonStyle::Selected
                            } else {
                                ButtonStyle::Plain
                            },
                            login != LoginStatus::Unavailable && enabled,
                            Action::AppLogin(!login_registered),
                            colors,
                            cx,
                        )),
                ).child(
                    panel(colors).p_4().flex().items_center().justify_between().gap_3()
                        .child(div().flex().flex_col().gap_1()
                            .child(div().font_weight(FontWeight::SEMIBOLD).child("登陆时仅显示菜单栏"))
                            .child(div().text_size(px(12.)).text_color(colors.muted).child(
                                if login == LoginStatus::Enabled {
                                    "从菜单栏选择「主界面」可打开窗口。"
                                } else {
                                    "开启「登陆时启动」并允许登录项后可设置。"
                                },
                            )))
                        .child(self.action_button(
                            "quiet-login",
                            if quiet_login { "已开启" } else { "已关闭" },
                            if quiet_login { ButtonStyle::Selected } else { ButtonStyle::Plain },
                            login == LoginStatus::Enabled,
                            Action::SetQuietLogin(!quiet_login), colors, cx,
                        )),
                ),
            )
            .child(self.update_preferences(colors, cx))
    }

    fn install_parallelism(&self, colors: Palette, cx: &Context<Self>) -> Div {
        let selected = self.snapshot.parallel_installs();
        div()
            .mt_2()
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .child(div().font_weight(FontWeight::SEMIBOLD).child("同时安装数量"))
            .child(div().flex().gap_1().children((MIN_PARALLEL_INSTALLS..=MAX_PARALLEL_INSTALLS).map(|limit| {
                button(
                    format!("parallel-installs-{limit}"),
                    &limit.to_string(),
                    if limit == selected { ButtonStyle::Selected } else { ButtonStyle::Plain },
                    true,
                    colors,
                )
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.controller.set_preference(Preference::ParallelInstalls(limit));
                    view.sync(cx);
                }))
            })))
    }

    fn open_update_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.update_dialog.is_some() || self.snapshot.update.release().is_none() {
            return;
        }
        self.open_actions = None;
        let dialog = UpdateDialog {
            previous_focus: window.focused(cx),
            github: cx.focus_handle(),
            cancel: cx.focus_handle(),
            install: cx.focus_handle(),
        };
        window.focus(&dialog.cancel);
        self.update_dialog = Some(dialog);
        cx.notify();
    }

    fn dismiss_update_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(release) = self.snapshot.update.offer() {
            self.controller.dismiss_update_notice(&release.version);
        }
        if let Some(previous) = self.update_dialog.take().and_then(|dialog| dialog.previous_focus) {
            window.focus(&previous);
        } else {
            window.blur();
        }
        self.sync(cx);
        cx.notify();
    }

    fn install_dialog_update(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(release) = self.snapshot.update.offer() else { return };
        let version = release.version.clone();
        self.dismiss_update_dialog(window, cx);
        let controller = self.controller.clone();
        let executor = cx.background_executor().clone();
        cx.spawn(async move |_, cx| {
            if executor.spawn(async move { controller.install_update(&version) }).await {
                let _ = cx.update(|cx| cx.quit());
            }
        }).detach();
    }

    fn render_update_dialog(&self, colors: Palette, cx: &Context<Self>) -> Option<Div> {
        let dialog = self.update_dialog.as_ref()?;
        let release = self.snapshot.update.release()?;
        let page = release.page.clone();
        let phase = self.snapshot.update.phase();
        let can_install = phase.is_none() && release.dmg.is_some();
        let error = match &self.snapshot.update {
            UpdateStatus::InstallFailed(_, message) => Some(message.as_str()),
            _ => None,
        };
        let title = match (phase, error) {
            (Some(_), _) => "正在更新 Usenv",
            (_, Some(_)) => "Usenv 更新失败",
            _ => "发现新版本",
        };
        let version_block = |label: &'static str, version: &str, background: Rgba, ink: Rgba| {
            div().flex_1().min_w_0().rounded_md().px_3().py_2().bg(background)
                .flex().flex_col().gap_1()
                .child(div().text_size(px(11.)).line_height(px(15.)).text_color(ink).child(label))
                .child(div().truncate().font_family("Menlo").text_size(px(17.)).line_height(px(22.))
                    .font_weight(FontWeight::SEMIBOLD).text_color(ink).child(version.to_owned()))
        };
        let github = dialog.github.clone();
        let github_link = div().id("update-dialog-github").track_focus(&github)
            .cursor_pointer().text_color(colors.accent).text_size(px(12.))
            .hover(|s| s.underline()).focus(|s| s.underline())
            .child("在 GitHub 查看 ↗")
            .on_click(move |_, window, cx| {
                window.focus(&github);
                cx.open_url(&page);
            });
        let controls = div().flex().items_center().gap_2()
            .child(button("update-dialog-cancel", if phase.is_some() { "隐藏" } else { "取消" }, ButtonStyle::Plain, true, colors)
                .track_focus(&dialog.cancel)
                .focus(move |style| style.bg(colors.control_hover))
                .on_click(cx.listener(|view, _, window, cx| view.dismiss_update_dialog(window, cx))))
            .when(can_install, |row| row.child(
                button("update-dialog-install", if error.is_some() { "重试更新" } else { "更新" },
                    ButtonStyle::Primary, true, colors)
                    .track_focus(&dialog.install)
                    .on_click(cx.listener(|view, _, window, cx| view.install_dialog_update(window, cx))),
            ));
        let body = div().px_5().pb_4().min_h_0().flex().flex_col().gap_2();
        let body = if let Some(phase) = phase {
            let body = body.child(div().font_weight(FontWeight::SEMIBOLD).child(phase.label()));
            if let UpdatePhase::Downloading { received, total } = phase {
                let fraction = received as f32 / total as f32;
                body.child(div().w_full().h(px(6.)).rounded_full().bg(colors.control)
                    .child(div().h_full().w(gpui::relative(fraction)).rounded_full().bg(colors.accent)))
                    .child(div().text_size(px(12.)).text_color(colors.muted)
                        .child(format!("{:.2} MB / {:.2} MB", received as f64 / 1e6, total as f64 / 1e6)))
            } else {
                body
            }
        } else {
            body.child(div().font_weight(FontWeight::SEMIBOLD)
                    .child(if error.is_some() { "失败原因" } else { "更新内容" }))
                .child(div().id("update-dialog-notes").min_h_0().max_h(px(200.))
                    .overflow_y_scroll().child(div().text_color(if error.is_some() { colors.danger } else { colors.ink })
                        .child(error.unwrap_or(&release.notes).trim().to_owned())))
                .child(div().pt_2().text_size(px(12.)).text_color(colors.muted)
                    .child(if can_install { "下载完成后，Usenv 会自动安装并重新启动。" }
                        else { "此版本暂未提供适用于本机且可校验的安装包。" }))
        };
        let card = div().w(px(468.)).max_w_full().max_h_full().flex().flex_col()
            .rounded_xl().border_1().border_color(colors.control).bg(colors.background).shadow_lg()
            .child(div().flex_shrink_0().p_5().pb_4().flex().flex_col().gap_3()
                .child(div().truncate().text_size(px(20.)).line_height(px(26.))
                    .font_weight(FontWeight::SEMIBOLD).child(title))
                .child(div().flex().items_center().gap_2()
                    .child(version_block("当前版本", env!("CARGO_PKG_VERSION"), colors.surface, colors.ink))
                    .child(div().flex_shrink_0().text_color(colors.muted).child("→"))
                    .child(version_block("新版本", &release.version, colors.accent_soft, colors.accent))))
            .child(body)
            .child(div().flex_shrink_0().px_5().py_3().border_t_1().border_color(colors.control)
                .flex().items_center().justify_between().child(github_link).child(controls));
        Some(div().absolute().inset_0().occlude().p_6().flex().items_center().justify_center()
            .bg(gpui::rgba(0x10182860))
            .capture_any_mouse_down(|_, window, _| window.prevent_default())
            .on_key_down(cx.listener(move |view, event: &KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "escape" => view.dismiss_update_dialog(window, cx),
                    "tab" => {
                        let Some(dialog) = &view.update_dialog else { return };
                        let buttons = [&dialog.github, &dialog.cancel, &dialog.install];
                        let buttons = &buttons[..if can_install { 3 } else { 2 }];
                        let current = buttons.iter().position(|button| button.is_focused(window)).unwrap_or(1);
                        let step = if event.keystroke.modifiers.shift { buttons.len() - 1 } else { 1 };
                        window.focus(buttons[(current + step) % buttons.len()]);
                    }
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .child(card))
    }

    fn update_preferences(&self, colors: Palette, cx: &Context<Self>) -> Div {
        let current = env!("CARGO_PKG_VERSION");
        let status = match &self.snapshot.update {
            UpdateStatus::Idle => None,
            UpdateStatus::Checking => Some("正在检查…".into()),
            UpdateStatus::NoRelease => Some("GitHub 暂无正式发布".into()),
            UpdateStatus::Current => Some("已是最新版本".into()),
            UpdateStatus::Available(release) => Some(format!("可更新到 {}", release.version)),
            UpdateStatus::Failed(_) => Some("更新检查失败".into()),
            UpdateStatus::InstallFailed(_, _) => Some("更新失败".into()),
            UpdateStatus::InProgress(_, phase) => Some(phase.label()),
        };
        let message = status.map_or_else(|| format!("当前版本 {current}"), |status| format!("当前版本 {current} · {status}"));
        let page = self.snapshot.update.release().map(|release| release.page.clone())
            .unwrap_or_else(|| updates::RELEASES.into());
        section("版本更新", colors.muted).child(
            panel(colors).p_4().flex().flex_col().gap_2()
                .child(div().flex().items_center().gap_2()
                    .child(div().flex_1().min_w_0().font_weight(FontWeight::SEMIBOLD).child(message))
                    .child(self.action_button(
                        "check-update", "检查更新", ButtonStyle::Plain, true,
                        Action::CheckUpdate, colors, cx,
                    ))
                    .child(button("github-releases", "发布说明", ButtonStyle::Plain, true, colors)
                        .on_click(move |_, _, cx| cx.open_url(&page)))
                    .children(self.snapshot.update.release().map(|_| {
                        let label = if self.snapshot.update.phase().is_some() { "查看进度" } else { "查看更新" };
                        button("show-update", label, ButtonStyle::Primary, true, colors)
                            .on_click(cx.listener(|view, _, window, cx| view.open_update_dialog(window, cx)))
                    })))
                .children(match &self.snapshot.update {
                    UpdateStatus::Failed(error) | UpdateStatus::InstallFailed(_, error) =>
                        Some(div().text_size(px(12.)).text_color(colors.danger).child(error.clone())),
                    _ => None,
                }),
        )
    }

    fn proxy_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "enter" if !event.keystroke.modifiers.platform => {
                let proxy = self.proxy.text.trim();
                if self.snapshot.busy.is_none() && proxy != self.snapshot.settings.proxy {
                    self.submit(Action::SetProxy(proxy.to_owned()), cx);
                }
                cx.stop_propagation();
            }
            "escape" => {
                self.proxy.replace_all(&self.snapshot.settings.proxy);
                cx.stop_propagation();
                cx.notify();
            }
            _ => self.proxy.key(event, cx),
        }
    }

    fn mise_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "enter" if !event.keystroke.modifiers.platform => {
                if let Page::Environment(environment) = self.page
                    && self.snapshot.busy.is_none()
                {
                    self.submit(
                        Action::InstallMise(environment, self.mise_version.version()),
                        cx,
                    );
                }
                cx.stop_propagation();
            }
            "escape" => {
                self.mise_version.replace_all("");
                cx.stop_propagation();
                cx.notify();
            }
            _ => self.mise_version.key(event, cx),
        }
    }
}

impl Render for Ui {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(release) = self.snapshot.update.offer()
            && self.snapshot.dismissed_update_version.as_ref() != Some(&release.version)
        {
            self.open_update_dialog(window, cx);
        }
        let colors = Palette::for_theme(self.snapshot.theme(), window.appearance());
        let fullscreen = window.is_fullscreen();
        let sidebar_width = px(if self.sidebar_collapsed { 80. } else { 192. });
        let content = if let Some(target) = &self.runtime_target {
            self.runtime_page(target, window, colors, cx)
        } else {
            let body = match self.page {
                Page::Environment(environment) => self.environment(environment, window, colors, cx),
                Page::Settings => self.settings(window, colors, cx),
            };
            div().flex().flex_col().gap_4().child(self.header(colors, cx)).child(body)
        };
        let scroll_id = if self.runtime_target.is_some() { "runtime-detail" } else { self.page.key() };
        div()
            .size_full()
            .relative()
            .flex()
            .bg(colors.background)
            .text_color(colors.ink)
            .text_size(px(13.))
            .line_height(px(20.))
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" && view.open_actions.is_some() {
                    view.close_actions(cx);
                    cx.stop_propagation();
                } else if event.keystroke.key == "tab" && !event.keystroke.modifiers.platform {
                    if event.keystroke.modifiers.shift {
                        window.focus_prev();
                    } else {
                        window.focus_next();
                    }
                    cx.stop_propagation();
                }
            }))
            .child(self.sidebar(sidebar_width, fullscreen, colors, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .id(SharedString::from(format!("scroll-{scroll_id}")))
                            .flex_1()
                            .min_h_0()
                            .mt_3()
                            .mb_3()
                            .overflow_y_scroll()
                            .px_7()
                            .py_3()
                            .child(div().max_w(px(1000.)).mx_auto().child(content)),
                    )
                    .children(self.task_output(colors, cx))
                    .child(self.feedback(colors, cx)),
            )
            .child(self.sidebar_toggle(sidebar_width, colors, cx))
            .children(self.copy_feedback(window, colors))
            .children(self.render_update_dialog(colors, cx).map(|dialog| deferred(dialog).with_priority(2)))
    }
}

fn runtime_title(runtime: &ToolRuntime) -> String {
    match runtime {
        ToolRuntime::Brew(formula) => format!("{formula} · Homebrew"),
        ToolRuntime::Mise { name, version } => format!("{name} {version} · Mise"),
    }
}

fn task_status(status: TaskStatus, colors: Palette) -> (&'static str, Rgba) {
    match status {
        TaskStatus::Queued => ("排队中", colors.muted),
        TaskStatus::Running => ("执行中", colors.accent),
        TaskStatus::Finished => ("完成", colors.success),
        TaskStatus::Failed => ("失败", colors.danger),
        TaskStatus::RefreshFailed => ("待刷新", colors.muted),
        TaskStatus::Cancelled => ("已取消", colors.muted),
    }
}

fn panel(colors: Palette) -> Div {
    div().rounded_lg().bg(colors.surface)
}

fn report_text(id: impl Into<SharedString>, content: &str, colors: Palette) -> Stateful<Div> {
    div().id(id.into()).max_h(px(210.)).overflow_y_scroll().px_3().py_2().rounded_md()
        .bg(colors.background).font_family("Menlo").text_size(px(11.))
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
        .child(content.to_owned())
}

fn card_list() -> Div {
    div().flex().flex_col().gap_1()
}

fn section(title: &'static str, caption: Rgba) -> Div {
    div().flex().flex_col().gap_2().child(
        div()
            .text_size(px(12.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(caption)
            .child(title),
    )
}

fn dot(color: Rgba) -> Div {
    div().size(px(6.)).flex_shrink_0().rounded_full().bg(color)
}

fn environment_icon(env: Environment, size: f32) -> Div {
    let symbol = match env {
        Environment::Node => "JS",
        Environment::Go => "Go",
        Environment::Rust => "Rs",
        Environment::Flutter => "Fl",
        Environment::Php => "Ph",
        Environment::Composer => "Cp",
        Environment::Python => "Py",
        Environment::Java => "Jv",
        Environment::Nginx => "Nx",
        Environment::Mysql => "My",
        Environment::Mariadb => "Ma",
        Environment::Redis => "Rd",
        Environment::Postgresql => "Pg",
        Environment::Meilisearch => "Ms",
        Environment::Elasticsearch => "Es",
    };
    div()
        .size(px(size))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .text_size(px(10.))
        .font_weight(FontWeight::BOLD)
        .child(symbol)
}

struct SidebarTooltip(&'static str, Palette);

impl Render for SidebarTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        panel(self.1)
            .px_2()
            .py_1()
            .text_size(px(12.))
            .text_color(self.1.ink)
            .child(self.0)
    }
}

fn keyboard_focus(element: Stateful<Div>) -> Stateful<Div> {
    element.tab_index(0).capture_any_mouse_down(|_, window, _| {
        // GPUI transfers focus on mouse-down, before the click handler runs.
        if !window.default_prevented() {
            window.prevent_default();
            window.blur();
        }
    })
}

fn button(
    id: impl Into<SharedString>,
    label: &str,
    style: ButtonStyle,
    enabled: bool,
    colors: Palette,
) -> Stateful<Div> {
    let (background, text, hovered) = match style {
        ButtonStyle::Plain => (colors.control, colors.ink, colors.control_hover),
        ButtonStyle::Primary => (colors.accent, colors.accent_text, colors.accent_hover),
        ButtonStyle::Selected => (colors.accent_soft, colors.accent, colors.control_hover),
        ButtonStyle::Sidebar => (colors.sidebar, colors.sidebar_ink, colors.sidebar_hover),
    };
    let focus_color = match style {
        ButtonStyle::Primary => colors.accent_text,
        _ => colors.accent,
    };
    let element = div()
        .id(id.into())
        .flex()
        .flex_shrink_0()
        .h(px(30.))
        .items_center()
        .justify_center()
        .rounded_md()
        .border_1()
        .border_color(gpui::rgba(0))
        .bg(background)
        .text_color(text)
        .text_size(px(12.))
        .font_weight(FontWeight::MEDIUM)
        .px_3()
        .child(label.to_owned());
    if enabled {
        keyboard_focus(element)
            .cursor_pointer()
            .hover(move |style| style.bg(hovered))
            .focus(move |style| style.border_color(focus_color).bg(hovered))
    } else {
        element.opacity(0.46)
    }
}

fn copy_button(
    id: impl Into<SharedString>,
    label: &str,
    text: String,
    colors: Palette,
    cx: &Context<Ui>,
) -> Stateful<Div> {
    copy_control(id, label, true, move || text.clone(), colors, cx)
}

fn copy_control(
    id: impl Into<SharedString>,
    label: &str,
    enabled: bool,
    text: impl Fn() -> String + 'static,
    colors: Palette,
    cx: &Context<Ui>,
) -> Stateful<Div> {
    let element = button(id, label, ButtonStyle::Plain, enabled, colors);
    if !enabled {
        return element;
    }
    let bounds = Rc::new(Cell::new(None));
    let track_bounds = bounds.clone();
    element.relative()
        .child(canvas(
            move |bounds, _, _| track_bounds.set(Some(bounds)),
            |_, (), _, _| {},
        ).absolute().inset_0())
        .on_click(cx.listener(move |view, event: &gpui::ClickEvent, _, cx| {
            view.close_actions(cx);
            cx.write_to_clipboard(ClipboardItem::new_string(text()));
            let click_bounds = match event {
                gpui::ClickEvent::Keyboard(event) => event.bounds.clone(),
                gpui::ClickEvent::Mouse(_) => bounds.take()
                    .unwrap_or_else(|| Bounds::new(event.position(), size(px(0.), px(0.)))),
            };
            view.show_copy_feedback(click_bounds, cx);
        }))
}

struct TextEdit {
    start: usize,
    removed: String,
    inserted: String,
    cursor_before: usize,
    anchor_before: usize,
}

#[derive(Default)]
struct LineEditor {
    text: String,
    cursor: usize,
    anchor: usize,
    undo: VecDeque<TextEdit>,
    redo: Vec<TextEdit>,
}

impl LineEditor {
    fn new(text: String) -> Self {
        let end = text.len();
        Self {
            text,
            cursor: end,
            anchor: end,
            ..Self::default()
        }
    }

    fn sync_text(&mut self, saved: &str) {
        if self.text != saved {
            *self = Self::new(saved.to_owned());
        }
    }

    fn version(&self) -> String {
        let value = self.text.trim();
        if value.is_empty() {
            "latest".into()
        } else {
            value.to_owned()
        }
    }

    fn key(&mut self, event: &KeyDownEvent, cx: &mut Context<Ui>) {
        let key = &event.keystroke;
        match (key.key.as_str(), key.modifiers.platform) {
            ("z", true) if !key.modifiers.alt && !key.modifiers.control => {
                if key.modifiers.shift {
                    self.redo();
                } else {
                    self.undo();
                }
            }
            ("a", true) => {
                self.anchor = 0;
                self.cursor = self.text.len();
            }
            ("c" | "x", true) => {
                let range = self.selection();
                if !range.is_empty() {
                    let selected = self.text[range.clone()].to_owned();
                    cx.write_to_clipboard(ClipboardItem::new_string(selected));
                    if key.key == "x" {
                        self.replace(range, "");
                    }
                }
            }
            ("v", true) => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    self.replace(self.selection(), &text);
                }
            }
            ("left" | "right" | "home" | "end", _) => {
                let selection = self.selection();
                let position = match key.key.as_str() {
                    "home" => 0,
                    "end" => self.text.len(),
                    "left" if key.modifiers.platform => 0,
                    "right" if key.modifiers.platform => self.text.len(),
                    "left" if !key.modifiers.shift && !selection.is_empty() => selection.start,
                    "right" if !key.modifiers.shift && !selection.is_empty() => selection.end,
                    "left" => self.previous(),
                    _ => self.next(),
                };
                self.move_to(position, key.modifiers.shift);
            }
            ("backspace" | "delete", _) => {
                let mut range = self.selection();
                if range.is_empty() {
                    if key.key == "backspace" {
                        range.start = self.previous();
                    } else {
                        range.end = self.next();
                    }
                }
                self.replace(range, "");
            }
            _ if !key.modifiers.platform && !key.modifiers.control => match key.key_char.as_deref()
            {
                Some(text) if !text.chars().any(char::is_control) => {
                    self.replace(self.selection(), text);
                }
                _ => return,
            },
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn field(
        &self,
        id: &'static str,
        focus: &FocusHandle,
        window: &Window,
        placeholder: &str,
        columns: usize,
        colors: Palette,
    ) -> Stateful<Div> {
        let focused = focus.is_focused(window);
        let field_focus = focus.clone();
        div()
            .id(id)
            .track_focus(focus)
            .on_mouse_down_out(move |_, window, _| {
                if field_focus.is_focused(window) {
                    window.blur();
                }
            })
            .tab_index(0)
            .flex_1()
            .min_w_0()
            .h(INPUT_HEIGHT)
            .rounded_md()
            .border_1()
            .border_color(if focused {
                colors.accent
            } else {
                gpui::rgba(0)
            })
            .bg(colors.control)
            .px_2()
            .flex()
            .items_center()
            .overflow_hidden()
            .cursor(gpui::CursorStyle::IBeam)
            .child(self.display(focused, placeholder, columns, colors))
    }

    fn selection(&self) -> Range<usize> {
        self.cursor.min(self.anchor)..self.cursor.max(self.anchor)
    }

    fn previous(&self) -> usize {
        self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index)
    }

    fn next(&self) -> usize {
        self.text[self.cursor..]
            .chars()
            .next()
            .map_or(self.cursor, |ch| self.cursor + ch.len_utf8())
    }

    fn move_to(&mut self, cursor: usize, extend: bool) {
        self.cursor = cursor;
        if !extend {
            self.anchor = cursor;
        }
    }

    fn replace_all(&mut self, text: &str) {
        self.replace(0..self.text.len(), text);
    }

    fn replace(&mut self, range: Range<usize>, text: &str) {
        let text: String = text
            .trim_matches(['\r', '\n'])
            .chars()
            .filter(|ch| !ch.is_control())
            .collect();
        let cursor = range.start + text.len();
        if &self.text[range.clone()] != text.as_str() {
            let edit = TextEdit {
                start: range.start,
                removed: self.text[range.clone()].to_owned(),
                inserted: text,
                cursor_before: self.cursor,
                anchor_before: self.anchor,
            };
            self.text.replace_range(range, &edit.inserted);
            if self.undo.len() == 128 {
                self.undo.pop_front();
            }
            self.undo.push_back(edit);
            self.redo.clear();
        }
        self.move_to(cursor, false);
    }

    fn undo(&mut self) {
        if let Some(edit) = self.undo.pop_back() {
            let range = edit.start..edit.start + edit.inserted.len();
            self.text.replace_range(range, &edit.removed);
            self.cursor = edit.cursor_before;
            self.anchor = edit.anchor_before;
            self.redo.push(edit);
        }
    }

    fn redo(&mut self) {
        if let Some(edit) = self.redo.pop() {
            let range = edit.start..edit.start + edit.removed.len();
            self.text.replace_range(range, &edit.inserted);
            self.move_to(edit.start + edit.inserted.len(), false);
            self.undo.push_back(edit);
        }
    }

    fn display(&self, focused: bool, placeholder: &str, columns: usize, colors: Palette) -> Div {
        let row = div()
            .flex()
            .items_center()
            .whitespace_nowrap()
            .font_family("Menlo")
            .text_size(px(12.));
        let caret = || div().w(px(1.)).h(px(16.)).bg(colors.accent).flex_shrink_0();
        if self.text.is_empty() {
            return row
                .min_w_0()
                .gap_1()
                .text_color(colors.placeholder)
                .children(focused.then(caret))
                .child(placeholder.to_owned());
        }
        if !focused {
            return row.min_w_0().truncate().child(self.text.clone());
        }

        let selection = self.selection();
        let start = self.text[..self.cursor]
            .char_indices()
            .rev()
            .nth(columns.saturating_sub(5))
            .map_or(0, |(index, _)| index);
        let end = self.text[start..]
            .char_indices()
            .nth(columns)
            .map_or(self.text.len(), |(index, _)| start + index);
        let selected_start = selection.start.clamp(start, end);
        let selected_end = selection.end.clamp(start, end);
        let mut row = row.child(self.text[start..selected_start].to_owned());
        if self.cursor <= selected_start {
            row = row.child(caret());
        } else {
            return row
                .child(
                    div()
                        .bg(colors.accent_soft)
                        .child(self.text[selected_start..selected_end].to_owned()),
                )
                .child(caret())
                .child(self.text[selected_end..end].to_owned());
        }
        row.child(
            div()
                .bg(colors.accent_soft)
                .child(self.text[selected_start..selected_end].to_owned()),
        )
        .child(self.text[selected_end..end].to_owned())
    }
}
