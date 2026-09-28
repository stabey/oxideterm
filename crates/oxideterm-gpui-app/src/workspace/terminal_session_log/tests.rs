use super::*;
use gpui::{TestAppContext, VisualTestContext};
use oxideterm_settings::TerminalSessionLogFileMode;
use oxideterm_workspace::PaneSplitChild;

struct TestRoot;

impl Render for TestRoot {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

fn fixture(cx: &mut VisualTestContext, directory: &Path) -> Entity<tabs::WorkspaceTabHostEntity> {
    let host = cx.new(|_| tabs::WorkspaceTabHostEntity::new());
    cx.update(|window, cx| {
        for id in 1..=3 {
            let preferences = TerminalUiPreferences {
                session_log_options: (id != 2).then(|| TerminalSessionLogOptions {
                    directory: directory.into(),
                    directory_template: String::new(),
                    file_name_template: "automatic.log".into(),
                    content_template: "{text}".into(),
                    include_control_sequences: false,
                    file_mode: TerminalSessionLogFileMode::Unique,
                    max_file_bytes: None,
                    retention_days: 0,
                    context: Default::default(),
                }),
                ..Default::default()
            };
            // This test needs real pane identities and logging ownership, without a shell process.
            // Live output and file contents are exercised by the terminal crate's SSH test.
            let session = Arc::new(parking_lot::Mutex::new(
                oxideterm_terminal::TerminalSession::recording_playback(
                    80,
                    24,
                    Default::default(),
                    100,
                ),
            ));
            let pane = cx.new(|cx| {
                TerminalPane::from_shared_session(session, preferences, window, cx).unwrap()
            });
            host.update(cx, |host, cx| {
                host.register_terminal_pane(
                    PaneId(id),
                    TerminalSessionId(id),
                    pane,
                    window.window_handle(),
                    cx,
                );
                host.bind_terminal_location(
                    TerminalSessionId(id),
                    tabs::TerminalLocation {
                        tab_id: TabId(1),
                        pane_id: PaneId(id),
                    },
                );
            });
        }
        host.update(cx, |host, _| {
            host.insert_and_select_main_tab(Tab {
                id: TabId(1),
                kind: TabKind::Workspace,
                title: "split".into(),
                title_source: TabTitleSource::Static,
                root_pane: Some(PaneNode::Group {
                    id: PaneId(9),
                    direction: SplitDirection::Horizontal,
                    children: vec![
                        PaneSplitChild::new(PaneNode::leaf(PaneId(3), TerminalSessionId(3)), 25.0),
                        PaneSplitChild::new(
                            PaneNode::Page {
                                pane_id: PaneId(4),
                                tab_id: TabId(4),
                            },
                            25.0,
                        ),
                        PaneSplitChild::new(PaneNode::leaf(PaneId(1), TerminalSessionId(1)), 25.0),
                        PaneSplitChild::new(PaneNode::leaf(PaneId(2), TerminalSessionId(2)), 25.0),
                    ],
                }),
                active_pane_id: Some(PaneId(1)),
            });
        });
    });
    host
}

#[gpui::test]
fn chooser_preserves_input_owner_and_starts_only_the_selected_terminal(cx: &mut TestAppContext) {
    let (_, cx) = cx.add_window_view(|_, _| TestRoot);
    let directory = tempfile::tempdir().unwrap();
    let host = fixture(cx, directory.path());
    cx.update(|window, cx| {
        let current = host.read(cx).panes()[&PaneId(1)].clone();
        let chosen = host.read(cx).panes()[&PaneId(3)].clone();
        let current_path = directory.path().join("current.log");
        let chosen_path = directory.path().join("chosen.log");
        current.update(cx, |pane, cx| {
            pane.focus(window, cx);
            pane.start_session_log_at_path(current_path.clone(), cx)
                .unwrap();
        });
        let focus = window.focused(cx);
        let targets = SessionLogTarget::for_tab(TabId(1), host.read(cx), cx);
        assert_eq!(
            targets
                .iter()
                .map(|target| target.pane_id)
                .collect::<Vec<_>>(),
            vec![PaneId(3), PaneId(1)]
        );
        let mut menu = SessionLogMenu {
            tab_id: TabId(1),
            window: window.window_handle(),
            position: gpui::point(px(0.0), px(0.0)),
            focus: cx.focus_handle(),
            return_focus: focus.clone(),
            mode: SessionLogMenuMode::Save,
            targets,
            selected: 1,
            show_picker: true,
            previous_preview: None,
            preview_generation: 0,
            preview_task: None,
            prompt_task: None,
        };
        assert!(
            !menu.actionable(1, host.read(cx), cx),
            "an existing log must not be replaced"
        );
        let next = menu.next_row(false, host.read(cx), cx).unwrap();
        menu.select(next);
        let target = menu.target(menu.selected).unwrap();
        assert_eq!(
            target.resolve(TabId(1), host.read(cx)),
            Some(chosen.clone())
        );
        assert_eq!(
            window.focused(cx),
            focus,
            "preview must not focus another terminal"
        );
        assert_eq!(
            host.read(cx).tab_by_id(TabId(1)).unwrap().active_pane_id,
            Some(PaneId(1))
        );
        fs::write(&current_path, b"existing log\n").unwrap();
        assert!(
            target
                .start_at_path(TabId(1), current_path.clone(), &host, cx)
                .is_err()
        );
        assert_eq!(fs::read(&current_path).unwrap(), b"existing log\n");
        assert_eq!(
            chosen.read(cx).session_log_status().state,
            TerminalSessionLogState::Idle
        );
        target
            .start_at_path(TabId(1), chosen_path.clone(), &host, cx)
            .unwrap();
        assert_eq!(chosen.read(cx).session_log_status().path, Some(chosen_path));
        assert_eq!(
            current.read(cx).session_log_status().path,
            Some(current_path)
        );
        assert_eq!(
            menu.next_row(true, host.read(cx), cx),
            None,
            "all panes are now saving"
        );
        chosen.update(cx, |pane, cx| pane.stop_session_log(cx).unwrap());
        assert_eq!(
            menu.next_row(true, host.read(cx), cx),
            Some(0),
            "skip the still-recording pane when wrapping upward"
        );
        current.update(cx, |pane, cx| pane.stop_session_log(cx).unwrap());
    });
}

#[gpui::test]
fn file_selection_target_rejects_closed_replaced_and_moved_panes(cx: &mut TestAppContext) {
    let (_, cx) = cx.add_window_view(|_, _| TestRoot);
    let directory = tempfile::tempdir().unwrap();
    let host = fixture(cx, directory.path());
    cx.update(|window, cx| {
        let target = SessionLogTarget::for_tab(TabId(1), host.read(cx), cx)[0].clone();
        let original = host.update(cx, |host, _| host.remove_terminal_pane(PaneId(3)).unwrap());
        assert_eq!(target.resolve(TabId(1), host.read(cx)), None);
        let replacement = host.read(cx).panes()[&PaneId(1)].clone();
        host.update(cx, |host, cx| {
            host.register_terminal_pane(
                PaneId(3),
                TerminalSessionId(3),
                replacement,
                window.window_handle(),
                cx,
            );
        });
        assert_eq!(
            target.resolve(TabId(1), host.read(cx)),
            None,
            "the same numeric ID cannot adopt a pending file dialog"
        );
        host.update(cx, |host, cx| {
            host.register_terminal_pane(
                PaneId(3),
                TerminalSessionId(3),
                original.clone(),
                window.window_handle(),
                cx,
            );
            host.bind_terminal_location(
                TerminalSessionId(3),
                tabs::TerminalLocation {
                    tab_id: TabId(1),
                    pane_id: PaneId(3),
                },
            );
            host.move_terminal_pane_to_tab(
                TabId(1),
                PaneId(3),
                Tab {
                    id: TabId(2),
                    kind: TabKind::LocalTerminal,
                    title: "moved".into(),
                    title_source: TabTitleSource::Static,
                    root_pane: Some(PaneNode::leaf(PaneId(3), TerminalSessionId(3))),
                    active_pane_id: Some(PaneId(3)),
                },
                window.window_handle(),
            );
        });
        assert_eq!(
            target.resolve(TabId(1), host.read(cx)),
            None,
            "a dialog from the old split cannot follow a moved pane"
        );
        let rejected_path = directory.path().join("stale.log");
        assert!(
            target
                .start_at_path(TabId(1), rejected_path.clone(), &host, cx)
                .is_err()
        );
        assert!(
            !rejected_path.exists(),
            "a stale selection must not create a file"
        );
        assert_eq!(target.resolve(TabId(2), host.read(cx)), Some(original));
    });
}
