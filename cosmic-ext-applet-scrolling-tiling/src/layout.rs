// Copyright 2026 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

use cosmic_comp_config::TilingEngine;

/// The layout presented for the active workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceLayoutMode {
    Floating,
    Tiling,
    Scrolling,
}

/// Independent changes required to reach a requested layout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LayoutTransition {
    pub tiling_engine: Option<TilingEngine>,
    pub workspace_tiled: Option<bool>,
}

pub fn derive_layout_mode(
    workspace_tiled: bool,
    tiling_engine: TilingEngine,
) -> WorkspaceLayoutMode {
    if !workspace_tiled {
        WorkspaceLayoutMode::Floating
    } else {
        match tiling_engine {
            TilingEngine::Classic => WorkspaceLayoutMode::Tiling,
            TilingEngine::Scrolling => WorkspaceLayoutMode::Scrolling,
        }
    }
}

pub fn plan_layout_transition(
    workspace_tiled: bool,
    tiling_engine: TilingEngine,
    requested: WorkspaceLayoutMode,
) -> LayoutTransition {
    let requested_engine = match requested {
        WorkspaceLayoutMode::Floating => None,
        WorkspaceLayoutMode::Tiling => Some(TilingEngine::Classic),
        WorkspaceLayoutMode::Scrolling => Some(TilingEngine::Scrolling),
    };

    LayoutTransition {
        tiling_engine: requested_engine.filter(|engine| *engine != tiling_engine),
        workspace_tiled: match requested {
            WorkspaceLayoutMode::Floating if workspace_tiled => Some(false),
            WorkspaceLayoutMode::Tiling | WorkspaceLayoutMode::Scrolling if !workspace_tiled => {
                Some(true)
            }
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floating_workspace_hides_the_global_engine() {
        assert_eq!(
            derive_layout_mode(false, TilingEngine::Classic),
            WorkspaceLayoutMode::Floating
        );
        assert_eq!(
            derive_layout_mode(false, TilingEngine::Scrolling),
            WorkspaceLayoutMode::Floating
        );
    }

    #[test]
    fn tiled_workspace_uses_the_global_engine() {
        assert_eq!(
            derive_layout_mode(true, TilingEngine::Classic),
            WorkspaceLayoutMode::Tiling
        );
        assert_eq!(
            derive_layout_mode(true, TilingEngine::Scrolling),
            WorkspaceLayoutMode::Scrolling
        );
    }

    #[test]
    fn floating_only_changes_the_workspace_state() {
        assert_eq!(
            plan_layout_transition(true, TilingEngine::Scrolling, WorkspaceLayoutMode::Floating,),
            LayoutTransition {
                tiling_engine: None,
                workspace_tiled: Some(false),
            }
        );
    }

    #[test]
    fn tiled_modes_select_the_engine_and_enable_the_workspace() {
        assert_eq!(
            plan_layout_transition(false, TilingEngine::Scrolling, WorkspaceLayoutMode::Tiling,),
            LayoutTransition {
                tiling_engine: Some(TilingEngine::Classic),
                workspace_tiled: Some(true),
            }
        );
        assert_eq!(
            plan_layout_transition(false, TilingEngine::Classic, WorkspaceLayoutMode::Scrolling,),
            LayoutTransition {
                tiling_engine: Some(TilingEngine::Scrolling),
                workspace_tiled: Some(true),
            }
        );
    }

    #[test]
    fn switching_tiled_engines_does_not_change_workspace_tiling() {
        assert_eq!(
            plan_layout_transition(true, TilingEngine::Classic, WorkspaceLayoutMode::Scrolling,),
            LayoutTransition {
                tiling_engine: Some(TilingEngine::Scrolling),
                workspace_tiled: None,
            }
        );
    }

    #[test]
    fn selecting_the_derived_mode_is_a_no_op() {
        for (workspace_tiled, engine) in [
            (false, TilingEngine::Classic),
            (false, TilingEngine::Scrolling),
            (true, TilingEngine::Classic),
            (true, TilingEngine::Scrolling),
        ] {
            let requested = derive_layout_mode(workspace_tiled, engine);
            assert_eq!(
                plan_layout_transition(workspace_tiled, engine, requested),
                LayoutTransition::default()
            );
        }
    }

    #[test]
    fn independent_updates_converge_in_either_order() {
        let requested = WorkspaceLayoutMode::Scrolling;
        let transition = plan_layout_transition(false, TilingEngine::Classic, requested);

        assert_eq!(
            derive_layout_mode(false, transition.tiling_engine.unwrap()),
            WorkspaceLayoutMode::Floating
        );
        assert_eq!(
            derive_layout_mode(
                transition.workspace_tiled.unwrap(),
                transition.tiling_engine.unwrap(),
            ),
            requested
        );

        assert_eq!(
            derive_layout_mode(transition.workspace_tiled.unwrap(), TilingEngine::Classic),
            WorkspaceLayoutMode::Tiling
        );
        assert_eq!(
            derive_layout_mode(
                transition.workspace_tiled.unwrap(),
                transition.tiling_engine.unwrap(),
            ),
            requested
        );
    }
}
