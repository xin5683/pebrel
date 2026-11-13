//! ConPTY 对账：把本地网格对齐到 conhost 的坐标系。
//!
//! 直通模式的 `ResizePseudoConsole` 不产生任何重绘字节，conhost 内部重排
//! 缓冲区留下的行漂移只能事后探测。首选按视口真实文本对账：只比较光标行
//! 不够，Claude Code 这类增量重绘 TUI 会在光标下方保留输入框边框和页脚行，
//! 两侧光标同行时内容仍可能相差数行——只按光标滚动会把整屏内容永久搬错行
//! （2026-11-12 缩放后输入错位事故，字节取证与本决策见
//! `architecture/notes/nebula_terminal/term/2026-11-12-conpty-content-realign.md`）。
//!
//! 文本探针不可用时退回只按光标行的 [`Term::conpty_realign`]；文本可用但
//! 证据不足时保持原样——宁可不修，也不盲滚。

use super::{Term, TermMode};
use crate::grid::Dimensions as _;
use crate::index::{Column, Line};
use crate::term::cell::Flags;

/// conhost 视口底部的一段真实文本，按内容对账的唯一真值。
///
/// 由 `event_loop` 的 `AttachConsole` 探针读取填充；`Term` 只消费快照，
/// 不关心它怎样获得。行号均为视口相对行（0 基，视口顶部为 0）。
#[derive(Clone, Debug)]
pub struct ConhostViewportText {
    /// conhost 视口高度（行数），必须与本地网格的 `screen_lines` 相等。
    pub rows: usize,
    /// conhost 报告的光标行。
    pub cursor_row: usize,
    /// `lines` 首行在视口中的行号。
    pub first_row: usize,
    /// 自 `first_row` 起逐行的视口文本，尾部空白已裁掉。
    pub lines: Vec<String>,
}

impl ConhostViewportText {
    /// 由探针读数构造快照，并校验它自洽。
    ///
    /// `lines` 必须是贴着视口底部的样本（`first_row + lines.len() == rows`）：
    /// 对账把“底部第几行”当作跨坐标系的行号锚，样本没到底的行号没有意义。
    /// 视口高度还没落地、读取中途失败等不自洽的读数返回 `None`。
    pub fn new(
        rows: usize,
        cursor_row: usize,
        first_row: usize,
        lines: Vec<String>,
    ) -> Option<Self> {
        if rows == 0 || cursor_row >= rows || lines.is_empty() {
            return None;
        }
        if first_row.checked_add(lines.len())? != rows {
            return None;
        }
        Some(Self { rows, cursor_row, first_row, lines })
    }
}

/// 采信位移所需的最小命中行数：两行偶然相同的横幅撑不起一次整屏滚动。
const MIN_SHIFT_MATCHES: usize = 3;

/// 命中文本的总字符数下限，过滤掉 `───` 之类重复出现的短行。
const MIN_SHIFT_CHARS: usize = 12;

impl<T> Term<T> {
    /// ConPTY 对账重锚：把主屏光标搬到 `target` 视口行。
    ///
    /// 只按光标行的兜底路径：[`Term::conpty_realign_to`] 拿不到文本真值时
    /// 才使用——它把“光标行差”当作“内容行差”，在光标下方还有内容的 TUI
    /// 里并不成立。
    ///
    /// 背景（字节级取证，2026-08）：`ResizePseudoConsole` 之后 conhost 的行
    /// 坐标与本地 reflow 的结果会朝两个方向偏。
    ///
    /// 一、塌缩（顶端对齐）：conhost 把自身缓冲区压成"视口高、零回滚、内容
    /// 顶端对齐"，光标停在内容末行——高于视口底；而本地 reflow 保留完整历史、
    /// 光标钉在视口底。此时 `target < cursor`，多出的顶部行滚入历史。
    ///
    /// 二、变宽/变窄导致的折行差（分屏、拖分界、面板开合）：conhost 的 buffer
    /// rewrap 与本地 reflow 的换行语义不同，同一批字节折出的行数可以差几行。
    /// conhost 折得更多时它的视口顶部还留着我们已经推进历史的行，提示符相应
    /// 更靠下，`target > cursor`；缺的行在光标上方补回，内容整体下移。
    ///
    /// 两个方向都必须处理：只修一边时另一边会让 PSReadLine 按 conhost 坐标
    /// 发出的绝对 CUP 永久落在错行——提示符停在一处，回显砸在几行之下
    /// （现场：分屏后 `prompts=[15]` 而 `cursor=19`）。该偏差在 conhost 内部
    /// 且非确定性，事前无法预测，conhost 也不重绘（直通模式零字节），唯一可靠
    /// 做法是事后向 conhost 要真值（`AttachConsole` +
    /// `GetConsoleScreenBufferInfo`，见 event_loop 的对账探针）并把本地网格滚
    /// 到同一坐标系。滚动后两侧光标同行，后续输出同步推进，永久对齐；被滚走
    /// 的行仍在历史里可回看。
    pub fn conpty_realign(&mut self, target: usize) {
        if self.mode.contains(TermMode::ALT_SCREEN) {
            return;
        }
        let cursor = self.grid.cursor.point.line.0;
        if cursor < 0 {
            return;
        }
        let cursor = cursor as usize;
        let delta = cursor.abs_diff(target);
        if delta == 0 || delta >= self.screen_lines() {
            return;
        }
        self.reset_input_cluster();
        let region = Line(0)..Line(self.screen_lines() as i32);
        // 选区跟着内容走：`rotate` 的正负与内容位移同向（上滚为负）。
        let rotation = if target < cursor { delta as i32 } else { -(delta as i32) };
        self.selection = self.selection.take().and_then(|s| s.rotate(self, &region, rotation));
        if target < cursor {
            self.grid.scroll_up(&region, delta);
            self.grid.cursor.point.line = self.grid.cursor.point.line - delta;
        } else {
            // 顶部补出的行是空的：被 conhost 留在视口里的那几行在我们这边
            // 已经进了历史，取不回来。视觉代价是几行空白，换来的是此后每一
            // 条绝对 CUP 都落在正确的行。
            self.grid.scroll_down(&region, delta);
            self.grid.cursor.point.line = self.grid.cursor.point.line + delta;
        }
        self.mark_fully_damaged();
    }

    /// 按 conhost 视口文本把本地网格对齐到同一坐标系。
    ///
    /// 位移定义为 `conhost 行 - 本地行`：正数把网格整体下移（顶部补空行），
    /// 负数上移（顶部行进入历史）。内容对账成立后光标直接采用 conhost 的
    /// 报告值——光标行才是字节流绝对 CUP 的锚，内容位置由它反推。
    ///
    /// 返回 `false` 表示证据不足、网格未动：一次错误的整屏滚动会把内容
    /// 永久搬错行，而放弃只保持现状，兜底探针还会再来。
    pub fn conpty_realign_to(&mut self, viewport: &ConhostViewportText) -> bool {
        if self.mode.contains(TermMode::ALT_SCREEN) {
            return false;
        }
        let screen_lines = self.screen_lines();
        if screen_lines < MIN_SHIFT_MATCHES
            || viewport.rows != screen_lines
            || viewport.lines.len() < MIN_SHIFT_MATCHES
        {
            return false;
        }

        let conhost: Vec<Option<String>> =
            viewport.lines.iter().map(|line| normalize_row(line)).collect();
        if conhost.iter().flatten().count() < MIN_SHIFT_MATCHES {
            return false;
        }
        let ours: Vec<Option<String>> =
            (0..screen_lines).map(|row| self.viewport_row(row)).collect();

        let Some(shift) = content_shift(&conhost, viewport.first_row, &ours) else {
            return false;
        };

        if shift != 0 {
            self.shift_screen(shift);
        }
        self.adopt_cursor_row(viewport.cursor_row);
        true
    }

    /// 视口 `row` 行的文本，按对账口径归一化；整行空白为 `None`。
    fn viewport_row(&self, row: usize) -> Option<String> {
        let line = &self.grid[Line(row as i32)];
        let mut raw = String::with_capacity(self.columns());
        for column in 0..self.columns() {
            let cell = &line[Column(column)];
            // 宽字符占位格与 conhost 的续格一样按空白计：两侧归一化后才会
            // 折出同一文本，占位格里残留的旧字符也不能参与比较。
            let c =
                if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    ' '
                } else {
                    cell.c
                };
            raw.push(c);
            if let Some(extra) = cell.zerowidth() {
                raw.extend(extra);
            }
        }
        normalize_row(&raw)
    }

    /// 整屏平移 `shift` 行：正数向下（顶部补空行），负数向上（顶部进历史）。
    fn shift_screen(&mut self, shift: i32) {
        let delta = shift.unsigned_abs() as usize;
        self.reset_input_cluster();
        let region = Line(0)..Line(self.screen_lines() as i32);
        // 选区跟着内容走：`rotate` 的正负与内容位移同向（上滚为负）。
        let rotation = if shift < 0 { delta as i32 } else { -(delta as i32) };
        self.selection = self.selection.take().and_then(|s| s.rotate(self, &region, rotation));
        if shift < 0 {
            self.grid.scroll_up(&region, delta);
        } else {
            self.grid.scroll_down(&region, delta);
        }
        self.mark_fully_damaged();
    }

    /// 采用 conhost 的光标行。
    fn adopt_cursor_row(&mut self, target: usize) {
        let target = Line((target as i32).min(self.screen_lines().saturating_sub(1) as i32));
        if self.grid.cursor.point.line == target {
            return;
        }
        // 待换行标志属于旧光标位置；搬了光标还留着它，下一个字符就会在
        // 错误的行上折行——正是这次事故要消灭的那类错位。
        self.grid.cursor.input_needs_wrap = false;
        self.grid.cursor.point.line = target;
        self.mark_fully_damaged();
    }
}

/// 在整行位移空间里找唯一可信的位移：`conhost 行 - 本地行`。
///
/// 每个候选位移给所有非空 conhost 行的**逐字相等**打分；最高分必须达到
/// 行数与字符数下限，且没有同分候选——同分意味着证据分辨不出真实位移。
fn content_shift(
    conhost: &[Option<String>],
    first_row: usize,
    ours: &[Option<String>],
) -> Option<i32> {
    let reach = ours.len().saturating_sub(1) as i64;
    let mut best: Option<(i32, usize, usize)> = None;
    let mut unique = true;
    for shift in -reach..=reach {
        let mut matches = 0usize;
        let mut chars = 0usize;
        for (index, row) in conhost.iter().enumerate() {
            let Some(row) = row else { continue };
            let our_row = first_row as i64 + index as i64 - shift;
            let Ok(our_row) = usize::try_from(our_row) else { continue };
            if ours.get(our_row).is_some_and(|our| our.as_deref() == Some(row.as_str())) {
                matches += 1;
                chars += row.chars().count();
            }
        }
        match best {
            Some((_, best_matches, _)) if matches < best_matches => {},
            Some((_, best_matches, _)) if matches == best_matches => unique = false,
            _ => {
                best = Some((shift as i32, matches, chars));
                unique = true;
            },
        }
    }

    let (shift, matches, chars) = best?;
    (unique && matches >= MIN_SHIFT_MATCHES && chars >= MIN_SHIFT_CHARS).then_some(shift)
}

/// 按对账口径归一化一行：连续空白压成一个空格、首尾空白裁掉；整行空白
/// 返回 `None`——空白行在哪一侧都能“匹配”，只配制造伪位移。
///
/// 宽字符续格在 conhost 侧可能是空格或 NUL，统一按空白合并，这样本地
/// 占位格与它折出同一文本。
fn normalize_row(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut pending_space = false;
    for ch in raw.chars() {
        if matches!(ch, ' ' | '\u{a0}' | '\t' | '\0') {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
    }
    if out.is_empty() { None } else { Some(out) }
}
