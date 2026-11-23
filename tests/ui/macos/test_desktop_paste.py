"""Verify Unicode paste and input isolation between macOS Desktop tabs."""
import shlex
import subprocess

from scenario import expect_shell_output, wait_for


def wait_for_route(cli, env, desktop, description, accepts):
    clients = ""

    def selected():
        nonlocal clients
        clients = subprocess.check_output([cli, "control", "clients"], env=env, text=True,
                                         timeout=5)
        routes = []
        for line in clients.splitlines():
            fields = line.split()[1:]
            route = dict(field.split("=", 1) for field in fields if "=" in field)
            if "pane-writer" in route and "terminal" in route and route.get("mode") == "Passthrough":
                pane, _ = route["pane-writer"].rsplit("/", 1)
                routes.append((pane, route["terminal"]))
        if len(routes) == 1 and accepts(routes[0]):
            return routes[0]

    try:
        return wait_for(description, selected, desktop)
    except AssertionError as error:
        raise AssertionError(f"{error}\nLast daemon client view:\n{clients}") from error


def verify_paste(ui, desktop, window, root, *, cli, env):
    def expect(marker, script, expected, *, probe=False):
        path = root / marker
        command = f'{script}printf %s "$corvid_tab_value" > {shlex.quote(str(path))}'
        expect_shell_output(ui, desktop, window, path, command, expected, probe=probe)

    first = wait_for_route(cli, env, desktop, "the first tab's input route", lambda route: True)
    expect("first-shell", "corvid_tab_value=first; ", "first", probe=True)

    ui.keys(window, "leader", "t", "n")
    wait_for_route(cli, env, desktop, "the new tab's input route",
                   lambda route: route[0] != first[0] and route[1] != first[1])
    # The first shell already has a value. Only the new shell can acknowledge this probe.
    expect("second-shell", '[ -z "${corvid_tab_value-}" ] && ', "", probe=True)

    pasted = "한글 e\u0301 🐦"
    expect("pasted", f"corvid_tab_value={shlex.quote(pasted)}; ", pasted)

    ui.keys(window, "leader", "t", "h", "return")
    wait_for_route(cli, env, desktop, "the restored first tab's input route",
                   lambda route: route == first)
    expect("first-shell-retained", "", "first", probe=True)
