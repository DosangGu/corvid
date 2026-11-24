"""Verify that an Agent Deck card click transfers macOS keyboard focus."""
import subprocess

from .test_desktop_paste import wait_for_route
from scenario import running, wait_for


def fields(line):
    return dict(field.split("=", 1) for field in line.split() if "=" in field)


def occupied_cards(cli, env, reader):
    slots = subprocess.check_output([*reader, "slots"], env=env, text=True, timeout=5)
    cards = [fields(line) for line in slots.splitlines()
             if line.startswith("slot=") and "availability=Available" in line.split()]
    if len(cards) != 2:
        return None

    agents = subprocess.check_output([cli, "control", "agents"], env=env, text=True, timeout=5)
    by_run = {line.split()[0]: fields(line) for line in agents.splitlines() if line}
    runs = [card["run"].rsplit("/", 1)[-1] for card in cards]
    if all(run in by_run for run in runs):
        return [by_run[run] for run in runs]


def verify_focus(ui, desktop, window, panel, backdrop, deck_command, root, *, cli, env, reader):
    cover = wait_for("backdrop window", lambda: ui.window(backdrop.pid), backdrop)
    cards = wait_for("two occupied Agent Deck slots",
                     lambda: occupied_cards(cli, env, reader), panel)
    initial = wait_for_route(cli, env, desktop, "initial Desktop writer", lambda route: True)
    first = 1 if initial[1] == cards[0]["terminal"] else 0

    for index, minimized in ((first, False), (1 - first, True)):
        name = cards[index]["session"]
        terminal = cards[index]["terminal"]
        if minimized:
            ui.minimize(window)
            wait_for("Desktop to minimize", lambda: ui.window(desktop.pid) is None, desktop)

        ui.click(cover, 160, 100)
        wait_for("the backdrop app to enter the foreground",
                 lambda: ui.frontmost() == cover[0], backdrop)
        wait_for("the previous Deck panel to close", lambda: ui.window(panel.pid) is None, panel)
        subprocess.run(deck_command, cwd=root, env=env, check=True, timeout=15)
        deck = wait_for("the Deck menu bar panel", lambda: ui.window(panel.pid), panel)
        assert ui.frontmost() != window[0], "Opening the Deck activated Desktop before the click"

        # Default eight-key layout: 12pt padding, 86pt cards, and 8pt gaps.
        # The first row sits below the 36pt header and 8pt top padding.
        ui.click(deck, 55 + 94 * index, 84)
        wait_for_route(cli, env, desktop, f"slot click to select {name}'s pane",
                       lambda route: route[1] == terminal)

        try:
            wait_for(f"the Desktop window to enter the foreground for {name}",
                     lambda: ui.frontmost() == window[0] and ui.window(desktop.pid) == window,
                     desktop, timeout=6)
        except AssertionError as error:
            raise AssertionError(
                f"{error}; selected {name}'s pane, but foreground PID was {ui.frontmost()}"
            ) from error

        token = f"macos-deck-focus-{name}"
        ui.submit(window, token, clear=True, activate=False)
        marker = root / f"input-{name}"
        wait_for(f"keyboard input in {name}'s pane",
                 lambda: marker.exists() and marker.read_text() == token, desktop)
