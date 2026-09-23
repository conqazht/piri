use crate::plugins::PiriEvent;
use anyhow::Result;
use log::{debug, info};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::{Config, WindowRuleConfig};
use crate::niri::NiriIpc;
use crate::plugins::window_utils::{self, WindowMatcher, WindowMatcherCache};
use crate::plugins::FromConfig;
use crate::utils::Throttle;

/// Window rule plugin config (for internal use)
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WindowRulePluginConfig {
    /// List of window rules
    pub rules: Vec<WindowRuleConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FloatingAction {
    floating: bool,
    width: Option<u32>,
    height: Option<u32>,
    reset_height: bool,
    centered: bool,
}

impl FloatingAction {
    fn should_resize(self) -> bool {
        self.floating && (self.width.is_some() || self.height.is_some() || self.reset_height)
    }

    fn should_center(self) -> bool {
        self.floating && self.centered
    }

    fn should_defer_until_floating(self, is_floating: bool) -> bool {
        self.floating && !is_floating && (self.should_resize() || self.should_center())
    }

    fn size_is_committed(self, layout: &niri_ipc::WindowLayout) -> bool {
        fn dimension_matches(expected: u32, tile: f64, window: i32) -> bool {
            const DECORATION_TOLERANCE: f64 = 8.0;
            (tile - expected as f64).abs() <= DECORATION_TOLERANCE
                || (window as f64 - expected as f64).abs() <= DECORATION_TOLERANCE
        }

        self.width
            .is_none_or(|width| dimension_matches(width, layout.tile_size.0, layout.window_size.0))
            && self.height.is_none_or(|height| {
                dimension_matches(height, layout.tile_size.1, layout.window_size.1)
            })
    }
}

impl FromConfig for WindowRulePluginConfig {
    fn from_config(config: &Config) -> Option<Self> {
        if config.window_rule.is_empty() {
            None
        } else {
            Some(Self {
                rules: config.window_rule.clone(),
            })
        }
    }
}

/// Window rule plugin that moves windows to workspaces based on app_id and title matching
pub struct WindowRulePlugin {
    niri: NiriIpc,
    config: WindowRulePluginConfig,
    /// Window matcher cache for regex pattern matching
    matcher_cache: Arc<WindowMatcherCache>,
    /// Last window ID that triggered focus command
    last_focused_window: Option<u64>,
    /// Throttle for focus command execution
    execution_throttle: Throttle,
    /// Set of rule indices that have already executed focus_command (when focus_command_once is true)
    executed_rules: HashSet<usize>,
    /// Last window ID that was processed by handle_focus_command (for throttling)
    last_handled_window: Option<u64>,
    /// Throttle for handle_focus_command
    handle_throttle: Throttle,
    /// Windows locked to a specific workspace (window_id -> workspace_name)
    locked_windows: HashMap<u64, String>,
    /// Floating actions successfully applied, keyed by (rule index, window ID)
    applied_floating_rules: HashSet<(usize, u64)>,
    /// Floating actions waiting for Niri to confirm the window entered the floating layer
    pending_floating_rules: HashMap<u64, (usize, FloatingAction)>,
    /// Center actions waiting for the resized floating geometry to be committed
    pending_center_rules: HashMap<u64, (usize, FloatingAction)>,
    /// Settle flags for the delayed fallback: set when the event path
    /// finishes a panel, so the fallback task skips already-settled windows
    settle_done: HashMap<u64, Arc<AtomicBool>>,
}

impl WindowRulePlugin {
    /// Execute focus command with de-duplication
    async fn execute_focus_rule(
        &mut self,
        window_id: u64,
        focus_command: &str,
        rule_index: usize,
        focus_once: bool,
    ) -> Result<()> {
        // If focus_once is true and this rule has already executed focus_command, skip
        if focus_once && self.executed_rules.contains(&rule_index) {
            return Ok(());
        }

        // Global throttle: prevent executing focus_command too frequently regardless of window ID
        if self.execution_throttle.check_and_update(Duration::from_millis(200)) {
            info!(
                "Executing focus_command for window {}: {}",
                window_id, focus_command
            );
            window_utils::execute_command(focus_command)?;

            // Mark this rule as having executed focus_command if focus_once is true
            if focus_once {
                self.executed_rules.insert(rule_index);
            }

            self.last_focused_window = Some(window_id);
        }

        Ok(())
    }

    /// Handle focus command execution for currently focused window
    async fn handle_focus_command(&mut self, window_id: u64) -> Result<()> {
        // Check if this is a programmatic focus change (e.g., from auto_fill)
        if window_utils::should_ignore_focus_change() {
            debug!(
                "Ignoring programmatic focus change for window {}",
                window_id
            );
            return Ok(());
        }

        // Global throttle: prevent processing focus changes too frequently
        if !self.handle_throttle.check_and_update(Duration::from_millis(200)) {
            return Ok(());
        }

        // Update tracking before processing
        self.last_handled_window = Some(window_id);

        let windows = self.niri.get_windows_raw().await?;
        let window = match windows.into_iter().find(|w| w.id == window_id) {
            Some(w) => w,
            None => {
                // Window not found - this is normal when a window is closing or has just closed
                // Silently return instead of erroring
                return Ok(());
            }
        };

        // Find matching rule without holding borrows on self
        let matched_rule = {
            let mut found = None;
            for (rule_index, rule) in self.config.rules.iter().enumerate() {
                if let Some(ref focus_command) = rule.focus_command {
                    let matcher = WindowMatcher::new(rule.app_id.as_deref(), rule.title.as_deref());
                    if self.matcher_cache.matches(
                        window.app_id.as_ref(),
                        Some(&window.title),
                        &matcher,
                    )? {
                        found = Some((rule_index, focus_command.clone(), rule.focus_command_once));
                        break;
                    }
                }
            }
            found
        };

        if let Some((rule_index, focus_command, focus_once)) = matched_rule {
            self.execute_focus_rule(window_id, &focus_command, rule_index, focus_once)
                .await?;
        }

        Ok(())
    }

    fn matching_rule_index(&self, window: &niri_ipc::Window) -> Result<Option<usize>> {
        for (rule_index, rule) in self.config.rules.iter().enumerate() {
            let matcher = WindowMatcher::new(rule.app_id.as_deref(), rule.title.as_deref());
            if self.matcher_cache.matches(
                window.app_id.as_ref(),
                window.title.as_ref(),
                &matcher,
            )? {
                return Ok(Some(rule_index));
            }
        }
        Ok(None)
    }

    /// Arrange floating panels managed by floating rules on a workspace side
    /// by side in a centered row, so a newly opened panel never covers
    /// earlier ones. Panels keep their vertical position; only horizontal
    /// moves are sent, and only to panels that actually need to move.
    /// Membership is decided by re-matching the rules, so panels stuck
    /// waiting for layout events are still included.
    async fn arrange_row(
        niri: &NiriIpc,
        rules: &[WindowRuleConfig],
        cache: &WindowMatcherCache,
        workspace_id: u64,
    ) -> Result<()> {
        const GAP: f64 = 16.0;
        let windows = niri.get_windows().await?;
        let mut panels: Vec<(u64, f64, f64, f64)> = Vec::new(); // id, x, y, w
        for w in &windows {
            if !w.floating || w.workspace_id != Some(workspace_id) {
                continue;
            }
            let Some(rule_idx) = Self::matching_floating_rule(rules, cache, w) else {
                continue;
            };
            if let Some(layout) = &w.layout {
                if let Some(pos) = layout.tile_pos {
                    let rule_w = rules[rule_idx].floating_width.map(|w| w as f64).unwrap_or(0.0);
                    let w_eff = layout.effective_width().max(rule_w);
                    if w_eff > 0.0 {
                        panels.push((w.id, pos[0], pos[1], w_eff));
                    }
                }
            }
        }
        if panels.len() < 2 {
            return Ok(());
        }
        panels.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        let total: f64 = panels.iter().map(|p| p.3).sum::<f64>() + GAP * (panels.len() - 1) as f64;
        let (out_w, _) = niri.get_output_size().await?;
        let mut x = ((out_w as f64 - total) / 2.0).max(0.0);
        for (id, cur_x, _, w) in panels {
            let dx = (x - cur_x).round() as i32;
            if dx != 0 {
                niri.move_window_relative(id, dx, 0).await?;
            }
            x += w + GAP;
        }
        Ok(())
    }

    /// Index of the floating rule matching a window, if any.
    fn matching_floating_rule(
        rules: &[WindowRuleConfig],
        cache: &WindowMatcherCache,
        window: &crate::niri::Window,
    ) -> Option<usize> {
        for (rule_index, rule) in rules.iter().enumerate() {
            if rule.floating != Some(true) {
                continue;
            }
            let matcher = WindowMatcher::new(rule.app_id.as_deref(), rule.title.as_deref());
            if cache
                .matches(window.app_id.as_ref(), Some(&window.title), &matcher)
                .unwrap_or(false)
            {
                return Some(rule_index);
            }
        }
        None
    }

    /// Shift a freshly centered panel until it no longer overlaps other
    /// floating windows on the same workspace (16px gap). Checks left and right
    /// candidate positions, choosing the one that fits within output bounds.
    async fn avoid_overlap(
        niri: &NiriIpc,
        window_id: u64,
        width: Option<u32>,
        height: Option<u32>,
    ) -> Result<()> {
        const GAP: f64 = 16.0;
        let windows = niri.get_windows().await?;
        let me = match windows.iter().find(|w| w.id == window_id) {
            Some(w) => w,
            None => return Ok(()),
        };
        let ws_id = me.workspace_id;
        let mut siblings: Vec<(f64, f64, f64, f64)> = Vec::new();
        for w in &windows {
            if w.id == window_id || !w.floating {
                continue;
            }
            if ws_id.is_some() && w.workspace_id != ws_id {
                continue;
            }
            if let Some(layout) = &w.layout {
                if let Some(pos) = layout.tile_pos {
                    let sw = layout.effective_width();
                    let sh = layout.effective_height();
                    if sw > 0.0 && sh > 0.0 {
                        siblings.push((pos[0], pos[1], sw, sh));
                    }
                }
            }
        }
        if siblings.is_empty() {
            return Ok(());
        }
        let Some((cx, cy, cw, ch)) = niri.get_window_position(window_id).await? else {
            return Ok(());
        };
        let pw = width.map(|w| (w as f64).max(cw as f64)).unwrap_or(cw as f64);
        let ph = height.map(|h| (h as f64).max(ch as f64)).unwrap_or(ch as f64);
        let cy = cy as f64;
        let overlaps = |x: f64| {
            siblings.iter().any(|(sx, sy, sw, sh)| {
                x < sx + sw && *sx < x + pw && cy < sy + sh && *sy < cy + ph
            })
        };
        let mut x = cx as f64;
        if overlaps(x) {
            let leftmost = siblings
                .iter()
                .filter(|(sx, sy, sw, sh)| {
                    x < sx + sw && *sx < x + pw && cy < sy + sh && *sy < cy + ph
                })
                .map(|(sx, _, _, _)| *sx)
                .fold(f64::INFINITY, f64::min);
            let rightmost = siblings
                .iter()
                .filter(|(sx, sy, sw, sh)| {
                    x < sx + sw && *sx < x + pw && cy < sy + sh && *sy < cy + ph
                })
                .map(|(sx, _, sw, _)| *sx + *sw)
                .fold(f64::NEG_INFINITY, f64::max);

            let out_w =
                niri.get_output_size().await.map(|(w, _)| w as f64).unwrap_or(f64::INFINITY);

            let left_cand = leftmost - pw - GAP;
            let right_cand = rightmost + GAP;

            let left_ok = left_cand >= 0.0 && !overlaps(left_cand);
            let right_ok = (right_cand + pw) <= out_w && !overlaps(right_cand);

            if left_ok && right_ok {
                if (cx as f64 - left_cand).abs() <= (right_cand - cx as f64).abs() {
                    x = left_cand;
                } else {
                    x = right_cand;
                }
            } else if left_ok {
                x = left_cand;
            } else if right_ok {
                x = right_cand;
            } else if left_cand >= 0.0 {
                x = left_cand;
            } else if right_cand + pw <= out_w {
                x = right_cand;
            } else {
                let left_room = leftmost;
                let right_room = (out_w - rightmost).max(0.0);
                if right_room > left_room {
                    x = (rightmost + GAP).min(out_w - pw).max(0.0);
                } else {
                    x = (leftmost - pw - GAP).max(0.0);
                }
            }
        }
        let dx = (x - cx as f64).round() as i32;
        if dx != 0 {
            niri.move_window_relative(window_id, dx, 0).await?;
        }
        Ok(())
    }

    /// Row-arrange rule-managed panels on this window's workspace.
    /// Best effort: failures are logged by the caller.
    async fn arrange_workspace_row(&self, window_id: u64) -> Result<()> {
        let windows = self.niri.get_windows().await?;
        let ws_id = match windows.iter().find(|w| w.id == window_id).and_then(|w| w.workspace_id) {
            Some(id) => id,
            None => self.niri.get_focused_workspace().await.map(|ws| ws.id)?,
        };
        Self::arrange_row(&self.niri, &self.config.rules, &self.matcher_cache, ws_id).await
    }

    async fn continue_floating_action(
        &mut self,
        rule_index: usize,
        window_id: u64,
        action: FloatingAction,
    ) -> Result<()> {
        if action.should_resize() {
            self.niri
                .set_floating_window_size(
                    window_id,
                    action.width,
                    action.height,
                    action.reset_height,
                )
                .await?;
            if action.should_center() {
                self.pending_center_rules.insert(window_id, (rule_index, action));
                // Fallback: the layout-commit event this waits for can arrive
                // before the pending entry exists (or never), leaving the
                // panel sized but never positioned/arranged. Settle after a
                // delay unless the event path already finished (flag).
                let done = Arc::new(AtomicBool::new(false));
                self.settle_done.insert(window_id, Arc::clone(&done));
                let niri = self.niri.clone();
                let rules = self.config.rules.clone();
                let cache = Arc::clone(&self.matcher_cache);
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    if done.load(Ordering::SeqCst) {
                        return;
                    }
                    let _ = niri
                        .set_floating_window_size(
                            window_id,
                            action.width,
                            action.height,
                            action.reset_height,
                        )
                        .await;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    if done.load(Ordering::SeqCst) {
                        return;
                    }
                    if action.should_center() {
                        let _ = niri.center_window(window_id).await;
                        let _ = Self::avoid_overlap(&niri, window_id, action.width, action.height)
                            .await;
                    }
                    if let Ok(windows) = niri.get_windows().await {
                        if let Some(ws_id) =
                            windows.iter().find(|w| w.id == window_id).and_then(|w| w.workspace_id)
                        {
                            let _ = Self::arrange_row(&niri, &rules, &cache, ws_id).await;
                        }
                    }
                });
                return Ok(());
            }
        }
        if action.should_center() {
            self.niri.center_window(window_id).await?;
            if let Err(e) =
                Self::avoid_overlap(&self.niri, window_id, action.width, action.height).await
            {
                debug!("Overlap avoidance for window {} failed: {:#}", window_id, e);
            }
        }
        self.applied_floating_rules.insert((rule_index, window_id));
        if let Err(e) = self.arrange_workspace_row(window_id).await {
            debug!("Row arrangement failed: {:#}", e);
        }
        Ok(())
    }

    async fn finish_pending_center_action(&mut self, window_id: u64) -> Result<()> {
        let Some((rule_index, action)) = self.pending_center_rules.remove(&window_id) else {
            return Ok(());
        };
        if let Some(done) = self.settle_done.get(&window_id) {
            done.store(true, Ordering::SeqCst);
        }

        if let Err(error) = self.niri.center_window(window_id).await {
            self.pending_center_rules.insert(window_id, (rule_index, action));
            return Err(error);
        }

        if let Err(e) =
            Self::avoid_overlap(&self.niri, window_id, action.width, action.height).await
        {
            debug!("Overlap avoidance for window {} failed: {:#}", window_id, e);
        }

        self.applied_floating_rules.insert((rule_index, window_id));
        if let Err(e) = self.arrange_workspace_row(window_id).await {
            debug!("Row arrangement failed: {:#}", e);
        }
        Ok(())
    }

    async fn finish_pending_floating_action(&mut self, window_id: u64) -> Result<()> {
        let Some((rule_index, action)) = self.pending_floating_rules.remove(&window_id) else {
            return Ok(());
        };

        if let Err(error) = self.continue_floating_action(rule_index, window_id, action).await {
            self.pending_floating_rules.insert(window_id, (rule_index, action));
            return Err(error);
        }
        Ok(())
    }

    async fn apply_floating_action(
        &mut self,
        rule_index: usize,
        window: &niri_ipc::Window,
        floating_action: FloatingAction,
    ) -> Result<()> {
        let window_id = window.id;
        let key = (rule_index, window_id);
        if self.applied_floating_rules.contains(&key) {
            return Ok(());
        }

        if self.pending_floating_rules.contains_key(&window_id) {
            if window.is_floating {
                self.finish_pending_floating_action(window_id).await?;
            }
            return Ok(());
        }
        if self.pending_center_rules.contains_key(&window_id) {
            return Ok(());
        }

        if floating_action.should_defer_until_floating(window.is_floating) {
            self.niri.set_window_floating(window_id, true).await?;
            self.pending_floating_rules.insert(window_id, (rule_index, floating_action));
            return Ok(());
        }

        self.niri.set_window_floating(window_id, floating_action.floating).await?;
        self.continue_floating_action(rule_index, window_id, floating_action).await
    }

    async fn handle_dynamic_window_rule(&mut self, window: &niri_ipc::Window) -> Result<()> {
        let Some(rule_index) = self.matching_rule_index(window)? else {
            return Ok(());
        };
        let rule = &self.config.rules[rule_index];
        let Some(floating) = rule.floating else {
            return Ok(());
        };
        let action = FloatingAction {
            floating,
            width: rule.floating_width,
            height: rule.floating_height,
            reset_height: rule.floating_reset_height,
            centered: rule.floating_centered,
        };
        self.apply_floating_action(rule_index, window, action).await
    }

    async fn handle_window_opened(&mut self, window: &niri_ipc::Window) -> Result<()> {
        let matched_rule = self.matching_rule_index(window)?.map(|rule_index| {
            let rule = &self.config.rules[rule_index];
            (
                rule_index,
                rule.open_on_workspace.clone(),
                rule.floating.map(|floating| FloatingAction {
                    floating,
                    width: rule.floating_width,
                    height: rule.floating_height,
                    reset_height: rule.floating_reset_height,
                    centered: rule.floating_centered,
                }),
                rule.focus_command.clone(),
                rule.focus_command_once,
            )
        });

        if let Some((rule_index, open_on_workspace, floating_action, focus_command, focus_once)) =
            matched_rule
        {
            // 1. Move to workspace if specified
            if let Some(ref workspace_name) = open_on_workspace {
                // Check for lock suffix '!'
                let (target, locked) = if let Some(name) = workspace_name.strip_suffix('!') {
                    (name, true)
                } else {
                    (workspace_name.as_str(), false)
                };

                window_utils::move_window_to_named_workspace(&self.niri, window, target).await?;

                if locked {
                    info!("Window {} locked to workspace '{}'", window.id, target);
                    self.locked_windows.insert(window.id, target.to_string());
                }
            }

            // 2. Move to the floating or tiling layer if specified.
            if let Some(floating_action) = floating_action {
                self.apply_floating_action(rule_index, window, floating_action).await?;
            }

            // 3. Execute focus command if specified (unified de-duplication)
            if let Some(ref focus_command) = focus_command {
                self.execute_focus_rule(window.id, focus_command, rule_index, focus_once)
                    .await?;
            }
        }
        Ok(())
    }

    /// Check if a locked window is still on the correct workspace; move it back if not.
    async fn enforce_workspace_lock(&self, window: &niri_ipc::Window) -> Result<()> {
        let Some(target_workspace) = self.locked_windows.get(&window.id) else {
            return Ok(());
        };

        // Resolve the target workspace ID
        let workspaces = self.niri.get_workspaces_for_mapping().await?;
        let (target_name, want_output) = target_workspace
            .split_once('@')
            .map(|(name, output)| (name, Some(output)))
            .unwrap_or((target_workspace.as_str(), None));

        let focused_output = self.niri.get_focused_output().await.ok().map(|o| o.name);

        let find_on_output = |output_name: &str| -> Option<&niri_ipc::Workspace> {
            workspaces.iter().find(|ws| {
                ws.output.as_deref().is_some_and(|o| {
                    o == output_name || super::extract_display_prefix(o) == Some(output_name)
                }) && (ws.name.as_deref() == Some(target_name) || ws.idx.to_string() == target_name)
            })
        };

        let matched_ws = if let Some(want) = want_output {
            find_on_output(want)
        } else {
            focused_output.as_deref().and_then(find_on_output).or_else(|| {
                workspaces.iter().find(|ws| {
                    ws.name.as_deref() == Some(target_name) || ws.idx.to_string() == target_name
                })
            })
        };

        if let Some(target) = matched_ws {
            if window.workspace_id != Some(target.id) {
                info!(
                    "Window {} escaped workspace '{}', moving back",
                    window.id, target_workspace
                );
                window_utils::move_window_to_named_workspace(&self.niri, window, target_workspace)
                    .await?;
            }
        }

        Ok(())
    }
}

#[async_trait::async_trait]
impl crate::plugins::Plugin for WindowRulePlugin {
    type Config = WindowRulePluginConfig;

    fn new(niri: NiriIpc, config: WindowRulePluginConfig) -> Self {
        info!(
            "Window rule plugin initialized with {} rules",
            config.rules.len()
        );
        Self {
            niri,
            config,
            matcher_cache: Arc::new(WindowMatcherCache::new()),
            last_focused_window: None,
            execution_throttle: Throttle::new(),
            executed_rules: HashSet::new(),
            last_handled_window: None,
            handle_throttle: Throttle::new(),
            locked_windows: HashMap::new(),
            applied_floating_rules: HashSet::new(),
            pending_floating_rules: HashMap::new(),
            pending_center_rules: HashMap::new(),
            settle_done: HashMap::new(),
        }
    }

    async fn handle_event(&mut self, event: &PiriEvent, _niri: &NiriIpc) -> Result<()> {
        match event {
            PiriEvent::WindowFocusChanged {
                id: Some(window_id),
            } => {
                tokio::time::sleep(Duration::from_millis(10)).await;
                self.handle_focus_command(*window_id).await?;
            }
            PiriEvent::WindowOpened { window } => {
                self.handle_window_opened(window).await?;
            }
            PiriEvent::WindowChanged { window } => {
                self.enforce_workspace_lock(window).await.ok();
                if let Some((_, action)) = self.pending_center_rules.get(&window.id) {
                    if action.size_is_committed(&window.layout) {
                        self.finish_pending_center_action(window.id).await?;
                    }
                } else {
                    self.handle_dynamic_window_rule(window).await?;
                }
            }
            PiriEvent::WindowToggleFloating { window } => {
                if window.is_floating {
                    self.finish_pending_floating_action(window.id).await?;
                }
            }
            PiriEvent::WindowsChanged { windows } => {
                for window in windows {
                    self.handle_dynamic_window_rule(window).await?;
                }
            }
            PiriEvent::WindowLayoutsChanged { changes } => {
                for (window_id, layout) in changes {
                    if let Some((_, action)) = self.pending_center_rules.get(window_id) {
                        if action.size_is_committed(layout) {
                            self.finish_pending_center_action(*window_id).await?;
                        }
                    }
                }
            }
            PiriEvent::WindowClosed { id } => {
                self.locked_windows.remove(id);
                self.applied_floating_rules.retain(|(_, window_id)| window_id != id);
                self.pending_floating_rules.remove(id);
                self.pending_center_rules.remove(id);
                self.settle_done.remove(id);
            }
            _ => {}
        }
        Ok(())
    }

    fn is_interested_in_event(&self, event: &PiriEvent) -> bool {
        matches!(
            event,
            PiriEvent::WindowOpened { .. }
                | PiriEvent::WindowChanged { .. }
                | PiriEvent::WindowToggleFloating { .. }
                | PiriEvent::WindowLayoutsChanged { .. }
                | PiriEvent::WindowsChanged { .. }
                | PiriEvent::WindowClosed { .. }
                | PiriEvent::WindowFocusChanged { id: Some(_) }
        )
    }

    async fn update_config(&mut self, config: WindowRulePluginConfig) -> Result<()> {
        info!(
            "Updating window rule plugin configuration: {} rules",
            config.rules.len()
        );
        self.config = config;
        self.matcher_cache.clear_cache();
        // Clear executed rules tracking since rule indices may have changed
        self.executed_rules.clear();
        self.applied_floating_rules.clear();
        self.pending_floating_rules.clear();
        self.pending_center_rules.clear();
        self.settle_done.clear();
        Ok(())
    }
}
