#import <AppKit/AppKit.h>

void corvid_desktop_drag_window(void *view) {
    __weak NSWindow *target = [(__bridge NSView *)view window];
    NSEvent *event = NSApp.currentEvent;
    if (event.type != NSEventTypeLeftMouseDown || event.window != target) return;

    // Dragging can dispatch resize and move callbacks. Leave GPUI's input update first.
    dispatch_async(dispatch_get_main_queue(), ^{
        NSWindow *window = target;
        if (window.isVisible && (NSEvent.pressedMouseButtons & 1)) {
            [window performWindowDragWithEvent:event];
        }
    });
}
