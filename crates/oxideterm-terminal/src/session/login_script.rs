use std::collections::VecDeque;

use oxideterm_ssh::LoginScriptStep;
use regex::Regex;
use zeroize::{Zeroize, Zeroizing};

const MAX_OUTPUT_BYTES: usize = 16 * 1024;

struct Step {
    source: LoginScriptStep,
    pattern: Option<Regex>,
}

#[derive(Default)]
struct Output(Zeroizing<String>);

impl vte::Perform for Output {
    fn print(&mut self, character: char) {
        // Retain only a bounded suffix, including prompts split across network packets.
        if self.0.len() + character.len_utf8() > MAX_OUTPUT_BYTES {
            let mut boundary = MAX_OUTPUT_BYTES / 2;
            while !self.0.is_char_boundary(boundary) {
                boundary += 1;
            }
            let tail = Zeroizing::new(self.0[boundary..].to_string());
            self.0.zeroize();
            self.0.push_str(&tail);
        }
        self.0.push(character);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' | b'\t' => self.print(byte as char),
            8 => {
                self.0.pop();
            }
            _ => {}
        }
    }
}

/// The SSH parser owns this state. Completion, cancellation and session teardown drop its secrets.
pub(super) struct LoginScriptRunner {
    steps: VecDeque<Step>,
    parser: vte::Parser,
    output: Output,
}

impl LoginScriptRunner {
    pub(super) fn new(
        steps: Vec<LoginScriptStep>,
    ) -> Result<Self, oxideterm_ssh::LoginScriptError> {
        if steps.len() > oxideterm_ssh::MAX_LOGIN_SCRIPT_STEPS + 1 {
            return Err(oxideterm_ssh::LoginScriptError::TooManySteps);
        }
        let steps = steps
            .into_iter()
            .map(|source| {
                let pattern = source.compile_pattern()?;
                Ok(Step { source, pattern })
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            steps,
            parser: vte::Parser::new(),
            output: Output::default(),
        })
    }

    pub(super) fn finished(&self) -> bool {
        self.steps.is_empty()
    }

    pub(super) fn advance(&mut self, bytes: &[u8]) -> Vec<Zeroizing<String>> {
        self.parser.advance(&mut self.output, bytes);
        let mut sends = Vec::new();
        loop {
            let Some(first) = self.steps.front() else {
                break;
            };
            if first.source.expect.is_empty() {
                let step = self.steps.pop_front().unwrap();
                sends.push(command_input(step.source.send.expose_secret()));
                continue;
            }
            if self.output.0.is_empty() {
                break;
            }
            // An optional prompt is skipped only when a later reachable prompt arrives.
            // A packet boundary by itself is never evidence that the prompt was absent.
            let mut matched = None;
            for (index, step) in self.steps.iter().enumerate() {
                if step.source.expect.is_empty() {
                    break;
                }
                let found = match &step.pattern {
                    Some(pattern) => pattern.is_match(&self.output.0),
                    None => self.output.0.contains(step.source.expect.expose_secret()),
                };
                if found {
                    matched = Some(index);
                    break;
                }
                if !step.source.optional {
                    break;
                }
            }
            let Some(index) = matched else {
                break;
            };
            for _ in 0..index {
                self.steps.pop_front();
            }
            let step = self.steps.pop_front().unwrap();
            sends.push(command_input(step.source.send.expose_secret()));
            // Replies cannot reuse output produced before the command was sent.
            self.output.0.zeroize();
        }
        if !sends.is_empty() {
            self.output.0.zeroize();
        }
        sends
    }
}

fn command_input(text: &str) -> Zeroizing<String> {
    let mut result = Zeroizing::new(String::with_capacity(text.len() + 1));
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                result.push('\r');
            }
            '\n' => result.push('\r'),
            other => result.push(other),
        }
    }
    if !result.ends_with('\r') {
        result.push('\r');
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(expect: &str, send: &str, is_regex: bool, optional: bool) -> LoginScriptStep {
        LoginScriptStep {
            expect: expect.into(),
            send: send.into(),
            is_regex,
            optional,
        }
    }

    #[test]
    fn split_colored_prompts_run_in_order_without_reusing_output() {
        let mut runner = LoginScriptRunner::new(vec![
            step("", "if true; then\r\n  pwd\nfi", false, false),
            step("Password: ", "test-response", false, false),
            step(r"user@host.*[$] $", "cd /srv/app", true, false),
            step("", "ls", false, false),
        ])
        .unwrap();
        assert_eq!(
            runner
                .advance(b"")
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>(),
            ["if true; then\r  pwd\rfi\r"]
        );
        assert!(runner.advance(b"\x1b[31mPass").is_empty());
        assert_eq!(
            runner
                .advance(b"word:\x1b[0m ")
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>(),
            ["test-response\r"]
        );
        assert!(runner.advance(b"word: ").is_empty());
        assert_eq!(
            runner
                .advance(b"user@host:~$ ")
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>(),
            ["cd /srv/app\r", "ls\r"]
        );
        assert!(runner.advance(b"user@host:~$ ").is_empty());
    }

    #[test]
    fn optional_prompt_survives_packets_and_skips_only_for_a_later_match() {
        for optional_present in [true, false] {
            let mut runner = LoginScriptRunner::new(vec![
                step("notice:", "accept", false, true),
                step("ready>", "work", false, false),
            ])
            .unwrap();
            assert!(runner.advance(b"banner\r\nnot").is_empty());
            if optional_present {
                assert_eq!(runner.advance(b"ice:")[0].as_str(), "accept\r");
            }
            assert_eq!(runner.advance(b"\r\nready>")[0].as_str(), "work\r");
            assert!(runner.finished());
        }
    }

    #[test]
    fn same_prompt_must_arrive_again_after_each_send() {
        for (expect, regex) in [("$ ", false), (r"[$] $", true), (".*", true)] {
            let mut runner = LoginScriptRunner::new(vec![
                step(expect, "one", regex, false),
                step(expect, "two", regex, false),
            ])
            .unwrap();
            assert!(runner.advance(b"").is_empty());
            assert_eq!(
                runner
                    .advance(b"$ $ ")
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>(),
                ["one\r"]
            );
            assert!(runner.advance(b"").is_empty());
            assert_eq!(
                runner
                    .advance(b"$ ")
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>(),
                ["two\r"]
            );
        }
    }

    #[test]
    fn large_output_and_split_utf8_do_not_match_hidden_control_strings() {
        let mut runner =
            LoginScriptRunner::new(vec![step("口令：", "response", false, false)]).unwrap();
        assert!(runner.advance("\x1b]0;口令：\x07".as_bytes()).is_empty());
        assert!(runner.advance(&vec![b'x'; MAX_OUTPUT_BYTES * 3]).is_empty());
        let prompt = "\x1b[32m口令：\x1b[0m".as_bytes();
        let split = 7;
        assert!(runner.advance(&prompt[..split]).is_empty());
        assert_eq!(
            runner
                .advance(&prompt[split..])
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>(),
            ["response\r"]
        );
    }

    #[test]
    fn invalid_regex_error_never_echoes_the_pattern() {
        assert!(matches!(
            LoginScriptRunner::new(vec![step("[private-token", "", true, false)]),
            Err(oxideterm_ssh::LoginScriptError::InvalidRegex)
        ));
    }
}
