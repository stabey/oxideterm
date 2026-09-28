use super::*;

use oxideterm_gpui_ui::{
    button::{ButtonRadius, ButtonVariant, ToolbarButtonOptions, toolbar_button},
    context_menu::{
        ContextMenuItemKind, context_menu_action, context_menu_backdrop, context_menu_content,
        context_menu_event_boundary, context_menu_item_row, context_menu_separator,
    },
    modal::{TAURI_POPOVER_LAYER_PRIORITY, overlay_content_boundary},
    motion::{self, MotionDuration},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionLogMenuMode {
    Save,
    Stop,
    Manage(PaneId),
}

#[cfg(test)]
mod tests;

#[derive(Clone)]
pub(super) struct SessionLogTarget {
    pane_id: PaneId,
    session_id: TerminalSessionId,
    pane: gpui::WeakEntity<TerminalPane>,
}

impl SessionLogTarget {
    fn for_tab(
        tab_id: TabId,
        host: &tabs::WorkspaceTabHostEntity,
        cx: &App,
    ) -> Vec<SessionLogTarget> {
        let Some(root) = host
            .tab_by_id(tab_id)
            .and_then(|tab| tab.root_pane.as_ref())
        else {
            return Vec::new();
        };
        let mut sessions = Vec::new();
        root.collect_session_ids(&mut sessions);
        sessions
            .into_iter()
            .filter_map(|session_id| {
                let pane_id = root.pane_id_for_session(session_id)?;
                let pane = host.panes().get(&pane_id)?;
                let terminal = pane.read(cx);
                (terminal.session_log_available()
                    || terminal.session_log_status().state != TerminalSessionLogState::Idle)
                    .then(|| SessionLogTarget {
                        pane_id,
                        session_id,
                        pane: pane.downgrade(),
                    })
            })
            .collect()
    }

    fn resolve(
        &self,
        tab_id: TabId,
        host: &tabs::WorkspaceTabHostEntity,
    ) -> Option<Entity<TerminalPane>> {
        let root = host.tab_by_id(tab_id)?.root_pane.as_ref()?;
        if root.session_id_for_pane(self.pane_id) != Some(self.session_id) {
            return None;
        }
        let pane = host.panes().get(&self.pane_id)?;
        (pane.downgrade() == self.pane).then(|| pane.clone())
    }

    fn start_at_path(
        &self,
        tab_id: TabId,
        path: PathBuf,
        host: &Entity<tabs::WorkspaceTabHostEntity>,
        cx: &mut App,
    ) -> std::result::Result<(), String> {
        let pane = self
            .resolve(tab_id, host.read(cx))
            .ok_or_else(|| "terminal session log target is unavailable".to_string())?;
        let canonical = path.canonicalize().ok();
        if host.read(cx).panes().values().any(|pane| {
            let status = pane.read(cx).session_log_status();
            status.state != TerminalSessionLogState::Idle
                && status.path.is_some_and(|active| {
                    active == path
                        || canonical.as_ref().is_some_and(|canonical| {
                            active.canonicalize().ok().as_ref() == Some(canonical)
                        })
                })
        }) {
            // Save-dialog overwrite confirmation must not truncate another live writer's file.
            return Err("terminal session log file is already in use".to_string());
        }
        pane.update(cx, |pane, cx| pane.start_session_log_at_path(path, cx))
    }
}

pub(super) struct SessionLogMenu {
    tab_id: TabId,
    window: AnyWindowHandle,
    position: Point<Pixels>,
    focus: FocusHandle,
    return_focus: Option<FocusHandle>,
    mode: SessionLogMenuMode,
    targets: Vec<SessionLogTarget>,
    selected: usize,
    show_picker: bool,
    previous_preview: Option<PaneId>,
    preview_generation: u64,
    // The popup owns both tasks; dismissal cancels stale dialogs and transition cleanup.
    preview_task: Option<Task<()>>,
    prompt_task: Option<Task<()>>,
}

impl SessionLogMenu {
    fn target(&self, index: usize) -> Option<&SessionLogTarget> {
        self.targets
            .get(if matches!(self.mode, SessionLogMenuMode::Manage(_)) {
                0
            } else {
                index
            })
    }

    fn row_count(&self) -> usize {
        if matches!(self.mode, SessionLogMenuMode::Manage(_)) {
            3
        } else {
            self.targets.len()
        }
    }

    fn next_row(
        &self,
        reverse: bool,
        host: &tabs::WorkspaceTabHostEntity,
        cx: &App,
    ) -> Option<usize> {
        let count = self.row_count();
        (1..=count)
            .map(|step| {
                if reverse {
                    (self.selected + count - step) % count
                } else {
                    (self.selected + step) % count
                }
            })
            .find(|&index| self.actionable(index, host, cx))
    }

    fn select(&mut self, index: usize) -> bool {
        if self.selected == index || index >= self.row_count() || self.prompt_task.is_some() {
            return false;
        }
        self.previous_preview = self.target(self.selected).map(|target| target.pane_id);
        self.selected = index;
        self.preview_generation = self.preview_generation.wrapping_add(1);
        true
    }

    fn actionable(&self, index: usize, host: &tabs::WorkspaceTabHostEntity, cx: &App) -> bool {
        if self.prompt_task.is_some() {
            return false;
        }
        let Some(pane) = self
            .target(index)
            .and_then(|target| target.resolve(self.tab_id, host))
        else {
            return false;
        };
        let pane = pane.read(cx);
        let status = pane.session_log_status();
        match self.mode {
            SessionLogMenuMode::Save => {
                pane.session_log_available() && status.state == TerminalSessionLogState::Idle
            }
            SessionLogMenuMode::Stop => status.state != TerminalSessionLogState::Idle,
            SessionLogMenuMode::Manage(_) if index < 2 => status.path.is_some(),
            SessionLogMenuMode::Manage(_) => status.state != TerminalSessionLogState::Idle,
        }
    }
}

impl WorkspaceApp {
    pub(super) fn session_log_menu_owns_window(&self, window_id: gpui::WindowId) -> bool {
        self.terminal_session_log_menu
            .as_ref()
            .is_some_and(|menu| menu.window.window_id() == window_id)
    }

    pub(super) fn tab_session_log_actions(&self, tab_id: TabId, cx: &App) -> (bool, bool) {
        let mut save = false;
        let mut stop = false;
        for target in SessionLogTarget::for_tab(tab_id, self.tab_host.read(cx), cx) {
            if let Some(pane) = target.resolve(tab_id, self.tab_host.read(cx)) {
                if pane.read(cx).session_log_status().state == TerminalSessionLogState::Idle {
                    save = true;
                } else {
                    stop = true;
                }
            }
        }
        (save, stop)
    }

    pub(super) fn begin_session_log_menu(
        &mut self,
        tab_id: TabId,
        mode: SessionLogMenuMode,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dismiss_transient_workspace_overlays(cx);
        let mut targets = SessionLogTarget::for_tab(tab_id, self.tab_host.read(cx), cx);
        targets.retain(|target| match mode {
            SessionLogMenuMode::Save => true,
            SessionLogMenuMode::Stop => {
                target
                    .resolve(tab_id, self.tab_host.read(cx))
                    .is_some_and(|pane| {
                        pane.read(cx).session_log_status().state != TerminalSessionLogState::Idle
                    })
            }
            SessionLogMenuMode::Manage(pane_id) => target.pane_id == pane_id,
        });
        if targets.is_empty() {
            return;
        }
        if !self.tab_host.read(cx).is_outside_main_window(tab_id)
            && self.active_tab_id(cx) != Some(tab_id)
        {
            self.set_active_tab(tab_id, window, cx);
        }
        // Preview focus belongs to the popup, while terminal input ownership stays unchanged.
        self.needs_active_pane_focus = false;
        let return_focus = window.focused(cx);
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let active = self
            .tab_by_id(tab_id, cx)
            .and_then(|tab| tab.active_pane_id);
        let selected = targets
            .iter()
            .position(|target| Some(target.pane_id) == active)
            .unwrap_or(0);
        let show_picker = matches!(mode, SessionLogMenuMode::Manage(_))
            || self
                .tab_by_id(tab_id, cx)
                .and_then(|tab| tab.root_pane.as_ref())
                .is_some_and(|root| root.pane_count() > 1);
        let mut menu = SessionLogMenu {
            tab_id,
            window: window.window_handle(),
            position,
            focus,
            return_focus,
            mode,
            targets,
            selected,
            show_picker,
            previous_preview: None,
            preview_generation: 0,
            preview_task: None,
            prompt_task: None,
        };
        if !menu.actionable(selected, self.tab_host.read(cx), cx) {
            menu.selected = (0..menu.row_count())
                .find(|&index| menu.actionable(index, self.tab_host.read(cx), cx))
                .unwrap_or(0);
        }
        self.terminal_session_log_menu = Some(menu);
        if !show_picker {
            self.activate_session_log_menu_row(window, cx);
        }
        cx.notify();
    }

    fn dismiss_session_log_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(menu) = self.terminal_session_log_menu.take() {
            window.focus(
                &menu
                    .return_focus
                    .unwrap_or_else(|| self.focus_handle.clone()),
                cx,
            );
            cx.notify();
        }
    }

    fn preview_session_log_row(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(menu) = self.terminal_session_log_menu.as_mut() else {
            return;
        };
        if !menu.select(index) {
            return;
        }
        let duration = motion::duration(&self.tokens, MotionDuration::Micro);
        menu.preview_task = self.tokens.motion.enabled.then(|| {
            cx.spawn(async move |weak, cx| {
                Timer::after(duration).await;
                let _ = weak.update(cx, |this, cx| {
                    if let Some(menu) = this.terminal_session_log_menu.as_mut() {
                        menu.previous_preview = None;
                        menu.preview_task = None;
                        cx.notify();
                    }
                });
            })
        });
        cx.notify();
    }

    pub(super) fn capture_session_log_menu_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.app_lock.locked || self.active_window_modal_owner(cx).is_some() {
            return false;
        }
        let Some(menu) = self
            .terminal_session_log_menu
            .as_ref()
            .filter(|menu| menu.window.window_id() == window.window_handle().window_id())
        else {
            return false;
        };
        match event.keystroke.key.as_str() {
            "escape" => self.dismiss_session_log_menu(window, cx),
            "enter" | "space" => self.activate_session_log_menu_row(window, cx),
            "up" | "down" | "tab" => {
                let reverse = event.keystroke.key == "up"
                    || (event.keystroke.key == "tab" && event.keystroke.modifiers.shift);
                let next = menu.next_row(reverse, self.tab_host.read(cx), cx);
                if let Some(next) = next {
                    self.preview_session_log_row(next, cx);
                }
            }
            _ => {}
        }
        true
    }

    fn activate_session_log_menu_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = self.terminal_session_log_menu.as_ref() else {
            return;
        };
        if !menu.actionable(menu.selected, self.tab_host.read(cx), cx) {
            return;
        }
        let Some(target) = menu.target(menu.selected).cloned() else {
            return;
        };
        match menu.mode {
            SessionLogMenuMode::Save => self.prompt_terminal_session_log_file(target, window, cx),
            SessionLogMenuMode::Stop => {
                self.dismiss_session_log_menu(window, cx);
                self.stop_terminal_session_log(target.pane_id, cx);
            }
            SessionLogMenuMode::Manage(_) => {
                let selected = menu.selected;
                self.dismiss_session_log_menu(window, cx);
                match selected {
                    0 => self.open_terminal_session_log(target.pane_id, cx),
                    1 => self.open_pane_session_log_directory(target.pane_id, cx),
                    _ => self.stop_terminal_session_log(target.pane_id, cx),
                }
            }
        }
    }

    fn prompt_terminal_session_log_file(
        &mut self,
        target: SessionLogTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = self.terminal_session_log_menu.as_ref() else {
            return;
        };
        let tab_id = menu.tab_id;
        let Some(pane) = target.resolve(tab_id, self.tab_host.read(cx)) else {
            return;
        };
        let directory = pane
            .read(cx)
            .session_log_status()
            .path
            .and_then(|path| path.parent().map(Path::to_path_buf))
            .or_else(|| {
                std::env::var_os("HOME")
                    .or_else(|| std::env::var_os("USERPROFILE"))
                    .map(PathBuf::from)
            })
            .unwrap_or_else(|| PathBuf::from("."));
        let suggested = format!(
            "oxideterm-{}-{}.log",
            chrono::Local::now().format("%Y%m%d-%H%M%S"),
            target.pane_id.0
        );
        let owner = window.window_handle();
        let receiver = cx.prompt_for_new_path(&directory, Some(&suggested));
        let task = cx.spawn(async move |weak, cx| {
            let selection = receiver.await;
            let _ = owner.update(cx, |_, window, cx| {
                weak.update(cx, |this, cx| {
                    if this.app_lock.locked || this.active_window_modal_owner(cx).is_some() {
                        this.terminal_session_log_menu = None;
                        cx.notify();
                        return;
                    }
                    let Some(menu) = this.terminal_session_log_menu.as_mut() else {
                        return;
                    };
                    menu.prompt_task = None;
                    if matches!(selection, Ok(Ok(None))) && menu.show_picker {
                        window.focus(&menu.focus, cx);
                        cx.notify();
                        return;
                    }
                    // A closed, replaced or moved pane cannot redirect a delayed file selection.
                    this.dismiss_session_log_menu(window, cx);
                    let result = match selection {
                        Ok(Ok(Some(path))) => {
                            target.start_at_path(tab_id, path, &this.tab_host, cx)
                        }
                        Ok(Ok(None)) => return,
                        _ => Err("could not select terminal session log file".to_string()),
                    };
                    let (key, variant) = if result.is_ok() {
                        (
                            "terminal.session_log.started",
                            TerminalNoticeVariant::Success,
                        )
                    } else {
                        (
                            "terminal.session_log.start_failed",
                            TerminalNoticeVariant::Error,
                        )
                    };
                    this.push_workspace_notice(
                        TerminalNotice {
                            title: this.i18n.t(key),
                            description: None,
                            status_text: None,
                            progress: None,
                            variant,
                        },
                        cx,
                    );
                    cx.notify();
                })
            });
        });
        self.terminal_session_log_menu.as_mut().unwrap().prompt_task = Some(task);
        cx.notify();
    }

    pub(super) fn render_session_log_preview(
        &self,
        pane_id: PaneId,
        window: &Window,
    ) -> Option<AnyElement> {
        let menu = self.terminal_session_log_menu.as_ref()?;
        if menu.window.window_id() != window.window_handle().window_id() {
            return None;
        }
        let selected = menu.target(menu.selected)?.pane_id == pane_id;
        if !selected && menu.previous_preview != Some(pane_id) {
            return None;
        }
        Some(motion::fade(
            &self.tokens,
            ("session-log-pane-preview", menu.preview_generation),
            div()
                .absolute()
                .inset_0()
                .border_2()
                .border_color(rgb(self.tokens.ui.accent))
                .bg(rgba((self.tokens.ui.accent << 8) | 0x18))
                .rounded(px(self.tokens.radii.md)),
            MotionDuration::Micro,
            selected,
        ))
    }

    pub(super) fn render_session_log_badge(
        &self,
        tab_id: TabId,
        pane_id: PaneId,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let state = self
            .tab_host
            .read(cx)
            .panes()
            .get(&pane_id)?
            .read(cx)
            .session_log_status()
            .state;
        if state == TerminalSessionLogState::Idle {
            return None;
        }
        let label = self.i18n.t(if state == TerminalSessionLogState::Paused {
            "terminal.session_log.paused_badge"
        } else {
            "terminal.session_log.saving_badge"
        });
        Some(
            toolbar_button(
                &self.tokens,
                label.clone(),
                Some(
                    svg()
                        .path(LucideIcon::FileText.path())
                        .size(px(12.0))
                        .into_any_element(),
                ),
                ToolbarButtonOptions::compact_text(
                    ButtonVariant::Secondary,
                    ButtonRadius::Sm,
                    22.0,
                    6.0,
                    self.tokens.metrics.ui_text_xs,
                ),
            )
            .id(("session-log-badge", pane_id.0))
            .flex_none()
            .text_color(rgb(self.tokens.ui.accent))
            .role(gpui::Role::Button)
            .aria_label(label)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.begin_session_log_menu(
                        tab_id,
                        SessionLogMenuMode::Manage(pane_id),
                        event.position,
                        window,
                        cx,
                    );
                    cx.stop_propagation();
                }),
            )
            .into_any_element(),
        )
    }

    pub(super) fn render_session_log_menu(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let menu = self.terminal_session_log_menu.as_ref()?;
        if menu.window.window_id() != window.window_handle().window_id() {
            return None;
        }
        if self.app_lock.locked || self.active_window_modal_owner(cx).is_some() {
            self.terminal_session_log_menu = None;
            cx.notify();
            return None;
        }
        let host = self.tab_host.read(cx);
        let mounted_here = host.detached_window_handle(menu.tab_id).map_or_else(
            || {
                !host.is_outside_main_window(menu.tab_id)
                    && self.active_tab_id(cx) == Some(menu.tab_id)
            },
            |owner| owner == menu.window,
        );
        if !mounted_here
            || !menu
                .targets
                .iter()
                .any(|target| target.resolve(menu.tab_id, host).is_some())
        {
            // Navigation already chose a new keyboard owner; do not restore stale focus.
            self.terminal_session_log_menu = None;
            cx.notify();
            return None;
        }
        if !menu.show_picker {
            return None;
        }
        let manage = matches!(menu.mode, SessionLogMenuMode::Manage(_));
        let heading = if manage {
            let target = menu.target(0)?;
            format!(
                "#{} · {}",
                target.pane_id.0,
                self.terminal_pane_label(target.pane_id, cx)
            )
        } else {
            self.i18n.t(if menu.mode == SessionLogMenuMode::Save {
                "terminal.session_log.choose_save_pane"
            } else {
                "terminal.session_log.choose_stop_pane"
            })
        };
        let active = self
            .tab_by_id(menu.tab_id, cx)
            .and_then(|tab| tab.active_pane_id);
        let mut body = context_menu_content(&self.tokens)
            .w(px(320.0))
            .child(
                div()
                    .px(px(self.tokens.metrics.ui_menu_item_padding_x))
                    .py(px(self.tokens.metrics.ui_menu_item_padding_y))
                    .text_size(px(self.tokens.metrics.ui_text_sm))
                    .truncate()
                    .child(heading),
            )
            .child(context_menu_separator(&self.tokens));
        for index in 0..menu.row_count() {
            let target = menu.target(index)?;
            let pane = target.resolve(menu.tab_id, self.tab_host.read(cx));
            let state = pane
                .as_ref()
                .map(|pane| pane.read(cx).session_log_status().state);
            let label = if manage {
                self.i18n.t(match index {
                    0 => "terminal.session_log.open_file",
                    1 => "terminal.session_log.open_directory",
                    _ => "terminal.session_log.stop",
                })
            } else {
                format!(
                    "#{} · {}",
                    target.pane_id.0,
                    self.terminal_pane_label(target.pane_id, cx)
                )
            };
            let status = if manage {
                String::new()
            } else if state == Some(TerminalSessionLogState::Logging) {
                self.i18n.t("terminal.session_log.saving_badge")
            } else if state == Some(TerminalSessionLogState::Paused) {
                self.i18n.t("terminal.session_log.paused_badge")
            } else if Some(target.pane_id) == active {
                self.i18n.t("terminal.session_log.current_pane")
            } else {
                String::new()
            };
            let enabled = menu.actionable(index, self.tab_host.read(cx), cx);
            let row =
                context_menu_item_row(&self.tokens, ContextMenuItemKind::Plain, false, !enabled)
                    .child(div().flex_1().min_w_0().truncate().child(label.clone()))
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(self.tokens.metrics.ui_text_xs))
                            .text_color(rgb(self.tokens.ui.text_muted))
                            .child(status),
                    )
                    .when(menu.selected == index, |row| {
                        row.bg(rgb(self.tokens.ui.bg_hover))
                    });
            body = body.child(
                context_menu_action(
                    row,
                    !enabled,
                    false,
                    cx.listener(move |this, _, window, cx| {
                        this.preview_session_log_row(index, cx);
                        this.activate_session_log_menu_row(window, cx);
                        cx.stop_propagation();
                    }),
                )
                .id(("session-log-pane-option", index))
                .role(gpui::Role::MenuItem)
                .aria_label(label)
                .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                    if *hovered {
                        this.preview_session_log_row(index, cx);
                    }
                })),
            );
        }
        if !manage {
            body = body.child(context_menu_separator(&self.tokens)).child(
                div()
                    .px(px(self.tokens.metrics.ui_menu_item_padding_x))
                    .py(px(self.tokens.metrics.ui_menu_item_padding_y))
                    .text_size(px(self.tokens.metrics.ui_text_xs))
                    .text_color(rgb(self.tokens.ui.text_muted))
                    .child(self.i18n.t("terminal.session_log.pane_picker_hint")),
            );
        }
        let popup =
            overlay_content_boundary(context_menu_event_boundary(body)).track_focus(&menu.focus);
        let position = menu.position;
        Some(
            context_menu_backdrop()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        this.dismiss_session_log_menu(window, cx);
                        cx.stop_propagation();
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, _, window, cx| {
                        this.dismiss_session_log_menu(window, cx);
                        cx.stop_propagation();
                    }),
                )
                .child(
                    deferred(
                        anchored()
                            .anchor(Corner::TopLeft)
                            .position(position)
                            .position_mode(AnchoredPositionMode::Window)
                            .child(motion::fade_in(
                                &self.tokens,
                                "session-log-menu-enter",
                                popup,
                                MotionDuration::Micro,
                            )),
                    )
                    .with_priority(TAURI_POPOVER_LAYER_PRIORITY),
                )
                .into_any_element(),
        )
    }
}
