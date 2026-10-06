use crate::process::Operation;
use crate::synctex::SourceLocation;
use serde::{Deserialize, Serialize};
use std::io;
use std::process::Command;

/// Editor selection is trusted configuration. Document paths are arguments, never shell code.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "lowercase", deny_unknown_fields)]
pub enum Editor {
    #[default]
    None,
    Socket {
        path: String,
    },
    Command {
        argv: Vec<String>,
    },
}

impl Editor {
    pub(crate) fn socket_mut(&mut self) -> Option<&mut String> {
        match self {
            Self::Socket { path } => Some(path),
            _ => None,
        }
    }

    pub fn validate(&self) -> io::Result<()> {
        match self {
            Self::Socket { path } if path.is_empty() => {
                Err(io::Error::other("editor socket path must not be empty"))
            }
            Self::Command { argv } => {
                if argv.is_empty() || argv[0].is_empty() || argv[0].contains(['{', '}']) {
                    return Err(io::Error::other(
                        "editor command requires a literal executable as argv[0]",
                    ));
                }
                let sample = SourceLocation {
                    file: String::new(),
                    line: 1,
                    byte_column: 0,
                    column: 1,
                    column_char: 1,
                    precise: false,
                };
                for argument in &argv[1..] {
                    expand(argument, &sample)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    pub fn deliver(&self, location: &SourceLocation, operation: &Operation) -> io::Result<()> {
        operation.check()?;
        match self {
            Self::None => Ok(()),
            Self::Socket { path } => {
                let mut stream = crate::ipc::connect(path, operation)?;
                crate::ipc::send(&mut stream, &serde_json::to_vec(location)?, operation)
            }
            Self::Command { argv } => {
                self.validate()?;
                let arguments = argv[1..]
                    .iter()
                    .map(|argument| expand(argument, location))
                    .collect::<io::Result<Vec<_>>>()?;
                let output =
                    crate::process::output(Command::new(&argv[0]).args(arguments), operation)?;
                if output.status.success() {
                    Ok(())
                } else {
                    Err(io::Error::other(format!(
                        "editor command exited with {}: {}",
                        output.status,
                        String::from_utf8_lossy(&output.stderr).trim()
                    )))
                }
            }
        }
    }
}

fn expand(template: &str, location: &SourceLocation) -> io::Result<String> {
    let mut result = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        result.push_str(&rest[..start]);
        let end = rest[start..]
            .find('}')
            .map(|offset| start + offset)
            .ok_or_else(|| io::Error::other("unclosed editor placeholder"))?;
        let value = match &rest[start + 1..end] {
            "file" => location.file.clone(),
            "line" => location.line.to_string(),
            "column" => location.column.to_string(),
            "column_char" => location.column_char.to_string(),
            "byte_column" => location.byte_column.to_string(),
            "column_byte" => (location.byte_column + 1).to_string(),
            unknown => {
                return Err(io::Error::other(format!(
                    "unknown editor placeholder: {unknown}"
                )));
            }
        };
        result.push_str(&value);
        rest = &rest[end + 1..];
    }
    result.push_str(rest);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_delivers_literal_filename_and_explicit_columns() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("args");
        let editor = Editor::Command {
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf '%s\\n' \"$@\" > \"$0\"".into(),
                output.to_str().unwrap().into(),
                "{file}".into(),
                "{line}:{column}:{column_char}:{byte_column}:{column_byte}".into(),
            ],
        };
        let location = SourceLocation {
            file: "$(touch should-not-exist); {line} ' \".tex".into(),
            line: 7,
            byte_column: 5,
            column: 4,
            column_char: 3,
            precise: true,
        };
        editor.deliver(&location, &Operation::default()).unwrap();
        assert_eq!(
            std::fs::read_to_string(output).unwrap(),
            format!("{}\n7:4:3:5:6\n", location.file)
        );
        assert!(
            Editor::Command {
                argv: vec!["false".into()]
            }
            .deliver(&location, &Operation::default())
            .is_err()
        );
        assert!(
            Editor::Command {
                argv: vec!["echo".into(), "{typo}".into()]
            }
            .validate()
            .is_err()
        );
    }
}
