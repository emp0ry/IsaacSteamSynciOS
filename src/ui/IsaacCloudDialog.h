#import <UIKit/UIKit.h>

NS_ASSUME_NONNULL_BEGIN

// Native UIAlertController on iPhone; an in-window UIKit dialog on PlayCover,
// whose synthesized touches cannot reliably operate Catalyst's native alerts.
@interface ICSDialogAction : NSObject
+ (instancetype)actionWithTitle:(NSString *)title style:(UIAlertActionStyle)style
                         handler:(void (^ _Nullable)(ICSDialogAction *action))handler;
@end

@interface ICSDialogController : UIViewController
@property(nonatomic, readonly) NSArray<UITextField *> *textFields;
+ (instancetype)alertControllerWithTitle:(NSString *)title message:(NSString *)message
                           preferredStyle:(UIAlertControllerStyle)style;
- (void)addAction:(ICSDialogAction *)action;
- (void)addTextFieldWithConfigurationHandler:(void (^)(UITextField *field))configuration;
@end

NS_ASSUME_NONNULL_END
