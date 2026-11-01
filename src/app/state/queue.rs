//! 批量转写队列与转写步骤看板：入队/持久化/恢复、活动项收尾与批量进度推进。

use std::path::PathBuf;

#[cfg(not(test))]
use crate::utils::AppConfig;

use super::{
    next_pending_index, AppState, ProcessStatus, QueueItem, QueueState, StageRow, StageRowState,
};

impl AppState {
    /// 把文件加入批量转写队列，返回实际新增的数量。
    ///
    /// 重复入队（同一路径已在队列里）会被跳过：用户常把整批文件拖两次，
    /// 不去重就会白跑一遍。路径按小写归一后比较，规避 Windows 盘符与大小写差异。
    pub fn enqueue_files(&mut self, paths: Vec<PathBuf>) -> usize {
        let mut existing: std::collections::HashSet<String> = self
            .batch_queue
            .iter()
            .map(|i| i.path.to_string_lossy().to_lowercase())
            .collect();
        let mut added = 0;
        for path in paths {
            if !existing.insert(path.to_string_lossy().to_lowercase()) {
                continue;
            }
            self.batch_queue.push(QueueItem::new(path));
            added += 1;
        }
        if added > 0 {
            // 入队即落盘：用户排好队就去干别的了，此刻不存的话一关窗就白排。
            self.persist_queue();
        }
        added
    }

    /// 批量队列的持久化路径（与 `config.toml` 同目录）。
    ///
    /// 为什么用 `resolve_path` 而不是相对当前目录：程序可能从开始菜单 / 快捷方式
    /// 启动，cwd 会变成 `C:\Windows\System32`，相对路径会把队列文件写到系统目录。
    pub fn queue_file_path() -> std::path::PathBuf {
        // 测试隔离：测试里改用**进程专属的临时目录**。
        //
        // 不加这一层的话，`cargo test` 会把这个文件写到开发机**真实的项目根**上：
        // 凡是用到 `persist_queue` 的单测（队列结算、进度定格等好几个）都会顺手
        // 把用户排好的队列覆盖成测试数据。这不是假设——本项目就发生过：跑完测试后
        // 项目根下的 `batch_queue.json` 里剩下的是测试用的 `a.mp4`，用户排的队没了。
        #[cfg(test)]
        {
            std::env::temp_dir()
                .join(format!("v2w_test_queue_{}", std::process::id()))
                .join(crate::utils::queue_store::QUEUE_FILE_NAME)
        }
        #[cfg(not(test))]
        {
            AppConfig::resolve_path(crate::utils::queue_store::QUEUE_FILE_NAME)
        }
    }

    /// 把当前队列落盘（原子写）。
    ///
    /// 队列是**易失状态**，所以这里刻意不返回错误、失败只记日志：一次写盘失败不该
    /// 打断用户正在做的事（他可能刚点了「开始全部」）。代价是极端情况下队列没存上，
    /// 那也比因为一个可选的辅助文件而报错强。
    pub fn persist_queue(&self) {
        let items: Vec<crate::utils::queue_store::StoredQueueItem> = self
            .batch_queue
            .iter()
            .map(|q| crate::utils::queue_store::StoredQueueItem {
                path: q.path.to_string_lossy().to_string(),
                duration: q.duration,
                status: match &q.state {
                    QueueState::Pending => "pending".to_string(),
                    QueueState::Running => "pending".to_string(),
                    QueueState::Done { .. } => "done".to_string(),
                    QueueState::Failed(_) => "failed".to_string(),
                },
            })
            .collect();
        let path = Self::queue_file_path();
        if let Err(e) = crate::utils::queue_store::save_queue(&path, &items) {
            tracing::warn!(error = %e, "批量队列落盘失败（不影响本次会话）");
        }
    }

    /// 启动时恢复上次的队列。
    ///
    /// `running` 状态**恢复为等待中**：上次退出时正在跑的那条要么已经跑完（DB 里
    /// 有记录，`find_cached_task` 会命中）、要么被中断，无论哪种都不该显示成
    /// 「转写中…」——那会让用户以为程序还在干活。
    pub fn restore_queue(&mut self) {
        let items = crate::utils::queue_store::load_queue(&Self::queue_file_path());
        if items.is_empty() {
            return;
        }
        self.batch_queue = items
            .into_iter()
            .map(|stored| {
                let mut item = QueueItem::new(std::path::PathBuf::from(&stored.path));
                item.duration = stored.duration;
                item.state = match stored.status.as_str() {
                    s if crate::utils::queue_store::is_finished(s) => {
                        // 句数未知（磁盘上只存了状态）：显示「已完成」，具体句数在
                        // 用户点开 / 重跑时才需要。写 0 而不是编一个数。
                        QueueState::Done { segments: 0 }
                    }
                    "failed" => QueueState::Failed("上次运行未完成".to_string()),
                    _ => QueueState::Pending,
                };
                item
            })
            .collect();
        tracing::info!(count = self.batch_queue.len(), "已恢复上次的批量队列");
    }

    /// 下一个还需要处理的队列下标。
    ///
    /// 只看「等待中」：已成功的不重复烧算力，失败的也不能在这里被自动重挑，
    /// 否则单个坏文件会让批量续跑卡在原地反复重试。
    pub fn queue_next_actionable(&self) -> Option<usize> {
        next_pending_index(&self.batch_queue)
    }

    /// 队列里某个路径的下标。按小写归一比较，与入队去重保持同一套判等规则，
    /// 避免「同一个文件因为盘符大小写差异被当成两条」。
    pub fn queue_index_of(&self, path: &std::path::Path) -> Option<usize> {
        let key = path.to_string_lossy().to_lowercase();
        self.batch_queue
            .iter()
            .position(|i| i.path.to_string_lossy().to_lowercase() == key)
    }

    /// 把某条队列项标记为「转写中」，让队列卡片立刻有反馈。
    ///
    /// 顺带把本条进度清零：条目重跑（失败重排、或用户再点一次「开始全部」）时，
    /// 不清就会带着上一轮残留的百分比进新任务——第一批进度事件到达前它一直挂在
    /// 卡片上，看起来就像「刚开始就已经跑到 80%」。
    ///
    /// 真正开工：铺好这一条的阶段产物目录，桌面端批量跑崩了可从产物续跑。
    ///
    /// 放在这里的理由：这是**批量模式**里「开始跑某一条」的唯一状态入口（`ui/actions.rs`
    /// 的 `start_processing_for` 在标记转写中时调用）。单文件模式不走队列，由开工路径直接
    /// 调 [`Self::prepare_stage_artifacts`]（同一支片子的同一口径，桌面端与 CLI 共用缓存）。
    /// 目录建不起来只记一条日志：续跑是加速手段，不能反过来挡住转写。
    pub fn mark_queue_running(&mut self, idx: usize) {
        let path = match self.batch_queue.get_mut(idx) {
            Some(item) => {
                item.state = QueueState::Running;
                item.progress = 0.0;
                item.stage = None;
                item.path.clone()
            }
            None => return,
        };
        if let Err(err) = self.prepare_stage_artifacts(&path) {
            tracing::warn!(
                error = %err,
                path = %path.display(),
                "阶段产物目录不可用，本次不落盘 / 不续跑"
            );
        }
    }

    /// 从队列里移除一条（等待中或已完成的条目都能删）。
    ///
    /// 删除会让后续下标整体前移，所以活动指针必须跟着修正。
    ///
    /// 唯一允许被删的「正在跑的那条」是其**尚在排队**（`batch_active` 尚未指向它）
    /// 的条目：真正 `Running` 的项在界面上不给删（见 `render_batch_queue_panel`）。
    /// 因此被删条目不可能恰是 `batch_active` 指向的那条，指针只需处理「前移」情形。
    pub fn remove_queue_item(&mut self, idx: usize) -> bool {
        if idx >= self.batch_queue.len() {
            return false;
        }
        self.batch_queue.remove(idx);
        // 删条目同样落盘（否则下次启动它又回来了）。
        self.persist_queue();
        self.batch_active = match self.batch_active {
            // 被删的正是活动项（理论上不会发生，防御性处理）：没有归属了，作废
            Some(active) if active == idx => None,
            // 删的是它前面的条目：整条队列前移一格，指针跟着前移
            Some(active) if active > idx => Some(active - 1),
            other => other,
        };
        true
    }

    /// 把队列里的某一条搬到指定下标（`from` → `to`，都是**当前**队列的下标）。
    ///
    /// 只动顺序，不碰 [`Self::batch_running`] / [`Self::status`]：正在跑的那条换了位置
    /// 也还是它自己在跑，重排不该打断或重启任何一条转写。
    ///
    /// 关键在活动指针：搬迁会让**所有**下标失效，所以这里不假定下标不变，而是先记下
    /// `batch_active` 那条的**路径**，搬完再按路径查回它的新位置。少了这一步，
    /// [`Self::finish_active_queue_item`] 会把完成/失败结算给它的邻居，而真正在跑的
    /// 那条永远停在「转写中」。
    ///
    /// 返回是否真的动了：越界、`from == to`、空队列一律 `false` 且不改任何状态。
    pub fn move_queue_item(&mut self, from: usize, to: usize) -> bool {
        let len = self.batch_queue.len();
        if from >= len || to >= len || from == to {
            return false;
        }
        // 记路径而不是下标：搬完只有「同一条」这个身份还可靠。`batch_active` 越界
        // （理论上的脏值）时拿到 `None`，此时保持原值不动——顺手把一个本就无效的指针
        // 「修」成别的样子只会更糟。
        let active_path = self
            .batch_active
            .and_then(|idx| self.batch_queue.get(idx))
            .map(|item| item.path.clone());
        let item = self.batch_queue.remove(from);
        self.batch_queue.insert(to, item);
        if let Some(path) = active_path {
            self.batch_active = self.queue_index_of(&path);
        }
        // 顺序也要落盘：用户排好队才关窗，下次启动必须还是这个顺序。
        self.persist_queue();
        true
    }

    /// 队列里还没跑完的条目数（等待中 + 失败待重试）。
    /// 失败项也算「待处理」——用户点「开始全部」时它们会被重新排队。
    pub fn queue_pending_count(&self) -> usize {
        self.batch_queue
            .iter()
            .filter(|i| i.state.is_actionable())
            .count()
    }

    /// 结算当前正在处理的队列项（成功后 `batch_active` 归位，避免重复结算）
    ///
    /// 结算完必须把 `ProcessStatus` 收回 `Idle`：调用点都在「一条转写已经结束」
    /// 之后，此刻没有任何东西在跑。批量**失败**路径若漏掉这一步，状态会一直停在
    /// `Processing`，而续跑入口 `start_processing_for` 见到 `Processing` 会直接
    /// return——一个坏文件就能让整批卡死在「转写中」，后面的文件永远不开跑
    /// （队列看着在跑、进度条也不动，用户只能手动「终止批量」才出得来）。
    pub fn finish_active_queue_item(&mut self, outcome: Result<usize, String>) {
        if let Some(idx) = self.batch_active.take() {
            if let Some(item) = self.batch_queue.get_mut(idx) {
                // 进度条定格：成功收到满格，失败保留出事那一刻的值不动
                // （失败信息本身走 `QueueState::Failed`，这里只负责不让进度条
                // 看起来还活着）。阶段名一律清掉：任务已经结束，再挂一个
                // 「语音识别」只会让人以为它还在跑。
                if outcome.is_ok() {
                    // 沿用「百分比」这套单位（面板也按 0~100 判满格），
                    // 不写 1.0：两处口径不一致时进度条会比文字状态早一帧消失。
                    item.progress = 100.0;
                }
                item.stage = None;
                item.state = match outcome {
                    Ok(segments) => QueueState::Done { segments },
                    Err(reason) => QueueState::Failed(reason),
                };
            }
            // 每条结算都落盘：批量跑几十条时用户随时可能关窗，进度必须留下，
            // 否则重启后「已完成」的条目又变回等待中，整批白跑。
            self.persist_queue();
        }
        // 无论有没有结算到具体条目（用户可能把正在跑的那条从队列里删了，
        // 于是 `batch_active` 已作废），这一条转写都结束了，状态都必须归位。
        // 只收「还在处理中」的状态：调用点可能已经先写入了更具体的失败原因
        // （如「未识别出任何有效字幕」），那类提示要留给用户看，不能被抹掉。
        if matches!(self.status, ProcessStatus::Processing { .. }) {
            self.status = ProcessStatus::Idle;
        }
    }

    /// 把队列里**路径匹配**的那一条结算为「已完成」。
    ///
    /// # 为什么需要它（这是一个真实的功能 bug 的修复）
    ///
    /// 队列卡片是可点的：[`Self::select_queue_item`] 会把该文件设成 `transcribe_file`。
    /// 用户点完队列卡片、再按「开始转写」，走的是**单文件入口**
    /// `MainWindow::start_processing`——那条路会显式把 `batch_running` 置 false
    /// （它的注释写明「单文件入口显式退出批量续跑语义」），于是收尾时
    /// `in_batch` 为假，[`Self::finish_active_queue_item`] 永远不会被调用。
    ///
    /// 结果：这一条其实已经转写完成、结果也已入库，队列里却一直停在「等待中」；
    /// 又因为入队时已按 `pending` 落过盘，**每次重启都会再冒出来**。用户看到的正是
    /// 「明明转写完了，队列里总挂着一条待转写」。
    ///
    /// 按**路径**匹配而不是按下标：单文件入口全程不碰 `batch_active`，且队列在
    /// 一次转写期间可能被增删（用户边跑边加片子），下标早就不可靠了。
    ///
    /// 已经结算过（`Done`）的条目直接返回 `false`：不重复写盘，也不改动
    /// 已有的真实句数。
    pub fn settle_queue_item_by_path(&mut self, path: &std::path::Path, segments: usize) -> bool {
        let key = path.to_string_lossy().to_lowercase();
        let Some(idx) = self
            .batch_queue
            .iter()
            .position(|i| i.path.to_string_lossy().to_lowercase() == key)
        else {
            return false;
        };
        if matches!(self.batch_queue[idx].state, QueueState::Done { .. }) {
            return false;
        }
        {
            let item = &mut self.batch_queue[idx];
            item.state = QueueState::Done { segments };
            // 与 \`finish_active_queue_item\` 同口径：进度条定格满格、阶段名清掉，
            // 避免卡片上还挂着「语音识别」让人以为它仍在跑。
            item.progress = 100.0;
            item.stage = None;
        }
        // 结算即落盘：与批量结算同一条规则，否则重启后这条又变回「等待中」。
        self.persist_queue();
        true
    }

    /// 把一次管线进度事件记到**当前正在跑的那一条**队列项上。
    ///
    /// 判据只用 `batch_active`：它就是「这一条」的唯一权威来源，进度事件里
    /// 不带文件名。`batch_active == None` 有两种情形都要**直接忽略**：
    /// 单文件转写、以及批量已收尾（`finish_batch_queue` 清掉指针之后）。
    /// 少了这层判断，单文件转写的百分比就会写进队列里某个「等待中」的条目，
    /// 批量模式连开都没开，卡片上却冒出进度。
    ///
    /// 阶段名沿用管线给的字符串（`PipelineEvent::Progress` 里的 `stage`），
    /// 这里不改写措辞：UI 换词就得跟着管线的阶段表改两处，迟早不同步。
    pub fn record_batch_progress(&mut self, stage: &str, progress: f64) {
        let Some(idx) = self.batch_active else {
            return;
        };
        let Some(item) = self.batch_queue.get_mut(idx) else {
            return;
        };
        // `abs()` 兜住上游万一发来的负值（管线目前只发 0.0~1.0 的比率），
        // 再换算成 0~100 的百分比。
        item.progress = (progress.abs() * 100.0).clamp(0.0, 100.0);
        item.stage = Some(stage.to_string());
    }

    /// 队列汇总：`(已完成数, 失败数, 累计字幕句数)`
    pub fn queue_summary(&self) -> (usize, usize, usize) {
        let mut done = 0;
        let mut failed = 0;
        let mut segments = 0;
        for item in &self.batch_queue {
            match item.state {
                QueueState::Done { segments: n } => {
                    done += 1;
                    segments += n;
                }
                QueueState::Failed(_) => failed += 1,
                _ => {}
            }
        }
        (done, failed, segments)
    }

    /// 队列累计时长（秒），用于批量完成弹窗
    pub fn queue_total_duration(&self) -> f64 {
        self.batch_queue.iter().map(|i| i.duration).sum()
    }

    /// 队列里失败条目的文件名，用于批量完成弹窗直接点名需要重试的对象
    pub fn queue_failed_names(&self) -> Vec<String> {
        self.batch_queue
            .iter()
            .filter(|i| matches!(i.state, QueueState::Failed(_)))
            .map(|i| i.name.clone())
            .collect()
    }

    /// 清空队列并退出批量模式（已转写完成、已落库的工程不受影响）
    pub fn clear_batch_queue(&mut self) {
        self.batch_queue.clear();
        self.batch_running = false;
        self.batch_active = None;
        // 清空也要落盘：否则下次启动会把「已经清掉的队列」又恢复回来。
        self.persist_queue();
    }

    /// 点「开始转写」那一刻：把全部阶段**全部**列出来，第一步置为进行中。
    ///
    /// 用户的要求是「点开始后界面就把步骤全显示出来」，所以这里不做懒加载：
    /// 一次把 [`Stage::ALL`] 铺满，后面的行先显示「等待」。
    pub fn reset_stage_rows(&mut self) {
        self.stage_rows = crate::core::Stage::ALL
            .into_iter()
            .map(|stage| StageRow {
                stage,
                state: StageRowState::Pending,
                progress: 0.0,
                secs: 0.0,
            })
            .collect();
        if let Some(first) = self.stage_rows.first_mut() {
            first.state = StageRowState::Running;
        }
        self.stage_row_started_at = Some(std::time::Instant::now());
        // 代次 +1：上一轮遗留的秒数刷新循环会看到代次变化后自行退出。
        self.stage_rows_tick = self.stage_rows_tick.wrapping_add(1);
    }

    /// 管线报告「进入某阶段」时推进看板。
    ///
    /// 目标阶段**之前**还没跑过的行一律记为「跳过」：管线可能整段不执行
    /// （关掉润色就没有「智能润色」、没有低置信窗口就没有「置信度救场」）。
    /// 停在第「等待」会让用户以为卡死在那一步。
    pub fn advance_stage_rows(&mut self, label: &str) {
        let Some(target) = crate::core::Stage::from_label(label) else {
            return;
        };
        let now = std::time::Instant::now();
        let elapsed = self
            .stage_row_started_at
            .map(|t| now.duration_since(t).as_secs_f64());
        let mut entered_new = false;
        for row in &mut self.stage_rows {
            if row.stage == target {
                if row.state != StageRowState::Running {
                    row.state = StageRowState::Running;
                    entered_new = true;
                }
                break;
            }
            match row.state {
                StageRowState::Running => {
                    row.state = StageRowState::Done;
                    row.progress = 1.0;
                    row.secs = elapsed.unwrap_or(0.0);
                }
                StageRowState::Pending => row.state = StageRowState::Skipped,
                _ => {}
            }
        }
        if entered_new {
            self.stage_row_started_at = Some(now);
        }
    }

    /// 更新**当前进行中那一行**的进度。
    ///
    /// 管线给的是全局进度（0.15 起步），直接画会让人以为每步都从头开始，
    /// 所以按阶段拆开：每行只显示自己那一段的完成度。
    pub fn set_stage_row_progress(&mut self, progress: f32) {
        if let Some(row) = self
            .stage_rows
            .iter_mut()
            .find(|r| r.state == StageRowState::Running)
        {
            row.progress = progress.clamp(0.0, 1.0);
        }
    }

    /// 任务收尾（完成 / 失败 / 取消）：把还在跑的那一行定住，其余未跑的标为跳过。
    ///
    /// 失败时**故意不把进度拉满**：留着一个没走完的条，比假装「全部完成」诚实。
    pub fn finish_stage_rows(&mut self) {
        let elapsed = self
            .stage_row_started_at
            .take()
            .map(|t| t.elapsed().as_secs_f64());
        for row in &mut self.stage_rows {
            match row.state {
                StageRowState::Running => {
                    row.state = StageRowState::Done;
                    row.secs = elapsed.unwrap_or(0.0);
                }
                StageRowState::Pending => row.state = StageRowState::Skipped,
                _ => {}
            }
        }
        self.stage_rows_tick = self.stage_rows_tick.wrapping_add(1);
    }
}
