"""Verify that an Agent Deck card click transfers macOS keyboard focus."""
import subprocess

from deck_focus_fixture import launch_agent
from .test_desktop_paste import wait_for_route
from scenario import wait_for


def fields(line):
    return dict(field.split("=", 1) for field in line.split() if "=" in field)


def agents_by_run(cli, env):
    agents = subprocess.check_output([cli, "control", "agents"], env=env, text=True, timeout=5)
    return {line.split()[0]: fields(line) for line in agents.splitlines() if line}


def launch_agents(cli, env, root, daemon):
    names = {}

    def new_terminal():
        terminals = {agent.get("terminal") for agent in agents_by_run(cli, env).values()}
        new = terminals - names.keys() - {None}
        if len(new) == 1:
            return new.pop()

    for name in ("first", "second"):
        launch_agent(cli, env, root, name)
        terminal = wait_for(f"{name} fixture terminal", new_terminal, daemon)
        names[terminal] = name
    return names


def occupied_cards(cli, env, reader):
    slots = subprocess.check_output([*reader, "slots"], env=env, text=True, timeout=5)
    cards = [fields(line) for line in slots.splitlines()
             if line.startswith("slot=") and "availability=Available" in line.split()]
    if len(cards) != 2:
        return None

    by_run = agents_by_run(cli, env)
    runs = [card["run"].rsplit("/", 1)[-1] for card in cards]
    if all(run in by_run for run in runs):
        return [by_run[run] for run in runs]


def open_deck(ui, panel, command, root, env, expanded):
    subprocess.run(command, cwd=root, env=env, check=True, timeout=15)
    deck = wait_for("the Deck menu bar panel", lambda: ui.window(panel.pid), panel)
    width, _ = ui.window_size(deck)
    if (width > 600) != expanded:
        # The 30pt expand control sits to the left of the 30pt close control.
        ui.click(deck, width - 57, 18)
        wait_for("the requested Deck layout", lambda: (ui.window_size(deck)[0] > 600) == expanded,
                 panel)
    return deck


def activate_finder(ui, desktop):
    subprocess.run(["open", "-b", "com.apple.finder"], check=True, timeout=10)
    wait_for("Finder to enter the foreground",
             lambda: ui.frontmost()[1] == "com.apple.finder", desktop)


def verify_focus(ui, desktop, window, panel, deck_command, root, *, cli, env, reader, names,
                 other=None):
    layouts = ((False, False), (True, False), (True, True), (False, True))
    numbered_layouts = enumerate(layouts) if other is None else [(len(layouts), layouts[0])]
    for scenario, (expanded, minimized) in numbered_layouts:
        cards = wait_for("two occupied Agent Deck slots",
                         lambda: occupied_cards(cli, env, reader), panel)
        initial = wait_for_route(cli, env, desktop, "initial Desktop writer", lambda route: True)
        index = 1 if initial[1] == cards[0]["terminal"] else 0
        terminal = cards[index]["terminal"]
        name = names[terminal]
        layout = "expanded" if expanded else "compact"
        state = "minimized" if minimized else "behind Finder"
        description = f"{layout} Deck, Desktop {state}, agent {name}"
        if other is not None:
            description += ", second Desktop running"
        print(f"CHECK {description}", flush=True)
        if minimized and ui.window(desktop.pid):
            ui.minimize(window)
            wait_for("Desktop to minimize", lambda: ui.window(desktop.pid) is None, desktop)

        # Switch away from Desktop as setup. The test driver never switches
        # back to Desktop after a slot click.
        activate_finder(ui, desktop)
        deck = open_deck(ui, panel, deck_command, root, env, expanded)
        assert ui.frontmost()[0] != window[0], "Opening the Deck activated Desktop before the click"
        # Toggling the layout is itself a click on the Deck panel.
        if ui.frontmost()[1] != "com.apple.finder":
            activate_finder(ui, desktop)

        # Center of an occupied card in the first row of the default 4x2 Deck.
        x, y = (108 + 194 * index, 297) if expanded else (55 + 94 * index, 84)
        ui.click(deck, x, y)
        wait_for_route(cli, env, desktop, f"slot click to select {name}'s pane",
                       lambda route: route[1] == terminal)

        def target_is_frontmost():
            pid, _ = ui.frontmost()
            if other is not None:
                assert pid != str(other.pid), "The competing Desktop received foreground focus"
            return pid == window[0] and ui.window(desktop.pid) == window

        try:
            wait_for(f"the Desktop window to enter the foreground for {name}",
                     target_is_frontmost, desktop, timeout=6)
        except AssertionError as error:
            raise AssertionError(
                f"{error}; selected {name}'s pane; foreground app was {ui.frontmost()}"
            ) from error

        token = f"414{scenario}"
        ui.keys_focused(window, "clear", *token, "return")
        marker = root / f"input-{name}"
        wait_for(f"keyboard input in {name}'s pane",
                 lambda: marker.exists() and marker.read_text() == token, desktop)
