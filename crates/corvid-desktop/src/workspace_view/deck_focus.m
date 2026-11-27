#import <AppKit/AppKit.h>
#include <stdbool.h>
#include <unistd.h>

extern bool corvid_desktop_activation_pending(const void *reply);
extern void corvid_desktop_activation_finished(void *reply, bool active);

static bool isFrontmost(void) {
    return NSApp.isActive &&
        NSWorkspace.sharedWorkspace.frontmostApplication.processIdentifier == getpid();
}

static void confirmActivation(NSWindow *window, void *reply, bool raised, unsigned int attempts) {
    if (!window || !corvid_desktop_activation_pending(reply)) {
        corvid_desktop_activation_finished(reply, false);
        return;
    }
    bool active = isFrontmost();
    if (!raised && active) {
        if (window.isMiniaturized) [window deminiaturize:nil];
        [window makeKeyAndOrderFront:nil];
        raised = true;
    }
    if (raised && active && window.isKeyWindow && !window.isMiniaturized) {
        corvid_desktop_activation_finished(reply, true);
    } else if (attempts == 0) {
        corvid_desktop_activation_finished(reply, false);
    } else {
        // AppKit may activate the app or finish restoring the window asynchronously.
        __weak NSWindow *target = window;
        dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 25 * NSEC_PER_MSEC),
            dispatch_get_main_queue(), ^{
                confirmActivation(target, reply, raised, attempts - 1);
            });
    }
}

static void activateDesktop(unsigned int peerPid) {
    NSRunningApplication *source = peerPid != 0
        ? [NSRunningApplication runningApplicationWithProcessIdentifier:(pid_t)peerPid]
        : nil;
    // The local socket identifies the Deck process that yielded activation.
    // Deck is an accessory app, so NSWorkspace.frontmostApplication stays on the
    // previous regular app even while Deck is active and yields activation.
    bool requested = source != nil
        && [NSRunningApplication.currentApplication activateFromApplication:source options:0];
    if (!requested) [NSApp activate];
}

void corvid_desktop_activate_window(void *view, unsigned int peerPid, void *reply) {
    __weak NSWindow *window = [(__bridge NSView *)view window];
    dispatch_async(dispatch_get_main_queue(), ^{
        if (window && corvid_desktop_activation_pending(reply) && !isFrontmost()) {
            activateDesktop(peerPid);
        }
        confirmActivation(window, reply, false, 60);
    });
}
