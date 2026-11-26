use crate::wire::TerminalHistory;
use crate::{TERMINAL_HISTORY_MAX_BYTES, TERMINAL_HISTORY_MAX_COMMANDS, TERMINAL_HISTORY_MAX_ROWS};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandOutput {
    pub text: String,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandOutputError {
    NoCommands,
    NoOutput,
}

pub fn validate_history(history: &TerminalHistory) -> Result<(), &'static str> {
    let mut size = history
        .lines
        .iter()
        .map(|line| line.len() + 8)
        .sum::<usize>();
    if !(1..=1024).contains(&history.columns)
        || history.lines.len() > TERMINAL_HISTORY_MAX_ROWS
        || size > TERMINAL_HISTORY_MAX_BYTES
        || history
            .lines
            .iter()
            .any(|line| line.chars().any(char::is_control))
    {
        return Err("invalid terminal history");
    }

    let mut previous_style = (0, 0);
    for style in &history.styles {
        let position = (style.row, style.offset);
        size += style.sgr.len() + 20;
        if size > TERMINAL_HISTORY_MAX_BYTES
            || position < previous_style
            || !history
                .lines
                .get(style.row as usize)
                .is_some_and(|line| line.is_char_boundary(style.offset as usize))
            || style.sgr.len() > 96
            || !style
                .sgr
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b';')
        {
            return Err("invalid terminal history style");
        }
        previous_style = position;
    }

    size += history.wrapped_rows.len() * 4;
    if size > TERMINAL_HISTORY_MAX_BYTES {
        return Err("invalid terminal history");
    }
    let mut previous_wrapped: Option<u32> = None;
    for &row in &history.wrapped_rows {
        if (row as usize) >= history.lines.len() || previous_wrapped.is_some_and(|prev| row <= prev)
        {
            return Err("invalid terminal history wrapped row");
        }
        previous_wrapped = Some(row);
    }

    if history.commands.len() > TERMINAL_HISTORY_MAX_COMMANDS {
        return Err("invalid terminal history commands");
    }
    size += history.commands.len() * 32;
    if size > TERMINAL_HISTORY_MAX_BYTES {
        return Err("invalid terminal history");
    }
    for cmd in &history.commands {
        if let Some(prompt_row) = cmd.prompt_row
            && (prompt_row as usize) >= history.lines.len()
        {
            return Err("invalid terminal history command prompt");
        }
        let Some(start_row) = cmd.output_start_row else {
            if cmd.output_start_offset.is_some()
                || cmd.output_end_row.is_some()
                || cmd.output_end_offset.is_some()
            {
                return Err("invalid terminal history command range");
            }
            continue;
        };
        let Some(start_line) = history.lines.get(start_row as usize) else {
            return Err("invalid terminal history command start");
        };
        let start_offset = cmd.output_start_offset.unwrap_or(0);
        if (start_offset as usize) > start_line.len()
            || !start_line.is_char_boundary(start_offset as usize)
        {
            return Err("invalid terminal history command start offset");
        }
        if let Some(end_row) = cmd.output_end_row {
            let Some(end_line) = history.lines.get(end_row as usize) else {
                return Err("invalid terminal history command end");
            };
            let end_offset = cmd.output_end_offset.unwrap_or(0);
            if (end_offset as usize) > end_line.len()
                || !end_line.is_char_boundary(end_offset as usize)
                || (end_row, end_offset) < (start_row, start_offset)
            {
                return Err("invalid terminal history command end offset");
            }
        } else if cmd.output_end_offset.is_some() {
            return Err("invalid terminal history command end offset");
        }
    }

    let mut previous_link: Option<(u32, u32)> = None;
    for link in &history.hyperlinks {
        size += 24 + link.uri.len() + link.id.as_deref().map_or(0, str::len);
        if size > TERMINAL_HISTORY_MAX_BYTES {
            return Err("invalid terminal history");
        }
        let Some(line) = history.lines.get(link.row as usize) else {
            return Err("invalid terminal history hyperlink");
        };
        if link.start_offset >= link.end_offset
            || (link.end_offset as usize) > line.len()
            || !line.is_char_boundary(link.start_offset as usize)
            || !line.is_char_boundary(link.end_offset as usize)
            || previous_link.is_some_and(|prev| (link.row, link.start_offset) < prev)
            || crate::sanitize_hyperlink(&link.uri, link.id.as_deref()).is_none()
        {
            return Err("invalid terminal history hyperlink");
        }
        previous_link = Some((link.row, link.end_offset));
    }

    Ok(())
}

pub fn jumpable_commands(history: &TerminalHistory) -> Vec<usize> {
    let has_execution = history
        .commands
        .iter()
        .any(|cmd| cmd.output_start_row.is_some());
    history
        .commands
        .iter()
        .enumerate()
        .filter_map(|(index, cmd)| {
            (cmd.prompt_row.is_some() && (!has_execution || cmd.output_start_row.is_some()))
                .then_some(index)
        })
        .collect()
}

pub fn previous_command(history: &TerminalHistory, current: Option<usize>) -> Option<usize> {
    let jumpable = jumpable_commands(history);
    if jumpable.is_empty() {
        return None;
    }
    match current {
        None => {
            let has_execution = history
                .commands
                .iter()
                .any(|cmd| cmd.output_start_row.is_some());
            if !has_execution && jumpable.len() >= 2 {
                let last_idx = *jumpable.last().unwrap();
                let last_row = history.commands[last_idx].prompt_row? as usize;
                let last_non_empty = history.lines.iter().rposition(|line| !line.is_empty());
                if last_non_empty.is_none_or(|row| last_row >= row) {
                    return Some(jumpable[jumpable.len() - 2]);
                }
            }
            jumpable.last().copied()
        }
        Some(cur) => jumpable.iter().copied().rfind(|&index| index < cur),
    }
}

pub fn next_command(history: &TerminalHistory, current: Option<usize>) -> Option<usize> {
    let cur = current?;
    jumpable_commands(history)
        .into_iter()
        .find(|&index| index > cur)
}

pub fn command_output(
    history: &TerminalHistory,
    index: usize,
) -> Result<CommandOutput, CommandOutputError> {
    let Some(cmd) = history.commands.get(index) else {
        return Err(CommandOutputError::NoCommands);
    };
    let Some(start_row) = cmd.output_start_row.map(|row| row as usize) else {
        return Err(CommandOutputError::NoOutput);
    };
    let start_offset = cmd.output_start_offset.unwrap_or(0) as usize;
    if start_row >= history.lines.len() || start_offset > history.lines[start_row].len() {
        return Err(CommandOutputError::NoOutput);
    }

    let (last_row, end_offset) = if let Some(end_row) = cmd.output_end_row.map(|row| row as usize) {
        if end_row >= history.lines.len() {
            return Err(CommandOutputError::NoOutput);
        }
        let eo = cmd.output_end_offset.unwrap_or(0) as usize;
        if eo == 0 {
            if end_row <= start_row {
                return Err(CommandOutputError::NoOutput);
            }
            (end_row - 1, history.lines[end_row - 1].len())
        } else {
            (end_row, eo.min(history.lines[end_row].len()))
        }
    } else {
        let mut last = None;
        for r in (start_row..history.lines.len()).rev() {
            let slice = if r == start_row {
                &history.lines[r][start_offset..]
            } else {
                history.lines[r].as_str()
            };
            if !slice.is_empty() {
                last = Some(r);
                break;
            }
        }
        let Some(r) = last else {
            return Err(CommandOutputError::NoOutput);
        };
        (r, history.lines[r].len())
    };

    if (last_row, end_offset) <= (start_row, start_offset) {
        return Err(CommandOutputError::NoOutput);
    }

    let (eff_start_row, eff_start_offset) = if start_offset > 0
        && start_row < last_row
        && history.lines[start_row][start_offset..].is_empty()
    {
        (start_row + 1, 0)
    } else {
        (start_row, start_offset)
    };

    let mut text = String::new();
    for r in eff_start_row..=last_row {
        let line = &history.lines[r];
        let from = if r == eff_start_row {
            eff_start_offset.min(line.len())
        } else {
            0
        };
        let to = if r == last_row {
            end_offset.min(line.len())
        } else {
            line.len()
        };
        if from < to {
            text.push_str(&line[from..to]);
        }
        if r < last_row && history.wrapped_rows.binary_search(&(r as u32)).is_err() {
            text.push('\n');
        }
    }

    if text.is_empty() {
        return Err(CommandOutputError::NoOutput);
    }

    Ok(CommandOutput {
        text,
        truncated: cmd.truncated,
    })
}

pub fn last_command_output(history: &TerminalHistory) -> Result<CommandOutput, CommandOutputError> {
    let Some(index) = history
        .commands
        .iter()
        .rposition(|cmd| cmd.output_start_row.is_some())
    else {
        return Err(CommandOutputError::NoCommands);
    };
    command_output(history, index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::HistoryCommand;

    fn history(lines: &[&str], wrapped: &[u32], commands: Vec<HistoryCommand>) -> TerminalHistory {
        TerminalHistory {
            columns: 80,
            lines: lines.iter().map(|s| (*s).to_owned()).collect(),
            wrapped_rows: wrapped.to_vec(),
            commands,
            ..Default::default()
        }
    }

    #[test]
    fn validates_wrapped_rows_and_commands() {
        let valid = history(
            &["$ echo hi", "hello", "$ "],
            &[0],
            vec![HistoryCommand {
                prompt_row: Some(0),
                output_start_row: Some(1),
                output_start_offset: Some(0),
                output_end_row: Some(2),
                output_end_offset: Some(0),
                finished: true,
                truncated: false,
            }],
        );
        assert!(validate_history(&valid).is_ok());

        let mut bad_wrap = valid.clone();
        bad_wrap.wrapped_rows = vec![1, 1];
        assert!(validate_history(&bad_wrap).is_err());

        let mut bad_end = valid.clone();
        bad_end.commands[0].output_end_row = Some(0);
        assert!(validate_history(&bad_end).is_err());

        let mut orphan_end = valid.clone();
        orphan_end.commands[0].output_start_row = None;
        assert!(validate_history(&orphan_end).is_err());

        let mut bad_utf8 = history(
            &["한글"],
            &[],
            vec![HistoryCommand {
                output_start_row: Some(0),
                output_start_offset: Some(1),
                ..Default::default()
            }],
        );
        assert!(validate_history(&bad_utf8).is_err());
        bad_utf8.commands[0].output_start_offset = Some(3);
        assert!(validate_history(&bad_utf8).is_ok());
    }

    #[test]
    fn navigates_commands_and_skips_empty_prompts_when_c_is_used() {
        let h = history(
            &["$ ls", "a", "$ ", "$ pwd", "/tmp", "$ "],
            &[],
            vec![
                HistoryCommand {
                    prompt_row: Some(0),
                    output_start_row: Some(1),
                    output_end_row: Some(2),
                    finished: true,
                    ..Default::default()
                },
                HistoryCommand {
                    prompt_row: Some(2),
                    ..Default::default()
                },
                HistoryCommand {
                    prompt_row: Some(3),
                    output_start_row: Some(4),
                    output_end_row: Some(5),
                    finished: true,
                    ..Default::default()
                },
                HistoryCommand {
                    prompt_row: Some(5),
                    ..Default::default()
                },
            ],
        );
        assert_eq!(jumpable_commands(&h), vec![0, 2]);
        assert_eq!(previous_command(&h, None), Some(2));
        assert_eq!(previous_command(&h, Some(2)), Some(0));
        assert_eq!(previous_command(&h, Some(0)), None);
        assert_eq!(next_command(&h, Some(0)), Some(2));
        assert_eq!(next_command(&h, Some(2)), None);
    }

    #[test]
    fn navigates_prompt_only_shells_without_c_markers() {
        let h = history(
            &["$ cmd1", "out1", "$ cmd2", "out2", "$ "],
            &[],
            vec![
                HistoryCommand {
                    prompt_row: Some(0),
                    ..Default::default()
                },
                HistoryCommand {
                    prompt_row: Some(2),
                    ..Default::default()
                },
                HistoryCommand {
                    prompt_row: Some(4),
                    ..Default::default()
                },
            ],
        );
        assert_eq!(jumpable_commands(&h), vec![0, 1, 2]);
        assert_eq!(previous_command(&h, None), Some(1));
        assert_eq!(previous_command(&h, Some(1)), Some(0));
    }

    #[test]
    fn extracts_wrapped_multiline_no_newline_and_running_output() {
        // Wrapped output + trailing newline before D at col 0
        let h = history(
            &["$ cmd", "abcdef", "gh", "line2", "$ "],
            &[1],
            vec![HistoryCommand {
                prompt_row: Some(0),
                output_start_row: Some(0),
                output_start_offset: Some(5),
                output_end_row: Some(4),
                output_end_offset: Some(0),
                finished: true,
                truncated: false,
            }],
        );
        assert_eq!(
            last_command_output(&h).unwrap(),
            CommandOutput {
                text: "abcdefgh\nline2".into(),
                truncated: false,
            }
        );

        // No trailing newline: D arrives at offset 3 on row 1
        let no_nl = history(
            &["$ printf hi", "hi$ "],
            &[],
            vec![HistoryCommand {
                prompt_row: Some(0),
                output_start_row: Some(1),
                output_start_offset: Some(0),
                output_end_row: Some(1),
                output_end_offset: Some(2),
                finished: true,
                truncated: true,
            }],
        );
        assert_eq!(
            last_command_output(&no_nl).unwrap(),
            CommandOutput {
                text: "hi".into(),
                truncated: true,
            }
        );

        // Running command without end_row skips trailing blank rows
        let running = history(
            &["$ tail", "line1", "", ""],
            &[],
            vec![HistoryCommand {
                prompt_row: Some(0),
                output_start_row: Some(1),
                finished: false,
                ..Default::default()
            }],
        );
        assert_eq!(
            last_command_output(&running).unwrap().text,
            "line1".to_owned()
        );

        // Empty output
        let empty = history(
            &["$ true", "$ "],
            &[],
            vec![HistoryCommand {
                prompt_row: Some(0),
                output_start_row: Some(1),
                output_end_row: Some(1),
                output_end_offset: Some(0),
                finished: true,
                ..Default::default()
            }],
        );
        assert_eq!(
            last_command_output(&empty),
            Err(CommandOutputError::NoOutput)
        );

        // No commands at all
        assert_eq!(
            last_command_output(&history(&["hello"], &[], vec![])),
            Err(CommandOutputError::NoCommands)
        );
    }

    #[test]
    fn validates_and_rejects_history_hyperlinks() {
        let mut valid = history(&["open link here"], &[], vec![]);
        valid.hyperlinks.push(crate::wire::HistoryHyperlink {
            row: 0,
            start_offset: 5,
            end_offset: 9,
            uri: "https://example.test".into(),
            id: Some("id1".into()),
        });
        assert_eq!(validate_history(&valid), Ok(()));

        let mut overlapping = valid.clone();
        overlapping.hyperlinks.push(crate::wire::HistoryHyperlink {
            row: 0,
            start_offset: 8,
            end_offset: 12,
            uri: "https://other.test".into(),
            id: None,
        });
        assert_eq!(
            validate_history(&overlapping),
            Err("invalid terminal history hyperlink")
        );

        let mut bad_uri = history(&["open link"], &[], vec![]);
        bad_uri.hyperlinks.push(crate::wire::HistoryHyperlink {
            row: 0,
            start_offset: 0,
            end_offset: 4,
            uri: "https://bad\x07".into(),
            id: None,
        });
        assert_eq!(
            validate_history(&bad_uri),
            Err("invalid terminal history hyperlink")
        );
    }
}
