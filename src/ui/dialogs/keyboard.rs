//! 模态框的键盘可达性（Enter / Tab）
//!
//! # 为什么单开一个模块
//!
//! 六个弹窗（确认 / 完成 / 基准 / 硬件引导 / 速查 / 命令面板）此前只有鼠标能操作：
//! 按钮上没有焦点句柄，键盘事件落不到它们身上，回车也无从触发。要补上键盘，
//! 每个弹窗都得做**同一件事**——所以判定逻辑（谁在最上层、回车等于哪个动作）集中
//! 在这里，弹窗文件只负责把 `track_focus` 接到自己的按钮上。
//!
//! # 为什么不改 `ui/mod.rs` 的 Esc 链，也不在渲染里 `window.focus()`
//!
//! - Esc 由 `Render::render` 的 `CancelOrClose` 优先级链负责（该文件本轮禁改），
//!   五个弹窗都被那条链覆盖（逐个确认过：`confirm_dialog` / 未完成引导 / `shortcuts_help_open`
//!   / `completion_dialog` / `benchmark_dialog` 各有一刀），这里**不重复处理**。
//! - 弹窗的 `render_*` 拿不到 `Window`（签名只有 `cx`，调用点在禁改文件里），没法在
//!   渲染路径上 `window.focus()`；而「回车要能触发主操作」又必须在焦点不在输入框里时
//!   也成立。于是走 GPUI 的 `App::intercept_keystrokes`：它在动作分发之前、且与焦点无关地
//!   为每个按键调一次，回调里带 `&mut Window`，正好同时满足「回车全局可用」与「Tab 需要
//!   Window 才能挪焦点」两件事。
//!
//! # 回车打到哪个动作
//!
//! 1. 焦点已经在本框的某个按钮上（用户 Tab 过来的）→ 激活**那个**按钮。
//!    少了这一条，「Tab 到『取消』再回车」会去执行「确认」，对确认框这种不可逆操作
//!    是实打实的误伤。
//! 2. 否则（焦点在别的输入框、或压根没焦点）→ 该框的**主操作**（见 [`primary_action`]），
//!    这正是「焦点在输入框里回车也要走主操作」那条要求。
//!
//! # 焦点句柄为什么放 `thread_local`
//!
//! `MainWindow` 的字段定义在 `ui/mod.rs`（本轮禁改），弹窗渲染路径又没有 `Window`，
//! 而焦点句柄**必须跨帧复用同一个**（每帧新建一个，`window.focus()` 就指到一个已经不在
//! 渲染树里的句柄上，键盘输入随即丢失）。`thread_local` 同时满足这两条，与同窗热词输入框
//! （`HOTWORDS_INPUT`）是同一套做法。每个弹窗一个面板句柄 + 若干按钮句柄（按钮句柄是 Tab 停靠点）。

use gpui::*;
use std::cell::RefCell;

use super::super::MainWindow;

/// 有键盘支持的模态框种类。顺序即 [`DialogKind::COUNT`] 的下标。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DialogKind {
    Completion,
    Benchmark,
    ShortcutsHelp,
    HardwareSetup,
    Confirm,
}

impl DialogKind {
    /// 句柄表长度。
    const COUNT: usize = 5;

    fn index(self) -> usize {
        self as usize
    }
}

/// 「现在开着哪些框」的只读快照。
///
/// 单独抽成一个不含 `MainWindow` 的结构，是为了让「谁在最上层」变成可单测的纯函数。
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct OpenDialogs {
    pub(crate) completion: bool,
    pub(crate) benchmark: bool,
    pub(crate) shortcuts_help: bool,
    pub(crate) command_palette: bool,
    pub(crate) hardware_setup: bool,
    pub(crate) confirm: bool,
}

/// 回车在某个框上对应的动作。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DialogAction {
    /// 主操作
    ConfirmAccept,
    CompletionGoToEditor,
    BenchmarkClose,
    ShortcutsHelpClose,
    HardwareSelectGpu,
    HardwareSelectCpu,
    /// 次级出口：只有焦点落在对应按钮上时才会被回车打到
    ConfirmCancel,
    CompletionClearBatch,
    HardwareDefer,
}

/// 谁在最上层（回车打到哪个框上）。
///
/// 顺序与 `ui/mod.rs` 的渲染顺序一致——越晚 `root.child(..)`，视觉上越靠上：
/// 完成框 → 基准框 → 命令面板 → 速查面板 → 硬件引导 → 确认框。
///
/// 命令面板返回 `None`：它自己的搜索框已经用 `on_key_down` 处理 Enter/↑/↓，这里再插一刀
/// 会让两边对同一按键各跑一遍。面板**之上**的框不受影响，仍然由本函数接管。
pub(crate) fn topmost_dialog(open: OpenDialogs) -> Option<DialogKind> {
    if open.confirm {
        return Some(DialogKind::Confirm);
    }
    if open.hardware_setup {
        return Some(DialogKind::HardwareSetup);
    }
    if open.shortcuts_help {
        return Some(DialogKind::ShortcutsHelp);
    }
    if open.command_palette {
        return None;
    }
    if open.benchmark {
        return Some(DialogKind::Benchmark);
    }
    if open.completion {
        return Some(DialogKind::Completion);
    }
    None
}

/// 某框的**主操作**（焦点不在本框按钮上时，回车打到哪儿）。
///
/// 确认 / 完成 / 基准 / 速查各有一个明确的主按钮；硬件引导没有「主按钮」，默认项就是
/// 当前高亮的那一档（`gpu_tier` = 配置里已是 GPU 档），回车等于确认它。
///
/// 这里**只会**返回主操作，绝不会返回取消 / 清空 / 稍后这类次级出口——测试守着这一点。
pub(crate) fn primary_action(kind: DialogKind, gpu_tier: bool) -> DialogAction {
    match kind {
        DialogKind::Confirm => DialogAction::ConfirmAccept,
        DialogKind::Completion => DialogAction::CompletionGoToEditor,
        DialogKind::Benchmark => DialogAction::BenchmarkClose,
        DialogKind::ShortcutsHelp => DialogAction::ShortcutsHelpClose,
        DialogKind::HardwareSetup => {
            if gpu_tier {
                DialogAction::HardwareSelectGpu
            } else {
                DialogAction::HardwareSelectCpu
            }
        }
    }
}

/// 一个框的焦点句柄：面板本身 + 面板内每个可 Tab 到的按钮（附该按钮自己的动作）。
#[derive(Default)]
struct DialogFocus {
    panel: Option<FocusHandle>,
    buttons: Vec<(FocusHandle, DialogAction)>,
}

/// 进程内（= 单窗口 UI 线程）的句柄表 + 注册过的按键拦截器。
struct Keyboard {
    entries: [DialogFocus; DialogKind::COUNT],
    /// 拦截器要读 `MainWindow` 的开框状态，句柄在注册时存一份。
    main: Option<WeakEntity<MainWindow>>,
    /// 拦截器订阅。`Subscription` 一 drop 就注销，必须留活到窗口结束。
    hook: Option<Subscription>,
}

impl Keyboard {
    fn new() -> Self {
        Self {
            entries: std::array::from_fn(|_| DialogFocus::default()),
            main: None,
            hook: None,
        }
    }

    /// 本框里当前拿到焦点的按钮对应哪个动作。
    fn focused_button_action(&self, kind: DialogKind, window: &Window) -> Option<DialogAction> {
        self.entries[kind.index()]
            .buttons
            .iter()
            .find(|(handle, _)| handle.is_focused(window))
            .map(|(_, action)| *action)
    }
}

thread_local! {
    static KEYBOARD: RefCell<Keyboard> = RefCell::new(Keyboard::new());
}

/// 取（或建）某框的面板焦点句柄。跨帧复用同一个，渲染里挂到卡片上即可。
pub(crate) fn panel_focus(kind: DialogKind, cx: &mut App) -> FocusHandle {
    KEYBOARD.with(|k| {
        let mut k = k.borrow_mut();
        let entry = &mut k.entries[kind.index()];
        entry.panel.get_or_insert_with(|| cx.focus_handle()).clone()
    })
}

/// 取（或建）某框第 `index` 个按钮的焦点句柄；`action` 是该按钮自己的动作。
///
/// 按钮句柄是 **Tab 停靠点**（`tab_stop(true)`）：GPUI 的 Tab 顺序表只收 `track_focus`
/// 过的元素，而「能不能被 Tab 停住」由句柄上的这个标记决定——不设它，`focus_next`
/// 会直接跨过按钮跑到别的元素上。
pub(crate) fn button_focus(
    kind: DialogKind,
    index: usize,
    action: DialogAction,
    cx: &mut App,
) -> FocusHandle {
    KEYBOARD.with(|k| {
        let mut k = k.borrow_mut();
        let entry = &mut k.entries[kind.index()];
        while entry.buttons.len() <= index {
            entry
                .buttons
                .push((cx.focus_handle().tab_stop(true), action));
        }
        entry.buttons[index].0.clone()
    })
}

/// 注册一次全局按键拦截。弹窗每次渲染都调用，第一次之后是空操作。
pub(crate) fn ensure_handler(cx: &mut Context<MainWindow>) {
    if KEYBOARD.with(|k| k.borrow().hook.is_some()) {
        return;
    }
    let weak = cx.weak_entity();
    let hook = cx.intercept_keystrokes(move |event: &KeystrokeEvent, window, cx| {
        if dispatch(&event.keystroke, window, cx) {
            // 拦下回车 / Tab：不让它们再进输入框或触发同键位的其它动作。
            cx.stop_propagation();
        }
    });
    KEYBOARD.with(|k| {
        let mut k = k.borrow_mut();
        k.main = Some(weak);
        k.hook = Some(hook);
    });
}

/// 按键分发：返回 `true` 表示这个键由弹窗键盘层接管。
///
/// 只碰回车 / Tab（Shift+Tab 同族）。带 Ctrl/Alt/Cmd 的回车（例如 `Ctrl+Enter` 开始转写）
/// 一律放行，避免抢掉已有键位；Esc 也一律放行，交给全局链。
fn dispatch(keystroke: &Keystroke, window: &mut Window, cx: &mut App) -> bool {
    let modifiers = &keystroke.modifiers;
    if modifiers.control || modifiers.alt || modifiers.platform {
        return false;
    }
    let is_enter = keystroke.key.eq_ignore_ascii_case("enter");
    let is_tab = keystroke.key.eq_ignore_ascii_case("tab");
    if !is_enter && !is_tab {
        return false;
    }

    let Some(main) = KEYBOARD.with(|k| k.borrow().main.clone()) else {
        return false;
    };
    let Ok(open) = main.read_with(cx, |this, _| this.open_dialogs()) else {
        return false;
    };
    let Some(kind) = topmost_dialog(open) else {
        return false;
    };

    if is_enter {
        // 焦点在本框按钮上就激活那个按钮，否则走主操作。两种情形都落到
        // `run_dialog_action`，和鼠标点击是同一条路。
        let action = KEYBOARD
            .with(|k| k.borrow().focused_button_action(kind, window))
            .unwrap_or_else(|| {
                let gpu_tier = main
                    .read_with(cx, |this, _| this.state.config.gpu.is_gpu_tier())
                    .unwrap_or(false);
                primary_action(kind, gpu_tier)
            });
        return main
            .update(cx, |this, cx| this.run_dialog_action(action, cx))
            .is_ok();
    }

    // Tab：先把焦点挪进这个框，再走 GPUI 的 Tab 顺序；框内的按钮是停靠点，
    // 所以第一次 Tab 落在第一个按钮，之后逐个前进，到末尾回到第一个。
    let panel = panel_focus(kind, cx);
    if !panel.contains_focused(window, cx) {
        window.focus(&panel);
    }
    if modifiers.shift {
        window.focus_prev();
    } else {
        window.focus_next();
    }
    true
}

impl MainWindow {
    /// 弹窗开闭状态快照。
    fn open_dialogs(&self) -> OpenDialogs {
        OpenDialogs {
            completion: self.completion_dialog.is_some(),
            benchmark: self.benchmark_dialog.is_some(),
            shortcuts_help: self.shortcuts_help_open,
            command_palette: self.command_palette.is_some(),
            hardware_setup: !self.state.config.gpu.hardware_init_completed,
            confirm: self.confirm_dialog.is_some(),
        }
    }

    /// 键盘路径的唯一出口：把 [`DialogAction`] 翻译成**与鼠标点击同一个方法**。
    ///
    /// 每条分支都指向该弹窗按钮 `on_click` 里调用的那个 `pub(crate) fn`，
    /// 不是复制一份逻辑——否则键盘与鼠标迟早分叉。
    pub(crate) fn run_dialog_action(&mut self, action: DialogAction, cx: &mut Context<Self>) {
        match action {
            DialogAction::ConfirmAccept => self.accept_confirm_dialog(cx),
            DialogAction::ConfirmCancel => self.cancel_confirm_dialog(cx),
            DialogAction::CompletionGoToEditor => self.completion_go_to_editor(cx),
            DialogAction::CompletionClearBatch => self.completion_clear_batch(cx),
            DialogAction::BenchmarkClose => self.close_benchmark_dialog(cx),
            DialogAction::ShortcutsHelpClose => self.close_shortcuts_help(cx),
            DialogAction::HardwareSelectGpu => self.choose_gpu_tier(cx),
            DialogAction::HardwareSelectCpu => self.choose_cpu_tier(cx),
            DialogAction::HardwareDefer => self.defer_hardware_setup_dialog(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    // 刻意不 `use super::*`：把被测的名字逐个列出来，签名一改就立刻报错。
    use super::{primary_action, topmost_dialog, DialogAction, DialogKind, OpenDialogs};

    fn opened(apply: impl FnOnce(&mut OpenDialogs)) -> OpenDialogs {
        let mut open = OpenDialogs::default();
        apply(&mut open);
        open
    }

    #[test]
    fn no_open_dialog_means_no_interception() {
        assert_eq!(topmost_dialog(OpenDialogs::default()), None);
    }

    #[test]
    fn lowest_dialogs_are_recognised_one_by_one() {
        assert_eq!(
            topmost_dialog(opened(|o| o.completion = true)),
            Some(DialogKind::Completion)
        );
        assert_eq!(
            topmost_dialog(opened(|o| o.benchmark = true)),
            Some(DialogKind::Benchmark)
        );
        assert_eq!(
            topmost_dialog(opened(|o| o.shortcuts_help = true)),
            Some(DialogKind::ShortcutsHelp)
        );
        assert_eq!(
            topmost_dialog(opened(|o| o.hardware_setup = true)),
            Some(DialogKind::HardwareSetup)
        );
        assert_eq!(
            topmost_dialog(opened(|o| o.confirm = true)),
            Some(DialogKind::Confirm)
        );
    }

    #[test]
    fn the_topmost_of_several_wins_in_render_order() {
        assert_eq!(
            topmost_dialog(opened(|o| {
                o.completion = true;
                o.benchmark = true;
            })),
            Some(DialogKind::Benchmark)
        );
        assert_eq!(
            topmost_dialog(opened(|o| {
                o.completion = true;
                o.benchmark = true;
                o.shortcuts_help = true;
            })),
            Some(DialogKind::ShortcutsHelp)
        );
        assert_eq!(
            topmost_dialog(opened(|o| {
                o.completion = true;
                o.benchmark = true;
                o.shortcuts_help = true;
                o.hardware_setup = true;
            })),
            Some(DialogKind::HardwareSetup)
        );
        assert_eq!(
            topmost_dialog(opened(|o| {
                o.completion = true;
                o.benchmark = true;
                o.shortcuts_help = true;
                o.hardware_setup = true;
                o.confirm = true;
            })),
            Some(DialogKind::Confirm)
        );
    }

    #[test]
    fn command_palette_keeps_enter_for_itself_only() {
        // 面板盖住的框（完成 / 基准）：放行给面板自己处理。
        assert_eq!(
            topmost_dialog(opened(|o| {
                o.command_palette = true;
                o.completion = true;
                o.benchmark = true;
            })),
            None
        );
        // 面板之上的框（速查 / 硬件引导 / 确认）：仍然接管。
        assert_eq!(
            topmost_dialog(opened(|o| {
                o.command_palette = true;
                o.shortcuts_help = true;
            })),
            Some(DialogKind::ShortcutsHelp)
        );
        assert_eq!(
            topmost_dialog(opened(|o| {
                o.command_palette = true;
                o.hardware_setup = true;
            })),
            Some(DialogKind::HardwareSetup)
        );
        assert_eq!(
            topmost_dialog(opened(|o| {
                o.command_palette = true;
                o.confirm = true;
            })),
            Some(DialogKind::Confirm)
        );
    }

    #[test]
    fn primary_action_matches_each_dialogs_main_button() {
        assert_eq!(
            primary_action(DialogKind::Confirm, false),
            DialogAction::ConfirmAccept
        );
        assert_eq!(
            primary_action(DialogKind::Completion, true),
            DialogAction::CompletionGoToEditor
        );
        assert_eq!(
            primary_action(DialogKind::Benchmark, false),
            DialogAction::BenchmarkClose
        );
        assert_eq!(
            primary_action(DialogKind::ShortcutsHelp, false),
            DialogAction::ShortcutsHelpClose
        );
    }

    #[test]
    fn hardware_wizard_confirms_the_highlighted_tier() {
        assert_eq!(
            primary_action(DialogKind::HardwareSetup, true),
            DialogAction::HardwareSelectGpu
        );
        assert_eq!(
            primary_action(DialogKind::HardwareSetup, false),
            DialogAction::HardwareSelectCpu
        );
    }

    /// 主操作永远不是「取消 / 清空队列 / 稍后再说」：确认框上按回车绝不能
    /// 走到取消或清空队列上去（那两个是次级出口，只有焦点在对应按钮上才该触发）。
    #[test]
    fn primary_action_never_takes_a_secondary_exit() {
        let kinds = [
            DialogKind::Confirm,
            DialogKind::Completion,
            DialogKind::Benchmark,
            DialogKind::ShortcutsHelp,
            DialogKind::HardwareSetup,
        ];
        for kind in kinds {
            for gpu_tier in [false, true] {
                let action = primary_action(kind, gpu_tier);
                assert!(
                    !matches!(
                        action,
                        DialogAction::ConfirmCancel
                            | DialogAction::CompletionClearBatch
                            | DialogAction::HardwareDefer
                    ),
                    "{kind:?} 的主操作应当是主按钮，实际是 {action:?}"
                );
            }
        }
    }
}
