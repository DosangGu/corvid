# Presentation UX

Desktop and TUI should provide as close to the same user experience as
practical. Review both clients when changing a user-facing workflow and keep
their keyboard interactions, defaults, feedback, and outcomes aligned. Use
platform-appropriate presentation. Document any necessary differences, such
as SSH connection setup or platform limitations, and explain them in the PR.

## Shared TUI and GUI Behavior

Desktop and TUI aim to provide the same user experience wherever practical.
Keep workflows, keyboard interactions, defaults, feedback, and outcomes
aligned across both clients. Use idiomatic graphical and terminal controls.
Document differences caused by SSH setup, platform capabilities, or other
constraints so users know what to expect in each client. Presentation code
maps gestures to shared domain operations, and the execution daemon remains the
authority for accepted state changes and terminal lifetimes.

| Area | Shared behavior |
|---|---|
| Hierarchy and identity | Workspace → Tab → split tree → Pane → optional Terminal; switching or moving a view preserves the existing terminal/run identity |
| Tabs | Preview tab contents with arrows or `h`/`l`, Enter to focus, `[`/`]` to move the highlighted tab one position, `n` to open, `x` to close, and Esc to exit tab mode. Desktop also supports dragging a tab title to either side of another tab |
| Panes | Browse the layout with arrows or `h`/`j`/`k`/`l`, Enter to focus, `r`/`d` to split, `x` to close the highlighted pane, and Esc to cancel |
| Focus and input | Selection targets an exact pane in one client; writer ownership, route fencing, and literal terminal input follow the same contract |
| Names | Rename sets the current tab's name. Each pane follows its terminal title, and an unnamed tab with one pane follows that pane's title |
| Session lifetime | Hiding a tab, closing a presentation window, or detaching is distinct from explicitly closing a terminal; daemon-owned sessions survive client loss |
| Capabilities and limits | Unsupported operations and resource limits are explicit; temporary platform gaps do not define a second set of workspace semantics |

Share layout transitions and their validation through `corvid-core` and the
daemon; keep rendering, mouse interaction, and key bindings in the presentations.
Equivalent action sequences must produce equivalent domain state and lifecycle
outcomes.

Both clients can create tabs and split or select panes on the daemon. A pane
writer can close views in its workspace when the target contains no pane
owned by another writer. The daemon removes the
target and stops its PTYs. Removing the last pane removes its tab, and removing
the last tab removes its workspace. When the active pane closes, the daemon
prefers an available live pane in the same tab. It then searches nearby tabs
in the same workspace, using the order before removal and preferring the
right tab at equal distance. Closing a background tab preserves focus.
A legacy writer can also fall back to another workspace it claimed.
If none is available, the client detaches.
Closing the final workspace leaves a fresh layout without a running shell.
Closing the GUI window releases its writer lease while the daemon keeps its PTYs
alive. Operation-by-operation parity remains subject to A32.

Tab reordering updates the daemon's persisted layout and is published through the
existing workspace watch. Both gestures keep the same tab, pane, terminal, and
input-route identities. Each move applies immediately, and leaving tab mode ends
the preview. Desktop negotiates `reorder_tab.v1` on the interactive connection.
