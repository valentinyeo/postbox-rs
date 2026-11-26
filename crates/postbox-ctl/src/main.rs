use postbox_core::ThemeChoice;
use postbox_core::control::ControlRequest;

fn parse_args(args: &[String]) -> Result<ControlRequest, String> {
    let cmd = args.first().map(String::as_str).unwrap_or("status");
    match cmd {
        "status" => Ok(ControlRequest::Status),
        "theme" => {
            let choice = match args.get(1).map(String::as_str) {
                Some("dark") | Some("zinc") => ThemeChoice::ZincDark,
                Some("amoled") => ThemeChoice::Amoled,
                Some("light") => ThemeChoice::Light,
                _ => return Err("usage: postboxctl theme <zinc|amoled|light>".into()),
            };
            Ok(ControlRequest::Theme { theme: choice })
        }
        "font" => {
            let percent = args
                .get(1)
                .and_then(|v| v.parse::<u16>().ok())
                .ok_or_else(|| "usage: postboxctl font <60..160>".to_owned())?;
            Ok(ControlRequest::Font { percent })
        }
        "search" => Ok(ControlRequest::Search {
            text: args.get(1).cloned().unwrap_or_default(),
        }),
        "view" => {
            let target = args.get(1).cloned().ok_or_else(|| {
                "usage: postboxctl view <N|inbox|whatsapp|slack|telegram|signal>".to_owned()
            })?;
            Ok(ControlRequest::View { target })
        }
        "archive" => {
            let id = args
                .get(1)
                .cloned()
                .ok_or_else(|| "usage: postboxctl archive <id>".to_owned())?;
            Ok(ControlRequest::Archive { id })
        }
        "unarchive" => {
            let id = args
                .get(1)
                .cloned()
                .ok_or_else(|| "usage: postboxctl unarchive <id>".to_owned())?;
            Ok(ControlRequest::Unarchive { id })
        }
        other => Err(format!("unknown command: {other}")),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&args) {
        Ok(req) => {
            let line = serde_json::to_string(&req).expect("serialize control request");
            println!("{line}");
        }
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_theme_and_font_commands() {
        assert_eq!(
            parse_args(&["theme".into(), "amoled".into()]).unwrap(),
            ControlRequest::Theme {
                theme: ThemeChoice::Amoled
            }
        );
        assert_eq!(
            parse_args(&["font".into(), "110".into()]).unwrap(),
            ControlRequest::Font { percent: 110 }
        );
    }
}
