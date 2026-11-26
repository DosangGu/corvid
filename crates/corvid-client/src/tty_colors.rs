#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::time::Duration;

use corvid_protocol::wire::TerminalColors;

#[cfg(any(target_os = "linux", target_os = "macos"))]
const PROBE_TIMEOUT: Duration = Duration::from_millis(80);

pub(crate) async fn probe_colors() -> Option<TerminalColors> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        probe_unix_colors().await
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn probe_unix_colors() -> Option<TerminalColors> {
    use std::fs::OpenOptions;
    use std::io::{ErrorKind, Read, Write};
    use std::os::unix::fs::OpenOptionsExt;

    use compio::runtime::fd::PollFd;
    use compio::time::timeout;
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
    use nix::libc::{O_NOCTTY, O_NONBLOCK};

    if crate::tty::Tty::check().is_err() {
        return None;
    }

    #[cfg(target_os = "linux")]
    let (input_path, output_path) = (
        std::path::PathBuf::from("/proc/self/fd/0"),
        std::path::PathBuf::from("/proc/self/fd/1"),
    );
    #[cfg(target_os = "macos")]
    let (input_path, output_path) = (
        nix::unistd::ttyname(std::io::stdin()).ok()?,
        nix::unistd::ttyname(std::io::stdout()).ok()?,
    );

    let input = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOCTTY | O_NONBLOCK)
        .open(input_path)
        .ok()?;
    let output = OpenOptions::new()
        .write(true)
        .custom_flags(O_NOCTTY | O_NONBLOCK)
        .open(output_path)
        .ok()?;
    let input = PollFd::new(input).ok()?;
    let mut output = output;

    enable_raw_mode().ok()?;
    struct RawGuard;
    impl Drop for RawGuard {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
        }
    }
    let _guard = RawGuard;

    // Query foreground (OSC 10), background (OSC 11), and DA1 sentinel so
    // terminals without OSC 10/11 support finish immediately instead of waiting.
    if output
        .write_all(b"\x1b]10;?\x07\x1b]11;?\x07\x1b[c")
        .is_err()
    {
        return None;
    }
    let _ = output.flush();

    let mut buffer = Vec::with_capacity(256);
    let mut chunk = [0u8; 128];
    let _ = timeout(PROBE_TIMEOUT, async {
        loop {
            match (&*input).read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    buffer.extend_from_slice(&chunk[..n]);
                    let (fg, bg, sentinel) = parse_osc_color_replies(&buffer);
                    if sentinel || (fg.is_some() && bg.is_some()) || buffer.len() >= 1024 {
                        break;
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    if input.read_ready().await.is_err() {
                        break;
                    }
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    })
    .await;

    let (fg, bg, _) = parse_osc_color_replies(&buffer);
    (fg.is_some() || bg.is_some()).then_some(TerminalColors {
        foreground_rgb: fg,
        background_rgb: bg,
        ansi_palette: Vec::new(),
    })
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
pub(crate) fn parse_osc_color_replies(bytes: &[u8]) -> (Option<u32>, Option<u32>, bool) {
    let mut fg = None;
    let mut bg = None;
    let mut sentinel = false;
    let mut index = 0;

    while index + 1 < bytes.len() {
        if bytes[index] != 0x1b {
            index += 1;
            continue;
        }
        match bytes[index + 1] {
            b']' => {
                let rest = &bytes[index + 2..];
                let Some((body_len, term_len)) = find_osc_terminator(rest) else {
                    break;
                };
                let body = &rest[..body_len];
                if let Some(rgb) = body.strip_prefix(b"10;rgb:") {
                    if let Some(parsed) = parse_osc_rgb(rgb) {
                        fg = Some(parsed);
                    }
                } else if let Some(rgb) = body.strip_prefix(b"11;rgb:")
                    && let Some(parsed) = parse_osc_rgb(rgb)
                {
                    bg = Some(parsed);
                }
                index += 2 + body_len + term_len;
            }
            b'[' => {
                let rest = &bytes[index + 2..];
                if let Some(end) = rest.iter().position(|&b| (0x40..=0x7e).contains(&b)) {
                    if rest[end] == b'c'
                        && rest[..end].starts_with(b"?")
                        && rest[1..end]
                            .iter()
                            .all(|&b| b.is_ascii_digit() || b == b';')
                    {
                        sentinel = true;
                    }
                    index += 2 + end + 1;
                } else {
                    break;
                }
            }
            _ => index += 1,
        }
    }

    (fg, bg, sentinel)
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn find_osc_terminator(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x07 {
            return Some((i, 1));
        }
        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
            return Some((i, 2));
        }
        i += 1;
    }
    None
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn parse_osc_rgb(bytes: &[u8]) -> Option<u32> {
    let mut parts = bytes.split(|&byte| byte == b'/');
    let r = parse_hex_channel(parts.next()?)?;
    let g = parse_hex_channel(parts.next()?)?;
    let b = parse_hex_channel(parts.next()?)?;
    parts
        .next()
        .is_none()
        .then_some((u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b))
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn parse_hex_channel(bytes: &[u8]) -> Option<u8> {
    if !(1..=4).contains(&bytes.len()) || !bytes.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let value = u32::from_str_radix(text, 16).ok()?;
    let max = (1_u32 << (bytes.len() * 4)) - 1;
    Some(((value * 255 + max / 2) / max) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_osc_10_11_and_da1_sentinel_with_bel_or_st() {
        let input = b"\x1b]10;rgb:cdcd/d3d3/dcdc\x07\x1b]11;rgb:2020/2424/2b2b\x1b\\\x1b[?64;1;2c";
        assert_eq!(
            parse_osc_color_replies(input),
            (Some(0xcd_d3_dc), Some(0x20_24_2b), true)
        );

        let two_digit = b"\x1b]10;rgb:ff/00/80\x07";
        assert_eq!(
            parse_osc_color_replies(two_digit),
            (Some(0xff_00_80), None, false)
        );

        let da1_only = b"\x1b[?6c";
        assert_eq!(parse_osc_color_replies(da1_only), (None, None, true));

        let malformed = b"\x1b]10;rgb:zzzz/0000/0000\x07\x1b]11;rgb:1111/2222\x07";
        assert_eq!(parse_osc_color_replies(malformed), (None, None, false));
    }
}
