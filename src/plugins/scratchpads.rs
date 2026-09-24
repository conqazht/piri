use anyhow::{Context, Result};
use async_trait::async_trait;
use log::{debug, info, warn};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::{Config, Direction, ScratchpadConfig};
use crate::ipc::IpcRequest;
use crate::niri::NiriIpc;
use crate::plugins::window_utils::{
    self, get_focused_window, matches_workspace, perform_swallow, register_sticky_window,
    WindowMatcher, WindowMatcherCache,
};
use crate::plugins::FromConfig;
use crate::plugins::PiriEvent;
use crate::utils::send_notification;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScratchpadsPluginConfig {
    pub scratchpads: HashMap<String, ScratchpadConfig>,
    pub default_size: String,
    pub default_margin: u32,
    pub move_to_workspace: Option<String>,
    pub tile_in_target_workspace: bool,
}

impl Default for ScratchpadsPluginConfig {
    fn default() -> Self {
        Self {
            scratchpads: HashMap::new(),
            default_size: "75% 60%".to_string(),
            default_margin: 50,
            move_to_workspace: None,
            tile_in_target_workspace: false,
        }
    }
}

impl FromConfig for ScratchpadsPluginConfig {
    fn from_config(config: &Config) -> Option<Self> {
        // Scratchpads plugin is always enabled if not explicitly disabled,
        // because it can be used dynamically via IPC even without initial config.
        Some(Self {
            scratchpads: config.scratchpads.clone(),
            default_size: config.piri.scratchpad.default_size.clone(),
            default_margin: config.piri.scratchpad.default_margin,
            move_to_workspace: config.piri.scratchpad.move_to_workspace.clone(),
            tile_in_target_workspace: config.piri.scratchpad.tile_in_target_workspace,
        })
    }
}

#[derive(Debug, Clone)]
struct ScratchpadState {
    window_id: Option<u64>,
    is_visible: bool,
    previous_focused_window: Option<u64>,
    config: ScratchpadConfig,
    is_dynamic: bool,
}

struct ScratchpadManager {
    niri: NiriIpc,
    states: HashMap<String, ScratchpadState>,
    pub matcher_cache: Arc<WindowMatcherCache>,
}

impl ScratchpadManager {
    fn new(niri: NiriIpc) -> Self {
        Self {
            niri,
            states: HashMap::new(),
            matcher_cache: Arc::new(WindowMatcherCache::new()),
        }
    }

    async fn get_target_position(
        &self,
        config: &ScratchpadConfig,
        window_width: u32,
        window_height: u32,
        is_visible: bool,
    ) -> Result<(i32, i32)> {
        let (output_width, output_height) = self.niri.get_output_size().await?;

        let (x, y) = if is_visible {
            window_utils::calculate_position(
                config.direction,
                output_width,
                output_height,
                window_width,
                window_height,
                config.margin,
            )
        } else {
            window_utils::calculate_hide_position(
                config.direction,
                output_width,
                output_height,
                window_width,
                window_height,
                config.margin,
            )
        };
        Ok((x, y))
    }

    async fn get_target_geometry(
        &self,
        config: &ScratchpadConfig,
        is_visible: bool,
    ) -> Result<(i32, i32, u32, u32)> {
        let (output_width, output_height) = self.niri.get_output_size().await?;
        let (width_ratio, height_ratio) = config.parse_size()?;
        let window_width = (output_width as f64 * width_ratio) as u32;
        let window_height = (output_height as f64 * height_ratio) as u32;

        let (x, y) = self
            .get_target_position(config, window_width, window_height, is_visible)
            .await?;
        Ok((x, y, window_width, window_height))
    }

    async fn setup_window(&mut self, window_id: u64, config: &ScratchpadConfig) -> Result<()> {
        debug!("Setting up window {} as scratchpad", window_id);
        self.niri.set_window_floating(window_id, true).await?;

        let (hide_x, hide_y, width, height) = self.get_target_geometry(config, false).await?;
        self.niri.resize_floating_window(window_id, width, height).await?;
        tokio::time::sleep(Duration::from_millis(100)).await;

        let (current_x, current_y, _, _) = self
            .niri
            .get_window_position_async(window_id)
            .await?
            .context("Failed to get window position")?;

        window_utils::move_window_to_position(
            &self.niri, window_id, current_x, current_y, hide_x, hide_y,
        )
        .await?;
        Ok(())
    }

    async fn sync_state(
        &mut self,
        name: &str,
        target_workspace: Option<String>,
        should_tile: bool,
    ) -> Result<()> {
        let (mut config, is_visible, window_id, is_dynamic) = {
            let state = self.states.get_mut(name).context("State not found")?;
            (
                state.config.clone(),
                state.is_visible,
                state.window_id.context("Window ID not found")?,
                state.is_dynamic,
            )
        };

        // Handle swallow_to_focus logic
        if config.swallow_to_focus {
            if is_visible {
                // When showing: perform swallow to focused window
                debug!(
                    "Swallow to focus enabled for scratchpad '{}', performing swallow operation",
                    name
                );
                let child_window = self
                    .niri
                    .get_windows_raw()
                    .await?
                    .into_iter()
                    .find(|w| w.id == window_id)
                    .context("Scratchpad window not found")?;

                match get_focused_window(&self.niri).await {
                    Ok(parent_window) => {
                        if parent_window.id != window_id {
                            debug!(
                                "Swallowing scratchpad window {} to focused window {}",
                                window_id, parent_window.id
                            );
                            perform_swallow(
                                &self.niri,
                                &parent_window,
                                &child_window,
                                window_id,
                                niri_ipc::ColumnDisplay::Tabbed,
                            )
                            .await?;
                            return Ok(());
                        } else {
                            debug!(
                                "Scratchpad window {} is already focused, skipping swallow",
                                window_id
                            );
                        }
                    }
                    Err(e) => {
                        warn!(
                            "Failed to get focused window for swallow operation: {}, falling back to normal show",
                            e
                        );
                    }
                }
            } else {
                // When hiding: ensure window is floating first
                debug!(
                    "Swallow to focus enabled for scratchpad '{}', ensuring window is floating before hide",
                    name
                );
                self.niri.set_window_floating(window_id, true).await?;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }

        if is_visible {
            // Check if window is currently tiled (e.g. docked in target workspace)
            let windows = self.niri.get_windows_raw().await?;
            let is_floating =
                windows.iter().find(|w| w.id == window_id).map(|w| w.floating).unwrap_or(false);

            if !is_floating {
                info!(
                    "Window {} is tiled; preparing for scratchpad show",
                    window_id
                );
                // 1. Make window floating in its current workspace
                self.niri.set_window_floating(window_id, true).await?;
                tokio::time::sleep(Duration::from_millis(50)).await;

                // 2. Resize to target dimensions
                let (target_x, target_y, target_width, target_height) = if is_dynamic {
                    let (output_width, output_height) = self.niri.get_output_size().await?;
                    let (tx, ty) = self
                        .get_target_position(&config, output_width / 2, output_height / 2, true)
                        .await?;
                    (tx, ty, output_width / 2, output_height / 2)
                } else {
                    self.get_target_geometry(&config, true).await?
                };
                self.niri.resize_floating_window(window_id, target_width, target_height).await?;
                tokio::time::sleep(Duration::from_millis(50)).await;

                // 3. Move it off-screen to hide position before pulling to active workspace
                let (output_width, output_height) = self.niri.get_output_size().await?;
                let (hide_x, hide_y) = window_utils::calculate_hide_position(
                    config.direction,
                    output_width,
                    output_height,
                    target_width,
                    target_height,
                    config.margin,
                );

                let (cur_x, cur_y) = match self.niri.get_window_position_async(window_id).await? {
                    Some((cx, cy, _, _)) => (cx, cy),
                    None => (
                        ((output_width.saturating_sub(target_width)) / 2) as i32,
                        ((output_height.saturating_sub(target_height)) / 2) as i32,
                    ),
                };
                let _ = window_utils::move_window_to_position(
                    &self.niri, window_id, cur_x, cur_y, hide_x, hide_y,
                )
                .await;
                tokio::time::sleep(Duration::from_millis(50)).await;

                // 4. Move to current workspace
                self.niri.move_floating_window(window_id).await?;
                tokio::time::sleep(Duration::from_millis(50)).await;

                // 5. Slide into view with animation
                let (actual_x, actual_y) =
                    match self.niri.get_window_position_async(window_id).await? {
                        Some((cx, cy, _, _)) => (cx, cy),
                        None => (hide_x, hide_y),
                    };
                window_utils::move_window_to_position(
                    &self.niri, window_id, actual_x, actual_y, target_x, target_y,
                )
                .await?;
                tokio::time::sleep(Duration::from_millis(100)).await;

                // 6. Focus window
                window_utils::focus_window(self.niri.clone(), window_id).await?;
                return Ok(());
            }

            // Move to current workspace if needed
            self.niri.move_floating_window(window_id).await?;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // Get current position and size (with retry to handle brief layout recalculations)
        let mut window_pos = self.niri.get_window_position_async(window_id).await?;
        if window_pos.is_none() {
            for _ in 0..5 {
                tokio::time::sleep(Duration::from_millis(40)).await;
                window_pos = self.niri.get_window_position_async(window_id).await?;
                if window_pos.is_some() {
                    break;
                }
            }
        }

        let (current_x, current_y, current_width, current_height) = match window_pos {
            Some(pos) => pos,
            None => {
                let (target_vis_x, target_vis_y, default_w, default_h) =
                    self.get_target_geometry(&config, true).await?;
                (target_vis_x, target_vis_y, default_w, default_h)
            }
        };

        // For dynamic scratchpads, update margin from current position before hiding
        if is_dynamic && !is_visible {
            let (output_width, output_height) = self.niri.get_output_size().await?;
            let new_margin = window_utils::extract_margin(
                config.direction,
                output_width,
                output_height,
                current_width,
                current_height,
                current_x,
                current_y,
            );
            debug!(
                "Updating dynamic scratchpad '{}' margin to {}",
                name, new_margin
            );
            config.margin = new_margin;
            // Update state with new margin
            if let Some(state) = self.states.get_mut(name) {
                state.config.margin = new_margin;
            }
        }

        let (target_x, target_y, target_width, target_height) = if is_dynamic {
            // For dynamic scratchpads, use current size to calculate target position
            let (tx, ty) = self
                .get_target_position(&config, current_width, current_height, is_visible)
                .await?;
            (tx, ty, current_width, current_height)
        } else {
            // For configured scratchpads, use config size
            self.get_target_geometry(&config, is_visible).await?
        };

        // Only resize for non-dynamic scratchpads when showing
        if is_visible && !is_dynamic {
            self.niri.resize_floating_window(window_id, target_width, target_height).await?;
        }

        window_utils::move_window_to_position(
            &self.niri, window_id, current_x, current_y, target_x, target_y,
        )
        .await?;
        tokio::time::sleep(Duration::from_millis(100)).await;

        if is_visible {
            window_utils::focus_window(self.niri.clone(), window_id).await?;
        } else {
            // Restore focus FIRST before moving the window to another workspace.
            // This prevents Niri from following the focused window to the target workspace.
            let previous_focused = {
                let state = self.states.get_mut(name).context("State not found")?;
                state.previous_focused_window.take()
            };
            if let Some(id) = previous_focused {
                debug!("Restoring focus to window {}", id);
                if let Err(e) = window_utils::focus_window(self.niri.clone(), id).await {
                    log::warn!("Failed to restore focus to window {}: {}", id, e);
                }
            }

            // After hiding and restoring focus, optionally move to a specific workspace if configured
            if let Some(ref workspace) = target_workspace {
                info!(
                    "Moving hidden scratchpad window {} to workspace {}",
                    window_id, workspace
                );
                if let Err(e) = self.niri.move_window_to_workspace(window_id, workspace).await {
                    log::warn!(
                        "Failed to move hidden scratchpad to workspace {}: {}",
                        workspace,
                        e
                    );
                }

                if should_tile {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    info!(
                        "Tiling hidden scratchpad window {} in workspace {}",
                        window_id, workspace
                    );
                    if let Err(e) = self.niri.set_window_floating(window_id, false).await {
                        log::warn!(
                            "Failed to tile scratchpad window in workspace {}: {}",
                            workspace,
                            e
                        );
                    }
                }
            }
        }

        Ok(())
    }

    async fn ensure_window_id(&mut self, name: &str) -> Result<u64> {
        let state = self.states.get_mut(name).context("State not found")?;

        if let Some(window_id) = state.window_id {
            // Fetch windows once and reuse for existence check
            let windows = self.niri.get_windows_raw().await?;
            if window_utils::window_exists_in_cache(&windows, window_id) {
                return Ok(window_id);
            }
            debug!(
                "Scratchpad window {} no longer exists, clearing ID",
                window_id
            );
            state.window_id = None;
            state.is_visible = false;
        }

        // For dynamic scratchpads, if the specific window is gone, we don't try to find/launch another one.
        if state.is_dynamic {
            let msg = format!("Dynamic scratchpad '{}' window no longer exists", name);
            self.states.remove(name);
            anyhow::bail!(msg);
        }

        info!("Finding or launching window for scratchpad {}", name);
        let config = state.config.clone();
        let patterns = vec![config.app_id.clone()];
        let matcher = WindowMatcher::new(Some(&patterns), None);

        let (window_id, is_newly_launched) = if let Some(window) =
            window_utils::find_window_by_matcher(self.niri.clone(), &matcher, &self.matcher_cache)
                .await?
        {
            (window.id, false)
        } else {
            window_utils::launch_application(&config.command).await?;
            let window = window_utils::wait_for_window(
                self.niri.clone(),
                &config.app_id,
                name,
                50,
                &self.matcher_cache,
            )
            .await?
            .context("Failed to launch/find window")?;
            (window.id, true)
        };

        if is_newly_launched {
            self.setup_window(window_id, &config).await?;
        }
        let state = self.states.get_mut(name).unwrap();
        state.window_id = Some(window_id);

        // Delegate sticky behavior to the sticky plugin via global registry
        if config.sticky {
            register_sticky_window(window_id, true);
        }

        Ok(window_id)
    }

    async fn toggle(
        &mut self,
        name: &str,
        config: Option<ScratchpadConfig>,
        global_move_to_workspace: Option<String>,
        global_tile_in_target: bool,
    ) -> Result<()> {
        // 1. Ensure state exists
        if !self.states.contains_key(name) {
            let config = config.context("No config provided for new scratchpad")?;
            self.states.insert(
                name.to_string(),
                ScratchpadState {
                    window_id: None,
                    is_visible: false,
                    previous_focused_window: None,
                    config,
                    is_dynamic: false,
                },
            );
        }

        // 2. Ensure window exists
        let window_id = self.ensure_window_id(name).await?;

        // 3. Resolve target workspace and tiling preference
        let (target_workspace, should_tile) = {
            let state = self.states.get(name).unwrap();
            let tw = state
                .config
                .move_to_workspace
                .clone()
                .or_else(|| global_move_to_workspace.clone());
            let st = state.config.tile_in_target_workspace.unwrap_or(global_tile_in_target);
            (tw, st)
        };

        // 4. Get workspace and window info
        let (current_workspace, windows) =
            window_utils::get_workspace_and_windows(&self.niri).await?;

        let is_in_target_ws = target_workspace
            .as_ref()
            .map(|tw| matches_workspace(&current_workspace, tw))
            .unwrap_or(false);

        let window_in_current_ws = windows.iter().any(|w| {
            w.id == window_id && window_utils::is_window_in_workspace(w, &current_workspace)
        });
        // 5. Singleton behavior:
        // When in target workspace and should_tile is enabled
        if is_in_target_ws && should_tile {
            info!(
                "Scratchpad '{}' is in target workspace {}, handling as singleton",
                name, current_workspace.name
            );

            // If window is currently elsewhere, move it here
            if !window_in_current_ws {
                if let Some(ref target) = target_workspace {
                    self.niri.move_window_to_workspace(window_id, target).await?;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }

            // Ensure window is in tiling mode in target workspace
            self.niri.set_window_floating(window_id, false).await?;
            tokio::time::sleep(Duration::from_millis(50)).await;

            let focused = self.niri.get_focused_window_id().await?;
            let state = self.states.get_mut(name).unwrap();
            state.is_visible = false;

            if focused == Some(window_id) {
                if let Some(prev) = state.previous_focused_window.take() {
                    let _ = window_utils::focus_window(self.niri.clone(), prev).await;
                    return Ok(());
                }
            } else {
                state.previous_focused_window = focused;
                window_utils::focus_window(self.niri.clone(), window_id).await?;
            }
            return Ok(());
        }

        // 6. Normal Scratchpad mode (floating show / hide)
        // Collect all scratchpad window IDs before getting mutable borrow
        let scratchpad_window_ids: Vec<u64> =
            self.states.values().filter_map(|s| s.window_id).collect();

        let state = self.states.get_mut(name).unwrap();

        if state.is_visible {
            if window_in_current_ws {
                // Floating window in current workspace: just hide it
                state.is_visible = false;
            } else {
                // Already visible but elsewhere, re-record focus and it will be moved in sync_state
                let focused = self.niri.get_focused_window_id().await?;
                state.previous_focused_window =
                    focused.filter(|&focused_id| !scratchpad_window_ids.contains(&focused_id));
            }
        } else {
            let focused = self.niri.get_focused_window_id().await?;
            state.previous_focused_window =
                focused.filter(|&focused_id| !scratchpad_window_ids.contains(&focused_id));
            state.is_visible = true;
        }

        // 7. Sync
        self.sync_state(name, target_workspace, should_tile).await
    }

    async fn add_current_window(
        &mut self,
        name: &str,
        direction: Direction,
        default_size: &str,
        default_margin: u32,
        swallow_to_focus: bool,
    ) -> Result<()> {
        let window = window_utils::get_focused_window(&self.niri).await?;
        let app_id = window
            .app_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("No app_id for current window"))?;

        // Check if scratchpad already exists
        if let Some(state) = self.states.get(name) {
            if let Some(wid) = state.window_id {
                if window_utils::window_exists(&self.niri, wid).await? {
                    // Window already exists, execute toggle logic
                    debug!(
                        "Scratchpad '{}' already exists with window {}, executing toggle",
                        name, wid
                    );
                    return self.toggle(name, None, None, false).await;
                }
            }
        }

        let config = ScratchpadConfig {
            direction,
            command: format!("# Window {} added dynamically", window.id),
            app_id,
            size: default_size.to_string(),
            margin: default_margin,
            swallow_to_focus,
            sticky: false,
            auto_hide_on_focus_loss: false,
            refocus: false,
            move_to_workspace: None,
            tile_in_target_workspace: None,
        };

        self.setup_window(window.id, &config).await?;

        self.states.insert(
            name.to_string(),
            ScratchpadState {
                window_id: Some(window.id),
                is_visible: false,
                previous_focused_window: None,
                config,
                is_dynamic: true,
            },
        );

        Ok(())
    }

    // Sticky behavior is delegated to the sticky plugin via global registry.
    // Scratchpads only register/unregister windows; sticky plugin handles workspace following.

    async fn handle_focus_loss(
        &mut self,
        focused_window_id: u64,
        move_to_workspace: Option<String>,
        tile_in_target_workspace: bool,
    ) -> Result<()> {
        // If focus shifted to another scratchpad, do not auto-hide any scratchpad.
        // This allows multiple scratchpads to be open at the same time.
        let scratchpad_window_ids: Vec<u64> =
            self.states.values().filter_map(|s| s.window_id).collect();
        if scratchpad_window_ids.contains(&focused_window_id) {
            return Ok(());
        }

        // Get current focused workspace and all windows
        let (current_workspace, windows) =
            window_utils::get_workspace_and_windows(&self.niri).await?;

        // Only auto-hide scratchpads that are currently in the CURRENT workspace.
        // If the user navigated to another workspace, scratchpads on the previous workspace
        // stay in place undisturbed.
        let names_to_hide: Vec<String> = self
            .states
            .iter()
            .filter(|(_, state)| {
                state.config.auto_hide_on_focus_loss
                    && state.is_visible
                    && state.window_id.is_some()
                    && state.window_id != Some(focused_window_id)
            })
            .filter(|(_, state)| {
                if let Some(wid) = state.window_id {
                    windows.iter().any(|w| {
                        w.id == wid
                            && w.floating
                            && window_utils::is_window_in_workspace(w, &current_workspace)
                    })
                } else {
                    false
                }
            })
            .map(|(name, _)| name.clone())
            .collect();

        for name in names_to_hide {
            info!(
                "Auto-hiding scratchpad '{}' due to focus loss on workspace {}",
                name, current_workspace.name
            );
            let (target_ws, should_tile) = {
                let state = self.states.get_mut(&name).unwrap();
                state.is_visible = false;
                let tw =
                    state.config.move_to_workspace.clone().or_else(|| move_to_workspace.clone());
                let st = state.config.tile_in_target_workspace.unwrap_or(tile_in_target_workspace);
                (tw, st)
            };

            if let Err(e) = self.sync_state(&name, target_ws, should_tile).await {
                warn!("Failed to auto-hide scratchpad '{}': {}", name, e);
            }
        }
        Ok(())
    }
}

/// Scratchpads plugin that wraps ScratchpadManager
pub struct ScratchpadsPlugin {
    manager: ScratchpadManager,
    config: ScratchpadsPluginConfig,
}

#[async_trait]
impl crate::plugins::Plugin for ScratchpadsPlugin {
    type Config = ScratchpadsPluginConfig;

    fn new(niri: NiriIpc, config: ScratchpadsPluginConfig) -> Self {
        let count = config.scratchpads.len();
        info!("Scratchpads plugin initialized with {} scratchpads", count);

        let mut manager = ScratchpadManager::new(niri);
        for (name, s_config) in &config.scratchpads {
            manager.states.insert(
                name.clone(),
                ScratchpadState {
                    window_id: None,
                    is_visible: false,
                    previous_focused_window: None,
                    config: s_config.clone(),
                    is_dynamic: false,
                },
            );
        }

        Self { manager, config }
    }

    async fn update_config(&mut self, config: ScratchpadsPluginConfig) -> Result<()> {
        info!("Updating scratchpads plugin configuration");

        // Merge configs
        for (name, s_config) in &config.scratchpads {
            if let Some(state) = self.manager.states.get_mut(name) {
                state.config = s_config.clone();
                state.is_dynamic = false; // It's in the config now
            } else {
                self.manager.states.insert(
                    name.clone(),
                    ScratchpadState {
                        window_id: None,
                        is_visible: false,
                        previous_focused_window: None,
                        config: s_config.clone(),
                        is_dynamic: false,
                    },
                );
            }
        }

        // Remove old states that are not dynamic and not in the new config
        self.manager
            .states
            .retain(|name, state| state.is_dynamic || config.scratchpads.contains_key(name));

        self.config = config;

        // Clear matcher cache to reflect potential regex changes in config
        self.manager.matcher_cache.clear_cache();

        Ok(())
    }

    async fn handle_ipc_request(&mut self, request: &IpcRequest) -> Result<Option<Result<()>>> {
        match request {
            IpcRequest::ScratchpadToggle { name } => {
                info!("Handling scratchpad toggle for: {}", name);

                let config = self.config.scratchpads.get(name).cloned();
                match self
                    .manager
                    .toggle(
                        name,
                        config,
                        self.config.move_to_workspace.clone(),
                        self.config.tile_in_target_workspace,
                    )
                    .await
                {
                    Ok(_) => Ok(Some(Ok(()))),
                    Err(e) => {
                        let error_msg = format!("Scratchpad '{}' error: {}", name, e);
                        send_notification("piri", &error_msg);
                        Err(e)
                    }
                }
            }
            IpcRequest::ScratchpadAdd {
                name,
                direction,
                swallow_to_focus,
            } => {
                info!(
                    "Handling scratchpad add for: {} with direction: {}, swallow_to_focus: {}",
                    name, direction, swallow_to_focus
                );

                let direction =
                    direction.parse().map_err(|e| anyhow::anyhow!("Invalid direction: {}", e))?;

                self.manager
                    .add_current_window(
                        name,
                        direction,
                        &self.config.default_size,
                        self.config.default_margin,
                        *swallow_to_focus,
                    )
                    .await?;

                Ok(Some(Ok(())))
            }
            _ => Ok(None), // Not handled by this plugin
        }
    }

    async fn handle_event(&mut self, event: &PiriEvent, _niri: &NiriIpc) -> Result<()> {
        if let PiriEvent::WindowFocusChanged {
            id: Some(window_id),
        } = event
        {
            self.manager
                .handle_focus_loss(
                    *window_id,
                    self.config.move_to_workspace.clone(),
                    self.config.tile_in_target_workspace,
                )
                .await?;
        }
        Ok(())
    }

    fn is_interested_in_event(&self, event: &PiriEvent) -> bool {
        let has_auto_hide = self.manager.states.values().any(|s| s.config.auto_hide_on_focus_loss);
        if !has_auto_hide {
            return false;
        }

        matches!(event, PiriEvent::WindowFocusChanged { id: Some(_) })
    }
}
