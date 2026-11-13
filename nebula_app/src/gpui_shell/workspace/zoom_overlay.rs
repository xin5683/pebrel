//! 聚焦 pane 的悬浮放大层。
//!
//! `WorkspaceTab.zoomed` 仍是唯一的逻辑状态；本模块只拥有它的瞬态表现：
//! 一次弹簧采样、一张 Windows 客户端快照（Ghost）和一个覆盖在分屏上的
//! 悬浮卡。终端网格在打开/关闭的第一帧就按最终几何布局，动画期间不再改
//! 真实 `TerminalView` 的 bounds——PTY 因此每个方向只 resize 一次。

use std::time::Instant;

use gpui::point;

use super::*;

/// 悬浮卡与终端卡内容区边缘的间距。放在这里而不是主题里：它描述的是
/// 「放大态」这一种布局，不是所有终端卡的通用几何。
const ZOOM_MARGIN: f32 = 40.0;
/// 背景分屏的压暗强度。太深会把分屏读成不可用，太浅又撑不起悬浮层级。
const BACKDROP_ALPHA: f32 = 0.42;
const STIFFNESS: f32 = 170.0;
const DAMPING: f32 = 18.0;
const SUBSTEP_SECONDS: f32 = 1.0 / 240.0;
const POSITION_EPSILON: f32 = 0.001;
const VELOCITY_EPSILON: f32 = 0.01;
/// 真实悬浮卡在飞行末段才淡入：飞行的空间感由缩放画面（截图或同色卡面）
/// 承担，卡片提前出现会和它形成双影，读起来像“突然出现一张大卡”。
const CARD_REVEAL: (f32, f32) = (0.82, 1.0);
const GHOST_MAX_EDGE: u32 = 4096;
const GHOST_MAX_BYTES: usize = 32 * 1024 * 1024;

struct Ghost {
    image: Arc<RenderImage>,
    /// paint 期 `paint_image` 失败时置位，下一帧由 `advance_zoom_motion`
    /// 释放纹理；失败不能每帧重试同一个不可用纹理。
    failed: Rc<Cell<bool>>,
}

pub(super) struct ZoomMotion {
    tab: usize,
    pane: u64,
    /// 0 = 旧分屏槽位，1 = 悬浮卡。弹簧可以过冲，渲染时按用途 clamp。
    position: f32,
    velocity: f32,
    target: f32,
    last_step: Instant,
    /// 打开时聚焦 pane 的槽位。窗口 resize 后作废，关闭退化为纯淡出。
    slot: Option<Bounds<Pixels>>,
    ghost: Option<Ghost>,
    /// 快照与槽位都按这一组几何取值；变化即失效。
    viewport: gpui::Size<Pixels>,
    scale: f32,
}

impl ZoomMotion {
    fn new(
        tab: usize,
        pane: u64,
        position: f32,
        target: f32,
        slot: Option<Bounds<Pixels>>,
        ghost: Option<Ghost>,
        viewport: gpui::Size<Pixels>,
        scale: f32,
        now: Instant,
    ) -> Self {
        Self {
            tab,
            pane,
            position,
            velocity: 0.0,
            target,
            last_step: now,
            slot,
            ghost,
            viewport,
            scale,
        }
    }
}

impl NebulaWorkspace {
    /// ctrl+shift+enter：聚焦 pane 悬浮放大开关（旧壳 ToggleZoom）。
    pub(super) fn toggle_zoom(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active = self.active;
        let Some(WorkspaceTab::Terminal { panes, focused, zoomed, .. }) = self.tabs.get(active)
        else {
            return;
        };
        if panes.len() < 2 {
            return;
        }
        let (pane, zoomed) = (*focused, !*zoomed);
        self.set_zoom(active, pane, zoomed, window, cx);
    }

    /// 放大态下要切到 `pane_id` 时，返回需要先还原的原 pane。
    /// 不先还原会直接改 `focused`，让浮层继续显示旧 pane、逻辑与画面分家。
    pub(super) fn zoom_pane_to_restore(&self, tab_ix: usize, pane_id: u64) -> Option<u64> {
        match self.tabs.get(tab_ix) {
            Some(WorkspaceTab::Terminal { panes, focused, zoomed: true, .. })
                if *focused != pane_id && panes.iter().any(|pane| pane.id == pane_id) =>
            {
                Some(*focused)
            },
            _ => None,
        }
    }

    /// 打开/关闭悬浮放大。返回逻辑状态是否真的发生了变化。
    ///
    /// 捕获发生在翻转状态之前：此时窗口仍显示旧构图（打开=分屏、关闭=
    /// 悬浮卡），Ghost 才裁得到正确的源画面。
    pub(super) fn set_zoom(
        &mut self,
        tab_ix: usize,
        pane_id: u64,
        zoomed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(tab) = self.tabs.get(tab_ix) else { return false };
        let WorkspaceTab::Terminal { panes, zoomed: current, focused, .. } = tab else {
            return false;
        };
        let (panes_len, pane_exists, current, focused) =
            (panes.len(), panes.iter().any(|pane| pane.id == pane_id), *current, *focused);
        if panes_len < 2 || !pane_exists {
            return false;
        }
        let now = cx.background_executor().now();
        let viewport = window.viewport_size();
        let scale = window.scale_factor();
        let reduced = cx.reduce_motion();
        if current == zoomed {
            if !zoomed || focused == pane_id {
                // 已经是请求的终态（包括动画途中重复请求同一方向）。
                return false;
            }
            // runtime `ZoomPane` 可以指定另一个 pane 继续放大；旧实现的语义
            // 是“换聚焦目标”。真实终端从旧卡搬回分屏、新 pane 搬进浮层，
            // 是一次结构变化；不沿用旧 Ghost，直接换卡。
            let slot = self.pane_bounds.borrow().get(&pane_id).copied();
            self.clear_zoom_motion(window);
            if let Some(WorkspaceTab::Terminal { focused, .. }) = self.tabs.get_mut(tab_ix) {
                *focused = pane_id;
            }
            // 换卡不飞 Ghost，但仍记录新 pane 的分屏槽位：之后还原时
            // 才能沿正常时间线缩回它的旧位置，而不是原地淡出。
            self.zoom_motion =
                Some(ZoomMotion::new(tab_ix, pane_id, 1.0, 1.0, slot, None, viewport, scale, now));
            self.mark_structural_resize(tab_ix, cx);
            if tab_ix == self.active {
                self.focus_active(window, cx);
            }
            cx.notify();
            return true;
        }

        let content =
            self.zoom_bounds.get().filter(|(ix, _)| *ix == tab_ix).map(|(_, bounds)| bounds);
        let slot = self.pane_bounds.borrow().get(&pane_id).copied();
        let card = content.and_then(zoom_card_bounds);
        // `content` 只在该 tab 刚渲染过时才存在，是“记录仍对应当前视口”的
        // 证据；后台/隐藏 tab 的 pane_bounds 可能已经过期，不能拿来当
        // Ghost 的起止点。
        let source = if zoomed { content.and(slot) } else { card };

        if let Some(motion) = self
            .zoom_motion
            .as_mut()
            .filter(|motion| motion.tab == tab_ix && motion.pane == pane_id)
        {
            let resized = motion.viewport != viewport || motion.scale != scale;
            if resized {
                motion.viewport = viewport;
                motion.scale = scale;
                // 旧槽位属于旧视口；resize 后不再可信，关闭方向退化为淡出。
                motion.slot = None;
                if let Some(ghost) = motion.ghost.take() {
                    let _ = window.drop_image(ghost.image);
                }
                motion.position = motion.target;
                motion.velocity = 0.0;
            }
            motion.target = if zoomed { 1.0 } else { 0.0 };
            // 从静止打开态反向关闭时没有在飞行的 Ghost，需要现抓一张当前卡。
            // 关闭方向的落点是放大前记下的分屏槽位；放大期间 `pane_bounds`
            // 记的是悬浮卡自身，不能拿来当收尾落点。
            if !reduced && !resized && motion.ghost.is_none() {
                let origin = if zoomed { motion.slot.or(slot) } else { motion.slot };
                if let (Some(source), Some(origin)) = (source, origin) {
                    motion.slot = Some(origin);
                    motion.ghost = capture_ghost(window, source, viewport, scale);
                }
            }
            if reduced {
                motion.position = motion.target;
                motion.velocity = 0.0;
            }
            motion.last_step = now;
        } else {
            // 换 pane 或上一个方向已经结束：旧瞬态可能还持有 Ghost 纹理，
            // 覆盖前必须显式释放。
            self.clear_zoom_motion(window);
            if reduced {
                self.zoom_motion = if zoomed {
                    Some(ZoomMotion::new(
                        tab_ix, pane_id, 1.0, 1.0, slot, None, viewport, scale, now,
                    ))
                } else {
                    None
                };
            } else {
                let (position, target) = if zoomed { (0.0, 1.0) } else { (1.0, 0.0) };
                // 没有既有运动时只可能是旧代码/测试直接翻 `zoomed`。打开方向
                // 仍按分屏槽位飞入；关闭方向没有可信槽位（此刻 pane_bounds
                // 记的是悬浮卡），退化为遮罩淡出。
                let (slot, ghost) = if zoomed {
                    let ghost = source
                        .zip(slot)
                        .and_then(|(source, _)| capture_ghost(window, source, viewport, scale));
                    (slot, ghost)
                } else {
                    (None, None)
                };
                self.zoom_motion = Some(ZoomMotion::new(
                    tab_ix, pane_id, position, target, slot, ghost, viewport, scale, now,
                ));
            }
        }

        if let Some(WorkspaceTab::Terminal { zoomed: state, focused, .. }) =
            self.tabs.get_mut(tab_ix)
        {
            *state = zoomed;
            *focused = pane_id;
        }
        // 真实终端从分屏槽位搬到悬浮卡（或反向）是一次性结构变化：
        // 只在这里同步下发一次，动画帧不再触碰网格/PTY。
        self.mark_structural_resize(tab_ix, cx);
        if tab_ix == self.active {
            self.focus_active(window, cx);
        }
        cx.notify();
        true
    }

    /// 当前 tab 若处于放大态，带关闭动画退出。供方向导航等结构性操作复用。
    pub(super) fn exit_zoom(
        &mut self,
        tab_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(WorkspaceTab::Terminal { zoomed: true, focused, panes, .. }) =
            self.tabs.get(tab_ix)
        else {
            return false;
        };
        if panes.len() < 2 {
            return false;
        }
        let pane = *focused;
        self.set_zoom(tab_ix, pane, false, window, cx)
    }

    pub(super) fn clear_zoom_motion(&mut self, window: &mut Window) {
        if let Some(motion) = self.zoom_motion.take()
            && let Some(ghost) = motion.ghost
        {
            let _ = window.drop_image(ghost.image);
        }
    }

    pub(super) fn clear_zoom_motion_for_tab(&mut self, tab_ix: usize, window: &mut Window) {
        if self.zoom_motion.as_ref().is_some_and(|motion| motion.tab == tab_ix) {
            self.clear_zoom_motion(window);
        }
    }

    /// 每个 animation frame 采样一次。布局翻到新状态后，动画只驱动 Ghost
    /// 和遮罩；position 由 `render_zoom_overlay` 当帧消费。
    pub(super) fn advance_zoom_motion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pane) = self.zoom_motion.as_ref().map(|motion| motion.pane) else { return };
        // 以 pane id 重新解析 tab：tab 重排/跨窗合并后下标会变，动画不能
        // 追着一个已经指向别的 tab 的旧下标跑。
        let Some(tab_ix) = self.tab_of_pane(pane) else {
            self.clear_zoom_motion(window);
            return;
        };
        let Some(motion) = self.zoom_motion.as_mut() else { return };
        motion.tab = tab_ix;
        let (logical, focused) = match self.tabs.get(tab_ix) {
            Some(WorkspaceTab::Terminal { zoomed, focused, .. }) => (*zoomed, *focused),
            _ => (false, pane),
        };
        // 旧代码/测试会直接改 `zoomed`。逻辑状态与动画目标不一致时以逻辑
        // 状态为准：关闭态不需要再飞 Ghost，打开态则把目标重新抬起。
        // 关闭途中允许焦点已经切到别的 pane：Ghost 只是收尾画面，不参与
        // 逻辑布局，不能因为焦点变了就把它截断。
        let stale = (!logical && motion.target >= 0.5) || (logical && focused != motion.pane);
        if !stale && logical && motion.target < 0.5 {
            motion.target = 1.0;
        }
        if stale {
            self.clear_zoom_motion(window);
            return;
        }

        let viewport = window.viewport_size();
        let scale = window.scale_factor();
        let reduced = cx.reduce_motion();
        let now = cx.background_executor().now();
        let Some(motion) = self.zoom_motion.as_mut() else { return };

        if motion.ghost.as_ref().is_some_and(|ghost| ghost.failed.get()) {
            if let Some(ghost) = motion.ghost.take() {
                let _ = window.drop_image(ghost.image);
            }
        }
        if motion.viewport != viewport || motion.scale != scale {
            // 快照和旧槽位都按旧视口取值；跨 DPI/resize 后直接瞬切到终态，
            // 避免把 Ghost 拉到一个已经不存在的位置。
            motion.viewport = viewport;
            motion.scale = scale;
            motion.slot = None;
            if let Some(ghost) = motion.ghost.take() {
                let _ = window.drop_image(ghost.image);
            }
            motion.position = motion.target;
            motion.velocity = 0.0;
        }

        let elapsed = now.saturating_duration_since(motion.last_step);
        motion.last_step = now;
        if reduced {
            motion.position = motion.target;
            motion.velocity = 0.0;
        } else if !elapsed.is_zero() {
            let (position, velocity) =
                spring_step(motion.position, motion.velocity, motion.target, elapsed.as_secs_f32());
            motion.position = position;
            motion.velocity = velocity;
        }

        let settled = (motion.position - motion.target).abs() < POSITION_EPSILON
            && motion.velocity.abs() < VELOCITY_EPSILON;
        if settled {
            motion.position = motion.target;
            motion.velocity = 0.0;
            if let Some(ghost) = motion.ghost.take() {
                let _ = window.drop_image(ghost.image);
            }
        }
        let finished = settled && motion.target == 0.0;
        if finished {
            self.zoom_motion = None;
        } else if !settled {
            window.request_animation_frame();
        }
    }

    /// 悬浮层画在终端卡内容区里：分屏先正常渲染（聚焦 pane 为占位），
    /// 这里再叠加压暗层、Ghost 和唯一一个真实终端实例。
    pub(super) fn render_zoom_overlay(
        &self,
        tab_ix: usize,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let Some(WorkspaceTab::Terminal { tree, panes, focused, zoomed, broadcast, .. }) =
            self.tabs.get(tab_ix)
        else {
            return None;
        };
        // 单 pane 没有放大语义；旧状态残留时也不该在满卡上再叠一张卡。
        if tree.is_leaf() {
            return None;
        }
        let motion = self
            .zoom_motion
            .as_ref()
            .filter(|motion| motion.tab == tab_ix && (!*zoomed || motion.pane == *focused));
        let position = match (*zoomed, motion) {
            (true, Some(motion)) => motion.position,
            (true, None) => 1.0,
            (false, Some(motion)) if motion.target < 0.5 => motion.position,
            _ => return None,
        };
        if position <= POSITION_EPSILON && !*zoomed {
            return None;
        }

        let backdrop = BACKDROP_ALPHA * smoothstep(0.0, 0.9, position);
        // 只做绝对定位：`.relative()` 会覆盖 position，让根节点回到流内并
        // 因为子节点全是 absolute 而缩成 0×0。
        let mut overlay = div().absolute().inset_0();
        overlay = overlay.child(
            div()
                .absolute()
                .inset_0()
                // 放大态遮罩独占指针；v1 不支持点击背景切换目标。
                // 卡片是后继兄弟节点，命中顺序在遮罩之上。
                .when(*zoomed, |backdrop| backdrop.occlude())
                .bg(gpui::black().opacity(backdrop)),
        );

        let card_style = crate::gpui_shell::theme::PaneCardStyle::current(cx);
        // 槽位是飞行的空间锚点：有槽位就画一张从槽位长到卡片（或缩回）的
        // 缩放画面，没有截图时退化为同色卡面。没有槽位（换卡、resize 后
        // 失效）就没有可信起止点，直接用真实卡。稳态放大卡本身就是最终
        // 画面，不再垫飞行层：半透明卡色叠两次会让卡比普通 pane 更实。
        let veil_slot =
            motion.and_then(|motion| motion.slot).filter(|_| !*zoomed || position < 1.0);
        if let Some(slot) = veil_slot {
            // 快照的圆角从槽位的 0 跟到悬浮卡半径，避免落地前四个方角
            // 从卡片的圆角缺口中露出来。
            let veil = render_zoom_veil(
                slot,
                motion.and_then(|motion| motion.ghost.as_ref()),
                position,
                px(card_style.radius) * position.clamp(0.0, 1.0),
                crate::gpui_shell::theme::card_content_bg(cx),
                cx.theme().border,
            );
            // 缩回槽位时快照内容是放大后的画面，和槽位里已经恢复的终端
            // 不一致；最后四分之一淡出，避免落地一帧换脸。
            let fade = if *zoomed { 1.0 } else { smoothstep(0.0, 0.25, position) };
            overlay = overlay.child(div().absolute().inset_0().opacity(fade).child(veil));
        }

        if *zoomed {
            let Some(pane) = panes.iter().find(|pane| pane.id == *focused) else {
                return None;
            };
            let ordinal = pane_header::pane_order(tree)
                .iter()
                .position(|id| *id == pane.id)
                .map_or(1, |at| at + 1);
            let content_background = crate::gpui_shell::theme::card_content_bg(cx);
            // 有飞行画面时等它到位再淡入真实卡，避免双影；没有空间锚点时
            // 没有可看的过渡，直接显示终态，不让聚焦 pane 的位置空一帧。
            let card_opacity = if veil_slot.is_some() {
                smoothstep(CARD_REVEAL.0, CARD_REVEAL.1, position)
            } else {
                1.0
            };
            let card = div()
                .size_full()
                .rounded(px(card_style.radius))
                .bg(content_background)
                .when(card_style.shadow, |card| {
                    card.shadow(vec![crate::gpui_shell::theme::card_shadow(cx)])
                })
                .overflow_hidden()
                .opacity(card_opacity)
                // 卡底色可能带壁纸透明度；悬浮卡必须自带同一层壁纸，
                // 否则透过去的是刚压暗的分屏而不是窗口背景。
                .child(crate::gpui_shell::wallpaper::card_layer(cx))
                .child(self.render_terminal_pane(
                    tab_ix,
                    pane,
                    ordinal,
                    true,
                    true,
                    true,
                    // 淡入期间不能复用缓存表面：缓存会把绘制时的透明度烘进
                    // 图元，透明度回升后画面仍停在卡底色上（白屏，直到终端
                    // notify 打破缓存）。见 `terminal_surface_live`。
                    card_opacity < 1.0,
                    *broadcast,
                    pane_header::HeaderCorners { top_left: true, top_right: true },
                    cx,
                ));
            overlay = overlay.child(div().absolute().inset_0().p(px(ZOOM_MARGIN)).child(card));
        }

        Some(overlay.into_any_element())
    }

    /// 终端 tab 内容：分屏树 + 目标几何记录 + 悬浮层。
    pub(super) fn terminal_tab_content(
        &self,
        tab_ix: usize,
        split: gpui::AnyElement,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let zoom_bounds = self.zoom_bounds.clone();
        div()
            .size_full()
            .relative()
            // 悬浮层的目标几何来自终端卡内容区本身；事件时（打开/关闭）
            // 读上一帧记录，避免在事件处理器里再算一次布局。
            .child(
                canvas(
                    move |bounds, _, _| zoom_bounds.set(Some((tab_ix, bounds))),
                    |_, _, _, _| (),
                )
                .absolute()
                .inset_0(),
            )
            .child(split)
            .children(self.render_zoom_overlay(tab_ix, cx))
            .into_any_element()
    }
}

/// 飞行中的悬浮卡画面：从分屏槽位到悬浮卡矩形的一张缩放快照。
///
/// 快照缺失（非 Windows、抓取失败）时只画同色圆角卡面——空间上的
/// “从哪里放大、缩回哪里”不依赖截图能否拿到。
fn render_zoom_veil(
    slot: Bounds<Pixels>,
    ghost: Option<&Ghost>,
    position: f32,
    radius: Pixels,
    background: gpui::Hsla,
    border: gpui::Hsla,
) -> gpui::AnyElement {
    let image = ghost.map(|ghost| Arc::downgrade(&ghost.image));
    let failed = ghost.map(|ghost| ghost.failed.clone());
    canvas(
        |_, _, _| (),
        move |content, _, window, _| {
            let Some(card) = zoom_card_bounds(content) else { return };
            let rect = lerp_bounds(slot, card, position);
            let corners = gpui::Corners::all(radius);
            window.paint_quad(gpui::fill(rect, background).corner_radii(corners));
            let Some(image) = image.as_ref().and_then(|image| image.upgrade()) else { return };
            if failed.as_ref().is_some_and(|failed| failed.get()) {
                return;
            }
            // `image_bounds` 是整张快照的标称矩形；快照本身已经是按 slot
            // 裁好的，两个矩形都传 `rect` 才是“整图铺满目标矩形”。
            let painted = window.paint_image(rect, rect, corners, image, 0, false);
            if painted.is_err()
                && let Some(failed) = failed.as_ref()
            {
                failed.set(true);
                // 让下一帧 `advance_zoom_motion` 看到失败并释放纹理。
                window.request_animation_frame();
            }
            // 边框画在画面之上，让飞行中的卡片仍有清晰边界。
            window.paint_quad(gpui::quad(
                rect,
                corners,
                gpui::transparent_black(),
                px(1.0),
                border,
                gpui::BorderStyle::Solid,
            ));
        },
    )
    .absolute()
    .inset_0()
    .into_any_element()
}

/// 捕捉整窗客户区再裁出 `source` 作为飞行画面。只支持 Windows；其余
/// 平台返回 None，渲染自动退化为同色卡面。
fn capture_ghost(
    window: &Window,
    source: Bounds<Pixels>,
    viewport: gpui::Size<Pixels>,
    scale: f32,
) -> Option<Ghost> {
    if !crate::platform::window_capture::supported() {
        return None;
    }
    // 同步抓取已经付过这次等待；抓到就用于动画，不因为“抓得慢”再把
    // 画面扔掉——那样只会既卡了按键又丢了过渡。
    let frame = crate::platform::window_capture::capture(window).ok()?;
    if frame.width != (f32::from(viewport.width) * scale).round() as u32
        || frame.height != (f32::from(viewport.height) * scale).round() as u32
    {
        return None;
    }

    let scale_x = |value: f32| (value * scale).clamp(0.0, frame.width as f32);
    let scale_y = |value: f32| (value * scale).clamp(0.0, frame.height as f32);
    let left = scale_x(f32::from(source.origin.x)).floor() as u32;
    let top = scale_y(f32::from(source.origin.y)).floor() as u32;
    let right = scale_x(f32::from(source.origin.x + source.size.width)).ceil() as u32;
    let bottom = scale_y(f32::from(source.origin.y + source.size.height)).ceil() as u32;
    let width = right.saturating_sub(left);
    let height = bottom.saturating_sub(top);
    if width == 0
        || height == 0
        || width > GHOST_MAX_EDGE
        || height > GHOST_MAX_EDGE
        || width as usize * height as usize * 4 > GHOST_MAX_BYTES
    {
        return None;
    }

    let row_bytes = width as usize * 4;
    let mut pixels = Vec::with_capacity(row_bytes * height as usize);
    for row in top..top + height {
        let start = ((row * frame.width + left) as usize) * 4;
        pixels.extend_from_slice(&frame.bgra[start..start + row_bytes]);
    }
    // 与主题揭示共用同一条 BGRA → RenderImage 路径。
    let image = image::RgbaImage::from_raw(width, height, pixels)?;
    Some(Ghost {
        image: Arc::new(RenderImage::new(vec![image::Frame::new(image)])),
        failed: Rc::new(Cell::new(false)),
    })
}

/// 悬浮卡目标矩形：终端卡内容区四边各收 `ZOOM_MARGIN`。
fn zoom_card_bounds(content: Bounds<Pixels>) -> Option<Bounds<Pixels>> {
    let width = f32::from(content.size.width);
    let height = f32::from(content.size.height);
    if width <= ZOOM_MARGIN * 2.0 || height <= ZOOM_MARGIN * 2.0 {
        return None;
    }
    Some(Bounds::new(
        point(content.origin.x + px(ZOOM_MARGIN), content.origin.y + px(ZOOM_MARGIN)),
        size(
            content.size.width - px(ZOOM_MARGIN * 2.0),
            content.size.height - px(ZOOM_MARGIN * 2.0),
        ),
    ))
}

fn lerp_bounds(from: Bounds<Pixels>, to: Bounds<Pixels>, t: f32) -> Bounds<Pixels> {
    Bounds::new(
        point(
            from.origin.x + (to.origin.x - from.origin.x) * t,
            from.origin.y + (to.origin.y - from.origin.y) * t,
        ),
        size(
            from.size.width + (to.size.width - from.size.width) * t,
            from.size.height + (to.size.height - from.size.height) * t,
        ),
    )
}

fn smoothstep(edge0: f32, edge1: f32, value: f32) -> f32 {
    let t = ((value - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// 半隐式欧拉 + 固定子步：帧长抖动不会改变弹簧轨迹，反向时保留速度。
fn spring_step(mut position: f32, mut velocity: f32, target: f32, dt: f32) -> (f32, f32) {
    let mut remaining = dt.clamp(0.0, 0.032);
    while remaining > 0.0 {
        let step = remaining.min(SUBSTEP_SECONDS);
        let acceleration = -STIFFNESS * (position - target) - DAMPING * velocity;
        velocity += acceleration * step;
        position += velocity * step;
        remaining -= step;
    }
    (position, velocity)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(w), px(h)))
    }

    #[test]
    fn spring_starts_at_rest_and_settles_on_target() {
        let mut position = 0.0;
        let mut velocity = 0.0;
        for _ in 0..240 {
            (position, velocity) = spring_step(position, velocity, 1.0, 1.0 / 240.0);
        }
        assert!((position - 1.0).abs() < 0.001, "position={position}");
        assert!(velocity.abs() < 0.01, "velocity={velocity}");
    }

    #[test]
    fn spring_reverses_without_resetting_position() {
        let mut position = 0.0;
        let mut velocity = 0.0;
        for _ in 0..30 {
            (position, velocity) = spring_step(position, velocity, 1.0, 1.0 / 240.0);
        }
        let before = position;
        assert!(before > 0.0 && before < 1.0);
        (position, velocity) = spring_step(position, velocity, 0.0, 1.0 / 240.0);
        assert!(position >= before, "the first reverse step must keep moving, not jump");
        assert!((position - before).abs() < 0.05);
    }

    #[test]
    fn card_inset_stays_inside_the_terminal_content() {
        let content = bounds(100.0, 50.0, 1000.0, 700.0);
        let card = zoom_card_bounds(content).unwrap();
        assert!((f32::from(card.origin.x) - 140.0).abs() < 0.001);
        assert!((f32::from(card.origin.y) - 90.0).abs() < 0.001);
        assert!((f32::from(card.size.width) - 920.0).abs() < 0.001);
        assert!((f32::from(card.size.height) - 620.0).abs() < 0.001);
        assert!(zoom_card_bounds(bounds(0.0, 0.0, 80.0, 80.0)).is_none());
    }

    #[test]
    fn ghost_interpolation_preserves_endpoints_and_handles_fractional_dpi() {
        let slot = bounds(10.5, 20.25, 300.5, 200.0);
        let card = bounds(40.0, 40.0, 900.0, 700.0);
        let at_slot = lerp_bounds(slot, card, 0.0);
        assert!((f32::from(at_slot.origin.x) - 10.5).abs() < 0.001);
        assert!((f32::from(at_slot.size.width) - 300.5).abs() < 0.001);
        let at_card = lerp_bounds(slot, card, 1.0);
        assert!((f32::from(at_card.origin.x) - 40.0).abs() < 0.001);
        assert!((f32::from(at_card.size.height) - 700.0).abs() < 0.001);
        let middle = lerp_bounds(slot, card, 0.5);
        assert!((f32::from(middle.origin.x) - 25.25).abs() < 0.001);
        assert!((f32::from(middle.size.width) - 600.25).abs() < 0.001);
    }

    #[test]
    fn smoothstep_clamps_and_matches_endpoints() {
        assert_eq!(smoothstep(0.3, 0.9, 0.0), 0.0);
        assert_eq!(smoothstep(0.3, 0.9, 1.0), 1.0);
        assert!((smoothstep(0.0, 1.0, 0.5) - 0.5).abs() < f32::EPSILON);
    }
}
