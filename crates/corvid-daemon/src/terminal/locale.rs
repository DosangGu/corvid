use std::ffi::{OsStr, OsString};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use portable_pty::CommandBuilder;

const UTF8_LOCALES: &[&str] = &["C.UTF-8", "C.utf8", "en_US.UTF-8", "UTF-8"];

enum ShellLocale {
    Inherited,
    UseCharacterLocale,
    SetCharacterLocale(OsString),
}

/// Align the default interactive shell's character encoding with the UTF-8 PTY.
pub(super) fn configure_shell(command: &mut CommandBuilder) {
    // Default shells inherit the same locale settings from the daemon.
    static SHELL_LOCALE: OnceLock<ShellLocale> = OnceLock::new();
    SHELL_LOCALE
        .get_or_init(|| ShellLocale::detect(command))
        .apply(command);
}

impl ShellLocale {
    fn detect(command: &CommandBuilder) -> Self {
        let all = nonempty_env(command, "LC_ALL");
        let lang = nonempty_env(command, "LANG");
        let ctype = nonempty_env(command, "LC_CTYPE").or(lang);
        if all.or(ctype).is_some_and(is_utf8) {
            return Self::Inherited;
        }

        // A forwarded LC_ALL=C can mask a usable LC_CTYPE or LANG.
        if all.is_some() && ctype.is_some_and(is_utf8) {
            return Self::UseCharacterLocale;
        }

        let fallback = lang.filter(|locale| is_utf8(locale)).or_else(|| {
            UTF8_LOCALES
                .iter()
                .map(OsStr::new)
                .find(|locale| is_utf8(locale))
        });
        fallback.map_or(Self::Inherited, |locale| {
            Self::SetCharacterLocale(locale.to_owned())
        })
    }

    fn apply(&self, command: &mut CommandBuilder) {
        match self {
            Self::Inherited => {}
            Self::UseCharacterLocale => command.env_remove("LC_ALL"),
            Self::SetCharacterLocale(locale) => {
                command.env_remove("LC_ALL");
                command.env("LC_CTYPE", locale);
            }
        }
    }
}

fn nonempty_env<'a>(command: &'a CommandBuilder, name: &str) -> Option<&'a OsStr> {
    command.get_env(name).filter(|value| !value.is_empty())
}

fn is_utf8(locale: &OsStr) -> bool {
    // A static Linux daemon can use a different libc from the host shell.
    Command::new("locale")
        .arg("charmap")
        .env_remove("LC_ALL")
        .env("LC_CTYPE", locale)
        .env("LANG", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|output| {
            output.status.success() && output.stdout.trim_ascii().eq_ignore_ascii_case(b"UTF-8")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command() -> CommandBuilder {
        let mut command = CommandBuilder::new_default_prog();
        command.env_clear();
        command
    }

    fn configure(command: &mut CommandBuilder) {
        ShellLocale::detect(command).apply(command);
    }

    fn utf8_locale() -> &'static OsStr {
        UTF8_LOCALES
            .iter()
            .map(OsStr::new)
            .find(|locale| is_utf8(locale))
            .expect("a UTF-8 locale is installed")
    }

    #[test]
    fn preserves_the_effective_utf8_locale() {
        let mut command = command();
        let locale = utf8_locale();
        command.env("LC_ALL", locale);
        command.env("LC_CTYPE", "C");
        configure(&mut command);
        assert_eq!(command.get_env("LC_ALL"), Some(locale));
        assert_eq!(command.get_env("LC_CTYPE"), Some(OsStr::new("C")));
    }

    #[test]
    fn removes_a_c_locale_override_masking_utf8() {
        let mut command = command();
        let locale = utf8_locale();
        command.env("LANG", locale);
        command.env("LC_ALL", "C");
        configure(&mut command);
        assert_eq!(command.get_env("LC_ALL"), None);
        assert_eq!(command.get_env("LANG"), Some(locale));
        assert_eq!(command.get_env("LC_CTYPE"), None);
    }

    #[test]
    fn supplies_utf8_character_handling_to_ascii_default_shells() {
        for locale in [None, Some(""), Some("C"), Some("POSIX")] {
            let mut command = command();
            command.env("LC_TIME", "C");
            if let Some(locale) = locale {
                command.env("LC_ALL", locale);
            }
            configure(&mut command);
            assert_eq!(command.get_env("LC_ALL"), None);
            assert!(command.get_env("LC_CTYPE").is_some_and(is_utf8));
            assert_eq!(command.get_env("LC_TIME"), Some(OsStr::new("C")));
        }
    }
}
