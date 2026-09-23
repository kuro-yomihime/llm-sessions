//! Hand-off, not launching.
//!
//! `llm-sessions` never spawns or supervises an agent. It prints the command that
//! resumes the selected session on stdout while the TUI renders on stderr.
//! Inspect the shell-quoted output before pasting it into a shell.

/// POSIX-shell quoting for a command the user can inspect and paste.
pub fn shell_quote(argument: &str) -> String {
    if !argument.is_empty()
        && argument
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./:=@,+".contains(&b))
    {
        return argument.to_string();
    }
    // Single quotes protect everything except a single quote itself.
    format!("'{}'", argument.replace('\'', r"'\''"))
}

pub fn shell_line(command: &[String]) -> String {
    command
        .iter()
        .map(|part| shell_quote(part))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_arguments_are_left_alone() {
        assert_eq!(shell_quote("codex"), "codex");
        assert_eq!(shell_quote("01a0-7b22"), "01a0-7b22");
        assert_eq!(shell_quote("/home/u/x.jsonl"), "/home/u/x.jsonl");
    }

    #[test]
    fn hostile_arguments_cannot_break_out() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("$(rm -rf /)"), "'$(rm -rf /)'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn a_command_joins_into_one_line() {
        let command = vec!["codex".to_string(), "resume".to_string(), "a b".to_string()];
        assert_eq!(shell_line(&command), "codex resume 'a b'");
    }
}
