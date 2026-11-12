#import "IsaacCloudDialog.h"

static BOOL ICSDesktopDialogs(void) {
    return NSClassFromString(@"NSApplication") != Nil;
}

@interface ICSDialogAction ()
@property(nonatomic, copy) NSString *title;
@property(nonatomic) UIAlertActionStyle style;
@property(nonatomic, copy) void (^handler)(ICSDialogAction *action);
@end

@implementation ICSDialogAction
+ (instancetype)actionWithTitle:(NSString *)title style:(UIAlertActionStyle)style
                         handler:(void (^)(ICSDialogAction *action))handler {
    if (!ICSDesktopDialogs()) {
        return (id)[UIAlertAction actionWithTitle:title style:style handler:(id)handler];
    }
    ICSDialogAction *action = [self new];
    action.title = title;
    action.style = style;
    action.handler = handler;
    return action;
}
@end

@interface ICSDialogController ()
@property(nonatomic, copy) NSString *message;
@property(nonatomic, strong) NSMutableArray<ICSDialogAction *> *choices;
@property(nonatomic, strong) NSMutableArray<UITextField *> *fields;
@property(nonatomic, strong) UIView *card;
@property(nonatomic, strong) UIScrollView *scroll;
@property(nonatomic, strong) UIStackView *stack;
@property(nonatomic) BOOL selected;
@end

@implementation ICSDialogController
+ (instancetype)alertControllerWithTitle:(NSString *)title message:(NSString *)message
                           preferredStyle:(UIAlertControllerStyle)style {
    if (!ICSDesktopDialogs()) {
        return (id)[UIAlertController alertControllerWithTitle:title message:message preferredStyle:style];
    }
    ICSDialogController *controller = [self new];
    controller.title = title;
    controller.message = message;
    controller.choices = [NSMutableArray array];
    controller.fields = [NSMutableArray array];
    controller.modalPresentationStyle = UIModalPresentationOverFullScreen;
    controller.modalTransitionStyle = UIModalTransitionStyleCrossDissolve;
    return controller;
}

- (NSArray<UITextField *> *)textFields { return self.fields.copy; }
- (void)addAction:(ICSDialogAction *)action { [self.choices addObject:action]; }
- (void)addTextFieldWithConfigurationHandler:(void (^)(UITextField *field))configuration {
    UITextField *field = [UITextField new];
    field.borderStyle = UITextBorderStyleRoundedRect;
    field.font = [UIFont systemFontOfSize:16];
    if (configuration) configuration(field);
    [self.fields addObject:field];
}

- (void)viewDidLoad {
    [super viewDidLoad];
    self.view.backgroundColor = [UIColor colorWithWhite:0 alpha:0.55];
    self.view.accessibilityViewIsModal = YES;
    self.card = [UIView new];
    self.card.backgroundColor = UIColor.secondarySystemBackgroundColor;
    self.card.layer.cornerRadius = 14;
    self.card.clipsToBounds = YES;
    [self.view addSubview:self.card];
    self.scroll = [UIScrollView new];
    self.scroll.alwaysBounceVertical = NO;
    [self.card addSubview:self.scroll];
    self.stack = [UIStackView new];
    self.stack.axis = UILayoutConstraintAxisVertical;
    self.stack.spacing = 12;
    [self.scroll addSubview:self.stack];

    UILabel *title = [UILabel new];
    title.text = self.title;
    title.numberOfLines = 0;
    title.font = [UIFont systemFontOfSize:20 weight:UIFontWeightBold];
    title.textAlignment = NSTextAlignmentCenter;
    [self.stack addArrangedSubview:title];
    UILabel *message = [UILabel new];
    message.text = self.message;
    message.numberOfLines = 0;
    message.font = [UIFont systemFontOfSize:14];
    message.textAlignment = NSTextAlignmentCenter;
    [self.stack addArrangedSubview:message];
    for (UITextField *field in self.fields) {
        [field.heightAnchor constraintEqualToConstant:40].active = YES;
        [self.stack addArrangedSubview:field];
    }
    [self.choices enumerateObjectsUsingBlock:^(ICSDialogAction *action, NSUInteger index, BOOL *stop) {
        (void)stop;
        UIButton *button = [UIButton buttonWithType:UIButtonTypeSystem];
        button.tag = (NSInteger)index;
        [button setTitle:action.title forState:UIControlStateNormal];
        [button setTitleColor:action.style == UIAlertActionStyleDestructive
            ? UIColor.systemRedColor : UIColor.systemBlueColor forState:UIControlStateNormal];
        button.titleLabel.font = [UIFont systemFontOfSize:17
            weight:action.style == UIAlertActionStyleCancel ? UIFontWeightSemibold : UIFontWeightRegular];
        button.layer.cornerRadius = 8;
        button.backgroundColor = UIColor.tertiarySystemBackgroundColor;
        [button.heightAnchor constraintEqualToConstant:44].active = YES;
        [button addTarget:self action:@selector(selectAction:) forControlEvents:UIControlEventTouchUpInside];
        [self.stack addArrangedSubview:button];
    }];
}

- (void)viewDidLayoutSubviews {
    [super viewDidLayoutSubviews];
    CGRect safe = UIEdgeInsetsInsetRect(self.view.bounds, self.view.safeAreaInsets);
    CGFloat width = MIN(520, MAX(160, CGRectGetWidth(safe) - 40));
    CGSize fitting = [self.stack systemLayoutSizeFittingSize:CGSizeMake(width - 32, 0)
        withHorizontalFittingPriority:UILayoutPriorityRequired
        verticalFittingPriority:UILayoutPriorityFittingSizeLevel];
    CGFloat height = MIN(fitting.height + 32, MAX(100, CGRectGetHeight(safe) - 40));
    self.card.frame = CGRectMake(CGRectGetMidX(safe) - width / 2,
        CGRectGetMidY(safe) - height / 2, width, height);
    self.scroll.frame = CGRectInset(self.card.bounds, 16, 16);
    self.stack.frame = CGRectMake(0, 0, width - 32, fitting.height);
    self.scroll.contentSize = self.stack.bounds.size;
}

- (void)selectAction:(UIButton *)button {
    if (self.selected || button.tag < 0 || (NSUInteger)button.tag >= self.choices.count) return;
    self.selected = YES;
    ICSDialogAction *choice = self.choices[(NSUInteger)button.tag];
    self.choices = [NSMutableArray array];
    [self.view endEditing:YES];
    // Run after dismissal so a follow-up dialog never races the current one.
    [self dismissViewControllerAnimated:YES completion:^{
        if (choice.handler) choice.handler(choice);
        choice.handler = nil;
    }];
}
@end
