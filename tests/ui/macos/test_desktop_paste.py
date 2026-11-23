"""Verify that a shell receives pasted Unicode through the macOS Desktop."""
import shlex

from scenario import expect_shell_output


def verify_paste(ui, desktop, window, root):
    # TODO: Stabilize macOS tab navigation in UI qualification
    # Synchronize tab selection and shell readiness before extending this smoke case.
    # Issue URL: https://github.com/DosangGu/corvid/issues/357
    pasted = "한글 e\u0301 🐦"
    marker = root / "pasted"
    command = f"printf %s {shlex.quote(pasted)} > {shlex.quote(str(marker))}"
    expect_shell_output(ui, desktop, window, marker, command, pasted, probe=True)
