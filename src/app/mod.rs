pub mod state;

pub use state::{
    AppState, PolishMode, ProcessStatus, ResourceMetrics, WhisperModelTier, WorkspaceTab,
};

/// 启动关键路径上出现**致命**错误时的统一用户文案。
///
/// 为什么需要它：`main` 里「初始化日志 / 读取配置 / 打开数据库」失败时会在**建窗
/// 之前**返回，进程直接退出。Release 版是 Windows GUI 子系统、没有控制台，`?` 冒泡
/// 出来的 `Error: …` 只写 stderr 就被丢掉，`tracing` 也拿不到（`?` 不落日志）——用户
/// 看到的是「双击没反应、什么都没有」。真正把这句话弹到用户面前的动作在 bin 里
/// （写日志 + 原生弹窗），但「用户到底看到什么」必须可测，所以这里只做纯字符串拼接。
///
/// `stage` 是失败环节（如「读取配置文件 config.toml」），`detail` 是底层错误原文。
pub fn format_startup_failure(stage: &str, detail: &str) -> String {
    format!(
        "Voice2Word 启动失败，窗口无法打开。\n\n\
         失败环节：{stage}\n\
         原因：{detail}\n\n\
         常见原因与处理：\n\
         · 安装/解压目录不可写：程序需要在同目录创建 `voice2word.db` 与 `logs\\`，\
         请换到可写目录或去掉只读属性；\n\
         · 配置文件损坏：修正 `config.toml` 的 TOML 语法，或先把它改名让程序重新生成；\n\
         · 数据库被占用或损坏：关闭其它 Voice2Word 实例后重试。\n\n\
         详细日志（若能成功创建）：安装目录下 `logs\\latest.txt`。"
    )
}

#[cfg(test)]
mod tests {
    /// 这些测试刻意不 `use super::*`，只显式引入被测函数。
    use super::format_startup_failure;

    #[test]
    fn startup_failure_message_carries_stage_and_detail() {
        let msg = format_startup_failure("读取配置文件 config.toml", "反序列化失败: expected `=`");
        assert!(msg.contains("启动失败"), "必须点明这是启动失败：{msg}");
        assert!(
            msg.contains("读取配置文件 config.toml"),
            "必须带失败环节：{msg}"
        );
        assert!(msg.contains("expected `=`"), "必须带底层原因原文：{msg}");
        assert!(msg.contains("logs"), "必须给出日志位置线索：{msg}");
    }

    #[test]
    fn startup_failure_message_survives_empty_detail() {
        // 底层错误串可能为空（例如某些平台调用只有错误码）；文案不能因此塌掉。
        let msg = format_startup_failure("打开数据库", "");
        assert!(msg.contains("打开数据库"));
        assert!(msg.contains("启动失败"));
    }
}
