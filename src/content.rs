//! Script-supplied content and its presentation options. Only the small
//! request crosses the socket; file decoding happens on its client thread.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use tiny_skia::Pixmap;

use crate::screens::{self, Screen};
use crate::theme::{Rgb, parse_hex};

#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Options {
    pub duration: Option<String>,
    pub until_next: bool,
    pub wait: bool,
    pub color: Option<String>,
    pub icon: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Source {
    Flash { message: String },
    Image { path: PathBuf },
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Submission {
    pub source: Source,
    pub options: Options,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Lifetime {
    #[default]
    Rotation,
    For(Duration),
    UntilNext,
}

pub struct Prepared {
    pub visual: Visual,
    pub lifetime: Lifetime,
    pub wait: bool,
}

pub enum Visual {
    Flash {
        message: String,
        color: Option<Rgb>,
        icon: Option<String>,
    },
    Image(Pixmap),
}

impl Prepared {
    /// A plain message for one rotation, as the legacy `flash TEXT` line
    /// asks for.
    pub fn message(message: String) -> Self {
        Self {
            visual: Visual::Flash {
                message,
                color: None,
                icon: None,
            },
            lifetime: Lifetime::Rotation,
            wait: false,
        }
    }

    pub fn screen(self) -> Box<dyn Screen> {
        match self.visual {
            Visual::Flash {
                message,
                color,
                icon,
            } => screens::message(message, color, icon),
            Visual::Image(picture) => screens::image(picture),
        }
    }
}

impl Submission {
    /// Parse real argv boundaries, preserving quoted messages and paths.
    /// `--` lets a message or filename begin with an option-like string.
    pub fn from_args(args: &[String]) -> Result<Self> {
        let (verb, args) = args
            .split_first()
            .ok_or_else(|| anyhow::anyhow!("missing command"))?;
        let mut options = Options::default();
        let mut words = Vec::new();
        let mut args = args.iter();
        let mut literal = false;
        while let Some(arg) = args.next() {
            if literal {
                words.push(arg.clone());
                continue;
            }
            match arg.as_str() {
                "--" => literal = true,
                "--for" => {
                    ensure!(options.duration.is_none(), "--for was given twice");
                    options.duration = Some(
                        args.next()
                            .ok_or_else(|| anyhow::anyhow!("--for needs a length"))?
                            .clone(),
                    );
                }
                "--until-next" => options.until_next = true,
                "--wait" => options.wait = true,
                "--color" | "--icon" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("{arg} needs a value"))?
                        .clone();
                    let slot = if arg == "--color" {
                        &mut options.color
                    } else {
                        &mut options.icon
                    };
                    ensure!(slot.is_none(), "{arg} was given twice");
                    *slot = Some(value);
                }
                _ if looks_like_option(arg) => {
                    bail!("unknown option {arg:?}; use -- before literal text or filenames")
                }
                _ => words.push(arg.clone()),
            }
        }
        let source = match verb.as_str() {
            "flash" => Source::Flash {
                message: words.join(" "),
            },
            "image" => {
                ensure!(
                    words.len() == 1,
                    "image needs one filename (quote paths containing spaces)"
                );
                Source::Image {
                    path: std::path::absolute(&words[0])?,
                }
            }
            _ => bail!("expected flash or image"),
        };
        let submission = Self { source, options };
        submission.validate()?;
        Ok(submission)
    }

    /// Whether this submission says nothing an old service's plain
    /// `flash TEXT` line can't.
    pub fn plain_message(&self) -> Option<&str> {
        match &self.source {
            Source::Flash { message }
                if self.options == Options::default() && !message.contains(['\n', '\r']) =>
            {
                Some(message)
            }
            _ => None,
        }
    }

    fn validate(&self) -> Result<(Lifetime, Option<Rgb>)> {
        let o = &self.options;
        ensure!(
            !(o.duration.is_some() && o.until_next),
            "use either --for or --until-next"
        );
        let lifetime = match &o.duration {
            Some(length) => {
                Lifetime::For(crate::control::parse_length(length).map_err(anyhow::Error::msg)?)
            }
            None if o.until_next => Lifetime::UntilNext,
            None => Lifetime::Rotation,
        };
        let color = o
            .color
            .as_deref()
            .map(|s| parse_hex(s).ok_or_else(|| anyhow::anyhow!("color must be #rrggbb or rrggbb")))
            .transpose()?;
        match &self.source {
            Source::Flash { message } => {
                ensure!(!message.trim().is_empty(), "flash needs a message");
                ensure!(message.len() <= 1024, "message is longer than 1024 bytes");
                if let Some(icon) = &o.icon {
                    ensure!(
                        !icon.trim().is_empty()
                            && icon.chars().count() <= 4
                            && !icon.chars().any(char::is_control),
                        "icon must be 1–4 printable characters from your theme font"
                    );
                }
            }
            Source::Image { path } => {
                ensure!(path.is_absolute(), "image path must be absolute");
                ensure!(
                    o.color.is_none() && o.icon.is_none(),
                    "--color and --icon apply to flash messages"
                );
            }
        }
        Ok((lifetime, color))
    }

    pub fn prepare(self) -> Result<Prepared> {
        let (lifetime, color) = self.validate()?;
        let visual = match self.source {
            Source::Flash { message } => Visual::Flash {
                message,
                color,
                icon: self.options.icon,
            },
            Source::Image { path } => Visual::Image(crate::picture::load(Path::new(&path))?),
        };
        Ok(Prepared {
            visual,
            lifetime,
            wait: self.options.wait,
        })
    }
}

/// A misspelled option rather than message text: `--wiat` or `-w`, but not
/// `-`, `-3°C` or a quoted phrase like "--verbose build failed".
fn looks_like_option(arg: &str) -> bool {
    let name = arg.strip_prefix("--").or_else(|| arg.strip_prefix('-'));
    name.and_then(|n| n.chars().next())
        .is_some_and(|c| c.is_ascii_alphabetic())
        && !arg.contains(char::is_whitespace)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn preserves_paths_messages_and_options_across_json() {
        let request = Submission::from_args(&args(&[
            "flash",
            "Build \"done\"\nready",
            "--for",
            "30s",
            "--color",
            "#ff8800",
            "--icon",
            "+",
            "--wait",
        ]))
        .unwrap();
        let json = serde_json::to_string(&request).unwrap();
        assert!(
            !json.contains('\n'),
            "one socket line even with quoted/newline text"
        );
        let decoded: Submission = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, request);
        let prepared = decoded.prepare().unwrap();
        assert_eq!(prepared.lifetime, Lifetime::For(Duration::from_secs(30)));
        assert!(prepared.wait);
        assert!(matches!(
            prepared.visual,
            Visual::Flash {
                color: Some(Rgb(255, 136, 0)),
                ..
            }
        ));
        let image = Submission::from_args(&args(&["image", "a b.png", "--until-next"])).unwrap();
        assert!(
            matches!(image.source, Source::Image { path } if path == std::path::absolute("a b.png").unwrap())
        );
        let literal = Submission::from_args(&args(&["flash", "--", "--wait"])).unwrap();
        assert!(!literal.options.wait);
        assert!(matches!(&literal.source, Source::Flash { message } if message == "--wait"));
        for (words, expected) in [
            (
                &["flash", "Tests", "passed", "-", "42/42"][..],
                "Tests passed - 42/42",
            ),
            (&["flash", "-3°C", "outside"], "-3°C outside"),
            (
                &["flash", "--verbose build failed"],
                "--verbose build failed",
            ),
        ] {
            let request = Submission::from_args(&args(words)).unwrap();
            assert!(
                matches!(&request.source, Source::Flash { message } if message == expected),
                "{words:?}"
            );
            assert_eq!(request.plain_message(), Some(expected));
        }
        assert_eq!(literal.plain_message(), Some("--wait"));
        assert_eq!(request.plain_message(), None, "options need JSON");
        assert!(matches!(
            Submission::from_args(&args(&["flash", "hello"]))
                .unwrap()
                .prepare()
                .unwrap()
                .lifetime,
            Lifetime::Rotation
        ));
    }

    #[test]
    fn rejects_ambiguous_or_invalid_options_before_contacting_the_service() {
        for words in [
            vec!["image"],
            vec!["image", "one.png", "two.png"],
            vec!["flash", " "],
            vec!["flash", "hello", "--for"],
            vec!["flash", "hello", "--for", "0s"],
            vec!["flash", "hello", "--for", "25h"],
            vec!["flash", "hello", "--for", "1s", "--until-next"],
            vec!["flash", "hello", "--for", "1s", "--for", "2s"],
            vec!["flash", "hello", "--color", "oops"],
            vec!["flash", "hello", "--icon", "12345"],
            vec!["flash", "hello", "--icon", "\n"],
            vec!["image", "x.png", "--color", "ff0000"],
            vec!["image", "x.png", "--icon", "+"],
            vec!["flash", "hello", "--unknown"],
            vec!["flash", "hello", "-w"],
        ] {
            assert!(Submission::from_args(&args(&words)).is_err(), "{words:?}");
        }
        let oversized = "x".repeat(1025);
        assert!(Submission::from_args(&args(&["flash", &oversized])).is_err());
        let request = Submission {
            source: Source::Image {
                path: "relative.png".into(),
            },
            options: Options::default(),
        };
        assert!(
            request.prepare().is_err(),
            "socket requests must name absolute paths too"
        );
        assert!(
            serde_json::from_str::<Submission>(
                r#"{"source":{"type":"flash","message":"ok","typo":1},"options":{}}"#
            )
            .is_err()
        );
    }
}
