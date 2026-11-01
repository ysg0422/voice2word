//! 字幕后台写入子系统：写入闸、按工程水位防回退、去抖派发与同步落库。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::storage::Database;

use super::AppState;

/// 脏字幕的落库去抖窗口：用户停手这么久之后，才把整份工程写回历史库。
///
/// # 为什么要去抖
///
/// 落库 = 把整份 `segments` 序列化成 JSON + 一条 sqlite UPDATE。5000 句的工程大约
/// 10~30ms。而拖时间轴的定位请求每 40ms 就来一次（见 `seek_by_mouse_x` 的节流），
/// 切句 / 暂停播放同样是高频动作——逐次写库等于让磁盘写把交互本身拖慢，
/// 用户看到的就是「拖一下卡一下」。
///
/// 停手 1.2s 后写一次：连续操作自动合成一次写盘。真崩了最多丢这 1.2s 内的编辑，
/// 而换工程 / 导出 / 关窗守卫走的都是**立刻写入**的那条路，不会丢。
pub const SEGMENTS_FLUSH_IDLE: std::time::Duration = std::time::Duration::from_millis(1200);

/// 单次「同步落库」等待后台写线程的最长时间。
///
/// 超过这个时间就不再干等，直接自己写（见 `flush_segments_if_dirty`）。宁可让当前这一份
/// 最新快照先落盘（即便可能越过那个还没写完的旧快照），也不拖住关闭窗口 / 导出 / 换工程
/// 这些「写完才能继续」的路径：那些路径的语义是「返回时数据已在库里」。
const SEGMENTS_SYNC_FLUSH_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// 待落库的一份一致快照（序列化好的整份字幕 + 目标行 + 修订号）。
///
/// 单独成型是为了让「取快照」与「写到哪一行」分开：前者必须留在渲染线程（要读正在编辑
/// 的 `segments`），后者挪到后台。
pub(super) struct SegmentWriteSnapshot {
    /// 目标行：派发那一刻的工程主键。
    pub(super) task_id: i64,
    /// 派发那一刻的字幕修订号，用于判断回报是否已经过期。
    pub(super) revision: u64,
    /// 序列化好的整份字幕（`segments_json` 参数原样带上）。
    ///
    /// # 为什么把序列化留在渲染线程，而不是把整份 Vec 交给后台
    ///
    /// `Database` 里给后台写库准备的入口是 `update_task_segments_json(id, &str)`：
    /// 它接收已经序列化好的 JSON，后台只解析一次算派生列（样例文本 / 句数 / 有无译文）。
    /// 于是后台线程不需要 `Vec<Segment>` 的所有权，快照可以是**纯 String**——
    /// 但代价是序列化发生在渲染线程上，这与本任务「把写库挪走」的目标并不冲突：
    /// 任务要求允许「序列化在当前线程做（要拿一致快照）」，真正必须挪走的是 sqlite 写。
    /// 另一个好处是 `Segment` 类型不必跨线程克隆，派发时只移动一段字符串。
    pub(super) json: String,
}

/// 一次「把字幕快照写进库」的结局（见 [`AppState::dispatch_segment_write`]）。
///
/// # 为什么需要区分「在飞」与「已保存」
///
/// 去抖路径派发成功后会**保留** `segments_dirty`，直到后台回报确认落库——脏标记的语义
/// 是「当前内容还没落库」，而派发只是把这份快照交出去，还没写完。关窗守卫读的就是它：
/// 若派发那一刻就清脏，用户「改完 → 去抖派发 → 立刻关窗」这条链上，守卫会以为没有未保存
/// 内容而放行，此时在飞写一旦失败，改动就**静默丢失**（失败回报要等下一次 poll 才被看到，
/// 而窗口已经关了）。
///
/// 但后台线程起不来、退回同步写的那条兜底也可能**当场写完**——那种结局必须直接清脏，
/// 否则「没有任何后台回报」会让脏标记永远挂着。所以调用方需要三态，而不是一个布尔。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SegmentDispatch {
    /// 已交给后台写线程：内容「在飞」，落库与否要等回报。
    Dispatched,
    /// 已经落库（同工程水位判定「无需再写」或同步兜底成功）。
    Saved,
    /// 没写进库（后台线程起不来且同步兜底也失败）：错误已挂上提示条。
    Failed,
}

/// 后台写线程回报给自己派发方的一条「写完了」信号。
///
/// # 为什么用通道，而不是「后台线程直接改 AppState」
///
/// 渲染线程按帧借用 `AppState`（`&mut`），后台线程若直接持有它就得共享可变引用——那等于
/// 把整个界面状态变成全局锁。通道把「后台只想说的一件事」抽成值：序号 + 修订号 + 结局。
/// 派发侧在渲染线程上收（`poll_background_writes`），于是**只有渲染线程写
/// `db_write_error` / `segments_dirty`**，不需要给 AppState 加锁，也不会出现
/// 「后台线程把一个已经换掉的工程标脏」。
///
/// # 错误怎么回到界面（本任务的难点）
///
/// 后台线程只把 `Result::Err` 的原样字符串塞进 `result` 投回来；`drain_segment_writes`
/// 在渲染线程上把它拼成「字幕保存失败：…」挂进 `db_write_error`，并（在快照仍是当前
/// 修订时）把 `segments_dirty` 置回触发重试。界面读的是原有字段，所以提示条与重试逻辑
/// 一行都不用改。
#[derive(Debug, Clone)]
pub(super) struct SegmentWriteDone {
    /// 派发时领到的单调写序号（见 `BackgroundWriteTracker::next_seq`）。
    pub(super) seq: u64,
    /// 快照对应的工程主键（原样带回，后台不查状态）。
    ///
    /// 只用于诊断日志与未来「按 task 归并失败」的扩展，回报判定不用它（判定看 seq +
    /// revision + error_at_dispatch 三件套）。留 `#[allow(dead_code)]` 而不是删掉：
    /// 删了之后 `tracing` 里那条「写过哪一行」的信息就再也补不回来了。
    #[allow(dead_code)]
    pub(super) task_id: i64,
    /// 落库时字幕内容的修订号：用来判断「这份快照是否已经被更新的编辑取代」。
    pub(super) revision: u64,
    /// 派发时 `db_write_error` 的值：只有它没被后续更晚的失败改过，才允许按本次结局处理。
    pub(super) error_at_dispatch: Option<String>,
    /// 写成功 / 失败原因（失败原因直接来自 sqlite 与它的前置校验）。
    pub(super) result: Result<(), String>,
}

/// 后台写线程的共享账本：单调写序号 + 按工程的落库水位。
///
/// 这两样东西共同实现「防回退」——旧快照的结局不许覆盖新状态：
///
/// - `next_seq`：派发序号只增不减，后台结果自带「我是哪一代」；同步写也从同一
///   序列领号，因此同步写与后台写在序号上可以直接比较先后；
/// - `applied`：**每个工程已经真正落库**的最大写序号。同步写与后台写都在写入闸
///   （[`BackgroundWrites::lock_writer`]）里先查它再写，于是更旧的快照不可能在
///   更新的内容之后落地——不依赖线程调度的先后（[`write_snapshot_under_gate`]）；
///   结果通道（`submit` / `drain_results`）也按它判断一条回报是不是已经被同工程
///   更新的写覆盖，因此不在这里囤失败队列。
///
/// `applied` 按**工程**分开记而不是记一个全局水位：全局水位会让「B 工程的新写」
/// 把「A 工程还没落地的最新快照」也一起吞掉，那是最难排查的一种丢写。
#[derive(Debug)]
pub(super) struct BackgroundWriteTracker {
    /// 下一个要发放的写序号。从 **1** 开始：0 专门表示「还没写过」，
    /// 于是「同工程水位 >= 待发序号」这条判据不会被第一次派发误命中。
    pub(super) next_seq: AtomicU64,
    /// 每个工程已经真正落库的最大写序号（`tasks.id` → seq）。
    ///
    /// 只在写入闸里读写，所以用普通 `Mutex` 而不是原子类型；条目数 = 本次运行
    /// 写过库的工程数，量级很小，不需要清理。
    applied: Mutex<HashMap<i64, u64>>,
}

impl Default for BackgroundWriteTracker {
    fn default() -> Self {
        Self {
            // 见字段注释：序号从 1 起，0 是「还没写过」的哨兵。
            next_seq: AtomicU64::new(1),
            applied: Mutex::new(HashMap::new()),
        }
    }
}

impl BackgroundWriteTracker {
    /// 某个工程已落库的最大写序号（0 = 还没写过）。
    pub(super) fn applied_seq(&self, task_id: i64) -> u64 {
        self.applied
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&task_id)
            .copied()
            .unwrap_or(0)
    }

    /// 记下一次**真正落库**的写。只增不减：并发下后到的更小序号绝不能把水位拉回去，
    /// 否则防回退就白做了。
    pub(super) fn note_applied(&self, task_id: i64, seq: u64) {
        let mut applied = self.applied.lock().unwrap_or_else(|e| e.into_inner());
        let entry = applied.entry(task_id).or_insert(0);
        if *entry < seq {
            *entry = seq;
        }
    }
}

/// 后台写入子系统（在 `AppState` 里以 `Arc` 持有一份）。
///
/// # 同一 task_id 的写入为什么能按序号顺序落地
///
/// 两道闸：
/// 1. **发放侧串行**：派发时 `writer` 这把锁把「查同工程水位 -> 领写序号 -> 起写线程」
///    做成对同步写与其它派发原子的临界区；
/// 2. **写入侧串行 + 按序号收口**：每个后台线程拿到快照后**先取同一把锁再写库**
///    （`write_guard`），同步写同样持锁写完；锁里同时完成「查同工程水位 -> 写 -> 记水位」
///    （[`write_snapshot_under_gate`]）。
///
/// 只靠「串行」是不够的：普通 `Mutex` 不保证先到先得，两个线程谁先拿到锁由调度决定，
/// 更晚派发（更新）的快照完全可能先写、再被更旧的快照盖掉。加上按工程的水位之后，
/// **更新的写一旦落库，更旧的写在锁里就会看到水位比自己新而直接放弃**——不依赖运气。
///
/// 再加上 `Database` 内部那条连接锁，写入本身也已经串行——这里额外加锁是为了**顺序**：
/// 光靠连接锁只能保证不交错，不保证先后。
///
/// # 为什么天然不会积压
///
/// 去抖路径停手 1.2s 才派发一次，而一次写只要 10~30ms，正常情况永远只有一个任务在飞。
/// 满负荷下（磁盘卡住 + 用户狂停手）最多也只是每 1.2s 多一个线程，线程写完即退，不占
/// 常驻资源；`in_flight_writes` 是「已派发还没收尾」的诊断计数器（派发时 +1、
/// 写线程退出时 -1），真正的等待判据是 `segments_write_pending`。
#[derive(Debug)]
pub(super) struct BackgroundWrites {
    pub(super) writer: Mutex<()>,
    pub(super) tracker: Arc<BackgroundWriteTracker>,
    pub(super) submit: std::sync::mpsc::Sender<SegmentWriteDone>,
    pub(super) results: Mutex<std::sync::mpsc::Receiver<SegmentWriteDone>>,
    pub(super) in_flight_writes: AtomicUsize,
}

impl Default for BackgroundWrites {
    fn default() -> Self {
        let (submit, results) = std::sync::mpsc::channel();
        Self {
            writer: Mutex::new(()),
            tracker: Arc::new(BackgroundWriteTracker::default()),
            submit,
            results: Mutex::new(results),
            in_flight_writes: AtomicUsize::new(0),
        }
    }
}

/// 同步落库等后台写收尾时的「停车位」：计数归零即唤醒，免得渲染线程按 2ms 轮询
/// 空转（关窗 / 换工程 / 导出最坏要空转满 [`SEGMENTS_SYNC_FLUSH_WAIT`]）。
///
/// 与 `AtomicUsize` 计数配套的经典组合：**通知方必须持锁再广播**，否则「等待方检查完
/// 谓词、还没睡下」窗口里到货的那次归零会把唤醒弄丢，等待方要睡满整个上限才发现。
/// 谓词检查在锁内做（[`Self::wait_until`]），两条交错路径都不丢唤醒：
/// - 通知方先归零再抢锁：等待方拿到锁后看到计数已为 0，压根不睡；
/// - 等待方持锁检查时通知方归零：通知方阻塞在锁上，等待方一睡下立刻被叫醒。
#[derive(Debug, Default)]
pub(super) struct PendingWait {
    lock: Mutex<()>,
    cv: std::sync::Condvar,
}

impl PendingWait {
    /// 计数归零后调用。先拿锁再广播（见类型文档）。
    pub(super) fn notify(&self) {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.cv.notify_all();
    }

    /// 等到 `pending` 归零或 `deadline` 到点；到点是否真归零由调用方再看计数。
    pub(super) fn wait_until(&self, pending: &AtomicUsize, deadline: std::time::Instant) {
        let mut guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        while pending.load(Ordering::SeqCst) > 0 {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            guard = self
                .cv
                .wait_timeout(guard, remaining)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

/// 「这一份后台写还没收尾」的凭证：**派发侧**领序号时 +1，任务收尾时 -1。
///
/// # 为什么 +1 在派发侧，而不是线程体里
///
/// 线程体里 +1 的写法有一个致命窗口：从派发返回到线程真正开跑之间，`pending` 还是 0。
/// 同步落库（关窗 / 换工程 / 导出）等的就是它——被这个窗口骗到就会「以为没有在飞的写」
/// 直接往下写，随后那条更旧的后台快照才落地，把新内容盖回去（旧快照的写序号比水位旧，
/// 本该被拦住）。计数点放在派发侧之后，`pending == 0` 才真的等于
/// 「没有已派发未收尾的写」。
///
/// 计数在**结果投递成功之后**才归零（`disarm`），所以 `pending == 0` 的含义是
/// 「结果已经在通道里」。剩下那些「没走到投递」的结局（提前放弃、panic、发送失败）
/// 由 `Drop` 兜底 -1，免得每个 return 分支都得记得递减（漏一处，计数就永远回不了零）。
///
/// 派发失败（线程都起不来）时没有线程来持有这份凭证，由派发侧自己 -1——见
/// `dispatch_segment_write` 的失败分支。
struct InFlightGuard {
    counter: Arc<AtomicUsize>,
    wait: Arc<PendingWait>,
}

impl InFlightGuard {
    /// 结果已经交给通道：这份写「收尾完成」的语义点位。
    ///
    /// 计数本身由 [`Drop`] 统一归还（而不是在这里）：投递成功、投递失败、panic
    /// 三种结局都必须让计数回零，否则同步落库会白等到超时。这里只承担语义标记，
    /// 挂着「没 disarm 的 Drop 就不递减」的误解看它会得出错误结论。
    fn disarm(self) {}
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
        // 归零即唤醒同步落库的停车位（见 [`PendingWait`]）。
        self.wait.notify();
    }
}

impl BackgroundWrites {
    /// 取 `writer` 锁，**容忍 Mutex 中毒**（与 `Database::lock_conn` 同策略）。
    ///
    /// 中毒只说明「上次有人在持锁时 panic 了」，守卫本身要保护的是**写入顺序**，不是数据：
    /// panic 的那个线程要么还没写、要么已经写完，锁里没有半截状态。若这里 `unwrap()`，
    /// 一次后台 panic 会让此后每一次派发与同步落库都连环 panic，整程序直接不能保存字幕
    /// ——比继续用这把锁危险得多。
    pub(super) fn lock_writer(&self) -> MutexGuard<'_, ()> {
        self.writer.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 排空后台结果通道：只留「还没有结论」的那些回报。
    ///
    /// # 这里丢掉的是什么（防回退的一部分）
    ///
    /// **同一个工程**已经有更新的写落库（`applied[task] > seq`）：这条迟到的回报描述的
    /// 是一个**已经被覆盖**的快照，再挂提示只会让用户以为「刚存好的东西又坏了」。留日志即可。
    ///
    /// 判据必须按工程比水位、不能拿全局最大写序号：写序号是跨工程共用的，但两个工程的
    /// 内容互不覆盖——拿 A 的新序号压 B 的迟到失败，会让 B 的失败既不提示、也不置脏，
    /// 而 B 当前修订其实还没落库（静默丢写，正是这条通道最不该发生的事）。
    ///
    /// 提示条的竞价（`error_at_dispatch` 与排空前快照不一致就整条不回写）**不在这里做**，
    /// 而是留给 [`drain_segment_writes`]：那里除了提示条，还要处理「这条失败描述的内容
    /// 到底还该不该重试」。判定只留一处，免得两处漂移成两个答案。
    ///
    /// 与 `lock_writer` 同策略容忍中毒：`results` 里装的是一次性判定用的值，没有半截状态；
    /// 而这里一旦 panic，界面就再也收不到写库失败的提示（错误被静默）。
    fn drain_results(&self) -> Vec<SegmentWriteDone> {
        let receiver = self.results.lock().unwrap_or_else(|e| e.into_inner());
        let mut kept = Vec::new();
        for done in receiver.try_iter() {
            if self.tracker.applied_seq(done.task_id) > done.seq {
                if let Err(err) = &done.result {
                    tracing::debug!(
                        seq = done.seq,
                        task_id = done.task_id,
                        error = %err,
                        "同工程已有更新的写落库，丢弃过期的后台写失败"
                    );
                }
                continue;
            }
            kept.push(done);
        }
        kept
    }
}

impl AppState {
    /// 把后台写线程的回报收进 `db_write_error` / `segments_dirty`（在渲染线程上调）。
    ///
    /// # 后台错误怎么回到界面
    ///
    /// 后台线程**不碰 `AppState`**（渲染线程按帧借用它，共享可变状态等于给整个界面加锁），
    /// 只把 `SegmentWriteDone` 投进 `segments_write_rx`。这里是唯一的消费点，
    /// 也是唯一写这两个字段的地方，规则与同步版一致：
    ///
    /// - **失败一律不静默**：`Result::Err` 会被挂进 `db_write_error`（界面渲染成提示条），
    ///   并把 `segments_dirty` 置回（若这份快照仍是当前修订），下一次 flush 会重试；
    /// - **成功按序号收口**：只认「当前最新」那个写序号与修订号的结果，
    ///   更旧的一律丢弃（见 `drain_segment_writes` 与 `BackgroundWriteTracker`）；
    /// - 本次快照已经被更新的编辑取代（`revision` 变了）时，失败不再置脏：
    ///   更新的内容本来就要重新走一遍落库，置脏只会让旧快照再写一次。
    ///
    /// 调用点：定时去抖（`flush_segments_if_due_at`）、同步落库前（`flush_segments_if_dirty`）、
    /// 登记待落库时（`defer_segments_flush`：高频编辑也会顺手把结果收掉，
    /// 失败提示因此最迟在下一次交互就出现，不必等 1.2s 的定时器）。
    pub fn poll_background_writes(&mut self) {
        // 快照必须在排空之前取：`drain_segment_writes` 用它与每条回报的「派发时提示条」
        // 比对，判定这条回报是否已经被别的写库改动过（见该方法文档）。
        let pre_drain_error = self.db_write_error.clone();
        let done = self.segments_writes.drain_results();
        if done.is_empty() {
            return;
        }
        drain_segment_writes(
            &mut self.db_write_error,
            &mut self.segments_dirty,
            self.segments_revision,
            pre_drain_error.as_deref(),
            &done,
        );
    }

    /// 给后台线程的过闸写字幕入口（转写收尾回写上一个工程的脏字幕）。
    ///
    /// 只暴露 `&SegmentWriteGate`（内部是 `Database` 的克隆 + `Arc<BackgroundWrites>`）：
    /// 后台线程因此拿不到 `AppState` 的其它字段，也就没有「后台改界面状态」的可能。
    /// 调用点在派发前还要拿一个「当时的序号」（`next_write_seq`）一起带过去。
    pub(crate) fn segments_write_gate(&self) -> &SegmentWriteGate {
        &self.segments_write_gate
    }

    /// 下一个要发放的写序号（只读快照，不领号）。
    ///
    /// 给转写收尾这类「在后台线程上写库」的调用点用：它不能自己 `fetch_add`
    /// （`next_seq` 的单调性靠「领号只在渲染线程」这个前提），于是带上派发那一刻的
    /// 待发号。过闸判定是「同工程水位 >= 该号」，因此旧收尾写遇到更新的写只会被跳过。
    pub(crate) fn next_write_seq(&self) -> u64 {
        self.segments_writes.tracker.next_seq.load(Ordering::SeqCst)
    }

    /// 是否还有后台字幕写在飞（诊断 / 测试用；关窗守卫看的是 `segments_dirty`）。
    pub fn background_segments_write_pending(&self) -> usize {
        self.segments_write_pending.load(Ordering::SeqCst)
    }

    /// 给后台写线程的一致快照（当前主键 + 序列化好的整份字幕 + 修订号）。
    ///
    /// 这里只做一次 `Vec<Segment>` 克隆：共享借用会让编辑与后台写交错，不可取；
    /// 克隆是浅层复制，换来的是序列化 + sqlite UPDATE 全部离开渲染线程。
    ///
    /// 没有主键（尚未落库）时返回 `None`：退化成按路径更新会改掉同路径的其它历史记录。
    ///
    /// 序列化**不在这里做**：它和 sqlite UPDATE 一起交给后台线程（`write_segments_to_db`），
    /// 这里只截一份一致的字幕快照。这样 5000 句工程的序列化开销（JSON 几十毫秒量级）
    /// 也不再压渲染线程。
    pub(super) fn segments_write_snapshot(&self) -> Option<SegmentWriteSnapshot> {
        let task_id = self.active_task_id?;
        let json = match serde_json::to_string(&self.segments) {
            Ok(json) => json,
            Err(err) => {
                // `Segment` 的字段都能被 `serde_json` 表达，这条路实际到不了。
                // 真到了说明内存里的内容已经不是本程序写进去的形状，重试一万次也还是
                // 同样的结果——只留日志，不派发（派发出去必然失败），也不动脏标记。
                tracing::warn!(error = %err, "字幕序列化失败，本次不派发后台写");
                return None;
            }
        };
        Some(SegmentWriteSnapshot {
            task_id,
            revision: self.segments_revision,
            json,
        })
    }

    /// 去抖路径专用：把当前快照**派发**给后台写线程（立刻返回，不阻塞渲染线程）。
    ///
    /// # 与同步版的分工
    ///
    /// - `flush_segments_if_dirty()` 给「换工程 / 导出 / 关窗守卫 / 翻译收尾」用：
    ///   返回时必须已经写进库（它还会把这里在飞的写等回来，见该函数与
    ///   `SEGMENTS_SYNC_FLUSH_WAIT`）；
    /// - 本函数给「停手 1.2s」的去抖路径用：只负责派发，
    ///   结果在 `poll_background_writes` 里收（写失败同样会挂 `db_write_error`）。
    ///
    /// # 脏标记：派发**不清**，落库被确认后才清
    ///
    /// 脏标记的语义是「当前内容还没落库」。派发只是把快照交给后台线程、还没写完，所以这里
    /// **保留** `segments_dirty`，等后台回报确认落库后在 `apply_segment_write_result` 里
    /// 清掉（只有 `revision` 仍是当前那一份才算）。
    ///
    /// # 为什么不能「派发即清」（本轮修的真实漏洞）
    ///
    /// 旧实现在派发成功那一刻就清脏，于是「改完 → 去抖派发 → 用户立刻关窗」这条链上，
    /// `flush_segments_before_close` 读到的 `segments_dirty` 是 `false`：它既不会做同步落库，
    /// 也不会因为在飞写的失败而拦住关窗。若那份在飞写恰好失败（磁盘满 / 库被锁），失败回报
    /// 要等下一次 poll 才被看到——而窗口已经关了，改动**静默丢失**。
    ///
    /// 不清脏会不会让定时器每个窗口重复派发同一份内容？不会：派发前已把
    /// `segments_flush_due` 置空，`flush_segments_if_due_at` 因此不会再次进来；这条路径
    /// 只由「一次新的编辑重新登记」触发。
    ///
    /// 返回「现在是否有一份后台写在飞」。调用方（`flush_segments_if_due_at`）据此决定要不要
    /// 重新登记 `segments_flush_due`：在飞时脏标记是刻意保留的，不能当成「派发失败」反复登记。
    pub fn flush_segments_if_dirty_async(&mut self) -> bool {
        if !self.segments_dirty {
            return false;
        }
        self.segments_flush_due = None;
        match self.segments_write_snapshot() {
            Some(snapshot) => match self.dispatch_segment_write(snapshot) {
                // 已交给后台：保留脏标记，等回报确认落库。
                SegmentDispatch::Dispatched => true,
                // 没起后台线程、但同步兜底已经写进去了：等同已保存，直接清脏。
                SegmentDispatch::Saved => {
                    self.segments_dirty = false;
                    false
                }
                // 兜底同步写也失败：错误已挂上提示条，脏标记保持（与旧实现一致）。
                SegmentDispatch::Failed => false,
            },
            // 没有主键：没有可写回的行，等同已保存（与同步版一致），脏标记直接清掉。
            None => {
                self.segments_dirty = false;
                false
            }
        }
    }

    /// 把一份字幕快照交给后台写线程；返回这一份的落库结局（见 [`SegmentDispatch`]）。
    ///
    /// # 防回退：派发前先按**同工程水位**检查
    ///
    /// 查水位、领序号都在 `db_write_writer` 锁里完成，和同步落库（也持有这把锁）
    /// 是互斥的，于是各种顺序都自洽：
    ///
    /// - 同步写先完成（水位被推高）：本快照的序号比水位旧，直接不派发——它写进去
    ///   就是**回退**，而库里已经有更新的版本；
    /// - 本快照先派发：序号比水位新，正常入队（后台线程写库前会再查一次水位，
    ///   挡住「排队等闸期间同步写 / 更新的后台写插进来」那种情形，见
    ///   [`write_snapshot_under_gate`]）。
    ///
    /// 派发失败（线程起不来）不静默：退回**过闸的同步写**，写不进去才挂提示条并把
    /// 脏标记放回，下一次去抖（或关窗守卫的同步落库）会重试。
    ///
    /// 返回三态而不是布尔：`Saved`（当场写完，调用方应清脏）与 `Dispatched`（交给后台、
    /// 尚未确认）对 `segments_dirty` 的处理相反，混成一个 `true` 就没法让脏标记钉在
    /// 「确认落库才清」上。
    pub(super) fn dispatch_segment_write(
        &mut self,
        snapshot: SegmentWriteSnapshot,
    ) -> SegmentDispatch {
        let writes = Arc::clone(&self.segments_writes);
        // 派发侧与写线程各持一份：`pending` 归派发侧记账（见 `InFlightGuard`），
        // `worker_pending` 交给线程在收尾时 -1。失败分支没有线程，还得自己还回去。
        let pending = Arc::clone(&self.segments_write_pending);
        let worker_pending = Arc::clone(&self.segments_write_pending);
        let worker_wait = Arc::clone(&self.segments_write_wait);
        // 写线程自己再拿一份 `Arc`：`writes` 会被 move 进闭包，派发侧的失败分支还要用。
        let worker_writes = Arc::clone(&self.segments_writes);
        let db = self.db.clone();
        // 快照会被 move 进闭包，失败分支只剩「当前渲染线程上的内容」可写；
        // 把主键先抄出来，兜底分支同样要按工程记水位。
        let task_id = snapshot.task_id;
        let seq = {
            let _guard = writes.lock_writer();
            let next = writes.tracker.next_seq.load(Ordering::SeqCst);
            if writes.tracker.applied_seq(task_id) >= next {
                // 同一工程已经有更新的内容落库：这份快照写进去就是回退。直接算
                // 「已保存」——它描述的内容本来就来自水位之前的状态。
                //
                // 老实说：在「领号只在渲染线程、且严格单调」的现状下这条判据**不会**
                // 触发（水位一定小于下一个待发号）。留着它是因为它是「同工程不许回退」
                // 这条规则的完整表述：以后若有人再加一条派发路径，这里就是派发侧唯一
                // 的拦截点；真正兜底的是落库时的 `write_snapshot_under_gate`。
                tracing::debug!(task_id, "同工程已有更新的写落库，放弃派发这次后台写");
                return SegmentDispatch::Saved;
            }
            let seq = writes.tracker.next_seq.fetch_add(1, Ordering::SeqCst);
            writes.in_flight_writes.fetch_add(1, Ordering::SeqCst);
            // 「在飞」从**派发**这一刻算起：线程还没开跑时结算方也不能漏掉它。
            pending.fetch_add(1, Ordering::SeqCst);
            seq
        };
        let done = SegmentWriteDone {
            seq,
            error_at_dispatch: self.db_write_error.clone(),
            revision: snapshot.revision,
            task_id: snapshot.task_id,
            result: Ok(()),
        };
        let spawn = std::thread::Builder::new()
            .name("voice2word-segments-write".to_string())
            .spawn(move || {
                // 计数器在闭包退出时自动 -1（`InFlightGuard` 的 Drop）：
                // 正常返回 / 提前放弃 / panic 三种结局都不会把它挂住。
                let in_flight = InFlightGuard {
                    counter: worker_pending,
                    wait: worker_wait,
                };
                let write_guard = worker_writes.lock_writer();
                // 持闸写库：闸里一次完成「查同工程水位 -> 写 -> 记水位」，
                // 因此不可能出现「更旧的快照在更新的内容之后落地」这种回退。
                let result = write_snapshot_under_gate(&worker_writes, &db, &snapshot, seq)
                    .map(|_| ())
                    .map_err(|err| err.to_string());
                drop(write_guard);
                let sent = worker_writes
                    .submit
                    .send(SegmentWriteDone { result, ..done })
                    .is_ok();
                // 诊断计数器（在飞快照数）这次写已经结束，无论结果有没有送出去都 -1：
                // 它只用于观测，不能像 `pending` 那样把「结果已进通道」当成归零条件
                // ——那会让它随着使用时长单调上涨，变成一个骗人的数。
                worker_writes
                    .in_flight_writes
                    .fetch_sub(1, Ordering::SeqCst);
                if sent {
                    // 结果已进通道：这一份可以算「不在飞」了（同步落库等的是它落地）。
                    in_flight.disarm();
                }
            });
        match spawn {
            Ok(_handle) => SegmentDispatch::Dispatched,
            Err(spawn_err) => {
                // 线程都没起来：退回同步写（写的是渲染线程上这份最新内容）。
                // 退路也必须过闸、也要记水位——否则这次「成功」在账本上不存在，
                // 更旧的在飞快照醒来后照样能把新内容盖回去。
                tracing::warn!(error = %spawn_err, "后台写线程启动失败，改为同步写库");
                // 没有线程来收尾：派发时记的那两个「在飞」计数都由这里还回去，
                // 否则关窗守卫会一直以为有一份写没落地。
                pending.fetch_sub(1, Ordering::SeqCst);
                // 这条计数也要唤醒停车位：没有线程来收尾，等的人只能靠这里知道。
                self.segments_write_wait.notify();
                writes.in_flight_writes.fetch_sub(1, Ordering::SeqCst);
                let fallback = {
                    let _guard = writes.lock_writer();
                    if writes.tracker.applied_seq(task_id) >= seq {
                        Ok(())
                    } else {
                        match self.sync_segments_to_db() {
                            Ok(()) => {
                                writes.tracker.note_applied(task_id, seq);
                                Ok(())
                            }
                            Err(err) => Err(err),
                        }
                    }
                };
                match fallback {
                    // 兜底已经写进去了：脏标记交给调用方按「已保存」清掉，不再谎报失败。
                    Ok(()) => SegmentDispatch::Saved,
                    Err(err) => {
                        self.segments_dirty = true;
                        self.record_db_write_error(format!(
                            "字幕保存失败：{err}（后台写线程启动失败：{spawn_err}）"
                        ));
                        SegmentDispatch::Failed
                    }
                }
            }
        }
    }

    /// 将编辑产生的脏字幕落库（同步；幂等，未脏时零开销）
    ///
    /// 换工程 / 导出 / 关窗守卫 / 结构性编辑（拆分合并删除）都走这条**立刻写入**的路；
    /// 高频交互走 `AppState::defer_segments_flush`（迟到 1.2s 后由
    /// `flush_segments_if_dirty_async` 派发到后台线程）。
    ///
    /// # 与后台写的关系（旧语义必须保住）
    ///
    /// 调用点要的是「**返回时数据已经在库里**」。后台路径（去抖）只负责派发，返回时写
    /// 可能还在飞——所以这里先等它们落地（上限 `SEGMENTS_SYNC_FLUSH_WAIT`），再自己写
    /// 当前快照。
    ///
    /// 等待超时也**不会丢写、不会回退**：本次写和后台写共用同一把写入闸，且都按
    /// **同工程水位**收口（见 [`Self::sync_segments_gated`] 与 [`write_snapshot_under_gate`]）。
    /// 超时只决定「什么时候拿到闸」，不决定最终库里是谁的内容——拿到闸更晚的那一份
    /// 一定更新，更旧的快照醒来后会看到水位比自己新而直接放弃。
    ///
    /// 顺手作废「待落库」登记：登记留着的话，定时器稍后还会把同一份（或更旧的）内容再写一遍。
    pub fn flush_segments_if_dirty(&mut self) {
        // 先把后台的回报收进 `db_write_error`：另一条入口只是定时器去抖，界面上就指望
        // 这里把「后台写失败」变成可见提示。不收的话一次失败要等到下一次交互才显示。
        self.poll_background_writes();
        // 等待是**优化**，不是顺序保证：计数从派发那一刻算起（见 `InFlightGuard`），
        // 所以这里等的是「已派发还没收尾」的写；即便超过上限不再等，最终顺序仍由
        // 写入闸 + 同工程水位决定（旧快照醒来会看到水位比自己新而放弃）。
        let pending = Arc::clone(&self.segments_write_pending);
        if pending.load(Ordering::SeqCst) > 0 {
            let deadline = std::time::Instant::now() + SEGMENTS_SYNC_FLUSH_WAIT;
            // 停车等唤醒而不是 2ms 轮询：后台写收尾（`InFlightGuard` Drop）或派发失败
            // 路径归零计数时会立刻叫醒这里；等待期间渲染线程不空转。
            // 上限语义不变：超时只决定「什么时候拿到闸」，最终顺序仍由写入闸 +
            // 同工程水位决定（见本函数文档）。
            self.segments_write_wait.wait_until(&pending, deadline);
            let still = pending.load(Ordering::SeqCst);
            if still > 0 {
                tracing::warn!(
                    pending = still,
                    "后台字幕写未在期限内收尾，同步落库不再等它（写入闸会保证落地顺序）"
                );
            }
        }
        // 等完之后再收一次后台回报：等待期间到货的结局若留在通道里，`segments_dirty` 就还是
        // 「派发时保留」的旧值——成功不置回会让下面白写一遍，失败不置回更糟：这次「同步落库」
        // 会把一份其实没落盘的改动当成已保存放行（关窗守卫的盲区）。先收干净，再决定要不要自己写。
        self.poll_background_writes();
        self.segments_flush_due = None;
        if self.segments_dirty {
            self.segments_dirty = false;
            // 写库失败不能静默：用户以为改动已保存，重启后却发现全丢了。
            // 记进 `db_write_error`，界面把它渲染成可见的提示条。
            // 没有主键（导入字幕的工程）时 `map` 得到 `None`：那**不是**「已保存」，
            // 而是「无处可写」。以前这里返回 Ok 且保持 dirty=false，于是关窗守卫读到
            // 「已保存」直接放行——一整份校对静默清零。现在保持脏，交给守卫做二次确认。
            let result = self
                .active_task_id
                .map(|task_id| self.sync_segments_gated(task_id));
            match result {
                None => {
                    self.segments_dirty = true;
                    self.db_write_error = None;
                }
                Some(Ok(())) => self.db_write_error = None,
                Some(Err(err)) => {
                    tracing::warn!(error = %err, "字幕写库失败，改动尚未持久化");
                    self.record_db_write_error(format!("字幕保存失败：{err}"));
                    // 写失败必须把脏标记重新置回：否则下面关窗守卫读到的
                    // `segments_dirty` 是「已保存」，直接放行关闭窗口，用户重启后
                    // 改动全丢（与 db_write_error 提示条同时存在，自相矛盾）。
                    // 保持脏 → 后续 flush 会重试，守卫也仍会拦截关闭。
                    self.segments_dirty = true;
                }
            }
        }
    }

    /// 同步落库的「过闸」版本：领号 -> 写 -> 记水位，全程持有写入闸。
    ///
    /// 为什么必须持闸写完：后台写线程也是在持闸期间写库的（见 `dispatch_segment_write`
    /// 里的 worker）。两者合起来让「谁更晚拿到闸」成为唯一顺序；配合同工程水位
    /// （[`BackgroundWriteTracker::applied`]），任意交错下库里留下的都是序号最大的那份，
    /// 不会出现「更旧的快照后落地」把新内容盖掉。
    fn sync_segments_gated(&self, task_id: i64) -> anyhow::Result<()> {
        let writes = &self.segments_writes;
        let _guard = writes.lock_writer();
        let seq = writes.tracker.next_seq.fetch_add(1, Ordering::SeqCst);
        if writes.tracker.applied_seq(task_id) >= seq {
            // 同一工程已经有更新的写落库（理论上到不了：同步写总是领最新的号），
            // 本次要写的内容反而更旧，放弃——算成功，绝不拿旧内容回退。
            tracing::debug!(task_id, seq, "同工程已有更新的写落库，放弃这次同步写");
            return Ok(());
        }
        // 写的是 `self.segments`（渲染线程上这份最新内容），与快照路径同源。
        self.sync_segments_to_db()?;
        // 记水位就够了：同工程里更早派发的回报会在 `drain_results` 看到
        // `applied > seq` 而被当成过期结论丢掉——用户刚存好的东西不该被一条
        // 迟到的旧失败说成「又坏了」。
        writes.tracker.note_applied(task_id, seq);
        Ok(())
    }

    /// 登记「稍后落库」：高频交互节点用它代替立刻写库（见 [`SEGMENTS_FLUSH_IDLE`]）。
    ///
    /// 不脏就不登记：空登记只会让定时器白白醒来查一次。
    ///
    /// 顺手收一次后台写的回报：编辑是最频繁的动作，失败提示因此最迟在下一次敲键 /
    /// 切句就出现，不必干等 1.2s 的定时器。
    pub fn defer_segments_flush(&mut self) {
        self.poll_background_writes();
        if self.segments_dirty {
            self.segments_flush_due = Some(std::time::Instant::now());
        }
    }

    /// 停手满 [`SEGMENTS_FLUSH_IDLE`] 就把待落库的改动交给后台写线程；
    /// 返回「这次真的派发了一次写」。
    ///
    /// # 为什么这里能返回 `true` 却还没写完
    ///
    /// 去抖路径的语义是「别再拖了、让它去写」，不是「已经写完」——写库本身
    /// （序列化 + sqlite UPDATE，5000 句约 10~30ms）已经挪到后台（见
    /// `flush_segments_if_dirty_async`）。返回值只用来驱动界面重绘。
    /// 需要「写完才继续」的调用点走的是同步版 [`Self::flush_segments_if_dirty`]。
    ///
    /// 写失败时后台回报会在 `poll_background_writes` 里把 `segments_dirty` 置回
    /// （提示条同时挂上 `db_write_error`）。
    ///
    /// # 失败**不会**被这条定时器自动重试（现状，别被旧注释骗了）
    ///
    /// 派发之后登记位已经被本函数清掉，而这里只有「这次没派发出去（序列化失败）」
    /// 才会重新登记——后台线程写失败**不会**重新登记。也就是说：一份内容写库失败后，
    /// 下一次重试发生在**用户下一次编辑**（`defer_segments_flush` 会重新登记）或
    /// 下一次同步落库（关窗守卫 / 换工程 / 导出）时。这是刻意的取舍：自动重试要
    /// 每秒重试一次，而提示条是按「；」追加的，持久性故障（磁盘满 / 库被外部进程锁住）
    /// 会把提示条刷成几百条重复文案，还会让渲染线程每分钟白付几十次整份序列化。
    /// 关窗守卫仍然拦着「脏着退出」，所以失败不会静默地丢数据。
    pub fn flush_segments_if_due(&mut self) -> bool {
        self.flush_segments_if_due_at(std::time::Instant::now())
    }

    /// [`Self::flush_segments_if_due`] 的可注入时钟版本（单测直接给时刻，不睡真实时间）。
    ///
    /// 返回值除了「这次真的派发了一次写」，还包括「后台回报刚刚改动了要显示的字段」
    /// （提示条 / 脏标记）：调用方（界面定时器）只按这个返回值决定要不要重绘，
    /// 而失败提示正是在这里被收进来的——不把这种变化报出去，用户不碰鼠标就看不到
    /// 「保存失败」的提示（状态早就更新了，界面却一直没重画）。
    pub fn flush_segments_if_due_at(&mut self, now: std::time::Instant) -> bool {
        let error_before_poll = self.db_write_error.clone();
        let dirty_before_poll = self.segments_dirty;
        // 到点与否都得先把后台回报收掉：否则失败置回的脏标记要等到下一个窗口才看得到，
        // 而那个窗口又可能因为「还没到点」直接返回，白拖一轮。
        self.poll_background_writes();
        let poll_changed_ui =
            self.db_write_error != error_before_poll || self.segments_dirty != dirty_before_poll;
        let Some(due) = self.segments_flush_due else {
            return poll_changed_ui;
        };
        if now.saturating_duration_since(due) < SEGMENTS_FLUSH_IDLE {
            return poll_changed_ui;
        }
        if !self.segments_dirty {
            // 期间已经被别的路（换工程 / 关窗 / 导出 / 后台写已完成）写过：登记作废，别一直挂着。
            self.segments_flush_due = None;
            return poll_changed_ui;
        }
        let dispatched = self.flush_segments_if_dirty_async();
        if self.segments_dirty && !dispatched {
            // 没派发出去（序列化失败 / 同步兜底也失败）：重新登记，下一个窗口再试。
            // `dispatched == true` 说明脏标记是「后台写在飞、还没确认落库」时被刻意保留的，
            // 不能当成失败反复登记（否则会退化成每 1.2s 重派发一次的刷屏重试）。
            self.segments_flush_due = Some(std::time::Instant::now());
        }
        true
    }

    /// 追加一条写库失败提示（`db_write_error`）。
    ///
    /// 已有提示时用「；」拼接而非覆盖：一次操作里可能先后失败两次（例如「上一个
    /// 工程的 flush 失败」+「当前转写结果入库失败」），只留最后一条会让用户以为
    /// 只有一处出了问题，去排查时漏掉另一条。成功落库时调用方仍会把它清成 `None`。
    pub fn record_db_write_error(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        self.db_write_error = Some(match self.db_write_error.take() {
            Some(prev) => format!("{prev}；{msg}"),
            None => msg,
        });
    }

    /// 标记字幕内容已变化（驱动清单搜索缓存失效）
    pub fn bump_segments_revision(&mut self) {
        self.segments_revision = self.segments_revision.wrapping_add(1);
    }

    /// 同步当前字幕到 SQLite 数据库。
    ///
    /// # 为什么返回 Result 而不是吞掉
    ///
    /// 此前这里是 `let _ = ...`，写库失败被完全静默。后果是**用户以为保存了**：
    /// 在剪辑台改了半天字幕、切走页面（触发 flush）、关掉程序，重启后发现改动
    /// 全没了——而界面上从未出现任何异常。磁盘满、数据库被锁、文件只读都会走到
    /// 这条路径。
    ///
    /// 现在把失败交给调用方（lush_segments_if_dirty 会把它挂到界面的提示条上）。
    /// 注意：**不重试**。写失败通常是持久性原因（磁盘满 / 只读），立刻重试只会
    /// 在每次 flush 时再失败一次；提示用户才是有效的处置。
    pub fn sync_segments_to_db(&self) -> anyhow::Result<()> {
        // 按当前工程的数据库主键写回。没有主键（尚未落库）时什么都不做：
        // 退化成按路径更新会把同路径的历史记录一起改掉。
        match self.active_task_id {
            Some(id) => self.db.update_task_segments(id, &self.segments),
            None => Ok(()),
        }
    }
}

/// 在**已持有写入闸**的前提下把一份快照写进库（防回退的最后一道）。
///
/// 只用 `Database` 已有的入口：`update_task_segments_json` 接收已经序列化好的
/// `segments_json`，自己解析一次算派生列（样例文本 / 句数 / 有无译文）并刷新 `duration`，
/// 所以这里不重复造轮子。后台线程因此不需要 `Vec<Segment>` 的所有权，快照可以是纯 String。
///
/// 本函数只拿到 `Database`（内部是 `Arc<Mutex<Connection>>`，天生可跨线程）与一个纯值快照，
/// 拿不到 `AppState`；调用方负责持闸，因此它同时被后台写线程与同步落库复用。
///
/// 顺序判定只用一个事实：`applied`（每个工程已经真正落库的最大写序号）。
/// 闸里先查后写，所以「查」与「写」之间不可能插进另一份写：
///
/// - `applied[task] >= seq`：同一工程已经有更新的快照落库，本次写进去就是**回退**，
///   直接放弃并返回 `false`。返回成功而不是错误：库里确实有更新的内容，
///   报错只会让界面把一份已经写完的最新内容重新标脏、再派发一轮；
/// - 否则真写，并在**写成功之后**把水位推到自己：失败时水位不动，后面那份更新的
///   快照照样会写。
///
/// 调用方必须持有 `BackgroundWrites::lock_writer()`（本函数要用这把锁保证
/// 「查水位 + 写 + 记水位」是一个原子段）。
pub(super) fn write_snapshot_under_gate(
    writes: &BackgroundWrites,
    db: &Database,
    snapshot: &SegmentWriteSnapshot,
    seq: u64,
) -> anyhow::Result<bool> {
    if writes.tracker.applied_seq(snapshot.task_id) >= seq {
        tracing::debug!(
            task_id = snapshot.task_id,
            seq,
            "同工程已有更新的写落库，放弃这次写入"
        );
        return Ok(false);
    }
    db.update_task_segments_json(snapshot.task_id, &snapshot.json)?;
    writes.tracker.note_applied(snapshot.task_id, seq);
    Ok(true)
}

/// 后台线程也能用的「过闸写字幕」入口（转写收尾专用）。
///
/// # 为什么要有这个结构，而不是让调用点传 `Arc<AppState>`
///
/// 转写收尾（`finish_db_work`）跑在后台线程上，它要把**上一个工程**的脏字幕写库。
/// 那条路径过去直接 `db.update_task_segments(id, segs)`：既没领写序号、也没按工程查水位，
/// 于是后台在飞的字幕写与它撞同一个 task 时，**旧内容可能覆盖新内容**——与写入闸要防的
/// 正是同一件事（见 `BackgroundWrites` 的文档）。
///
/// 修法不是「把 `AppState` 塞进后台线程」：`AppState` 是渲染线程按帧 `&mut` 借用的，
/// 搬进去就得给整个界面状态上锁。真正需要的只有两样东西——`Database`（内部
/// `Arc<Mutex<Connection>>`，天生可 `Send + Sync`）与按工程的水位账本。把这两样
/// 打成一个小句柄 `SegmentWriteGate`，后台线程只拿它，规则仍然只有一份
/// （`write_snapshot_under_gate`），不复制一套平行实现。
///
/// # 序号为什么由调用方给，而不是这里领
///
/// 收尾路径在后台线程上，而 `next_seq` 的单调性依赖「领号只在渲染线程」这个前提
/// （派发侧与同步写都在渲染线程上领号）。所以这里不自己 `fetch_add`，而是让调用点
/// 带上它那份快照的序号：派发那一刻（渲染线程）从 `next_write_seq` 取一个当时值即可，
/// 水位判定（`applied[task] >= seq`）因此照样成立——旧收尾写遇到更新的写只会被跳过。
#[derive(Clone)]
pub(crate) struct SegmentWriteGate {
    pub(super) db: Database,
    pub(super) writes: Arc<BackgroundWrites>,
}

impl SegmentWriteGate {
    /// 把一份**上一工程**的脏字幕快照过闸写库；返回「这次真的写了没有」。
    ///
    /// 全程持有写入闸，闸里一次完成「查同工程水位 → 写 → 记水位」，与同步落库
    /// （`AppState::sync_segments_gated`）和后台写线程共用同一把锁、同一个账本。
    pub(crate) fn write_finished_task_segments(
        &self,
        task_id: i64,
        seq: u64,
        segments: &[crate::subtitle::Segment],
    ) -> anyhow::Result<bool> {
        let _guard = self.writes.lock_writer();
        if self.writes.tracker.applied_seq(task_id) >= seq {
            // 同一个工程已经有更新的内容落库：这次写回去就是回退。算成功——库里
            // 那份更新的内容本来就是用户最后看到的状态。
            tracing::debug!(
                task_id,
                seq,
                "同工程已有更新的写落库，放弃转写收尾的脏字幕回写"
            );
            return Ok(false);
        }
        self.db.update_task_segments(task_id, segments)?;
        self.writes.tracker.note_applied(task_id, seq);
        Ok(true)
    }
}

/// 把后台写线程的回报并进 UI 状态（**纯函数**，只在渲染线程上跑，便于单测钉死判定）。
///
/// # 判定规则（防回退的核心）
///
/// 一条回报只有在**两件事同时成立**时才算数：
///
/// （调用方 `BackgroundWrites::drain_results` 已经先筛掉「同工程已有更新的写落库」的
/// 过期回报，这里不再重复判水位——水位是按工程算的，判定只留一处，免得两处漂移。）
/// 2. `pre_drain_error`（**本轮排空之前**的 `db_write_error`；调用方在调用前取快照）
///    与这条回报的「派发时提示条」一致——否则说明后台写这段时间里用户别的落库也
///    失败 / 成功过，那是**更新**的事实，不能被这次结局回写提示条。用调用方给的快照
///    而不是 `db_write_error` 的当前值：同一轮里可能同时收到失败与成功两条，若拿已被
///    前一条改过的字段去比后一条的派发快照，后一条会被误判成「别人改过」整条丢掉；
/// 3. 这份快照仍然是当前内容（`revision` 没变）或者这次写**成功**。旧快照的失败
///    不再置脏：用户已经又改过，新的内容会自己走一遍落库，置脏只会让定时器把那份
///    旧内容再派发一次（而它注定写不进最新状态）。
///
/// 成功：把提示条清空（与同步版 flush 成功一致），并在快照仍是当前修订时**清掉**
/// `segments_dirty`——脏标记从「派发即清」改成「确认落库才清」，收口就在这里
/// （见 `flush_segments_if_dirty_async`）。
/// 失败：追加「字幕保存失败：…」；**只有**这条回报本身就是**同工程**最新的一条
/// **且** `revision` 还是当前的才把 `segments_dirty` 置回，
/// 让定时器 / 关窗守卫重试。
///
/// # 为什么置脏还要看「是不是最新一条」
///
/// 同一轮排空里可能同时收到一份旧快照的失败与一份更新的成功。旧失败若也把脏标记
/// 置回，界面会显示「有未保存改动」并要求再写一遍——而库里那一段内容其实已经被
/// 更新的成功写进去了。这与「旧快照的失败不回写当前状态」是同一条原则。
///
/// # 提示条竞价输了的失败：提示条不动，但脏标记必须补上
///
/// 第 2 条只否定「提示条该不该回写」，不该顺手否定「这份内容还没落库」这个事实。
/// 一条落在**当前修订**上的失败如果既不挂提示、也不置脏，用户就永远看不到它、也不会
/// 有任何一条路径去重试——用户改的东西一直躺在内存里，等到关窗时的同步落库来救。
/// 所以竞价输了的失败仍然按修订号补脏（只补脏，不碰提示条）。
///
/// 「同工程已经有更新的写落库」这种过期回报由调用方（`drain_results`）先筛掉；本函数
/// 只判「这条回报该不该回写界面状态」，因此不需要再去比一个全局水位。
pub(super) fn drain_segment_writes(
    db_write_error: &mut Option<String>,
    segments_dirty: &mut bool,
    current_revision: u64,
    pre_drain_error: Option<&str>,
    done: &[SegmentWriteDone],
) {
    // 「本批里每个工程最新的一条」：脏标记只在**同工程**没有更新的结论时才补。
    // 写序号是跨工程共用的，但两个工程的内容互不覆盖——拿 A 的新序号去压 B 的失败，
    // 会让 B 的失败既不提示也不重试，而 B 的当前修订其实还没落库。
    let mut newest_by_task: HashMap<i64, u64> = HashMap::new();
    for write in done {
        let slot = newest_by_task.entry(write.task_id).or_insert(write.seq);
        *slot = (*slot).max(write.seq);
    }
    for write in done {
        if pre_drain_error != write.error_at_dispatch.as_deref() {
            // 派发之后又发生过别的失败 / 成功：**提示条**不能按这条回写（更新的事实
            // 已经占位）。但脏标记必须照修订号对齐——要么失败补上（见函数文档「提示条
            // 竞价输了的失败」），要么成功清掉（否则内容已落库、脏标记还挂着，关窗守卫会白等）。
            match &write.result {
                Err(_) => {
                    mark_segments_dirty_for_revision(
                        segments_dirty,
                        write.revision,
                        current_revision,
                    );
                }
                Ok(()) => {
                    if write.revision == current_revision {
                        *segments_dirty = false;
                    }
                }
            }
            tracing::debug!(
                seq = write.seq,
                "派发之后提示条已被别的写库改动过，本次后台写结局不再回写提示条"
            );
            continue;
        }
        apply_segment_write_result(
            db_write_error,
            segments_dirty,
            write,
            current_revision,
            newest_by_task
                .get(&write.task_id)
                .is_some_and(|newest| *newest == write.seq),
        );
    }
}

/// 一条后台写结局落到状态上的**唯一入口**（纯函数，供 `poll_background_writes` 与测试复用）。
///
/// `newest`：这条回报是不是同一轮里**同一个工程**的最新一条（调用方按工程分别取 max 判定）。
///
/// # 错误通道不静默（唯一的例外在调用方）
///
/// 失败会写进 `db_write_error`（与同步版同文案、同「；」拼接规则），界面提示条读的
/// 就是这个字段——后台线程与渲染线程之间没有任何别的错误通路。
///
/// 唯一的例外是「提示条竞价输给更新事实」的失败：调用方（[`drain_segment_writes`]）
/// 不会把它的文案挂上去，但会保留「这份内容还没落库」的脏标记。宁可少一条过期文案，
/// 也不能没有重试信号。
///
/// # 为什么「置脏」必须看修订号
///
/// `segments_dirty` 的语义是「**当前**内容还没落库」。后台写的是**过去某一刻**的快照：
/// 它失败时若直接把脏标记置回，而用户其实已经又改过（`segments_revision` 更大），
/// 定时器稍后会把那份**旧快照**当成当前内容派发一次——落进库的就不是用户最新改的东西。
/// 所以只有「失败的那份快照正好还是当前修订」才置脏；否则脏标记归更新的那份编辑负责，
/// 由下一次 `defer_segments_flush` / 去抖重新标记。
///
/// 同理，已经被同一轮里更新的一条越过（`newest == false`）的失败也不置脏：那段内容
/// 已经有更新的写落库了，再标脏只会让用户看到一次多余的「未保存」与一次无谓的重写。
pub(super) fn apply_segment_write_result(
    db_write_error: &mut Option<String>,
    segments_dirty: &mut bool,
    write: &SegmentWriteDone,
    current_revision: u64,
    newest: bool,
) {
    match &write.result {
        Ok(()) => {
            *db_write_error = None;
            // 内容已确认落库，脏标记的语义（「当前内容还没落库」）到此为止。派发时不再清脏
            // （见 `flush_segments_if_dirty_async`），清脏的责任就落在这里——只有快照仍是当前
            // 修订才算数；否则用户已经又改过，那份新内容会自己走一遍落库，别替它清。
            if write.revision == current_revision {
                *segments_dirty = false;
            }
        }
        Err(err) => {
            // 与同步版文案、拼接规则完全一致：界面提示条读的就是这个字段。
            *db_write_error = Some(match db_write_error.take() {
                Some(prev) => format!("{prev}；字幕保存失败：{err}"),
                None => format!("字幕保存失败：{err}"),
            });
            // 失败不能静默之外，还得**能重试**：把脏标记放回，让定时器 / 关窗守卫
            // 再走一遍。只对「落库时那份快照就是当前内容、且这条就是最新一条」的失败
            // 置回，理由见函数文档。
            if newest {
                mark_segments_dirty_for_revision(segments_dirty, write.revision, current_revision);
            }
        }
    }
}

/// 失败发生在旧修订上时，`segments_dirty` 该怎么处理（纯函数）。
///
/// - `queued_revision == current_revision`：这份快照仍是当前内容，放回脏标记以便重试；
/// - 否则：用户已经又改过（更新修订号），新的内容会自己走一遍落库，不要动脏标记。
pub(super) fn mark_segments_dirty_for_revision(
    segments_dirty: &mut bool,
    queued_revision: u64,
    current_revision: u64,
) -> bool {
    if queued_revision == current_revision {
        *segments_dirty = true;
        true
    } else {
        false
    }
}
