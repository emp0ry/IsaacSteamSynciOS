#import <Foundation/Foundation.h>
#import <objc/runtime.h>
#import "IsaacCloudCore.h"

extern void ICSInstallLifecycleAdapter(void);
extern void ICSInstallUI(void);
extern void ICSInstallGameStateDetector(void);
extern bool ICSGameOwnsImage(const char *imageName);

static const uint64_t ICSPrelaunchTimeoutMilliseconds = 8000;
static NSString *const ICSPreflightFinishedNotification = @"IsaacCloudSyncPreflightFinished";
static id (*ICSOriginalDelegateInit)(id, SEL);

static void ICSInstallHostAdapters(void) {
    dispatch_async(dispatch_get_main_queue(), ^{
        ICSInstallLifecycleAdapter();
        ICSInstallGameStateDetector();
        ICSInstallUI();
        [NSNotificationCenter.defaultCenter
            postNotificationName:ICSPreflightFinishedNotification object:nil];
    });
}

static id ICSDelegateInit(id object, SEL selector) {
    static dispatch_once_t onceToken;
    dispatch_once(&onceToken, ^{
        // UIKit constructs the native AAPLAppDelegate before creating the
        // game view. Dyld has finished initializing images at this boundary,
        // so Steam workers can initialize networking without the constructor
        // holding dyld's initialization lock. No executable memory is patched.
        ICSCoreLog("bootstrap", "native delegate startup barrier entered");
        BOOL completed = ICSCorePreflight(ICSPrelaunchTimeoutMilliseconds);
        ICSCoreLog("bootstrap", completed ? "startup barrier completed"
            : "startup barrier timed out; local play allowed and late replacement disabled");
        ICSInstallHostAdapters();
    });
    return ICSOriginalDelegateInit(object, selector);
}

__attribute__((constructor))
static void IsaacCloudSyncBootstrap(void) {
    @autoreleasepool {
        NSString *home = NSHomeDirectory();
        if (ICSCoreStart(home.fileSystemRepresentation) != 0) {
            NSLog(@"[IsaacCloud] bootstrap: core initialization failed");
            return;
        }

        // Capture local identities in the constructor, but never wait here:
        // a worker's first network call may need another image to initialize.
        // Override init only on the verified game's own delegate, not on its
        // UIKit/NSObject superclass or the delegate of a container host.
        Class delegate = objc_lookUpClass("AAPLAppDelegate");
        SEL selector = @selector(init);
        Method method = delegate ? class_getInstanceMethod(delegate, selector) : NULL;
        if (delegate != Nil && ICSGameOwnsImage(class_getImageName(delegate)) && method != NULL) {
            ICSOriginalDelegateInit = (id (*)(id, SEL))method_getImplementation(method);
            if (class_addMethod(delegate, selector, (IMP)ICSDelegateInit,
                                method_getTypeEncoding(method))) {
                ICSCoreLog("bootstrap", "native delegate startup barrier armed");
                return;
            }
        }
        ICSCoreLog("bootstrap", "unverified startup boundary; automatic pre-load replacement disabled");
        ICSCoreSyncNow("foreground");
        ICSInstallHostAdapters();
    }
}
