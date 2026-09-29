use super::super::form_state::LoginScriptStepDraft;
use super::*;

impl WorkspaceApp {
    pub(super) fn render_login_script_steps(
        &self,
        steps: &[LoginScriptStepDraft],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut content = div()
            .flex()
            .flex_col()
            .gap(px(self.tokens.spacing.three))
            .child(self.i18n.t("ssh.form.login_script"))
            .child(self.render_connection_hint(self.i18n.t("ssh.form.login_script_hint")));
        for (index, step) in steps.iter().enumerate() {
            let controls = div()
                .flex()
                .gap(px(self.tokens.spacing.two))
                .child(self.login_script_button(
                    "ssh.form.login_script_up",
                    index == 0,
                    move |form| {
                        form.login_script.swap(index, index - 1);
                    },
                    cx,
                ))
                .child(self.login_script_button(
                    "ssh.form.login_script_down",
                    index + 1 == steps.len(),
                    move |form| {
                        form.login_script.swap(index, index + 1);
                    },
                    cx,
                ))
                .child(self.login_script_button(
                    "ssh.form.login_script_remove",
                    false,
                    move |form| {
                        form.login_script.remove(index);
                    },
                    cx,
                ));
            content = content.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(self.tokens.spacing.two))
                    .p(px(self.tokens.spacing.three))
                    .border_1()
                    .border_color(rgb(self.tokens.ui.border))
                    .rounded(px(self.tokens.radii.md))
                    .child(controls)
                    .child(self.render_connection_field(
                        self.i18n.t("ssh.form.login_script_expect"),
                        &step.expect,
                        self.i18n.t("ssh.form.login_script_expect_placeholder"),
                        NewConnectionField::LoginScriptExpect(index),
                        false,
                        cx,
                    ))
                    .child(self.render_connection_multiline_field(
                        self.i18n.t("ssh.form.login_script_send"),
                        &step.send,
                        self.i18n.t("ssh.form.post_connect_command_placeholder"),
                        NewConnectionField::LoginScriptSend(index),
                        cx,
                    ))
                    .child(self.render_connection_checkbox(
                        self.i18n.t("ssh.form.login_script_regex"),
                        step.is_regex,
                        move |form| {
                            form.login_script[index].is_regex = !form.login_script[index].is_regex;
                        },
                        cx,
                    ))
                    .child(self.render_connection_checkbox(
                        self.i18n.t("ssh.form.login_script_optional"),
                        step.optional,
                        move |form| {
                            form.login_script[index].optional = !form.login_script[index].optional;
                        },
                        cx,
                    )),
            );
        }
        content
            .child(self.login_script_button(
                "ssh.form.login_script_add",
                steps.len() >= oxideterm_connections::MAX_LOGIN_SCRIPT_STEPS,
                |form| form.login_script.push(LoginScriptStepDraft::default()),
                cx,
            ))
            .into_any_element()
    }

    fn login_script_button(
        &self,
        key: &'static str,
        disabled: bool,
        action: impl Fn(&mut NewConnectionForm) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.workspace_toolbar_action_button(
            self.i18n.t(key),
            None,
            ToolbarButtonOptions {
                button: ButtonOptions {
                    variant: ButtonVariant::Outline,
                    size: ButtonSize::Sm,
                    radius: ButtonRadius::Md,
                    disabled,
                },
                ..ToolbarButtonOptions::default()
            },
            cx.listener(move |this, _event, _window, cx| {
                if disabled {
                    return;
                }
                // Retire IME state before indices move, so composition cannot target another step.
                this.ime_marked_text = None;
                this.clear_ime_selection();
                this.update_connection_form_state(cx, |state| {
                    if let Some(form) = state.form.as_mut() {
                        action(form);
                        form.focused_field = NewConnectionField::PostConnectCommand;
                        form.field_focused = false;
                        clear_connection_selection(form);
                        form.error = None;
                    }
                });
                this.close_new_connection_select(cx);
                cx.stop_propagation();
                cx.notify();
            }),
        )
        .into_any_element()
    }
}
