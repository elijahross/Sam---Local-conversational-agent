// Lab: a virtual display (private CGVirtualDisplay, as used by DeskPad/BetterDisplay) that lives
// while this process runs. Prints its displayID + frame, places it diagonally (corner contact
// only) so the cursor rarely wanders onto it, then waits N seconds.
#import <Foundation/Foundation.h>
#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>

@interface CGVirtualDisplayDescriptor : NSObject
@property (retain, nonatomic) dispatch_queue_t queue;
@property (retain, nonatomic) NSString *name;
@property (nonatomic) unsigned int maxPixelsHigh;
@property (nonatomic) unsigned int maxPixelsWide;
@property (nonatomic) CGSize sizeInMillimeters;
@property (nonatomic) unsigned int productID;
@property (nonatomic) unsigned int vendorID;
@property (nonatomic) unsigned int serialNum;
@property (copy, nonatomic) void (^terminationHandler)(id, id);
@end
@interface CGVirtualDisplayMode : NSObject
- (instancetype)initWithWidth:(unsigned int)w height:(unsigned int)h refreshRate:(double)r;
@end
@interface CGVirtualDisplaySettings : NSObject
@property (retain, nonatomic) NSArray *modes;
@property (nonatomic) unsigned int hiDPI;
@end
@interface CGVirtualDisplay : NSObject
@property (readonly, nonatomic) unsigned int displayID;
- (instancetype)initWithDescriptor:(CGVirtualDisplayDescriptor *)d;
- (BOOL)applySettings:(CGVirtualDisplaySettings *)s;
@end

int main(int argc, const char *argv[]) {
    @autoreleasepool {
        int seconds = argc > 1 ? atoi(argv[1]) : 60;
        NSApplication *app = [NSApplication sharedApplication];
        [app setActivationPolicy:NSApplicationActivationPolicyProhibited];
        CGVirtualDisplayDescriptor *d = [[CGVirtualDisplayDescriptor alloc] init];
        d.queue = dispatch_get_main_queue();
        d.name = @"Rowan Workspace";
        d.maxPixelsWide = 1920; d.maxPixelsHigh = 1200;
        d.sizeInMillimeters = CGSizeMake(518, 324);
        d.productID = 0x5257; d.vendorID = 0x5257; d.serialNum = 1;
        d.terminationHandler = ^(id a, id b) { NSLog(@"virtual display terminated"); };
        CGVirtualDisplay *v = [[CGVirtualDisplay alloc] initWithDescriptor:d];
        if (!v) { printf("{\"error\":\"CGVirtualDisplay init failed\"}\n"); return 1; }
        CGVirtualDisplaySettings *s = [[CGVirtualDisplaySettings alloc] init];
        s.hiDPI = 0;
        s.modes = @[[[CGVirtualDisplayMode alloc] initWithWidth:1920 height:1200 refreshRate:60]];
        BOOL ok = [v applySettings:s];
        CGDirectDisplayID did = v.displayID;
        // Place diagonally below-left of the main display: only a corner touches, so the
        // cursor doesn't slide onto the invisible screen during normal use.
        CGDirectDisplayID main = CGMainDisplayID();
        CGRect mb = CGDisplayBounds(main);
        CGDisplayConfigRef cfg;
        if (CGBeginDisplayConfiguration(&cfg) == kCGErrorSuccess) {
            CGConfigureDisplayOrigin(cfg, did, (int)(mb.origin.x - 1920), (int)(mb.origin.y + mb.size.height));
            CGCompleteDisplayConfiguration(cfg, kCGConfigureForSession);
        }
        [NSThread sleepForTimeInterval:1.0];
        CGRect b = CGDisplayBounds(did);
        printf("{\"display_id\":%u,\"applied\":%d,\"x\":%.0f,\"y\":%.0f,\"w\":%.0f,\"h\":%.0f,\"screens\":%lu}\n", did, ok, b.origin.x, b.origin.y, b.size.width, b.size.height, (unsigned long)[NSScreen screens].count);
        fflush(stdout);
        [[NSRunLoop mainRunLoop] runUntilDate:[NSDate dateWithTimeIntervalSinceNow:seconds]];
        printf("{\"done\":true}\n");
    }
    return 0;
}
