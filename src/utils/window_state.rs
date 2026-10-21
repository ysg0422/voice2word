//! 记住主窗口的大小与位置。
//!
//! 为什么不写进 `config.toml`：那是**随仓库分发**的配置，每关一次窗就改一次，
//! 会让 `git status` 永远脏一条；而窗口几何本就是「这台机器 + 这块屏」的临时状态，
//! 与 `batch_queue.json` 同类，因此单独一个根目录文件（已在 `.gitignore` 中）。
//!
//! 读盘失败 / 字段缺失 / 数值离谱一律当作「没有记录」：宁可回到默认居中，
//! 也不要因为一个坏掉的 JSON 让程序开在一个看不见的位置。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 落盘文件名（相对项目根）。
pub const WINDOW_STATE_FILE: &str = "window_state.json";

/// 与 `main.rs` 的 `window_min_size` 保持一致：比这更小的记录一律视为损坏。
const MIN_W: f32 = 960.0;
const MIN_H: f32 = 600.0;
/// 上限只是挡住「1e9」这类脏值；正常屏幕远达不到。
const MAX_DIM: f32 = 16384.0;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowGeometry {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl WindowGeometry {
    /// 数值是否可用：有限、尺寸在 [最小, 上限] 内、坐标不荒唐。
    ///
    /// 坐标只做粗筛（±一万六），真正的「是否在屏幕内」由调用方拿显示器边界判断——
    /// 这里拿不到屏幕信息，也不该在纯函数里猜。
    pub fn is_usable(&self) -> bool {
        let finite = [self.x, self.y, self.width, self.height]
            .iter()
            .all(|v| v.is_finite());
        if !finite {
            return false;
        }
        self.width >= MIN_W
            && self.height >= MIN_H
            && self.width <= MAX_DIM
            && self.height <= MAX_DIM
            && self.x.abs() <= MAX_DIM
            && self.y.abs() <= MAX_DIM
    }

    /// 把左上角拉回屏幕内，保证窗口至少有一部分可见（换显示器 / 改分辨率之后）。
    ///
    /// `screen` 是**逻辑像素**的屏幕尺寸；拿不到时原样返回。
    pub fn clamp_origin_to(&self, screen: Option<(f32, f32)>) -> WindowGeometry {
        let Some((sw, sh)) = screen else {
            return *self;
        };
        // 留一小截边距：完全贴边时窗口边框（含右上角关闭按钮）会压到屏幕外。
        const MARGIN: f32 = 40.0;
        let max_x = (sw - MARGIN).max(0.0);
        let max_y = (sh - MARGIN).max(0.0);
        WindowGeometry {
            x: self.x.clamp(MARGIN - self.width, max_x),
            y: self.y.clamp(0.0, max_y),
            ..*self
        }
    }
}

fn state_path() -> PathBuf {
    crate::utils::AppConfig::resolve_path(WINDOW_STATE_FILE)
}

/// 读取上次的窗口几何；任何异常都返回 `None`（回到默认居中）。
pub fn load() -> Option<WindowGeometry> {
    let path = state_path();
    let raw = std::fs::read_to_string(&path).ok()?;
    // 必须容忍 UTF-8 BOM：这个文件是**可被用户/编辑器碰到**的，
    // 而 PowerShell 的 `Set-Content -Encoding UTF8`、记事本的「另存为 UTF-8」
    // 都会在开头写一个 U+FEFF。`serde_json::from_str` 遇到 BOM 会直接报错，
    // 于是整份记录被当成「不存在」——用户明明存过，窗口却每次都回到默认位置。
    let raw = raw.strip_prefix('\u{FEFF}').unwrap_or(&raw);
    let geo: WindowGeometry = serde_json::from_str(raw).ok()?;
    geo.is_usable().then_some(geo)
}

/// 保存窗口几何。**尽力而为**：写失败只记一条 debug，绝不能因此挡住关窗。
pub fn save(geo: WindowGeometry) {
    if !geo.is_usable() {
        // 最小化 / 全屏切换的瞬间可能读到 0 尺寸，那种值存下来只会污染下次启动。
        return;
    }
    let path = state_path();
    let Ok(json) = serde_json::to_string_pretty(&geo) else {
        return;
    };
    if let Err(err) = std::fs::write(&path, json) {
        tracing::debug!(path = %path.display(), error = %err, "窗口几何写入失败（忽略）");
    }
}

#[cfg(test)]
mod tests {
    use super::WindowGeometry;

    fn geo(x: f32, y: f32, w: f32, h: f32) -> WindowGeometry {
        WindowGeometry {
            x,
            y,
            width: w,
            height: h,
        }
    }

    /// 坏值一律判不可用：否则程序会开在看不见的地方，用户只能去任务管理器里杀掉。
    #[test]
    fn rejects_absurd_geometry() {
        assert!(geo(100.0, 100.0, 1280.0, 860.0).is_usable());
        // 比最小尺寸还小（最小化瞬间读到的 0）
        assert!(!geo(0.0, 0.0, 0.0, 0.0).is_usable());
        assert!(!geo(0.0, 0.0, 800.0, 860.0).is_usable());
        // 非有限值 / 天文数字
        assert!(!geo(f32::NAN, 0.0, 1280.0, 860.0).is_usable());
        assert!(!geo(0.0, 0.0, f32::INFINITY, 860.0).is_usable());
        assert!(!geo(0.0, 0.0, 1e9, 1e9).is_usable());
    }

    /// 带 BOM 的记录必须能读：PowerShell / 记事本另存 UTF-8 都会加 BOM，
    /// 而 `serde_json` 遇到 BOM 直接报错——那会让「记住窗口位置」对一部分用户完全失效。
    #[test]
    fn parses_json_with_utf8_bom() {
        let raw = "\u{FEFF}{\"x\":60.0,\"y\":80.0,\"width\":1000.0,\"height\":700.0}";
        let stripped = raw.strip_prefix('\u{FEFF}').unwrap_or(raw);
        let geo: WindowGeometry = serde_json::from_str(stripped).expect("去 BOM 后应能解析");
        assert_eq!(geo.width, 1000.0);
        // 反证：不去 BOM 就解析不了（这条钉住「为什么必须 strip」）
        assert!(serde_json::from_str::<WindowGeometry>(raw).is_err());
    }

    /// 换显示器 / 改分辨率后，旧坐标可能落在屏幕外：必须拉回来，且不能拉成负数尺寸。
    #[test]
    fn clamps_origin_into_screen() {
        let off = geo(5000.0, 5000.0, 1280.0, 860.0);
        let got = off.clamp_origin_to(Some((1920.0, 1080.0)));
        assert!(got.x < 1920.0, "左上角不能留在屏幕外: {got:?}");
        assert!(got.y < 1080.0, "左上角不能留在屏幕外: {got:?}");
        assert_eq!(got.width, 1280.0);
        assert_eq!(got.height, 860.0);

        // 负坐标（第二块屏在主屏左侧被拔掉）也拉回来
        let left = geo(-2000.0, 100.0, 1280.0, 860.0);
        assert!(left.clamp_origin_to(Some((1920.0, 1080.0))).x > -2000.0);

        // 拿不到屏幕尺寸：原样返回，不做猜测
        assert_eq!(off.clamp_origin_to(None), off);
    }
}
