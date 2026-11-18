#import <AppKit/AppKit.h>

extern NSStatusBarButton *corvid_status_button(void);

enum { PanelKeep, PanelOpen, PanelToggle, PanelHide };

static id globalMonitor;
static id localMonitor;
static NSUInteger presentationGeneration;
static NSSize requestedSize;

void corvid_panel_prepare(void) {
    [NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory];
}

static void hidePanel(NSWindow *panel) {
    presentationGeneration++;
    if (globalMonitor) [NSEvent removeMonitor:globalMonitor];
    if (localMonitor) [NSEvent removeMonitor:localMonitor];
    globalMonitor = nil;
    localMonitor = nil;
    [panel orderOut:nil];
}

static bool belongsToPanel(NSWindow *window, NSWindow *panel) {
    for (NSWindow *candidate = window; candidate; candidate = candidate.parentWindow) {
        if (candidate == panel) return true;
    }
    return false;
}

static NSRect statusButtonFrame(NSStatusBarButton *button) {
    if (!button.window || button.isHiddenOrHasHiddenAncestor) return NSZeroRect;
    return [button.window convertRectToScreen:[button convertRect:button.bounds toView:nil]];
}

static NSScreen *screenContaining(NSPoint point) {
    for (NSScreen *screen in NSScreen.screens) {
        if (NSPointInRect(point, screen.frame)) return screen;
    }
    return nil;
}

static NSScreen *statusAnchor(NSRect *anchor) {
    *anchor = statusButtonFrame(corvid_status_button());
    if (NSIsEmptyRect(*anchor)) return nil;
    NSScreen *screen = screenContaining(NSMakePoint(NSMidX(*anchor), NSMidY(*anchor)));
    if (!screen) return nil;

    // AppKit can expose a placeholder frame near the display origin until
    // the status button is laid out. Only use frames that reach the menu bar.
    CGFloat menuBarBottom = NSMaxY(screen.frame) - NSStatusBar.systemStatusBar.thickness;
    return NSMaxY(*anchor) > menuBarBottom ? screen : nil;
}

static bool onStatusButton(void) {
    NSRect anchor;
    return statusAnchor(&anchor) != nil && NSPointInRect(NSEvent.mouseLocation, anchor);
}

static void dismissOnOutsideClick(NSWindow *panel) {
    if (globalMonitor || localMonitor) return;
    NSEventMask clicks = NSEventMaskLeftMouseDown | NSEventMaskRightMouseDown | NSEventMaskOtherMouseDown;
    __weak NSWindow *weakPanel = panel;
    globalMonitor = [NSEvent addGlobalMonitorForEventsMatchingMask:clicks handler:^(NSEvent *event) {
        (void)event;
        if (!onStatusButton()) hidePanel(weakPanel);
    }];
    localMonitor = [NSEvent addLocalMonitorForEventsMatchingMask:clicks handler:^NSEvent *(NSEvent *event) {
        if (!belongsToPanel(event.window, weakPanel) && !onStatusButton()) {
            hidePanel(weakPanel);
        }
        return event;
    }];
}

static void placePanel(NSWindow *panel, NSRect work, NSRect anchor) {
    NSSize requested = requestedSize;
    NSSize content = panel.contentView.bounds.size;
    NSSize frame = panel.frame.size;
    // Allow scrolling within the panel when the requested view is larger than
    // the available display. Leave room for the AppKit frame around the view.
    if (!NSIsEmptyRect(work)) {
        requested.width = MIN(requested.width, MAX(1, work.size.width - (frame.width - content.width)));
        requested.height = MIN(requested.height, MAX(1, work.size.height - (frame.height - content.height)));
    }
    if (!NSEqualSizes(content, requested)) [panel setContentSize:requested];
    if (NSIsEmptyRect(work)) return;

    frame = panel.frame.size;
    CGFloat x = NSIsEmptyRect(anchor) ? NSMaxX(work) - frame.width - 4 : NSMidX(anchor) - frame.width / 2;
    CGFloat top = NSIsEmptyRect(anchor) ? NSMaxY(work) : NSMinY(anchor);
    CGFloat y = top - frame.height - 4;
    [panel setFrameOrigin:NSMakePoint(
        MAX(NSMinX(work), MIN(x, NSMaxX(work) - frame.width)),
        MAX(NSMinY(work), MIN(y, NSMaxY(work) - frame.height))
    )];
}

static void presentPanel(NSWindow *panel, NSUInteger generation, NSUInteger retries) {
    if (!panel || generation != presentationGeneration) return;

    NSRect anchor;
    NSScreen *screen = statusAnchor(&anchor);
    if (!screen && retries > 0) {
        // The status item can exist before AppKit lays it out in the menu bar.
        __weak NSWindow *target = panel;
        dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 25 * NSEC_PER_MSEC), dispatch_get_main_queue(), ^{
            presentPanel(target, generation, retries - 1);
        });
        return;
    }

    if (!screen) {
        screen = screenContaining(NSEvent.mouseLocation) ?: panel.screen ?: NSScreen.mainScreen;
        anchor = NSZeroRect;
    }
    placePanel(panel, screen.visibleFrame, anchor);
    [panel makeKeyAndOrderFront:nil];
    dismissOnOutsideClick(panel);
}

void corvid_panel_hide(void *view) {
    NSWindow *panel = [(__bridge NSView *)view window];
    dispatch_async(dispatch_get_main_queue(), ^{ hidePanel(panel); });
}

void corvid_panel_update(void *view, double width, double height, uint8_t presentation) {
    __weak NSWindow *weakPanel = [(__bridge NSView *)view window];
    // Resize, measure and position after GPUI releases the current update.
    dispatch_async(dispatch_get_main_queue(), ^{
        NSPanel *panel = (NSPanel *)weakPanel;
        if (!panel) return;
        if (presentation == PanelHide || (presentation == PanelToggle && panel.visible)) {
            hidePanel(panel);
            return;
        }

        panel.hidesOnDeactivate = NO;
        panel.movable = NO;
        panel.titleVisibility = NSWindowTitleHidden;
        for (NSWindowButton button = NSWindowCloseButton; button <= NSWindowZoomButton; button++) {
            [panel standardWindowButton:button].hidden = YES;
        }
        requestedSize = NSMakeSize(width, height);
        if (presentation == PanelOpen || presentation == PanelToggle) {
            presentPanel(panel, ++presentationGeneration, 10);
        } else if (panel.visible) {
            NSRect anchor;
            NSScreen *screen = statusAnchor(&anchor);
            placePanel(panel, (screen ?: panel.screen ?: NSScreen.mainScreen).visibleFrame,
                screen ? anchor : NSZeroRect);
        }
    });
}
