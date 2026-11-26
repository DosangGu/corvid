use crate::wire::ReportedDirectory;

pub const MAX_REPORTED_HOST_BYTES: usize = 256;
pub const MAX_REPORTED_PATH_BYTES: usize = 1024;

pub fn parse_osc7_or_pwd(raw: &str) -> Option<ReportedDirectory> {
    let raw = raw.trim_end_matches('\0');
    if raw.is_empty() {
        return None;
    }

    if let Some(rest) = strip_file_scheme(raw) {
        let authority_and_path = rest.strip_prefix("//")?;
        let (raw_host, raw_path) = match authority_and_path.find('/') {
            Some(slash) => (&authority_and_path[..slash], &authority_and_path[slash..]),
            None => (authority_and_path, ""),
        };
        if raw_path.is_empty() {
            return None;
        }
        let decoded_path = percent_decode_utf8(raw_path)?;
        let normalized_path = normalize_file_uri_path(&decoded_path)?;
        let host = normalize_host(raw_host)?;
        return sanitize_reported_directory(&ReportedDirectory {
            host,
            path: normalized_path,
        });
    }

    // Raw absolute path from Windows Terminal OSC 9;9 or VS Code OSC 633;P;Cwd=...
    if !is_raw_absolute_path(raw) {
        return None;
    }
    sanitize_reported_directory(&ReportedDirectory {
        host: None,
        path: raw.to_owned(),
    })
}

pub fn sanitize_reported_directory(dir: &ReportedDirectory) -> Option<ReportedDirectory> {
    if dir.path.is_empty()
        || dir.path.len() > MAX_REPORTED_PATH_BYTES
        || dir.path.chars().any(is_unsafe_char)
        || !is_raw_absolute_path(&dir.path)
    {
        return None;
    }
    let host = match dir.host.as_deref().map(str::trim).filter(|h| !h.is_empty()) {
        Some(host) => {
            let trimmed = host.trim_end_matches('.');
            if trimmed.is_empty()
                || trimmed.len() > MAX_REPORTED_HOST_BYTES
                || trimmed
                    .chars()
                    .any(|ch| is_unsafe_char(ch) || ch.is_whitespace())
            {
                return None;
            }
            if trimmed.eq_ignore_ascii_case("localhost") {
                None
            } else {
                Some(trimmed.to_owned())
            }
        }
        None => None,
    };
    Some(ReportedDirectory {
        host,
        path: dir.path.clone(),
    })
}

pub fn format_reported_directory(
    dir: &ReportedDirectory,
    local_host: Option<&str>,
    max_chars: usize,
) -> String {
    let remote_host = dir.host.as_deref().and_then(|host| {
        let host = host.trim().trim_end_matches('.');
        if host.is_empty() || host.eq_ignore_ascii_case("localhost") {
            return None;
        }
        if let Some(local) = local_host.map(|h| h.trim().trim_end_matches('.'))
            && !local.is_empty()
            && host.eq_ignore_ascii_case(local)
        {
            return None;
        }
        Some(host)
    });

    let path_budget = remote_host
        .map(|host| max_chars.saturating_sub(host.chars().count() + 1))
        .unwrap_or(max_chars)
        .max(8);
    let compact_path = compact_path(&dir.path, path_budget);
    match remote_host {
        Some(host) => format!("{host}:{compact_path}"),
        None => compact_path,
    }
}

fn compact_path(path: &str, max_chars: usize) -> String {
    if path.chars().count() <= max_chars {
        return path.to_owned();
    }
    let sep = if path.contains('\\') && !path.contains('/') {
        '\\'
    } else {
        '/'
    };
    let parts: Vec<&str> = path.split(sep).filter(|s| !s.is_empty()).collect();
    if parts.len() >= 2 {
        let two = format!(
            "…{sep}{}{sep}{}",
            parts[parts.len() - 2],
            parts[parts.len() - 1]
        );
        if two.chars().count() <= max_chars {
            return two;
        }
    }
    if let Some(last) = parts.last() {
        let one = format!("…{sep}{last}");
        if one.chars().count() <= max_chars {
            return one;
        }
    }
    let keep = max_chars.saturating_sub(1);
    let tail: String = path
        .chars()
        .rev()
        .take(keep)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{tail}")
}

fn strip_file_scheme(raw: &str) -> Option<&str> {
    let prefix = raw.get(..5)?;
    prefix.eq_ignore_ascii_case("file:").then(|| &raw[5..])
}

fn normalize_host(raw: &str) -> Option<Option<String>> {
    let decoded = percent_decode_utf8(raw)?;
    let trimmed = decoded.trim().trim_end_matches('.');
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("localhost") {
        return Some(None);
    }
    if trimmed.len() > MAX_REPORTED_HOST_BYTES
        || trimmed
            .chars()
            .any(|ch| is_unsafe_char(ch) || ch.is_whitespace())
    {
        return None;
    }
    Some(Some(trimmed.to_owned()))
}

fn normalize_file_uri_path(path: &str) -> Option<String> {
    // Handle Windows drive URI paths like `/C:/Users/dev` -> `C:/Users/dev`.
    let bytes = path.as_bytes();
    if bytes.len() >= 3
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && bytes[2] == b':'
        && (bytes.len() == 3 || bytes[3] == b'/')
    {
        return Some(path[1..].to_owned());
    }
    Some(path.to_owned())
}

fn is_raw_absolute_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    if path.starts_with('/') || path.starts_with("\\\\") {
        return true;
    }
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

fn percent_decode_utf8(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = hex_val(*bytes.get(i + 1)?)?;
            let lo = hex_val(*bytes.get(i + 2)?)?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn is_unsafe_char(ch: char) -> bool {
    ch.is_control()
        || matches!(
            ch,
            '\u{061c}' | '\u{200e}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_osc7_local_remote_utf8_and_windows_paths() {
        assert_eq!(
            parse_osc7_or_pwd("file:///home/dev/corvid"),
            Some(ReportedDirectory {
                host: None,
                path: "/home/dev/corvid".into(),
            })
        );
        assert_eq!(
            parse_osc7_or_pwd("file://localhost/tmp/work"),
            Some(ReportedDirectory {
                host: None,
                path: "/tmp/work".into(),
            })
        );
        assert_eq!(
            parse_osc7_or_pwd("file://prod-box/srv/my%20app/%ED%95%9C%EA%B8%80"),
            Some(ReportedDirectory {
                host: Some("prod-box".into()),
                path: "/srv/my app/한글".into(),
            })
        );
        assert_eq!(
            parse_osc7_or_pwd("file:///C:/Users/dev/project"),
            Some(ReportedDirectory {
                host: None,
                path: "C:/Users/dev/project".into(),
            })
        );
        assert_eq!(
            parse_osc7_or_pwd(r"C:\Users\dev\work"),
            Some(ReportedDirectory {
                host: None,
                path: r"C:\Users\dev\work".into(),
            })
        );
    }

    #[test]
    fn rejects_relative_malformed_control_and_oversized_paths() {
        assert_eq!(parse_osc7_or_pwd(""), None);
        assert_eq!(parse_osc7_or_pwd("relative/path"), None);
        assert_eq!(parse_osc7_or_pwd("https://example.test/a"), None);
        assert_eq!(parse_osc7_or_pwd("file://host/bad%ZZ"), None);
        assert_eq!(parse_osc7_or_pwd("file://host/bad\x07path"), None);
        assert_eq!(parse_osc7_or_pwd("/bad\u{202e}path"), None);
        let huge = format!("/{}", "a".repeat(MAX_REPORTED_PATH_BYTES + 1));
        assert_eq!(parse_osc7_or_pwd(&huge), None);
    }

    #[test]
    fn formats_and_compacts_local_and_remote_directories() {
        let local = ReportedDirectory {
            host: Some("my-mac.local.".into()),
            path: "/Users/dev/source/DosangGu/corvid".into(),
        };
        assert_eq!(
            format_reported_directory(&local, Some("my-mac.local"), 24),
            "…/DosangGu/corvid"
        );

        let remote = ReportedDirectory {
            host: Some("prod-box".into()),
            path: "/srv/deployments/current/app".into(),
        };
        assert_eq!(
            format_reported_directory(&remote, Some("my-mac"), 28),
            "prod-box:…/current/app"
        );
    }
}
