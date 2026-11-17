#import <AppKit/AppKit.h>

extern NSStatusBarButton *corvid_status_button(void);

enum { PanelKeep, PanelOpen, PanelToggle, PanelHide };

static id globalMonitor;
static id localMonitor;

void corvid_panel_prepare(void) {
    [NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory];
}

static void hidePanel(NSWindow *panel) {
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
    if (!button.window) return NSZeroRect;
    return [button.window convertRectToScreen:[button convertRect:button.bounds toView:nil]];
}

static bool onStatusButton(void) {
    return NSPointInRect(NSEvent.mouseLocation, statusButtonFrame(corvid_status_button()));
}

static void dismissOnOutsideClick(NSWindow *panel) {
    if (globalMonitor || localMonitor) return;
    NSEventMask clicks = NSEventMaskLeftMouseDown | NSEventMaskRightMouseDown | NSEventMaskOtherMouseDown;
    __weak NSWindow *weakPanel = panel;
    globalMonitor = [NSEvent addGlobalMonitorForEventsMatchingMask:clicks handler:^(NSEvent *event) {
        if (!onStatusButton()) hidePanel(weakPanel);
    }];
    localMonitor = [NSEvent addLocalMonitorForEventsMatchingMask:clicks handler:^NSEvent *(NSEvent *event) {
        if (!belongsToPanel(event.window, weakPanel) && !onStatusButton()) {
            hidePanel(weakPanel);
        }
        return event;
    }];
}

static void placePanel(NSWindow *panel, NSSize requested) {
    NSStatusBarButton *button = corvid_status_button();
    NSScreen *screen = button.window.screen ?: panel.screen ?: NSScreen.mainScreen;
    NSRect work = screen.visibleFrame;
    NSRect anchor = button.window ? statusButtonFrame(button) : work;
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
    CGFloat x = NSMidX(anchor) - frame.width / 2;
    CGFloat y = NSMinY(anchor) - frame.height - 4;
    if (!button.window) y = NSMidY(work) - frame.height / 2;
    [panel setFrameOrigin:NSMakePoint(
        MAX(NSMinX(work), MIN(x, NSMaxX(work) - frame.width)),
        MAX(NSMinY(work), MIN(y, NSMaxY(work) - frame.height))
    )];
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
        placePanel(panel, NSMakeSize(width, height));
        if (presentation == PanelOpen || presentation == PanelToggle) {
            [panel makeKeyAndOrderFront:nil];
            dismissOnOutsideClick(panel);
        }
    });
}
