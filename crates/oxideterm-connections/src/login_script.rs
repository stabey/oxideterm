use serde::{Deserialize, Serialize};

use crate::SecretString;

pub const MAX_LOGIN_SCRIPT_STEPS: usize = 64;
pub const MAX_LOGIN_SCRIPT_TEXT_BYTES: usize = 8192;

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum LoginScriptError {
    #[error("Login script has too many steps")]
    TooManySteps,
    #[error("Login script text is too long")]
    TextTooLong,
    #[error("Login script has an invalid regular expression")]
    InvalidRegex,
}

impl LoginScriptError {
    pub fn translation_key(self) -> &'static str {
        match self {
            Self::TooManySteps => "ssh.form.login_script_too_many_steps",
            Self::TextTooLong => "ssh.form.login_script_text_too_long",
            Self::InvalidRegex => "ssh.form.login_script_invalid_regex",
        }
    }
}

pub fn validate_login_script(steps: &[LoginScriptStep]) -> Result<(), LoginScriptError> {
    if steps.len() > MAX_LOGIN_SCRIPT_STEPS {
        return Err(LoginScriptError::TooManySteps);
    }
    for step in steps {
        step.compile_pattern()?;
    }
    Ok(())
}

/// One terminal-local action. SecretString redacts diagnostics and clears both fields on drop.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct LoginScriptStep {
    pub expect: SecretString,
    pub send: SecretString,
    #[serde(default)]
    pub is_regex: bool,
    #[serde(default)]
    pub optional: bool,
}

impl LoginScriptStep {
    pub fn command(command: impl Into<String>) -> Self {
        Self {
            send: SecretString::new(command),
            ..Self::default()
        }
    }

    pub fn compile_pattern(&self) -> Result<Option<regex::Regex>, LoginScriptError> {
        if self.expect.expose_secret().len() > MAX_LOGIN_SCRIPT_TEXT_BYTES
            || self.send.expose_secret().len() > MAX_LOGIN_SCRIPT_TEXT_BYTES
        {
            return Err(LoginScriptError::TextTooLong);
        }
        if self.is_regex && !self.expect.is_empty() {
            regex::RegexBuilder::new(self.expect.expose_secret())
                .size_limit(256 * 1024)
                .build()
                .map(Some)
                .map_err(|_| LoginScriptError::InvalidRegex)
        } else {
            Ok(None)
        }
    }
}

/// Preserve the published startup command before the ordered interactive steps.
pub fn terminal_login_script(
    command: Option<&str>,
    steps: &[LoginScriptStep],
) -> Vec<LoginScriptStep> {
    command
        .filter(|command| !command.trim().is_empty())
        .map(|command| LoginScriptStep::command(command.trim()))
        .into_iter()
        .chain(steps.iter().cloned())
        .collect()
}
