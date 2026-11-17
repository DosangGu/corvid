#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#include <stdint.h>

uintptr_t corvid_retain_deck_process(uint32_t pid) {
    @autoreleasepool {
        NSRunningApplication *application =
            [NSRunningApplication runningApplicationWithProcessIdentifier:(pid_t)pid];
        return (uintptr_t)(__bridge_retained void *)application;
    }
}

void corvid_release_deck_process(uintptr_t handle) {
    (void)CFBridgingRelease((void *)handle);
}

bool corvid_deck_process_visible(uintptr_t handle) {
    @autoreleasepool {
        NSRunningApplication *application = (__bridge NSRunningApplication *)(void *)handle;
        if (!application || application.terminated) return false;
        NSArray *windows = CFBridgingRelease(CGWindowListCopyWindowInfo(
            kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements, kCGNullWindowID));
        for (NSDictionary *window in windows) {
            if ([window[(__bridge NSString *)kCGWindowOwnerPID] intValue] == application.processIdentifier &&
                [window[(__bridge NSString *)kCGWindowLayer] intValue] == NSPopUpMenuWindowLevel) {
                return true;
            }
        }
        return false;
    }
}

bool corvid_retire_legacy_deck(uintptr_t handle) {
    @autoreleasepool {
        NSRunningApplication *application = (__bridge NSRunningApplication *)(void *)handle;
        if (!application || application.terminated ||
            ![application.executableURL.lastPathComponent isEqualToString:@"corvid-agent-deck"]) {
            return false;
        }
        return [application terminate];
    }
}
