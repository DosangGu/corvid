"""Verify Unicode paste and input isolation between Windows Desktop tabs."""
from scenario import expect_shell_output


def verify_paste(ui, desktop, window, root):
    def mark(marker, value="$tag"):
        return f"[IO.File]::WriteAllText((Join-Path $env:CORVID_UI_ROOT '{marker}'), {value})"

    def expect(marker, command, expected, *, probe=False):
        expect_shell_output(ui, desktop, window, root / marker, command, expected, probe=probe)

    expect("first", "$tag='first'; " + mark("first"), "first", probe=True)
    ui.keys(window, "leader", "t", "n")
    # The first shell already has $tag. Only a fresh tab can acknowledge this probe.
    fresh = "if ([string]::IsNullOrEmpty($tag)) { " + mark("ready", "'ready'") + " }"
    expect("ready", fresh, "ready", probe=True)

    pasted = "한글 e\u0301 🐦"
    expect("pasted", f"$tag='{pasted}'; " + mark("pasted"), pasted)
    ui.keys(window, "leader", "t", "h", "return")
    expect("isolated", mark("isolated"), "first", probe=True)
