// SPDX-License-Identifier: GPL-3.0-only

use crate::{
    backend::render::{
        ACTIVE_GROUP_COLOR, BackdropShader, GROUP_COLOR, IndicatorShader, Key, Usage,
        element::AsGlowRenderer,
    },
    shell::{
        CosmicSurface, Direction, FocusResult, MoveResult, OverviewMode, ResizeMode, Trigger,
        element::{
            CosmicMapped, CosmicMappedRenderElement, CosmicStack, CosmicWindow,
            resize_indicator::ResizeIndicator,
            stack::{
                CosmicStackRenderElement, MoveResult as StackMoveResult,
                TAB_HEIGHT as STACK_TAB_HEIGHT,
            },
            swap_indicator::SwapIndicator,
            window::CosmicWindowRenderElement,
        },
        focus::{
            FocusStackMut, FocusTarget,
            target::{KeyboardFocusTarget, PointerFocusTarget, WindowGroup},
        },
        grabs::ResizeEdge,
        layout::{
            Orientation,
            scrolling::{
                ColumnDropTarget, ColumnResizeAnchor, FocusNavigation, ScrollingLayout, TileMove,
            },
        },
    },
    utils::{prelude::*, tween::EaseRectangle},
    wayland::{
        handlers::xdg_shell::popup::get_popup_toplevel,
        protocols::{
            toplevel_info::{
                toplevel_enter_output, toplevel_enter_workspace, toplevel_leave_output,
                toplevel_leave_workspace,
            },
            workspace::WorkspaceHandle,
        },
    },
};

use cosmic_comp_config::{AppearanceConfig, TilingEngine};
use cosmic_settings_config::shortcuts::action::{FocusDirection, ResizeDirection};
use id_tree::{InsertBehavior, MoveBehavior, Node, NodeId, NodeIdError, RemoveBehavior, Tree};
use keyframe::{
    ease,
    functions::{EaseInOutCubic, Linear},
};
use smallvec::SmallVec;
use smithay::{
    backend::{
        drm::DrmNode,
        renderer::{
            element::{
                Id, RenderElement,
                utils::{
                    ConstrainAlign, ConstrainScaleBehavior, RescaleRenderElement,
                    constrain_render_elements,
                },
            },
            glow::GlowRenderer,
        },
    },
    desktop::{PopupKind, WindowSurfaceType, layer_map_for_output, space::SpaceElement},
    input::Seat,
    output::Output,
    reexports::wayland_server::Client,
    utils::{IsAlive, Logical, Physical, Point, Rectangle, Scale, Size},
    wayland::{compositor::add_blocker, seat::WaylandFocus},
};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Weak},
    time::{Duration, Instant},
};
use tracing::trace;
use wayland_backend::server::ClientId;

mod blocker;
mod grabs;
pub use self::blocker::*;
pub use self::grabs::*;

pub const ANIMATION_DURATION: Duration = Duration::from_millis(200);
pub const MINIMIZE_ANIMATION_DURATION: Duration = Duration::from_millis(320);
pub const MOUSE_ANIMATION_DELAY: Duration = Duration::from_millis(150);
pub const INITIAL_MOUSE_ANIMATION_DELAY: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, PartialEq)]
pub struct NodeDesc {
    pub handle: WorkspaceHandle,
    pub node: NodeId,
    pub stack_window: Option<CosmicSurface>,
    pub focus_stack: Vec<NodeId>,
}

#[derive(Debug, Clone, PartialEq)]
enum TargetZone {
    Initial,
    InitialPlaceholder(NodeId),
    WindowStack(NodeId, Rectangle<i32, Local>),
    WindowSplit(NodeId, Direction),
    GroupEdge(NodeId, Direction),
    GroupInterior(NodeId, usize),
}

impl TargetZone {
    fn is_window_zone(&self) -> bool {
        matches!(
            self,
            TargetZone::WindowStack(..) | TargetZone::WindowSplit(_, _)
        )
    }
}

fn scrolling_drop_target(
    tree: &Tree<Data>,
    target: Option<&TargetZone>,
) -> ColumnDropTarget<NodeId> {
    let boundary_tile = |node_id: &NodeId, first: bool| {
        let tiles = tree.traverse_pre_order_ids(node_id).ok()?.filter(|id| {
            matches!(
                tree.get(id).map(|node| node.data()),
                Ok(Data::Mapped { .. })
                    | Ok(Data::Placeholder {
                        type_: PlaceholderType::GrabbedWindow,
                        ..
                    })
            )
        });
        if first {
            tiles.into_iter().next()
        } else {
            tiles.last()
        }
    };

    match target {
        None | Some(TargetZone::InitialPlaceholder(_)) => ColumnDropTarget::Original,
        Some(TargetZone::Initial) => ColumnDropTarget::Last,
        Some(TargetZone::WindowStack(..)) => ColumnDropTarget::Discard,
        Some(TargetZone::WindowSplit(node_id, Direction::Left)) => {
            ColumnDropTarget::Before(node_id.clone())
        }
        Some(TargetZone::WindowSplit(node_id, Direction::Right)) => {
            ColumnDropTarget::After(node_id.clone())
        }
        Some(TargetZone::WindowSplit(node_id, Direction::Up)) => {
            ColumnDropTarget::Above(node_id.clone())
        }
        Some(TargetZone::WindowSplit(node_id, Direction::Down)) => {
            ColumnDropTarget::Below(node_id.clone())
        }
        Some(TargetZone::GroupEdge(node_id, Direction::Left)) => boundary_tile(node_id, true)
            .map(ColumnDropTarget::Before)
            .unwrap_or(ColumnDropTarget::First),
        Some(TargetZone::GroupEdge(node_id, Direction::Right)) => boundary_tile(node_id, false)
            .map(ColumnDropTarget::After)
            .unwrap_or(ColumnDropTarget::Last),
        Some(TargetZone::GroupEdge(node_id, Direction::Up)) => boundary_tile(node_id, true)
            .map(ColumnDropTarget::Above)
            .unwrap_or(ColumnDropTarget::First),
        Some(TargetZone::GroupEdge(node_id, Direction::Down)) => boundary_tile(node_id, false)
            .map(ColumnDropTarget::Below)
            .unwrap_or(ColumnDropTarget::Last),
        Some(TargetZone::GroupInterior(node_id, index)) => {
            let Some((orientation, children)) = tree.get(node_id).ok().and_then(|node| {
                if let Data::Group { orientation, .. } = node.data() {
                    Some((*orientation, node.children().to_vec()))
                } else {
                    None
                }
            }) else {
                return ColumnDropTarget::Last;
            };
            let previous = children
                .get(*index)
                .and_then(|child| boundary_tile(child, false));
            let next = children
                .get(index + 1)
                .and_then(|child| boundary_tile(child, true));
            match orientation {
                Orientation::Vertical => previous
                    .map(ColumnDropTarget::After)
                    .or_else(|| next.map(ColumnDropTarget::Before))
                    .unwrap_or(ColumnDropTarget::Last),
                Orientation::Horizontal => previous
                    .map(ColumnDropTarget::Below)
                    .or_else(|| next.map(ColumnDropTarget::Above))
                    .unwrap_or(ColumnDropTarget::Last),
            }
        }
    }
}

fn scrolling_edge_focus_result(direction: FocusDirection) -> FocusResult {
    match direction {
        FocusDirection::Up | FocusDirection::Down => FocusResult::None,
        _ => FocusResult::Handled,
    }
}

#[derive(Debug, Clone, Default)]
struct TreeQueue {
    trees: VecDeque<(Tree<Data>, Duration, Option<TilingBlocker>)>,
    animation_start: Option<Instant>,
    retired_blocker: Option<TilingBlocker>,
}

impl TreeQueue {
    pub fn push_tree(
        &mut self,
        tree: Tree<Data>,
        duration: impl Into<Option<Duration>>,
        blocker: Option<TilingBlocker>,
    ) {
        self.trees
            .push_back((tree, duration.into().unwrap_or(Duration::ZERO), blocker))
    }

    /// Keep direct-manipulation updates bounded while the compositor has not
    /// started presenting the previous zero-duration target yet.
    fn push_direct_tree(&mut self, tree: Tree<Data>, blocker: Option<TilingBlocker>) {
        if self.animation_start.is_none()
            && self.trees.len() > 1
            && self
                .trees
                .back()
                .is_some_and(|(_, duration, _)| *duration == Duration::ZERO)
        {
            let (queued_tree, _, queued_blocker) = self.trees.back_mut().unwrap();
            let retired = queued_blocker.take();
            *queued_tree = tree;
            *queued_blocker = blocker;
            if let Some(retired) = retired {
                self.retire_blocker(retired);
            }
            return;
        }
        self.push_tree(tree, Duration::ZERO, blocker);
    }

    fn retire_blocker(&mut self, blocker: TilingBlocker) {
        if let Some(retired) = self.retired_blocker.as_mut() {
            retired.absorb(blocker);
        } else {
            self.retired_blocker = Some(blocker);
        }
    }

    fn retire_queued_blockers(&mut self) {
        let blockers = self
            .trees
            .iter_mut()
            .filter_map(|(_, _, blocker)| blocker.take())
            .collect::<Vec<_>>();
        for blocker in blockers {
            self.retire_blocker(blocker);
        }
    }

    /// Snapshot the rectangles currently being rendered. The target topology
    /// is retained so repeated moves keep stable node identities, while every
    /// node shared with the reference tree starts at its current visual rect.
    fn current_visual_tree(&self) -> Tree<Data> {
        let Some(reference) = self.trees.front().map(|entry| &entry.0) else {
            return Tree::new();
        };
        let Some(start) = self.animation_start else {
            return reference.copy_clone();
        };
        let Some((target, duration, _)) = self.trees.get(1) else {
            return reference.copy_clone();
        };

        let percentage = if *duration == Duration::ZERO {
            1.0
        } else {
            let elapsed = Instant::now().duration_since(start).as_secs_f32();
            let total = duration.as_secs_f32();
            ease(EaseInOutCubic, 0.0, 1.0, (elapsed / total).clamp(0.0, 1.0))
        };
        let mut visual = target.copy_clone();
        let Some(root) = target.root_node_id() else {
            return visual;
        };
        for node_id in target
            .traverse_pre_order_ids(root)
            .expect("target tree root must be traversable")
            .collect::<Vec<_>>()
        {
            let Ok(new) = target.get(&node_id) else {
                continue;
            };
            let minimized_from = match new.data() {
                Data::Mapped {
                    minimize_rect: Some(rect),
                    ..
                } => Some(*rect),
                _ => None,
            };
            let old_geometry = minimized_from.or_else(|| {
                reference
                    .get(&node_id)
                    .ok()
                    .map(|old| *old.data().geometry())
            });
            let Some(old_geometry) = old_geometry else {
                continue;
            };
            let geometry = if minimized_from.is_some() {
                ease(
                    EaseInOutCubic,
                    EaseRectangle(old_geometry),
                    EaseRectangle(*new.data().geometry()),
                    percentage,
                )
                .unwrap()
            } else {
                ease(
                    Linear,
                    EaseRectangle(old_geometry),
                    EaseRectangle(*new.data().geometry()),
                    percentage,
                )
                .unwrap()
            };
            if let Ok(node) = visual.get_mut(&node_id) {
                node.data_mut().update_geometry(geometry);
                if let Data::Mapped { minimize_rect, .. } = node.data_mut() {
                    *minimize_rect = None;
                }
            }
        }
        visual
    }

    /// Stop any queued transition at its current visual rectangles. Configure
    /// blockers remain tracked independently until their surfaces catch up.
    fn interrupt(&mut self) -> Tree<Data> {
        let visual = self.current_visual_tree();
        self.retire_queued_blockers();
        self.trees.clear();
        self.trees
            .push_back((visual.copy_clone(), Duration::ZERO, None));
        self.animation_start = None;
        visual
    }

    fn prepare_direct_tree(&mut self) -> Tree<Data> {
        if self.animation_start.is_some() {
            return self.interrupt();
        }
        if self.trees.len() > 1 {
            if self
                .trees
                .back()
                .is_some_and(|(_, duration, _)| *duration == Duration::ZERO)
            {
                return self.trees.back().unwrap().0.copy_clone();
            }
            return self.interrupt();
        }
        self.trees.back().unwrap().0.copy_clone()
    }

    /// Animate toward the newest authoritative tree without waiting behind an
    /// older Scrolling transition or allowing the queue to grow.
    fn push_retargetable_tree(
        &mut self,
        mut tree: Tree<Data>,
        duration: Duration,
        blocker: Option<TilingBlocker>,
    ) {
        if self.animation_start.is_some() || self.trees.len() > 1 {
            clear_minimize_rects(&mut tree);
            self.interrupt();
        }
        self.push_tree(tree, duration, blocker);
    }

    fn update_retired_blocker(&mut self, clients: &mut HashMap<ClientId, Client>) {
        let Some(blocker) = self.retired_blocker.as_ref() else {
            return;
        };
        if blocker.is_ready() {
            clients.extend(blocker.clients());
        }
        if blocker.is_processed() {
            self.retired_blocker = None;
        }
    }
}

fn clear_minimize_rects(tree: &mut Tree<Data>) {
    let Some(root) = tree.root_node_id().cloned() else {
        return;
    };
    for node_id in tree
        .traverse_pre_order_ids(&root)
        .expect("tree root must be traversable")
        .collect::<Vec<_>>()
    {
        if let Ok(node) = tree.get_mut(&node_id)
            && let Data::Mapped { minimize_rect, .. } = node.data_mut()
        {
            *minimize_rect = None;
        }
    }
}

#[derive(Debug, Clone)]
struct ScrollingLiveResize {
    target: ScrollingResizeTarget,
    tiles: Vec<NodeId>,
    finishing: bool,
}

#[derive(Debug, Clone)]
pub struct TilingLayout {
    output: Output,
    queue: TreeQueue,
    tiling_engine: TilingEngine,
    scrolling: ScrollingLayout,
    scrolling_live_resize: Option<ScrollingLiveResize>,
    backdrop_id: Id,
    swapping_stack_surface_id: Id,
    last_overview_hover: Option<(Option<Instant>, TargetZone)>,
    pub theme: cosmic::Theme,
    pub appearance: AppearanceConfig,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PillIndicator {
    Outer(Direction),
    Inner(usize),
}

#[derive(Debug, Clone)]
pub enum Data {
    Group {
        orientation: Orientation,
        sizes: Vec<i32>,
        last_geometry: Rectangle<i32, Local>,
        alive: Arc<()>,
        pill_indicator: Option<PillIndicator>,
    },
    Mapped {
        mapped: CosmicMapped,
        last_geometry: Rectangle<i32, Local>,
        minimize_rect: Option<Rectangle<i32, Local>>,
    },
    Placeholder {
        id: Id,
        last_geometry: Rectangle<i32, Local>,
        type_: PlaceholderType,
    },
}

#[derive(Debug, Clone)]
pub enum PlaceholderType {
    GrabbedWindow,
    DropZone,
}

impl Data {
    fn new_group(orientation: Orientation, geo: Rectangle<i32, Local>) -> Data {
        Data::Group {
            orientation,
            sizes: vec![
                match orientation {
                    Orientation::Vertical => geo.size.w / 2,
                    Orientation::Horizontal => geo.size.h / 2,
                };
                2
            ],
            last_geometry: geo,
            alive: Arc::new(()),
            pill_indicator: None,
        }
    }

    fn is_group(&self) -> bool {
        matches!(self, Data::Group { .. })
    }
    fn is_mapped(&self, mapped: Option<&CosmicMapped>) -> bool {
        match mapped {
            Some(m) => matches!(self, Data::Mapped { mapped, .. } if m == mapped),
            None => matches!(self, Data::Mapped { .. }),
        }
    }
    fn is_stack(&self) -> bool {
        match self {
            Data::Mapped { mapped, .. } => mapped.is_stack(),
            _ => false,
        }
    }
    fn is_placeholder(&self) -> bool {
        matches!(self, Data::Placeholder { .. })
    }

    fn orientation(&self) -> Orientation {
        match self {
            Data::Group { orientation, .. } => *orientation,
            _ => panic!("Not a group"),
        }
    }

    fn add_window(&mut self, idx: usize) {
        match self {
            Data::Group {
                sizes,
                last_geometry,
                orientation,
                ..
            } => {
                let last_length = match orientation {
                    Orientation::Horizontal => last_geometry.size.h,
                    Orientation::Vertical => last_geometry.size.w,
                };
                let equal_sizing = last_length / (sizes.len() as i32 + 1); // new window size
                let remainder = last_length - equal_sizing; // size for the rest of the windowns

                for size in sizes.iter_mut() {
                    *size = ((*size as f64 / last_length as f64) * remainder as f64).round() as i32;
                }
                let used_size: i32 = sizes.iter().sum();
                let new_size = last_length - used_size;

                sizes.insert(idx, new_size);
            }
            _ => panic!("Adding window to leaf?"),
        }
    }

    fn swap_windows(&mut self, i: usize, j: usize) {
        match self {
            Data::Group { sizes, .. } => {
                sizes.swap(i, j);
            }
            _ => panic!("Swapping windows to a leaf?"),
        }
    }

    fn remove_window(&mut self, idx: usize) {
        match self {
            Data::Group {
                sizes,
                last_geometry,
                orientation,
                ..
            } => {
                let last_length = match orientation {
                    Orientation::Horizontal => last_geometry.size.h,
                    Orientation::Vertical => last_geometry.size.w,
                };
                let old_size = sizes.remove(idx);
                let remaining_size: i32 = sizes.iter().sum();

                for size in sizes.iter_mut() {
                    *size +=
                        ((*size as f64 / remaining_size as f64) * old_size as f64).round() as i32;
                }
                let used_size: i32 = sizes.iter().sum();
                let overflow = last_length - used_size;
                if overflow != 0 {
                    *sizes.last_mut().unwrap() += overflow;
                }
            }
            _ => panic!("Added window to leaf?"),
        }
    }

    fn geometry(&self) -> &Rectangle<i32, Local> {
        match self {
            Data::Group { last_geometry, .. } => last_geometry,
            Data::Mapped { last_geometry, .. } => last_geometry,
            Data::Placeholder { last_geometry, .. } => last_geometry,
        }
    }

    pub(super) fn update_geometry(&mut self, geo: Rectangle<i32, Local>) {
        match self {
            Data::Group {
                orientation,
                sizes,
                last_geometry,
                ..
            } => {
                let previous_length = match orientation {
                    Orientation::Horizontal => last_geometry.size.h,
                    Orientation::Vertical => last_geometry.size.w,
                };
                let new_length = match orientation {
                    Orientation::Horizontal => geo.size.h,
                    Orientation::Vertical => geo.size.w,
                };

                sizes.iter_mut().for_each(|len| {
                    *len = (((*len as f64) / (previous_length as f64)) * (new_length as f64))
                        .round() as i32;
                });
                let sum: i32 = sizes.iter().sum();

                // fix rounding issues
                if sum != new_length {
                    let diff = new_length - sum;
                    *sizes.last_mut().unwrap() += diff;
                }
                *last_geometry = geo;
            }
            Data::Mapped { last_geometry, .. } | Data::Placeholder { last_geometry, .. } => {
                *last_geometry = geo;
            }
        }
    }

    fn len(&self) -> usize {
        match self {
            Data::Group { sizes, .. } => sizes.len(),
            _ => 1,
        }
    }
}

#[derive(Debug, Clone)]
enum FocusedNodeData {
    Group(Vec<NodeId>, Weak<()>),
    Window(CosmicMapped),
}

#[derive(Debug, Clone)]
pub struct RestoreTilingState {
    pub parent: Option<id_tree::NodeId>,
    pub sibling: Option<id_tree::NodeId>,
    pub orientation: Orientation,
    pub idx: usize,
    pub sizes: Vec<i32>,
}

impl TilingLayout {
    pub fn new(
        theme: cosmic::Theme,
        appearance: AppearanceConfig,
        output: &Output,
        tiling_engine: TilingEngine,
    ) -> TilingLayout {
        TilingLayout {
            queue: TreeQueue {
                trees: {
                    let mut queue = VecDeque::new();
                    queue.push_back((Tree::new(), Duration::ZERO, None));
                    queue
                },
                animation_start: None,
                retired_blocker: None,
            },
            output: output.clone(),
            tiling_engine,
            scrolling: ScrollingLayout::default(),
            scrolling_live_resize: None,
            backdrop_id: Id::new(),
            swapping_stack_surface_id: Id::new(),
            last_overview_hover: None,
            theme,
            appearance,
        }
    }

    pub fn mode(&self) -> TilingEngine {
        self.tiling_engine
    }

    /// Switch tiling engines without changing the tree, mapped windows, or
    /// scrolling column state.
    ///
    /// The newest queued tree is authoritative. Replacing the queue avoids
    /// rendering or waiting on geometry from the previous engine while the
    /// newly selected engine configures every mapped leaf immediately. The
    /// persistent scrolling model keeps surviving columns in their prior
    /// order; leaves first mapped while classic is active append in tree
    /// pre-order when scrolling resumes.
    pub fn set_mode(&mut self, tiling_engine: TilingEngine) {
        if self.tiling_engine == tiling_engine {
            return;
        }

        let gaps = self.gaps();
        let mut tree = self
            .queue
            .trees
            .back()
            .expect("tiling layout must always contain a tree")
            .0
            .copy_clone();
        if let Some(resize) = self.scrolling_live_resize.take() {
            Self::set_scrolling_tiles_resizing(&tree, &resize.tiles, false);
        }
        let mut retired_blockers = self
            .queue
            .trees
            .iter_mut()
            .filter_map(|(_, _, blocker)| blocker.take())
            .collect::<Vec<_>>();

        self.tiling_engine = tiling_engine;
        self.last_overview_hover = TilingLayout::clear_transient_tree_state(&mut tree)
            .map(|node_id| (None, TargetZone::InitialPlaceholder(node_id)));

        // update_positions_for sends the new geometry/configures. Both its
        // blocker and blockers from superseded queue entries still need to be
        // polled so their surface commit barriers can be released.
        if let Some(blocker) = self.update_positions_for(&mut tree, gaps) {
            retired_blockers.push(blocker);
        }
        self.queue.trees.clear();
        self.queue
            .trees
            .push_back((tree.copy_clone(), Duration::ZERO, None));
        for blocker in retired_blockers {
            self.queue
                .trees
                .push_back((tree.copy_clone(), Duration::ZERO, Some(blocker)));
        }
        self.queue.animation_start = None;
    }

    fn clear_transient_tree_state(tree: &mut Tree<Data>) -> Option<NodeId> {
        let Some(root) = tree.root_node_id().cloned() else {
            return None;
        };
        let mut grabbed_placeholder = None;

        for id in tree
            .traverse_pre_order_ids(&root)
            .expect("root node must be traversable")
            .collect::<Vec<_>>()
        {
            match tree.get_mut(&id).map(|node| node.data_mut()) {
                Ok(Data::Placeholder {
                    type_: PlaceholderType::GrabbedWindow,
                    ..
                }) => {
                    grabbed_placeholder.get_or_insert(id);
                }
                Ok(Data::Placeholder {
                    type_: PlaceholderType::DropZone,
                    ..
                }) => TilingLayout::unmap_internal(tree, &id),
                Ok(Data::Group { pill_indicator, .. }) => {
                    *pill_indicator = None;
                }
                Ok(Data::Mapped { minimize_rect, .. }) => {
                    *minimize_rect = None;
                }
                _ => {}
            }
        }

        grabbed_placeholder
    }

    pub fn set_output(&mut self, output: &Output) {
        let gaps = self.gaps();
        let mut tree = self.queue.trees.back().unwrap().0.copy_clone();

        for node in tree
            .root_node_id()
            .and_then(|root_id| tree.traverse_pre_order(root_id).ok())
            .into_iter()
            .flatten()
        {
            if let Data::Mapped { mapped, .. } = node.data() {
                mapped.output_leave(&self.output);
                mapped.output_enter(output, mapped.bbox());
            }
        }

        self.output = output.clone();
        let blocker = self.update_positions_for(&mut tree, gaps);
        self.queue.push_tree(tree, None, blocker);
    }

    pub fn map<'a>(
        &mut self,
        window: CosmicMapped,
        focus_stack: Option<impl Iterator<Item = &'a FocusTarget> + 'a>,
        direction: Option<Direction>,
    ) {
        window.output_enter(&self.output, window.bbox());
        window.set_bounds(self.output.geometry().size.as_logical());
        self.map_internal(window, focus_stack, direction, None);
    }

    /// Map a newly created toplevel. Scrolling uses this explicit lifecycle
    /// signal to place it beside the focused column; all restoration and
    /// transfer paths continue through `map`/`remap` and reconciliation.
    pub fn map_new_toplevel<'a>(
        &mut self,
        window: CosmicMapped,
        focus_stack: Option<impl Iterator<Item = &'a FocusTarget> + 'a>,
    ) {
        window.output_enter(&self.output, window.bbox());
        window.set_bounds(self.output.geometry().size.as_logical());

        let gaps = self.gaps();
        let mut tree = self.queue.trees.back().unwrap().0.copy_clone();
        let last_active = focus_stack
            .and_then(|focus_stack| TilingLayout::last_active_window(&tree, focus_stack))
            .map(|(node_id, _)| node_id);

        TilingLayout::map_to_tree(
            &mut tree,
            window.clone(),
            &self.output,
            last_active.clone(),
            None,
            None,
        );
        if self.tiling_engine == TilingEngine::Scrolling
            && let Some(new_id) = window.tiling_node_id.lock().unwrap().clone()
        {
            self.scrolling
                .insert_new_toplevel(new_id, last_active.as_ref());
        }

        let blocker = self.update_positions_for(&mut tree, gaps);
        self.queue.push_tree(tree, ANIMATION_DURATION, blocker);
    }

    pub fn map_internal<'a>(
        &mut self,
        window: impl Into<CosmicMapped>,
        focus_stack: Option<impl Iterator<Item = &'a FocusTarget> + 'a>,
        direction: Option<Direction>,
        minimize_rect: Option<Rectangle<i32, Local>>,
    ) {
        let gaps = self.gaps();

        let mut tree = self.queue.trees.back().unwrap().0.copy_clone();
        let last_active = focus_stack
            .and_then(|focus_stack| TilingLayout::last_active_window(&tree, focus_stack))
            .map(|(node_id, _)| node_id);
        let duration = if minimize_rect.is_some() {
            MINIMIZE_ANIMATION_DURATION
        } else {
            ANIMATION_DURATION
        };

        TilingLayout::map_to_tree(
            &mut tree,
            window,
            &self.output,
            last_active,
            direction,
            minimize_rect,
        );
        let blocker = self.update_positions_for(&mut tree, gaps);
        self.queue.push_tree(tree, duration, blocker);
    }

    pub fn remap<'a>(
        &mut self,
        window: CosmicMapped,
        from: Option<Rectangle<i32, Local>>,
        tiling_state: Option<RestoreTilingState>,
        focus_stack: Option<impl Iterator<Item = &'a FocusTarget> + 'a>,
    ) {
        let gaps = self.gaps();
        let mut tree = self.queue.trees.back().unwrap().0.copy_clone();
        window.output_enter(&self.output, window.bbox());
        window.set_bounds(self.output.geometry().size.as_logical());

        if let Some(RestoreTilingState {
            parent,
            sibling,
            orientation,
            idx,
            mut sizes,
        }) = tiling_state
        {
            if let Some(node) = parent.as_ref().and_then(|parent| tree.get_mut(parent).ok())
                && let Data::Group {
                    orientation: current_orientation,
                    sizes: current_sizes,
                    ..
                } = node.data_mut()
            {
                let parent_id = parent.unwrap();

                if *current_orientation == orientation && sizes.len() == current_sizes.len() + 1 {
                    let previous_length: i32 = sizes.iter().copied().sum();
                    let new_length: i32 = current_sizes.iter().copied().sum();
                    if previous_length != new_length {
                        // rescale sizes
                        sizes.iter_mut().for_each(|len| {
                            *len = (((*len as f64) / (previous_length as f64))
                                * (new_length as f64))
                                .round() as i32;
                        });
                        let sum: i32 = sizes.iter().sum();

                        // fix rounding issues
                        if sum != new_length {
                            let diff = new_length - sum;
                            *sizes.last_mut().unwrap() += diff;
                        }
                    }
                }

                *current_sizes = sizes;
                let new_node = Node::new(Data::Mapped {
                    mapped: window.clone(),
                    last_geometry: Rectangle::from_size((100, 100).into()),
                    minimize_rect: from,
                });
                let new_id = tree
                    .insert(new_node, InsertBehavior::UnderNode(&parent_id))
                    .unwrap();
                tree.make_nth_sibling(&new_id, idx).unwrap();
                *window.tiling_node_id.lock().unwrap() = Some(new_id);

                let blocker = self.update_positions_for(&mut tree, gaps);
                self.queue
                    .push_tree(tree, MINIMIZE_ANIMATION_DURATION, blocker);
                return;
            }

            if sibling
                .as_ref()
                .is_some_and(|sibling| tree.get(sibling).is_ok())
            {
                let sibling_id = sibling.unwrap();
                let new_node = Node::new(Data::Mapped {
                    mapped: window.clone(),
                    last_geometry: Rectangle::from_size((100, 100).into()),
                    minimize_rect: from,
                });

                let new_id = tree.insert(new_node, InsertBehavior::AsRoot).unwrap();
                let group_id =
                    TilingLayout::new_group(&mut tree, &sibling_id, &new_id, orientation).unwrap();
                tree.make_nth_sibling(&new_id, idx).unwrap();
                if let Data::Group {
                    sizes: default_sizes,
                    last_geometry,
                    ..
                } = tree.get_mut(&group_id).unwrap().data_mut()
                {
                    match orientation {
                        Orientation::Horizontal => {
                            last_geometry.size.h = sizes.iter().copied().sum()
                        }
                        Orientation::Vertical => last_geometry.size.w = sizes.iter().copied().sum(),
                    };
                    *default_sizes = sizes;
                }

                *window.tiling_node_id.lock().unwrap() = Some(new_id);

                let blocker = self.update_positions_for(&mut tree, gaps);
                self.queue
                    .push_tree(tree, MINIMIZE_ANIMATION_DURATION, blocker);
                return;
            }
        }

        // else add as new_window
        self.map_internal(window, focus_stack, None, from);
    }

    fn map_to_tree(
        tree: &mut Tree<Data>,
        window: impl Into<CosmicMapped>,
        output: &Output,
        node: Option<NodeId>,
        direction: Option<Direction>,
        minimize_rect: Option<Rectangle<i32, Local>>,
    ) {
        let window = window.into();
        let new_window = Node::new(Data::Mapped {
            mapped: window.clone(),
            last_geometry: Rectangle::from_size((100, 100).into()),
            minimize_rect,
        });

        let window_id = if let Some(direction) = direction {
            if let Some(root_id) = tree.root_node_id().cloned() {
                let orientation = match direction {
                    Direction::Left | Direction::Right => Orientation::Vertical,
                    Direction::Up | Direction::Down => Orientation::Horizontal,
                };

                let new_id = tree.insert(new_window, InsertBehavior::AsRoot).unwrap();
                TilingLayout::new_group(tree, &root_id, &new_id, orientation).unwrap();
                tree.make_nth_sibling(
                    &new_id,
                    match direction {
                        Direction::Left | Direction::Up => 1,
                        Direction::Right | Direction::Down => 0,
                    },
                )
                .unwrap();
                new_id
            } else {
                tree.insert(new_window, InsertBehavior::AsRoot).unwrap()
            }
        } else if let Some(ref node_id) = node {
            let orientation = {
                let window_size = tree.get(node_id).unwrap().data().geometry().size;
                if window_size.w > window_size.h {
                    Orientation::Vertical
                } else {
                    Orientation::Horizontal
                }
            };
            let new_id = tree.insert(new_window, InsertBehavior::AsRoot).unwrap();
            TilingLayout::new_group(tree, node_id, &new_id, orientation).unwrap();
            new_id
        } else {
            // nothing? then we add to the root
            if let Some(root_id) = tree.root_node_id().cloned() {
                let orientation = {
                    let output_size = output.geometry().size;
                    if output_size.w > output_size.h {
                        Orientation::Vertical
                    } else {
                        Orientation::Horizontal
                    }
                };
                let new_id = tree.insert(new_window, InsertBehavior::AsRoot).unwrap();
                TilingLayout::new_group(tree, &root_id, &new_id, orientation).unwrap();
                new_id
            } else {
                tree.insert(new_window, InsertBehavior::AsRoot).unwrap()
            }
        };

        *window.tiling_node_id.lock().unwrap() = Some(window_id);
    }

    pub fn replace_window(&mut self, old: &CosmicMapped, new: &CosmicMapped) {
        let gaps = self.gaps();
        let Some(old_id) = old.tiling_node_id.lock().unwrap().clone() else {
            return;
        };
        let mut tree = self.queue.trees.back().unwrap().0.copy_clone();

        if let Ok(node) = tree.get_mut(&old_id) {
            let data = node.data_mut();
            match data {
                Data::Mapped { mapped, .. } => {
                    assert_eq!(mapped, old);
                    *mapped = new.clone();
                    *new.tiling_node_id.lock().unwrap() = Some(old_id);
                    *old.tiling_node_id.lock().unwrap() = None;
                }
                _ => unreachable!("Tiling id points to group"),
            }

            old.output_leave(&self.output);
            new.output_enter(&self.output, new.bbox());

            let blocker = self.update_positions_for(&mut tree, gaps);
            self.queue.push_tree(tree, ANIMATION_DURATION, blocker);
        }
    }

    pub fn move_tree<'a>(
        this: &mut Self,
        other: &mut Self,
        other_handle: &WorkspaceHandle,
        seat: &Seat<State>,
        focus_stack: impl Iterator<Item = &'a FocusTarget> + 'a,
        desc: NodeDesc,
        direction: Option<Direction>,
    ) -> Option<KeyboardFocusTarget> {
        let this_handle = &desc.handle;
        let mut this_tree = this.queue.trees.back().unwrap().0.copy_clone();
        let mut other_tree = other.queue.trees.back().unwrap().0.copy_clone();

        match desc.stack_window {
            Some(stack_surface) => {
                let node = this_tree.get(&desc.node).ok()?;
                let Data::Mapped {
                    mapped: this_mapped,
                    ..
                } = node.data()
                else {
                    return None;
                };
                let this_stack = this_mapped.stack_ref()?;
                this_stack.remove_window(&stack_surface);
                if !this_stack.alive() {
                    let _ = this.unmap(this_mapped, None);
                }

                let mapped: CosmicMapped = CosmicWindow::new(
                    stack_surface,
                    this_stack.loop_handle(),
                    this.theme.clone(),
                    this.appearance,
                )
                .into();
                if this.output != other.output {
                    mapped.output_leave(&this.output);
                    mapped.output_enter(&other.output, mapped.bbox());
                    for (ref surface, _) in mapped.windows() {
                        toplevel_leave_output(surface, &this.output);
                        toplevel_enter_output(surface, &other.output);
                    }
                }
                for (ref surface, _) in mapped.windows() {
                    toplevel_leave_workspace(surface, this_handle);
                    toplevel_enter_workspace(surface, other_handle);
                }

                mapped.set_tiled(true);
                other.map(mapped.clone(), Some(focus_stack), direction);
                Some(KeyboardFocusTarget::Element(mapped))
            }
            None => {
                let node = this_tree.get(&desc.node).ok()?;
                let mut children = node
                    .children()
                    .iter()
                    .map(|child_id| (desc.node.clone(), child_id.clone()))
                    .collect::<Vec<_>>();
                let node = Node::new(node.data().clone());

                let id = match other_tree.root_node_id() {
                    None => other_tree.insert(node, InsertBehavior::AsRoot).unwrap(),
                    Some(_) => {
                        let focused_node = seat
                            .get_keyboard()
                            .unwrap()
                            .current_focus()
                            .and_then(|target| {
                                TilingLayout::currently_focused_node(&other_tree, target)
                            })
                            .map(|(id, _)| id)
                            .unwrap_or(other_tree.root_node_id().unwrap().clone());
                        let orientation = if let Some(direction) = direction {
                            match direction {
                                Direction::Left | Direction::Right => Orientation::Vertical,
                                Direction::Up | Direction::Down => Orientation::Horizontal,
                            }
                        } else {
                            let window_size = other_tree
                                .get(&focused_node)
                                .unwrap()
                                .data()
                                .geometry()
                                .size;
                            if window_size.w > window_size.h {
                                Orientation::Vertical
                            } else {
                                Orientation::Horizontal
                            }
                        };
                        let id = other_tree.insert(node, InsertBehavior::AsRoot).unwrap();
                        TilingLayout::new_group(&mut other_tree, &focused_node, &id, orientation)
                            .unwrap();
                        other_tree
                            .make_nth_sibling(
                                &id,
                                match direction {
                                    Some(Direction::Right) | Some(Direction::Down) => 0,
                                    _ => 1,
                                },
                            )
                            .unwrap();
                        id
                    }
                };

                for (parent_id, _) in children.iter_mut() {
                    *parent_id = id.clone();
                }
                if let Data::Mapped { mapped, .. } = other_tree.get_mut(&id).unwrap().data_mut() {
                    if this.output != other.output {
                        mapped.output_leave(&this.output);
                        mapped.output_enter(&other.output, mapped.bbox());
                        for (ref surface, _) in mapped.windows() {
                            toplevel_leave_output(surface, &this.output);
                            toplevel_enter_output(surface, &other.output);
                        }
                    }
                    for (ref surface, _) in mapped.windows() {
                        toplevel_leave_workspace(surface, this_handle);
                        toplevel_enter_workspace(surface, other_handle);
                    }

                    *mapped.tiling_node_id.lock().unwrap() = Some(id.clone());
                }

                while !children.is_empty() {
                    for (parent_id, child_id) in std::mem::take(&mut children) {
                        let new_children = this_tree
                            .children_ids(&child_id)
                            .unwrap()
                            .cloned()
                            .collect::<Vec<_>>();
                        let child_node = this_tree
                            .remove_node(child_id, RemoveBehavior::OrphanChildren)
                            .unwrap();
                        let maybe_mapped = match child_node.data() {
                            Data::Mapped { mapped, .. } => Some(mapped.clone()),
                            _ => None,
                        };
                        let new_id = other_tree
                            .insert(child_node, InsertBehavior::UnderNode(&parent_id))
                            .unwrap();
                        if let Some(mapped) = maybe_mapped {
                            if this.output != other.output {
                                mapped.output_leave(&this.output);
                                mapped.output_enter(&other.output, mapped.bbox());
                                for (ref surface, _) in mapped.windows() {
                                    toplevel_leave_output(surface, &this.output);
                                    toplevel_enter_output(surface, &other.output);
                                }
                            }
                            for (ref surface, _) in mapped.windows() {
                                toplevel_leave_workspace(surface, this_handle);
                                toplevel_enter_workspace(surface, other_handle);
                            }

                            *mapped.tiling_node_id.lock().unwrap() = Some(new_id.clone());
                        }
                        children.extend(
                            new_children
                                .into_iter()
                                .map(|new_child| (new_id.clone(), new_child)),
                        );
                    }
                }
                let this_gaps = this.gaps();
                let other_gaps = other.gaps();

                TilingLayout::unmap_internal(&mut this_tree, &desc.node);
                let blocker = this.update_positions_for(&mut this_tree, this_gaps);
                this.queue.push_tree(this_tree, ANIMATION_DURATION, blocker);

                let blocker = other.update_positions_for(&mut other_tree, other_gaps);
                other
                    .queue
                    .push_tree(other_tree, ANIMATION_DURATION, blocker);

                other.node_desc_to_focus(&NodeDesc {
                    handle: *other_handle,
                    node: id.clone(),
                    stack_window: None,
                    focus_stack: Vec::new(), // node_desc_to_focus doesn't use this
                })
            }
        }
    }

    pub fn swap_trees(
        this: &mut Self,
        mut other: Option<&mut Self>,
        this_desc: &NodeDesc,
        other_desc: &NodeDesc,
    ) -> Option<KeyboardFocusTarget> {
        let other_output = other
            .as_ref()
            .map(|other| other.output.clone())
            .unwrap_or(this.output.clone());
        if this.output == other_output
            && this_desc.handle == other_desc.handle
            && this_desc.node == other_desc.node
            && (this_desc.stack_window == other_desc.stack_window
                || this_desc.stack_window.is_some() != other_desc.stack_window.is_some())
        {
            return None;
        }

        let mut this_tree = this.queue.trees.back().unwrap().0.copy_clone();
        let mut other_tree = match other.as_mut() {
            Some(other) => Some(other.queue.trees.back().unwrap().0.copy_clone()),
            None => {
                if this.output != other_output {
                    Some(this.queue.trees.back().unwrap().0.copy_clone())
                } else {
                    None
                }
            }
        };

        match (&this_desc.stack_window, &other_desc.stack_window) {
            (None, None) if other_tree.is_none() => {
                if this_desc.node != other_desc.node {
                    this_tree
                        .swap_nodes(
                            &this_desc.node,
                            &other_desc.node,
                            id_tree::SwapBehavior::TakeChildren,
                        )
                        .unwrap();
                }
            }
            (None, None) => {
                let Some(other_tree) = other_tree.as_mut() else {
                    unreachable!()
                };
                let this_node = this_tree.get_mut(&this_desc.node).ok()?;
                let other_node = other_tree.get_mut(&other_desc.node).ok()?;

                if let Data::Mapped { mapped, .. } = this_node.data_mut() {
                    if this.output != other_output {
                        mapped.output_leave(&this.output);
                        mapped.output_enter(&other_output, mapped.bbox());
                        for (ref surface, _) in mapped.windows() {
                            toplevel_leave_output(surface, &this.output);
                            toplevel_enter_output(surface, &other_output);
                        }
                    }
                    for (ref surface, _) in mapped.windows() {
                        toplevel_leave_workspace(surface, &this_desc.handle);
                        toplevel_enter_workspace(surface, &other_desc.handle);
                    }
                    *mapped.tiling_node_id.lock().unwrap() = Some(other_desc.node.clone());
                }
                if let Data::Mapped { mapped, .. } = other_node.data_mut() {
                    if this.output != other_output {
                        mapped.output_leave(&other_output);
                        mapped.output_enter(&this.output, mapped.bbox());
                        for (ref surface, _) in mapped.windows() {
                            toplevel_leave_output(surface, &other_output);
                            toplevel_enter_output(surface, &this.output);
                        }
                    }
                    for (ref surface, _) in mapped.windows() {
                        toplevel_leave_workspace(surface, &other_desc.handle);
                        toplevel_enter_workspace(surface, &this_desc.handle);
                    }
                    *mapped.tiling_node_id.lock().unwrap() = Some(this_desc.node.clone());
                }

                // swap data
                let other_data = other_node.data().clone();
                other_node.replace_data(this_node.replace_data(other_data));

                // swap children
                let mut this_children = this_node
                    .children()
                    .iter()
                    .map(|child_id| (other_desc.node.clone(), child_id.clone()))
                    .collect::<Vec<_>>();
                let mut other_children = other_node
                    .children()
                    .iter()
                    .map(|child_id| (this_desc.node.clone(), child_id.clone()))
                    .collect::<Vec<_>>();

                while !this_children.is_empty() {
                    for (parent_id, child_id) in std::mem::take(&mut this_children) {
                        let new_children = this_tree
                            .children_ids(&child_id)
                            .unwrap()
                            .cloned()
                            .collect::<Vec<_>>();
                        let child_node = this_tree
                            .remove_node(child_id, RemoveBehavior::OrphanChildren)
                            .unwrap();
                        let maybe_mapped = match child_node.data() {
                            Data::Mapped { mapped, .. } => Some(mapped.clone()),
                            _ => None,
                        };
                        let new_id = other_tree
                            .insert(child_node, InsertBehavior::UnderNode(&parent_id))
                            .unwrap();
                        if let Some(mapped) = maybe_mapped {
                            if this.output != other_output {
                                mapped.output_leave(&this.output);
                                mapped.output_enter(&other_output, mapped.bbox());
                                for (ref surface, _) in mapped.windows() {
                                    toplevel_leave_output(surface, &this.output);
                                    toplevel_enter_output(surface, &other_output);
                                }
                            }
                            for (ref surface, _) in mapped.windows() {
                                toplevel_leave_workspace(surface, &this_desc.handle);
                                toplevel_enter_workspace(surface, &other_desc.handle);
                            }
                            *mapped.tiling_node_id.lock().unwrap() = Some(new_id.clone());
                        }
                        this_children.extend(
                            new_children
                                .into_iter()
                                .map(|new_child| (new_id.clone(), new_child)),
                        );
                    }
                }

                while !other_children.is_empty() {
                    for (parent_id, child_id) in std::mem::take(&mut other_children) {
                        let new_children = other_tree
                            .children_ids(&child_id)
                            .unwrap()
                            .cloned()
                            .collect::<Vec<_>>();
                        let child_node = other_tree
                            .remove_node(child_id, RemoveBehavior::OrphanChildren)
                            .unwrap();
                        let maybe_mapped = match child_node.data() {
                            Data::Mapped { mapped, .. } => Some(mapped.clone()),
                            _ => None,
                        };
                        let new_id = this_tree
                            .insert(child_node, InsertBehavior::UnderNode(&parent_id))
                            .unwrap();
                        if let Some(mapped) = maybe_mapped {
                            if this.output != other_output {
                                mapped.output_leave(&other_output);
                                mapped.output_enter(&this.output, mapped.bbox());
                                for (ref surface, _) in mapped.windows() {
                                    toplevel_leave_output(surface, &other_output);
                                    toplevel_enter_output(surface, &this.output);
                                }
                            }
                            for (ref surface, _) in mapped.windows() {
                                toplevel_leave_workspace(surface, &other_desc.handle);
                                toplevel_enter_workspace(surface, &this_desc.handle);
                            }
                            *mapped.tiling_node_id.lock().unwrap() = Some(new_id.clone());
                        }
                        other_children.extend(
                            new_children
                                .into_iter()
                                .map(|new_child| (new_id.clone(), new_child)),
                        );
                    }
                }
            }
            (Some(this_surface), None) => {
                if !this_surface.alive() {
                    return None;
                }
                let this_node = this_tree.get_mut(&this_desc.node).ok()?;
                let Data::Mapped {
                    mapped: this_mapped,
                    ..
                } = this_node.data()
                else {
                    return None;
                };
                let this_mapped = this_mapped.clone();
                let geometry = *this_node.data().geometry();
                assert!(this_mapped.is_stack());

                let other_tree = other_tree.as_mut().unwrap_or(&mut this_tree);
                if other_tree.get(&other_desc.node).is_err() {
                    return None;
                }

                let surfaces = other_tree
                    .traverse_pre_order(&other_desc.node)
                    .unwrap()
                    .flat_map(|node| match node.data() {
                        Data::Mapped { mapped, .. } => Some(mapped.windows().map(|(s, _)| s)),
                        _ => None,
                    })
                    .flatten()
                    .collect::<Vec<_>>();
                let cleanup = other_tree
                    .children_ids(&other_desc.node)
                    .unwrap()
                    .cloned()
                    .collect::<Vec<_>>();
                for node in cleanup {
                    let _ = other_tree.remove_node(node, RemoveBehavior::DropChildren);
                }

                let this_stack = this_mapped.stack_ref()?;
                let this_idx = this_stack
                    .surfaces()
                    .position(|s| &s == this_surface)
                    .unwrap();
                for (i, surface) in surfaces.into_iter().enumerate() {
                    if this.output != other_output {
                        surface.output_leave(&other_output);
                        surface.output_enter(&this.output, surface.bbox());
                        toplevel_leave_output(&surface, &other_output);
                        toplevel_enter_output(&surface, &this.output);
                    }
                    if this_desc.handle != other_desc.handle {
                        toplevel_leave_workspace(&surface, &other_desc.handle);
                        toplevel_enter_workspace(&surface, &this_desc.handle);
                    }
                    this_stack.add_window(surface, Some(this_idx + i), None);
                }
                if this.output != other_output {
                    this_surface.output_leave(&this.output);
                    toplevel_leave_output(this_surface, &this.output);
                    toplevel_enter_output(this_surface, &other_output);
                }
                if this_desc.handle != other_desc.handle {
                    toplevel_leave_workspace(this_surface, &this_desc.handle);
                    toplevel_enter_workspace(this_surface, &other_desc.handle);
                }
                this_stack.remove_window(this_surface);

                let mapped: CosmicMapped = CosmicWindow::new(
                    this_surface.clone(),
                    this_stack.loop_handle(),
                    this.theme.clone(),
                    this.appearance,
                )
                .into();
                mapped.set_tiled(true);
                mapped.refresh();
                mapped.output_enter(&other_output, mapped.bbox());

                *mapped.tiling_node_id.lock().unwrap() = Some(other_desc.node.clone());
                other_tree
                    .get_mut(&other_desc.node)
                    .unwrap()
                    .replace_data(Data::Mapped {
                        mapped,
                        last_geometry: geometry,
                        minimize_rect: None,
                    });
            }
            (None, Some(other_surface)) => {
                if !other_surface.alive() {
                    return None;
                }

                let other_tree = other_tree.as_mut().unwrap_or(&mut this_tree);
                let other_node = other_tree.get_mut(&other_desc.node).ok()?;
                let Data::Mapped {
                    mapped: other_mapped,
                    ..
                } = other_node.data()
                else {
                    return None;
                };
                let other_mapped = other_mapped.clone();
                let geometry = *other_node.data().geometry();
                assert!(other_mapped.is_stack());

                let surfaces = this_tree
                    .traverse_pre_order(&this_desc.node)
                    .unwrap()
                    .flat_map(|node| match node.data() {
                        Data::Mapped { mapped, .. } => Some(mapped.windows().map(|(s, _)| s)),
                        _ => None,
                    })
                    .flatten()
                    .collect::<Vec<_>>();
                let cleanup = this_tree
                    .children_ids(&this_desc.node)
                    .unwrap()
                    .cloned()
                    .collect::<Vec<_>>();
                for node in cleanup {
                    let _ = this_tree.remove_node(node, RemoveBehavior::DropChildren);
                }

                let other_stack = other_mapped.stack_ref()?;
                let other_idx = other_stack
                    .surfaces()
                    .position(|s| &s == other_surface)
                    .unwrap();
                for (i, surface) in surfaces.into_iter().enumerate() {
                    if this.output != other_output {
                        surface.output_leave(&this.output);
                        surface.output_enter(&other_output, surface.bbox());
                        toplevel_leave_output(&surface, &this.output);
                        toplevel_enter_output(&surface, &other_output);
                    }
                    if this_desc.handle != other_desc.handle {
                        toplevel_leave_workspace(&surface, &this_desc.handle);
                        toplevel_enter_workspace(&surface, &other_desc.handle);
                    }
                    other_stack.add_window(surface, Some(other_idx + i), None);
                }
                if this.output != other_output {
                    other_surface.output_leave(&other_output);
                    toplevel_leave_output(other_surface, &other_output);
                    toplevel_enter_output(other_surface, &this.output);
                }
                if this_desc.handle != other_desc.handle {
                    toplevel_leave_workspace(other_surface, &other_desc.handle);
                    toplevel_enter_workspace(other_surface, &this_desc.handle);
                }
                other_stack.remove_window(other_surface);

                let mapped: CosmicMapped = CosmicWindow::new(
                    other_surface.clone(),
                    other_stack.loop_handle(),
                    this.theme.clone(),
                    this.appearance,
                )
                .into();
                mapped.set_tiled(true);
                mapped.refresh();
                mapped.output_enter(&this.output, mapped.bbox());

                *mapped.tiling_node_id.lock().unwrap() = Some(this_desc.node.clone());
                this_tree
                    .get_mut(&this_desc.node)
                    .unwrap()
                    .replace_data(Data::Mapped {
                        mapped,
                        last_geometry: geometry,
                        minimize_rect: None,
                    });
            }
            (Some(this_surface), Some(other_surface)) => {
                if !this_surface.alive() || !other_surface.alive() {
                    return None;
                }

                let this_mapped =
                    this_tree
                        .get(&this_desc.node)
                        .ok()
                        .and_then(|node| match node.data() {
                            Data::Mapped { mapped, .. } => Some(mapped.clone()),
                            _ => None,
                        })?;
                let other_tree = other_tree.as_mut().unwrap_or(&mut this_tree);
                let other_mapped =
                    other_tree
                        .get(&other_desc.node)
                        .ok()
                        .and_then(|node| match node.data() {
                            Data::Mapped { mapped, .. } => Some(mapped.clone()),
                            _ => None,
                        })?;

                let this_stack = this_mapped.stack_ref()?;
                let other_stack = other_mapped.stack_ref()?;

                let this_idx = this_stack
                    .surfaces()
                    .position(|s| &s == this_surface)
                    .unwrap();
                let other_idx = other_stack
                    .surfaces()
                    .position(|s| &s == other_surface)
                    .unwrap();
                let this_was_active = &this_stack.active() == this_surface;
                let other_was_active = &other_stack.active() == other_surface;
                this_stack.add_window(other_surface.clone(), Some(this_idx), None);
                this_stack.remove_window(this_surface);
                other_stack.add_window(this_surface.clone(), Some(other_idx), None);

                if this.output != other_output {
                    toplevel_leave_output(this_surface, &this.output);
                    toplevel_leave_output(other_surface, &other_output);
                    toplevel_enter_output(this_surface, &other_output);
                    toplevel_enter_output(other_surface, &this.output);
                }
                if this_desc.handle != other_desc.handle {
                    toplevel_leave_workspace(this_surface, &this_desc.handle);
                    toplevel_leave_workspace(other_surface, &other_desc.handle);
                    toplevel_enter_workspace(this_surface, &other_desc.handle);
                    toplevel_enter_workspace(other_surface, &this_desc.handle);
                }

                other_stack.remove_window(other_surface);
                if this_was_active {
                    this_stack.set_active(other_surface);
                }
                if other_was_active {
                    other_stack.set_active(this_surface);
                }

                return other
                    .as_ref()
                    .unwrap_or(&this)
                    .node_desc_to_focus(other_desc);
            }
        }

        let this_gaps = this.gaps();
        let blocker = this.update_positions_for(&mut this_tree, this_gaps);
        this.queue.push_tree(this_tree, ANIMATION_DURATION, blocker);

        let has_other_tree = other_tree.is_some();
        if let Some(mut other_tree) = other_tree {
            if let Some(other) = other.as_mut() {
                let other_gaps = other.gaps();
                let blocker = other.update_positions_for(&mut other_tree, other_gaps);
                other
                    .queue
                    .push_tree(other_tree, ANIMATION_DURATION, blocker);
            } else {
                let blocker = this.update_positions_for(&mut other_tree, this_gaps);
                this.queue
                    .push_tree(other_tree, ANIMATION_DURATION, blocker);
            }
        }

        match (&this_desc.stack_window, &other_desc.stack_window) {
            (None, None) if !has_other_tree => this.node_desc_to_focus(this_desc),
            //(None, Some(_)) => None,
            _ => other
                .as_ref()
                .unwrap_or(&this)
                .node_desc_to_focus(other_desc),
        }
    }

    pub fn node_desc_to_focus(&self, desc: &NodeDesc) -> Option<KeyboardFocusTarget> {
        let tree = &self.queue.trees.back().unwrap().0;
        let data = tree.get(&desc.node).ok()?.data();
        match data {
            Data::Mapped { mapped, .. } => Some(KeyboardFocusTarget::Element(mapped.clone())),
            Data::Group { alive, .. } => Some(KeyboardFocusTarget::Group(WindowGroup {
                node: desc.node.clone(),
                alive: Arc::downgrade(alive),
                focus_stack: tree
                    .children_ids(&desc.node)
                    .unwrap()
                    .cloned()
                    .collect::<Vec<_>>(),
            })),
            _ => None,
        }
    }

    pub fn tree(&self) -> &Tree<Data> {
        &self.queue.trees.back().unwrap().0
    }

    pub fn unmap(
        &mut self,
        window: &CosmicMapped,
        to: Option<Rectangle<i32, Local>>,
    ) -> Result<Option<RestoreTilingState>, NodeIdError> {
        let node_id = window
            .tiling_node_id
            .lock()
            .unwrap()
            .clone()
            .ok_or(NodeIdError::NodeIdNoLongerValid)?;
        let state = {
            let tree = &self.queue.trees.back().unwrap().0;
            tree.get(&node_id).unwrap().parent().and_then(|parent_id| {
                let parent = tree.get(parent_id).unwrap();
                let idx = parent
                    .children()
                    .iter()
                    .position(|id| id == &node_id)
                    .unwrap();
                if let Data::Group {
                    orientation, sizes, ..
                } = parent.data()
                {
                    if sizes.len() == 2 {
                        // this group will be flattened
                        Some(RestoreTilingState {
                            parent: None,
                            sibling: parent.children().iter().find(|&id| id != &node_id).cloned(),
                            orientation: *orientation,
                            idx,
                            sizes: sizes.clone(),
                        })
                    } else {
                        Some(RestoreTilingState {
                            parent: Some(parent_id.clone()),
                            sibling: None,
                            orientation: *orientation,
                            idx,
                            sizes: sizes.clone(),
                        })
                    }
                } else {
                    None
                }
            })
        };

        if self.unmap_window_internal(window, to.is_some()) {
            if let Some(to) = to {
                let tree = &mut self
                    .queue
                    .trees
                    .get_mut(self.queue.trees.len() - 2)
                    .unwrap()
                    .0;
                if let Data::Mapped {
                    minimize_rect: minimize_to,
                    ..
                } = tree.get_mut(&node_id).unwrap().data_mut()
                {
                    *minimize_to = Some(to);
                }
            }

            window.output_leave(&self.output);
            window.set_tiled(false);
            *window.tiling_node_id.lock().unwrap() = None;
        } else {
            return Err(NodeIdError::InvalidNodeIdForTree);
        }

        Ok(state)
    }

    pub fn unmap_as_placeholder(
        &mut self,
        window: &CosmicMapped,
        type_: PlaceholderType,
    ) -> Option<NodeId> {
        let node_id = window.tiling_node_id.lock().unwrap().take()?;

        if self.tiling_engine == TilingEngine::Scrolling {
            self.prepare_scrolling_direct_tree();
        }

        // Initialize last_overview_hover to the placeholder position so that
        // dropping without mouse movement restores the window to its original position
        if matches!(type_, PlaceholderType::GrabbedWindow) {
            self.scrolling.begin_drag(&node_id);
            self.last_overview_hover =
                Some((None, TargetZone::InitialPlaceholder(node_id.clone())));
        }

        let data = self
            .queue
            .trees
            .back_mut()
            .unwrap()
            .0
            .get_mut(&node_id)
            .unwrap()
            .data_mut();
        *data = Data::Placeholder {
            id: Id::new(),
            last_geometry: *data.geometry(),
            type_,
        };

        window.output_leave(&self.output);
        window.set_tiled(false);
        Some(node_id)
    }

    fn unmap_window_internal(&mut self, mapped: &CosmicMapped, minimizing: bool) -> bool {
        let tiling_node_id = mapped.tiling_node_id.lock().unwrap().as_ref().cloned();
        let gaps = self.gaps();

        if let Some(node_id) = tiling_node_id
            && self
                .queue
                .trees
                .back()
                .unwrap()
                .0
                .get(&node_id)
                .map(|node| node.data().is_mapped(Some(mapped)))
                .unwrap_or(false)
        {
            let mut tree = self.queue.trees.back().unwrap().0.copy_clone();

            TilingLayout::unmap_internal(&mut tree, &node_id);

            let duration = if minimizing {
                MINIMIZE_ANIMATION_DURATION
            } else {
                ANIMATION_DURATION
            };
            let blocker = self.update_positions_for(&mut tree, gaps);
            if self.tiling_engine == TilingEngine::Scrolling {
                self.queue.push_retargetable_tree(tree, duration, blocker);
            } else {
                self.queue.push_tree(tree, duration, blocker);
            }

            return true;
        }
        false
    }

    fn unmap_internal(tree: &mut Tree<Data>, node: &NodeId) {
        let parent_id = tree.get(node).ok().and_then(|node| node.parent()).cloned();
        let position = parent_id.as_ref().and_then(|parent_id| {
            tree.children_ids(parent_id)
                .unwrap()
                .position(|id| id == node)
        });
        let parent_parent_id = parent_id.as_ref().and_then(|parent_id| {
            tree.get(parent_id)
                .ok()
                .and_then(|node| node.parent())
                .cloned()
        });

        // remove self
        trace!(?node, "Removing node.");
        let _ = tree.remove_node(node.clone(), RemoveBehavior::DropChildren);

        // fixup parent node
        if let Some(id) = parent_id {
            let position = position.unwrap();
            let group = tree.get_mut(&id).unwrap().data_mut();
            assert!(group.is_group());

            if group.len() > 2 {
                group.remove_window(position);
            } else {
                trace!("Removing Group");
                let other_child = tree.children_ids(&id).unwrap().next().cloned().unwrap();
                let fork_pos = parent_parent_id.as_ref().and_then(|parent_id| {
                    tree.children_ids(parent_id).unwrap().position(|i| i == &id)
                });
                let _ = tree.remove_node(id.clone(), RemoveBehavior::OrphanChildren);
                tree.move_node(
                    &other_child,
                    parent_parent_id
                        .as_ref()
                        .map(MoveBehavior::ToParent)
                        .unwrap_or(MoveBehavior::ToRoot),
                )
                .unwrap();
                if let Some(old_pos) = fork_pos {
                    tree.make_nth_sibling(&other_child, old_pos).unwrap();
                }
            }
        }
    }

    // TODO: Move would needs this to be accurate during animations
    pub fn element_geometry(&self, elem: &CosmicMapped) -> Option<Rectangle<i32, Local>> {
        if let Some(id) = elem.tiling_node_id.lock().unwrap().as_ref() {
            let node = self.queue.trees.back().unwrap().0.get(id).ok()?;
            let data = node.data();
            assert!(data.is_mapped(Some(elem)));
            let geo = *data.geometry();
            Some(geo)
        } else {
            None
        }
    }

    fn prepare_scrolling_direct_tree(&mut self) -> Tree<Data> {
        let gaps = self.gaps();
        let tree = self.queue.prepare_direct_tree();
        self.scrolling
            .sync_viewport_from_tree(&self.output, &tree, gaps);
        tree
    }

    fn set_scrolling_tiles_resizing(tree: &Tree<Data>, tiles: &[NodeId], resizing: bool) {
        for tile in tiles {
            if let Ok(node) = tree.get(tile)
                && let Data::Mapped { mapped, .. } = node.data()
            {
                mapped.set_resizing(resizing);
            }
        }
    }

    fn set_scrolling_live_resize(
        &mut self,
        tree: &Tree<Data>,
        target: ScrollingResizeTarget,
        tiles: Vec<NodeId>,
    ) {
        if self
            .scrolling_live_resize
            .as_ref()
            .is_some_and(|resize| !resize.finishing && resize.target == target)
        {
            return;
        }
        if let Some(previous) = self.scrolling_live_resize.take() {
            Self::set_scrolling_tiles_resizing(tree, &previous.tiles, false);
        }
        Self::set_scrolling_tiles_resizing(tree, &tiles, true);
        self.scrolling_live_resize = Some(ScrollingLiveResize {
            target,
            tiles,
            finishing: false,
        });
    }

    fn scrolling_live_resize_target_exists(&self, target: &ScrollingResizeTarget) -> bool {
        match target {
            ScrollingResizeTarget::Column { tile, .. } => {
                self.scrolling.tiles_in_column(tile).is_some()
            }
            ScrollingResizeTarget::Rows { upper, lower } => self
                .scrolling
                .row_pair(upper, 1)
                .is_some_and(|pair| pair == (upper.clone(), lower.clone())),
        }
    }

    fn update_scrolling_live_resize_state(&mut self) {
        let Some(resize) = self.scrolling_live_resize.clone() else {
            return;
        };
        if !self.scrolling_live_resize_target_exists(&resize.target) {
            let tree = &self.queue.trees.back().unwrap().0;
            Self::set_scrolling_tiles_resizing(tree, &resize.tiles, false);
            self.scrolling_live_resize = None;
            return;
        }
        if !resize.finishing {
            return;
        }
        let tree = &self.queue.trees.back().unwrap().0;
        let pending = resize
            .tiles
            .into_iter()
            .filter(|tile| match tree.get(tile).ok().map(|node| node.data()) {
                Some(Data::Mapped { mapped, .. }) => live_resize_tile_is_pending(
                    mapped.active_window().latest_size_committed(),
                    mapped.is_resizing(false).unwrap_or(false),
                ),
                _ => false,
            })
            .collect::<Vec<_>>();
        if pending.is_empty() {
            self.scrolling_live_resize = None;
        } else if let Some(resize) = self.scrolling_live_resize.as_mut() {
            // A fast client returns to normal rendering without waiting for a
            // slower adjacent tile to finish its final configure.
            resize.tiles = pending;
        }
    }

    fn push_scrolling_animation(&mut self, tree: Tree<Data>, blocker: Option<TilingBlocker>) {
        debug_assert_eq!(self.tiling_engine, TilingEngine::Scrolling);
        self.queue
            .push_retargetable_tree(tree, ANIMATION_DURATION, blocker);
    }

    /// Keep a newly focused scrolling column inside the output viewport.
    pub fn ensure_visible(&mut self, elem: &CosmicMapped) {
        if self.tiling_engine != TilingEngine::Scrolling {
            return;
        }
        let Some(node_id) = elem.tiling_node_id.lock().unwrap().clone() else {
            return;
        };
        if !self.scrolling.ensure_visible(&node_id) {
            return;
        }

        let gaps = self.gaps();
        let mut tree = self.queue.trees.back().unwrap().0.copy_clone();
        let blocker =
            self.scrolling
                .update_positions_ensuring(&self.output, &mut tree, gaps, Some(&node_id));
        self.push_scrolling_animation(tree, blocker);
    }

    /// Pan Scrolling geometry immediately. This changes only the virtual
    /// viewport; focus, tile order, and window sizes remain untouched.
    pub fn pan_scrolling_viewport(&mut self, delta_x: f64) -> bool {
        if self.tiling_engine != TilingEngine::Scrolling || !delta_x.is_finite() {
            return false;
        }
        let interrupted = self.queue.animation_start.is_some() || self.queue.trees.len() > 1;
        let gaps = self.gaps();
        let mut tree = self.prepare_scrolling_direct_tree();
        if !self.scrolling.pan_viewport(delta_x) {
            return interrupted;
        }

        let blocker = self
            .scrolling
            .update_positions_ensuring(&self.output, &mut tree, gaps, None);
        self.queue.push_direct_tree(tree, blocker);
        true
    }

    pub fn cycle_scrolling_column_width(
        &mut self,
        direction: ResizeDirection,
        seat: &Seat<State>,
    ) -> bool {
        if self.tiling_engine != TilingEngine::Scrolling {
            return false;
        }

        let Some(target) = seat.get_keyboard().unwrap().current_focus() else {
            return false;
        };
        let Some((node_id, FocusedNodeData::Window(_))) =
            TilingLayout::currently_focused_node(self.tree(), target)
        else {
            return false;
        };
        self.cycle_scrolling_column_width_for(&node_id, direction)
    }

    fn cycle_scrolling_column_width_for(
        &mut self,
        node_id: &NodeId,
        direction: ResizeDirection,
    ) -> bool {
        if self.tiling_engine != TilingEngine::Scrolling {
            return false;
        }

        let mut tree = self.queue.trees.back().unwrap().0.copy_clone();
        if !self.scrolling.cycle_column_width(node_id, direction) {
            return false;
        }

        let gaps = self.gaps();
        let blocker =
            self.scrolling
                .update_positions_ensuring(&self.output, &mut tree, gaps, Some(node_id));
        self.push_scrolling_animation(tree, blocker);
        true
    }

    pub fn center_scrolling_column(&mut self, seat: &Seat<State>) -> bool {
        if self.tiling_engine != TilingEngine::Scrolling {
            return false;
        }

        let mut tree = self.queue.trees.back().unwrap().0.copy_clone();
        let Some(target) = seat.get_keyboard().unwrap().current_focus() else {
            return false;
        };
        let Some((node_id, FocusedNodeData::Window(mapped))) =
            TilingLayout::currently_focused_node(&tree, target)
        else {
            return false;
        };
        if mapped.is_maximized(true) || mapped.is_fullscreen(true) {
            return false;
        }
        if !self.scrolling.request_center(&node_id) {
            return false;
        }

        let gaps = self.gaps();
        let blocker = self.update_positions_for(&mut tree, gaps);
        self.push_scrolling_animation(tree, blocker);
        true
    }

    pub fn move_current_node(&mut self, direction: Direction, seat: &Seat<State>) -> MoveResult {
        let gaps = self.gaps();

        let mut tree = self.queue.trees.back().unwrap().0.copy_clone();

        let Some(target) = seat.get_keyboard().unwrap().current_focus() else {
            return MoveResult::None;
        };
        let Some((node_id, data)) = TilingLayout::currently_focused_node(&tree, target) else {
            return MoveResult::None;
        };

        if self.tiling_engine == TilingEngine::Scrolling {
            let current_focus = match &data {
                FocusedNodeData::Window(window) => KeyboardFocusTarget::Element(window.clone()),
                FocusedNodeData::Group(focus_stack, alive) => {
                    KeyboardFocusTarget::Group(WindowGroup {
                        node: node_id.clone(),
                        alive: alive.clone(),
                        focus_stack: focus_stack.clone(),
                    })
                }
            };

            if matches!(direction, Direction::Up | Direction::Down) {
                return match self.scrolling.move_tile_vertical(&node_id, direction) {
                    TileMove::Moved => {
                        let blocker = self.update_positions_for(&mut tree, gaps);
                        self.push_scrolling_animation(tree, blocker);
                        MoveResult::Handled
                    }
                    TileMove::Edge => MoveResult::MoveFurther(current_focus),
                    TileMove::Unavailable => MoveResult::Handled,
                };
            }

            let moved = {
                let moved = self.scrolling.move_column(&node_id, direction);
                if moved {
                    self.scrolling.ensure_visible(&node_id);
                }
                moved
            };
            if moved {
                let blocker = self.update_positions_for(&mut tree, gaps);
                self.push_scrolling_animation(tree, blocker);
            }

            return MoveResult::Handled;
        }

        // stacks may handle movement internally
        if let FocusedNodeData::Window(window) = data.clone() {
            match window.handle_move(direction) {
                StackMoveResult::Handled => return MoveResult::Done,
                StackMoveResult::MoveOut(surface, loop_handle) => {
                    let mapped: CosmicMapped = CosmicWindow::new(
                        surface,
                        loop_handle,
                        self.theme.clone(),
                        self.appearance,
                    )
                    .into();
                    mapped.output_enter(&self.output, mapped.bbox());
                    let orientation = match direction {
                        Direction::Left | Direction::Right => Orientation::Vertical,
                        Direction::Up | Direction::Down => Orientation::Horizontal,
                    };

                    let new_node = Node::new(Data::Mapped {
                        mapped: mapped.clone(),
                        last_geometry: Rectangle::from_size((100, 100).into()),
                        minimize_rect: None,
                    });
                    let new_id = tree.insert(new_node, InsertBehavior::AsRoot).unwrap();
                    TilingLayout::new_group(&mut tree, &node_id, &new_id, orientation).unwrap();
                    tree.make_nth_sibling(
                        &new_id,
                        match direction {
                            Direction::Left | Direction::Up => 0,
                            Direction::Right | Direction::Down => 1,
                        },
                    )
                    .unwrap();
                    *mapped.tiling_node_id.lock().unwrap() = Some(new_id);

                    let blocker = self.update_positions_for(&mut tree, gaps);
                    self.queue.push_tree(tree, ANIMATION_DURATION, blocker);
                    return MoveResult::ShiftFocus(mapped.into());
                }
                StackMoveResult::Default => {} // continue normally
            }
        }

        let mut child_id = node_id.clone();
        // Without a parent to start with, just return
        let Some(og_parent) = tree.get(&node_id).unwrap().parent().cloned() else {
            return match data {
                FocusedNodeData::Window(window) => MoveResult::MoveFurther(window.into()),
                FocusedNodeData::Group(focus_stack, alive) => MoveResult::MoveFurther(
                    WindowGroup {
                        node: node_id,
                        alive,
                        focus_stack,
                    }
                    .into(),
                ),
            };
        };
        let og_idx = tree
            .children_ids(&og_parent)
            .unwrap()
            .position(|id| id == &child_id)
            .unwrap();
        let mut maybe_parent = Some(og_parent.clone());

        while let Some(parent) = maybe_parent {
            let parent_data = tree.get(&parent).unwrap().data();
            let orientation = parent_data.orientation();
            let len = parent_data.len();

            // which child are we?
            let idx = tree
                .children_ids(&parent)
                .unwrap()
                .position(|id| id == &child_id)
                .unwrap();

            // if the orientation does not match..
            if matches!(
                (orientation, direction),
                (Orientation::Horizontal, Direction::Right)
                    | (Orientation::Horizontal, Direction::Left)
                    | (Orientation::Vertical, Direction::Up)
                    | (Orientation::Vertical, Direction::Down)
            ) {
                // ...create a new group with our parent (cleanup will remove any one-child-groups afterwards)
                TilingLayout::new_group(
                    &mut tree,
                    &parent,
                    &node_id,
                    match direction {
                        Direction::Left | Direction::Right => Orientation::Vertical,
                        Direction::Up | Direction::Down => Orientation::Horizontal,
                    },
                )
                .unwrap();
                tree.make_nth_sibling(
                    &node_id,
                    if direction == Direction::Left || direction == Direction::Up {
                        0
                    } else {
                        1
                    },
                )
                .unwrap();

                tree.get_mut(&og_parent)
                    .unwrap()
                    .data_mut()
                    .remove_window(og_idx);

                let blocker = self.update_positions_for(&mut tree, gaps);
                self.queue.push_tree(tree, ANIMATION_DURATION, blocker);
                return MoveResult::Done;
            }

            // now if the orientation matches

            // if we are not already in this group, we just move into it (up)
            if child_id != node_id {
                tree.move_node(&node_id, MoveBehavior::ToParent(&parent))
                    .unwrap();
                tree.make_nth_sibling(
                    &node_id,
                    if direction == Direction::Left || direction == Direction::Up {
                        idx
                    } else {
                        idx + 1
                    },
                )
                .unwrap();
                tree.get_mut(&parent).unwrap().data_mut().add_window(idx);
                tree.get_mut(&og_parent)
                    .unwrap()
                    .data_mut()
                    .remove_window(og_idx);

                let blocker = self.update_positions_for(&mut tree, gaps);
                self.queue.push_tree(tree, ANIMATION_DURATION, blocker);
                return MoveResult::Done;
            }

            // we can maybe move inside the group, if we don't run out of elements
            if let Some(next_idx) = match (orientation, direction) {
                (Orientation::Horizontal, Direction::Down)
                | (Orientation::Vertical, Direction::Right)
                    if idx < (len - 1) =>
                {
                    Some(idx + 1)
                }
                (Orientation::Horizontal, Direction::Up)
                | (Orientation::Vertical, Direction::Left)
                    if idx > 0 =>
                {
                    Some(idx - 1)
                }
                _ => None,
            } {
                // if we can, we need to check the next element and move "into" it
                let next_child_id = tree
                    .children_ids(&parent)
                    .unwrap()
                    .nth(next_idx)
                    .unwrap()
                    .clone();

                let result = if tree.get(&next_child_id).unwrap().data().is_stack()
                    && tree.get(&node_id).unwrap().data().is_mapped(None)
                    && !tree.get(&node_id).unwrap().data().is_stack()
                    && len == 2
                {
                    let node = tree
                        .remove_node(node_id, RemoveBehavior::DropChildren)
                        .unwrap();

                    let stack_data = tree.get_mut(&next_child_id).unwrap().data_mut();
                    let mapped = match stack_data {
                        Data::Mapped { mapped, .. } => mapped.clone(),
                        _ => unreachable!(),
                    };
                    let stack = mapped.stack_ref().unwrap();

                    let surface = match node.data() {
                        Data::Mapped { mapped, .. } => mapped.active_window(),
                        _ => unreachable!(),
                    };
                    stack.add_window(
                        surface,
                        match direction {
                            Direction::Right => Some(0),
                            _ => None,
                        },
                        Some(seat),
                    );
                    tree.get_mut(&og_parent)
                        .unwrap()
                        .data_mut()
                        .remove_window(og_idx);

                    MoveResult::ShiftFocus(mapped.into())
                } else if tree.get(&next_child_id).unwrap().data().is_group() && len == 2 {
                    // if it is a group, we want to move into the group
                    tree.move_node(&node_id, MoveBehavior::ToParent(&next_child_id))
                        .unwrap();
                    let group_orientation = tree.get(&next_child_id).unwrap().data().orientation();
                    match (group_orientation, direction) {
                        (Orientation::Horizontal, Direction::Down)
                        | (Orientation::Vertical, Direction::Right) => {
                            tree.make_first_sibling(&node_id).unwrap();
                            tree.get_mut(&next_child_id)
                                .unwrap()
                                .data_mut()
                                .add_window(0);
                        }
                        (Orientation::Horizontal, Direction::Up)
                        | (Orientation::Vertical, Direction::Left) => {
                            tree.make_last_sibling(&node_id).unwrap();
                            let group = tree.get_mut(&next_child_id).unwrap().data_mut();
                            group.add_window(group.len());
                        }
                        _ => {
                            // we want the middle
                            let group_len = tree.get(&next_child_id).unwrap().data().len();
                            if group_len.is_multiple_of(2) {
                                tree.make_nth_sibling(&node_id, group_len / 2).unwrap();
                                tree.get_mut(&next_child_id)
                                    .unwrap()
                                    .data_mut()
                                    .add_window(group_len / 2);
                            } else {
                                // we move again by making a new fork
                                let old_id = tree
                                    .children_ids(&next_child_id)
                                    .unwrap()
                                    .nth(group_len / 2)
                                    .unwrap()
                                    .clone();
                                TilingLayout::new_group(
                                    &mut tree,
                                    &old_id,
                                    &node_id,
                                    !group_orientation,
                                )
                                .unwrap();
                                tree.make_nth_sibling(
                                    &node_id,
                                    if direction == Direction::Left || direction == Direction::Up {
                                        1
                                    } else {
                                        0
                                    },
                                )
                                .unwrap();
                            }
                        }
                    };
                    tree.get_mut(&og_parent)
                        .unwrap()
                        .data_mut()
                        .remove_window(og_idx);

                    MoveResult::Done
                } else if len == 2 && child_id == node_id {
                    // if we are just us two in the group, lets swap
                    tree.make_nth_sibling(&node_id, next_idx).unwrap();
                    // also swap sizes
                    tree.get_mut(&og_parent)
                        .unwrap()
                        .data_mut()
                        .swap_windows(idx, next_idx);

                    MoveResult::Done
                } else {
                    // else we make a new fork
                    TilingLayout::new_group(&mut tree, &next_child_id, &node_id, orientation)
                        .unwrap();
                    tree.make_nth_sibling(
                        &node_id,
                        if direction == Direction::Left || direction == Direction::Up {
                            1
                        } else {
                            0
                        },
                    )
                    .unwrap();
                    tree.get_mut(&og_parent)
                        .unwrap()
                        .data_mut()
                        .remove_window(og_idx);

                    MoveResult::Done
                };

                let blocker = self.update_positions_for(&mut tree, gaps);
                self.queue.push_tree(tree, ANIMATION_DURATION, blocker);
                return result;
            }

            // We have reached the end of our parent group, try to move out even higher.
            maybe_parent = tree.get(&parent).unwrap().parent().cloned();
            child_id = parent.clone();
        }

        match data {
            FocusedNodeData::Window(window) => MoveResult::MoveFurther(window.into()),
            FocusedNodeData::Group(focus_stack, alive) => MoveResult::MoveFurther(
                WindowGroup {
                    node: node_id,
                    alive,
                    focus_stack,
                }
                .into(),
            ),
        }
    }

    pub fn next_focus<'a>(
        &self,
        direction: FocusDirection,
        seat: &Seat<State>,
        focus_stack: impl Iterator<Item = &'a FocusTarget> + 'a,
        swap_desc: Option<NodeDesc>,
    ) -> FocusResult {
        let tree = &self.queue.trees.back().unwrap().0;

        let Some(target) = seat.get_keyboard().unwrap().current_focus() else {
            return FocusResult::None;
        };
        let Some(focused) = TilingLayout::currently_focused_node(tree, target).or_else(|| {
            TilingLayout::last_active_window(tree, focus_stack)
                .map(|(id, mapped)| (id, FocusedNodeData::Window(mapped)))
        }) else {
            return FocusResult::None;
        };

        let (last_node_id, data) = focused;

        if self.tiling_engine == TilingEngine::Scrolling {
            match direction {
                FocusDirection::Left
                | FocusDirection::Right
                | FocusDirection::Up
                | FocusDirection::Down => {
                    return match self.scrolling.next_focus(tree, &last_node_id, direction) {
                        FocusNavigation::Target(node_id) => {
                            match tree.get(&node_id).unwrap().data() {
                                Data::Mapped { mapped, .. } => {
                                    FocusResult::Some(mapped.clone().into())
                                }
                                _ => FocusResult::Handled,
                            }
                        }
                        FocusNavigation::Edge => scrolling_edge_focus_result(direction),
                        FocusNavigation::Unavailable => FocusResult::Handled,
                    };
                }
                _ => {}
            }
        }

        if direction == FocusDirection::In {
            if swap_desc
                .as_ref()
                .map(|desc| desc.node == last_node_id)
                .unwrap_or(false)
            {
                return FocusResult::Handled; // abort
            }

            if let FocusedNodeData::Group(mut stack, _) = data.clone() {
                let maybe_id = stack.pop().unwrap();
                let id = if tree
                    .children_ids(&last_node_id)
                    .unwrap()
                    .any(|id| id == &maybe_id)
                {
                    Some(maybe_id)
                } else {
                    tree.children_ids(&last_node_id).unwrap().next().cloned()
                };

                if let Some(id) = id {
                    return match tree.get(&id).unwrap().data() {
                        Data::Mapped { mapped, .. } => {
                            if let Some(stack) = mapped.stack_ref() {
                                stack.focus_stack();
                            }
                            FocusResult::Some(mapped.clone().into())
                        }
                        Data::Group { alive, .. } => FocusResult::Some(
                            WindowGroup {
                                node: id,
                                alive: Arc::downgrade(alive),
                                focus_stack: stack,
                            }
                            .into(),
                        ),
                        Data::Placeholder { .. } => FocusResult::None,
                    };
                }
            }
        }

        let mut node_id = last_node_id.clone();
        while let Some(group) = tree.get(&node_id).unwrap().parent() {
            let child = node_id.clone();
            let group_data = tree.get(group).unwrap().data();
            let main_orientation = group_data.orientation();
            assert!(group_data.is_group());

            if direction == FocusDirection::Out {
                if swap_desc
                    .as_ref()
                    .map(|desc| {
                        tree.traverse_pre_order_ids(group)
                            .unwrap()
                            .any(|node| node == desc.node)
                    })
                    .unwrap_or(false)
                {
                    return FocusResult::Handled; // abort
                }

                return FocusResult::Some(
                    WindowGroup {
                        node: group.clone(),
                        alive: match group_data {
                            Data::Group { alive, .. } => Arc::downgrade(alive),
                            _ => unreachable!(),
                        },
                        focus_stack: match data {
                            FocusedNodeData::Group(mut stack, _) => {
                                stack.push(child);
                                stack
                            }
                            _ => vec![child],
                        },
                    }
                    .into(),
                );
            }

            // which child are we?
            let idx = tree
                .children_ids(group)
                .unwrap()
                .position(|id| id == &child)
                .unwrap();
            let len = group_data.len();

            let focus_subtree = match (main_orientation, direction) {
                (Orientation::Horizontal, FocusDirection::Down)
                | (Orientation::Vertical, FocusDirection::Right)
                    if idx < (len - 1) =>
                {
                    tree.children_ids(group).unwrap().nth(idx + 1)
                }
                (Orientation::Horizontal, FocusDirection::Up)
                | (Orientation::Vertical, FocusDirection::Left)
                    if idx > 0 =>
                {
                    tree.children_ids(group).unwrap().nth(idx - 1)
                }
                _ => None, // continue iterating
            };

            if focus_subtree.is_some() {
                let mut node_id = focus_subtree;
                while node_id.is_some() {
                    if let Some(desc) = swap_desc.as_ref()
                        && let Some(replacement_id) = tree
                            .ancestor_ids(node_id.unwrap())
                            .unwrap()
                            .find(|anchestor| *anchestor == &desc.node)
                            .or_else(|| {
                                tree.children_ids(node_id.unwrap())
                                    .unwrap()
                                    .find(|child| *child == &desc.node)
                            })
                    {
                        return match tree.get(replacement_id).unwrap().data() {
                            Data::Group { alive, .. } => {
                                FocusResult::Some(KeyboardFocusTarget::Group(WindowGroup {
                                    node: replacement_id.clone(),
                                    alive: Arc::downgrade(alive),
                                    focus_stack: tree
                                        .children_ids(replacement_id)
                                        .unwrap()
                                        .cloned()
                                        .collect(),
                                }))
                            }
                            Data::Mapped { mapped, .. } => {
                                if let Some(stack) = mapped.stack_ref()
                                    && desc.stack_window.is_none()
                                    && replacement_id == &desc.node
                                {
                                    stack.focus_stack();
                                }
                                FocusResult::Some(KeyboardFocusTarget::Element(mapped.clone()))
                            }
                            _ => unreachable!(),
                        };
                    }

                    match tree.get(node_id.unwrap()).unwrap().data() {
                        Data::Group { orientation, .. } if orientation == &main_orientation => {
                            // if the group is layed out in the direction we care about,
                            // we can just use the first or last element (depending on the direction)
                            match direction {
                                FocusDirection::Down | FocusDirection::Right => {
                                    node_id = tree
                                        .children_ids(node_id.as_ref().unwrap())
                                        .unwrap()
                                        .next();
                                }
                                FocusDirection::Up | FocusDirection::Left => {
                                    node_id = tree
                                        .children_ids(node_id.as_ref().unwrap())
                                        .unwrap()
                                        .last();
                                }
                                _ => unreachable!(),
                            }
                        }
                        Data::Group { .. } => {
                            let center = {
                                let geo = tree.get(&last_node_id).unwrap().data().geometry();
                                let mut point = geo.loc;
                                match direction {
                                    FocusDirection::Down => {
                                        point += Point::from((geo.size.w / 2 - 1, geo.size.h))
                                    }
                                    FocusDirection::Up => point.x += geo.size.w / 2 - 1,
                                    FocusDirection::Left => point.y += geo.size.h / 2 - 1,
                                    FocusDirection::Right => {
                                        point += Point::from((geo.size.w, geo.size.h / 2 - 1))
                                    }
                                    _ => unreachable!(),
                                };
                                point.to_f64()
                            };

                            let distance = |candidate: &&NodeId| -> f64 {
                                let geo = tree.get(candidate).unwrap().data().geometry();
                                let mut point = geo.loc;
                                match direction {
                                    FocusDirection::Up => {
                                        point += Point::from((geo.size.w / 2, geo.size.h))
                                    }
                                    FocusDirection::Down => point.x += geo.size.w,
                                    FocusDirection::Right => point.y += geo.size.h / 2,
                                    FocusDirection::Left => {
                                        point += Point::from((geo.size.w, geo.size.h / 2))
                                    }
                                    _ => unreachable!(),
                                };
                                let point = point.to_f64();
                                ((point.x - center.x).powi(2) + (point.y - center.y).powi(2)).sqrt()
                            };

                            node_id = tree
                                .children_ids(node_id.as_ref().unwrap())
                                .unwrap()
                                .min_by(|node1, node2| {
                                    distance(node1).abs().total_cmp(&distance(node2).abs())
                                });
                        }
                        Data::Mapped { mapped, .. } => {
                            if let Some(stack) = mapped.stack_ref()
                                && swap_desc
                                    .as_ref()
                                    .map(|desc| {
                                        desc.stack_window.is_none()
                                            && &desc.node == node_id.unwrap()
                                    })
                                    .unwrap_or(false)
                            {
                                stack.focus_stack();
                            }
                            return FocusResult::Some(mapped.clone().into());
                        }
                        Data::Placeholder { .. } => return FocusResult::None,
                    }
                }
            } else {
                node_id = group.clone();
            }
        }

        FocusResult::None
    }

    pub fn update_orientation(&mut self, new_orientation: Option<Orientation>, seat: &Seat<State>) {
        let gaps = self.gaps();

        let Some(target) = seat.get_keyboard().unwrap().current_focus() else {
            return;
        };

        let mut tree = self.queue.trees.back().unwrap().0.copy_clone();
        if let Some((last_active, _)) = TilingLayout::currently_focused_node(&tree, target)
            && let Some(group) = tree.get(&last_active).unwrap().parent().cloned()
            && let &mut Data::Group {
                ref mut orientation,
                ref mut sizes,
                ref last_geometry,
                ..
            } = tree.get_mut(&group).unwrap().data_mut()
        {
            let previous_length = match orientation {
                Orientation::Horizontal => last_geometry.size.h,
                Orientation::Vertical => last_geometry.size.w,
            };
            let new_orientation = new_orientation.unwrap_or(!*orientation);
            let new_length = match new_orientation {
                Orientation::Horizontal => last_geometry.size.h,
                Orientation::Vertical => last_geometry.size.w,
            };

            sizes.iter_mut().for_each(|len| {
                *len = (((*len as f64) / (previous_length as f64)) * (new_length as f64)).round()
                    as i32;
            });
            let sum: i32 = sizes.iter().sum();
            if sum < new_length {
                *sizes.last_mut().unwrap() += new_length - sum;
            }

            *orientation = new_orientation;

            let blocker = self.update_positions_for(&mut tree, gaps);
            self.queue.push_tree(tree, ANIMATION_DURATION, blocker);
        }
    }

    pub fn toggle_stacking(
        &mut self,
        mapped: &CosmicMapped,
        mut focus_stack: FocusStackMut,
    ) -> Option<KeyboardFocusTarget> {
        let gaps = self.gaps();
        let node_id = mapped.tiling_node_id.lock().unwrap().clone()?;
        let mut tree = self.queue.trees.back().unwrap().0.copy_clone();
        if tree.get(&node_id).is_err() {
            return None;
        }

        let result = if mapped.is_window() {
            // if it is just a window
            match tree.get_mut(&node_id).unwrap().data_mut() {
                Data::Mapped { mapped, .. } => {
                    mapped.convert_to_stack(
                        (&self.output, mapped.bbox()),
                        self.theme.clone(),
                        self.appearance,
                    );
                    focus_stack.append(mapped.clone());
                    KeyboardFocusTarget::Element(mapped.clone())
                }
                _ => unreachable!(),
            }
        } else {
            // if we have a stack
            let mut surfaces = mapped.windows().map(|(s, _)| s);
            let first = surfaces.next().expect("Stack without a window?");
            let focused = mapped.active_window();

            let mut new_elements = Vec::new();

            let handle = match tree.get_mut(&node_id).unwrap().data_mut() {
                Data::Mapped { mapped, .. } => {
                    let handle = mapped.loop_handle();
                    mapped.convert_to_surface(
                        first,
                        (&self.output, mapped.bbox()),
                        self.theme.clone(),
                        self.appearance,
                    );
                    new_elements.push(mapped.clone());
                    handle
                }
                _ => unreachable!(),
            };

            // map the rest
            let mut current_node = node_id.clone();
            for other in surfaces {
                other.try_force_undecorated(false);
                other.set_tiled(false);
                let focused = other == focused;
                let window = CosmicMapped::from(CosmicWindow::new(
                    other,
                    handle.clone(),
                    self.theme.clone(),
                    self.appearance,
                ));
                window.output_enter(&self.output, window.bbox());

                {
                    let layer_map = layer_map_for_output(&self.output);
                    window.set_bounds(layer_map.non_exclusive_zone().size);
                }

                TilingLayout::map_to_tree(
                    &mut tree,
                    window.clone(),
                    &self.output,
                    Some(current_node),
                    None,
                    None,
                );

                let node = window.tiling_node_id.lock().unwrap().clone().unwrap();
                if focused {
                    new_elements.insert(0, window);
                } else {
                    new_elements.push(window);
                }
                current_node = node;
            }

            let node = tree.get(&node_id).unwrap();
            let node_id = if current_node != node_id {
                node.parent().cloned().unwrap_or(node_id)
            } else {
                node_id
            };

            for elem in new_elements.iter().rev() {
                focus_stack.append(elem.clone());
            }

            match tree.get(&node_id).unwrap().data() {
                Data::Group { alive, .. } => KeyboardFocusTarget::Group(WindowGroup {
                    node: node_id.clone(),
                    alive: Arc::downgrade(alive),
                    focus_stack: vec![
                        new_elements[0]
                            .tiling_node_id
                            .lock()
                            .unwrap()
                            .clone()
                            .unwrap(),
                    ],
                }),
                Data::Mapped { mapped, .. } => KeyboardFocusTarget::Element(mapped.clone()),
                _ => unreachable!(),
            }
        };

        let blocker = self.update_positions_for(&mut tree, gaps);
        self.queue.push_tree(tree, ANIMATION_DURATION, blocker);

        Some(result)
    }

    pub fn toggle_stacking_focused(
        &mut self,
        seat: &Seat<State>,
        mut focus_stack: FocusStackMut,
    ) -> Option<KeyboardFocusTarget> {
        let gaps = self.gaps();
        let target = seat.get_keyboard().unwrap().current_focus()?;
        let mut tree = self.queue.trees.back().unwrap().0.copy_clone();
        if let Some((last_active, last_active_data)) =
            TilingLayout::currently_focused_node(&tree, target)
        {
            match last_active_data {
                FocusedNodeData::Window(mapped) => {
                    return self.toggle_stacking(&mapped, focus_stack);
                }
                FocusedNodeData::Group(_, _) => {
                    let mut handle = None;
                    let surfaces = tree
                        .traverse_pre_order(&last_active)
                        .unwrap()
                        .flat_map(|node| match node.data() {
                            Data::Mapped { mapped, .. } => {
                                if handle.is_none() {
                                    handle = Some(mapped.loop_handle());
                                }
                                Some(mapped.windows().map(|(s, _)| s))
                            }
                            _ => None,
                        })
                        .flatten()
                        .collect::<Vec<_>>();

                    if surfaces.is_empty() {
                        return None;
                    }
                    let handle = handle.unwrap();
                    let stack = CosmicStack::new(
                        surfaces.into_iter(),
                        handle,
                        self.theme.clone(),
                        self.appearance,
                    );

                    for child in tree
                        .children_ids(&last_active)
                        .unwrap()
                        .cloned()
                        .collect::<Vec<_>>()
                        .into_iter()
                    {
                        tree.remove_node(child, RemoveBehavior::DropChildren)
                            .unwrap();
                    }
                    let data = tree.get_mut(&last_active).unwrap().data_mut();

                    let geo = *data.geometry();
                    stack.output_enter(&self.output, stack.bbox());
                    stack.set_activate(true);
                    stack.active().send_configure();
                    stack.refresh();

                    let mapped = CosmicMapped::from(stack);
                    *mapped.last_geometry.lock().unwrap() = Some(geo);
                    *mapped.tiling_node_id.lock().unwrap() = Some(last_active);
                    focus_stack.append(mapped.clone());
                    *data = Data::Mapped {
                        mapped: mapped.clone(),
                        last_geometry: geo,
                        minimize_rect: None,
                    };

                    let blocker = self.update_positions_for(&mut tree, gaps);
                    self.queue.push_tree(tree, ANIMATION_DURATION, blocker);

                    return Some(KeyboardFocusTarget::Element(mapped));
                }
            }
        }

        None
    }

    pub fn recalculate(&mut self) {
        let gaps = self.gaps();

        let mut tree = self.queue.trees.back().unwrap().0.copy_clone();
        let blocker = self.update_positions_for(&mut tree, gaps);
        self.queue.push_tree(tree, ANIMATION_DURATION, blocker);
    }

    #[profiling::function]
    pub fn refresh(&mut self) {
        let dead_windows = self
            .mapped()
            .map(|(w, _)| w.clone())
            .filter(|w| !w.alive())
            .collect::<Vec<_>>();
        for dead_window in dead_windows.iter() {
            self.unmap_window_internal(dead_window, false);
        }

        for (mapped, _) in self.mapped() {
            mapped.refresh();
        }
    }

    pub fn animations_going(&self) -> bool {
        self.queue.animation_start.is_some()
    }

    pub fn update_animation_state(&mut self) -> HashMap<ClientId, Client> {
        let mut clients = HashMap::new();
        self.queue.update_retired_blocker(&mut clients);
        self.update_scrolling_live_resize_state();
        let mut ready_trees = 0;
        for (_, _, blocker) in self.queue.trees.iter().skip(1) {
            if let Some(blocker) = blocker.as_ref() {
                if blocker.is_processed() {
                    ready_trees += 1;
                }
                if blocker.is_ready() {
                    clients.extend(blocker.clients());
                    continue;
                }
                break;
            } else {
                ready_trees += 1;
            }
        }

        if let Some(start) = self.queue.animation_start {
            let duration_since_start = Instant::now().duration_since(start);
            if duration_since_start
                >= self
                    .queue
                    .trees
                    .get(1)
                    .expect("Animation going without second tree?")
                    .1
            {
                let _ = self.queue.animation_start.take();
                let _ = self.queue.trees.pop_front();
                ready_trees -= 1;
                let front = self.queue.trees.front_mut().unwrap();
                if let Some(root_id) = front.0.root_node_id() {
                    for node in front
                        .0
                        .traverse_pre_order_ids(root_id)
                        .unwrap()
                        .collect::<Vec<_>>()
                        .into_iter()
                    {
                        if let Data::Mapped { minimize_rect, .. } =
                            front.0.get_mut(&node).unwrap().data_mut()
                        {
                            minimize_rect.take();
                        }
                    }
                }
                let _ = front.2.take();
            } else {
                return clients;
            }
        }

        // merge
        let other_duration = if ready_trees > 1 {
            self.queue
                .trees
                .drain(1..ready_trees)
                .fold(None, |res, (_, duration, _)| {
                    Some(
                        res.map(|old_duration: Duration| old_duration.max(duration))
                            .unwrap_or(duration),
                    )
                })
        } else {
            None
        };

        // start
        if ready_trees > 0 {
            let (_, duration, _) = self.queue.trees.get_mut(1).unwrap();
            *duration = other_duration
                .map(|other| other.max(*duration))
                .unwrap_or(*duration);
            self.queue.animation_start = Some(Instant::now());
        }

        clients
    }

    pub fn possible_resizes(tree: &Tree<Data>, mut node_id: NodeId) -> ResizeEdge {
        let mut edges = ResizeEdge::empty();

        while let Some(group_id) = tree.get(&node_id).unwrap().parent().cloned() {
            let orientation = tree.get(&group_id).unwrap().data().orientation();

            let node_idx = tree
                .children_ids(&group_id)
                .unwrap()
                .position(|id| id == &node_id)
                .unwrap();
            let total = tree.children_ids(&group_id).unwrap().count();
            if orientation == Orientation::Vertical {
                if node_idx > 0 {
                    edges.insert(ResizeEdge::LEFT);
                }
                if node_idx < total - 1 {
                    edges.insert(ResizeEdge::RIGHT);
                }
            } else {
                if node_idx > 0 {
                    edges.insert(ResizeEdge::TOP);
                }
                if node_idx < total - 1 {
                    edges.insert(ResizeEdge::BOTTOM);
                }
            }

            node_id = group_id;
        }

        edges
    }

    fn scrolling_focused_tile(&self, seat: &Seat<State>) -> Option<NodeId> {
        if self.tiling_engine != TilingEngine::Scrolling {
            return None;
        }
        let target = seat.get_keyboard().unwrap().current_focus()?;
        let (node_id, FocusedNodeData::Window(_)) =
            TilingLayout::currently_focused_node(self.tree(), target)?
        else {
            return None;
        };
        self.scrolling.contains(&node_id).then_some(node_id)
    }

    pub fn scrolling_resize_target(
        &self,
        requested: &NodeId,
        edge: ResizeEdge,
        seat: &Seat<State>,
    ) -> Option<ScrollingResizeTarget> {
        let focused = self.scrolling_focused_tile(seat)?;
        if requested != &focused {
            return None;
        }
        if edge.contains(ResizeEdge::LEFT) {
            Some(ScrollingResizeTarget::Column {
                tile: focused,
                anchor: ColumnResizeAnchor::Right,
            })
        } else if edge.contains(ResizeEdge::RIGHT) {
            Some(ScrollingResizeTarget::Column {
                tile: focused,
                anchor: ColumnResizeAnchor::Left,
            })
        } else if edge.contains(ResizeEdge::TOP) {
            let (upper, lower) = self.scrolling.row_pair(&focused, -1)?;
            Some(ScrollingResizeTarget::Rows { upper, lower })
        } else if edge.contains(ResizeEdge::BOTTOM) {
            let (upper, lower) = self.scrolling.row_pair(&focused, 1)?;
            Some(ScrollingResizeTarget::Rows { upper, lower })
        } else {
            None
        }
    }

    pub(super) fn resize_scrolling_column(
        &mut self,
        node_id: &NodeId,
        delta: i32,
        anchor: ColumnResizeAnchor,
    ) -> Option<bool> {
        if self.tiling_engine != TilingEngine::Scrolling {
            return None;
        }
        let mut tree = self.prepare_scrolling_direct_tree();
        let changed = self
            .scrolling
            .resize_column_by_pixels(node_id, delta, anchor)?;
        if changed {
            let tiles = self
                .scrolling
                .tiles_in_column(node_id)
                .unwrap_or_else(|| vec![node_id.clone()]);
            self.set_scrolling_live_resize(
                &tree,
                ScrollingResizeTarget::Column {
                    tile: node_id.clone(),
                    anchor,
                },
                tiles.clone(),
            );
            let gaps = self.gaps();
            let blocker = self.scrolling.update_positions_for_live_resize(
                &self.output,
                &mut tree,
                gaps,
                &tiles,
                false,
            );
            if let Some(blocker) = blocker {
                self.queue.retire_blocker(blocker);
            }
            self.queue.push_direct_tree(tree, None);
        }
        Some(changed)
    }

    pub(super) fn finish_scrolling_resize(&mut self) {
        if self.tiling_engine != TilingEngine::Scrolling {
            return;
        }
        let gaps = self.gaps();
        let mut tree = self.prepare_scrolling_direct_tree();
        let blocker = if let Some(resize) = self.scrolling_live_resize.clone() {
            if matches!(resize.target, ScrollingResizeTarget::Column { .. }) {
                self.scrolling.finish_column_resize();
            }
            if let Some(state) = self.scrolling_live_resize.as_mut() {
                state.finishing = true;
            }
            Self::set_scrolling_tiles_resizing(&tree, &resize.tiles, false);
            self.scrolling.update_positions_for_live_resize(
                &self.output,
                &mut tree,
                gaps,
                &resize.tiles,
                true,
            )
        } else {
            // Preserve the prior no-motion behavior, including automatic
            // centering for a single-column horizontal grab.
            self.scrolling.finish_column_resize();
            self.scrolling
                .update_positions_preserving_viewport(&self.output, &mut tree, gaps)
        };
        if let Some(blocker) = blocker {
            self.queue.retire_blocker(blocker);
        }
        self.queue.push_direct_tree(tree, None);
    }

    pub(super) fn resize_scrolling_rows(
        &mut self,
        upper: &NodeId,
        lower: &NodeId,
        delta: i32,
    ) -> Option<bool> {
        if self.tiling_engine != TilingEngine::Scrolling {
            return None;
        }
        let mut tree = self.prepare_scrolling_direct_tree();
        let changed = self.scrolling.resize_rows_by_pixels(upper, lower, delta)?;
        if changed {
            let tiles = vec![upper.clone(), lower.clone()];
            self.set_scrolling_live_resize(
                &tree,
                ScrollingResizeTarget::Rows {
                    upper: upper.clone(),
                    lower: lower.clone(),
                },
                tiles.clone(),
            );
            let gaps = self.gaps();
            let blocker = self.scrolling.update_positions_for_live_resize(
                &self.output,
                &mut tree,
                gaps,
                &tiles,
                false,
            );
            if let Some(blocker) = blocker {
                self.queue.retire_blocker(blocker);
            }
            self.queue.push_direct_tree(tree, None);
        }
        Some(changed)
    }

    pub fn resize_request(
        &self,
        mut node_id: NodeId,
        edge: ResizeEdge,
    ) -> Option<(NodeId, usize, Orientation)> {
        let tree = self.tree();

        while let Some(group_id) = tree.get(&node_id).unwrap().parent().cloned() {
            let orientation = tree.get(&group_id).unwrap().data().orientation();
            let node_idx = tree
                .children_ids(&group_id)
                .unwrap()
                .position(|id| id == &node_id)
                .unwrap();
            let total = tree.children_ids(&group_id).unwrap().count();
            if orientation == Orientation::Vertical {
                if node_idx > 0 && edge.contains(ResizeEdge::LEFT) {
                    return Some((group_id, node_idx - 1, orientation));
                }
                if node_idx < total - 1 && edge.contains(ResizeEdge::RIGHT) {
                    return Some((group_id, node_idx, orientation));
                }
            } else {
                if node_idx > 0 && edge.contains(ResizeEdge::TOP) {
                    return Some((group_id, node_idx - 1, orientation));
                }
                if node_idx < total - 1 && edge.contains(ResizeEdge::BOTTOM) {
                    return Some((group_id, node_idx, orientation));
                }
            }

            node_id = group_id;
        }

        None
    }

    pub fn resize(
        &mut self,
        focused: &KeyboardFocusTarget,
        direction: ResizeDirection,
        edges: ResizeEdge,
        amount: i32,
    ) -> bool {
        let gaps = self.gaps();

        let mut tree = self.queue.trees.back().unwrap().0.copy_clone();
        let Some(root_id) = tree.root_node_id() else {
            return false;
        };
        let Some(mut node_id) = (match TilingLayout::currently_focused_node(&tree, focused.clone())
        {
            Some((_id, FocusedNodeData::Window(mapped))) =>
            // we need to make sure the id belongs to this tree..
            {
                tree.traverse_pre_order_ids(root_id)
                    .unwrap()
                    .find(|id| tree.get(id).unwrap().data().is_mapped(Some(&mapped)))
            }
            Some((id, FocusedNodeData::Group(_, _))) => Some(id), // in this case the workspace handle was already matched, so the id is to be trusted
            _ => None,
        }) else {
            return false;
        };

        while let Some(group_id) = tree.get(&node_id).unwrap().parent().cloned() {
            let orientation = tree.get(&group_id).unwrap().data().orientation();
            if !((orientation == Orientation::Vertical
                && (edges.contains(ResizeEdge::LEFT) || edges.contains(ResizeEdge::RIGHT)))
                || (orientation == Orientation::Horizontal
                    && (edges.contains(ResizeEdge::TOP) || edges.contains(ResizeEdge::BOTTOM))))
            {
                node_id = group_id.clone();
                continue;
            }

            let node_idx = tree
                .children_ids(&group_id)
                .unwrap()
                .position(|id| id == &node_id)
                .unwrap();
            let Some(other_idx) = (match edges {
                x if x.intersects(ResizeEdge::TOP_LEFT) => node_idx.checked_sub(1),
                _ => {
                    if tree.children_ids(&group_id).unwrap().count() - 1 > node_idx {
                        Some(node_idx + 1)
                    } else {
                        None
                    }
                }
            }) else {
                node_id = group_id.clone();
                continue;
            };

            let data = tree.get_mut(&group_id).unwrap().data_mut();

            match data {
                Data::Group { sizes, .. } => {
                    let (shrink_idx, grow_idx) = if direction == ResizeDirection::Inwards {
                        (node_idx, other_idx)
                    } else {
                        (other_idx, node_idx)
                    };

                    if sizes[shrink_idx] + sizes[grow_idx]
                        < match orientation {
                            Orientation::Vertical => 720,
                            Orientation::Horizontal => 480,
                        }
                    {
                        return true;
                    };

                    let old_size = sizes[shrink_idx];
                    sizes[shrink_idx] =
                        (old_size - amount).max(if orientation == Orientation::Vertical {
                            360
                        } else {
                            240
                        });
                    let diff = old_size - sizes[shrink_idx];
                    sizes[grow_idx] += diff;
                }
                _ => unreachable!(),
            }

            let should_configure =
                tree.traverse_pre_order(&group_id)
                    .unwrap()
                    .all(|node| match node.data() {
                        Data::Mapped { mapped, .. } => mapped.latest_size_committed(),
                        _ => true,
                    });
            if should_configure {
                let blocker = self.update_positions_for(&mut tree, gaps);
                self.queue.push_tree(tree, None, blocker);
            }

            return true;
        }

        true
    }

    pub fn stacking_indicator(&self) -> Option<Rectangle<i32, Local>> {
        if let Some(TargetZone::WindowStack(_, geo)) =
            self.last_overview_hover.as_ref().map(|(_, zone)| zone)
        {
            Some(*geo)
        } else {
            None
        }
    }

    pub fn cleanup_drag(&mut self) {
        self.scrolling.cancel_drag();
        let old_tree = &self.queue.trees.back().unwrap().0;
        let mut new_tree = None;

        if let Some(root) = old_tree.root_node_id() {
            for id in old_tree.traverse_pre_order_ids(root).unwrap() {
                match old_tree.get(&id).map(|node| node.data()) {
                    Ok(Data::Placeholder { .. }) => {
                        // Copy a tree on write
                        let new_tree = new_tree.get_or_insert_with(|| old_tree.copy_clone());
                        TilingLayout::unmap_internal(new_tree, &id)
                    }
                    Ok(Data::Group { pill_indicator, .. }) if pill_indicator.is_some() => {
                        let new_tree = new_tree.get_or_insert_with(|| old_tree.copy_clone());
                        match new_tree.get_mut(&id).unwrap().data_mut() {
                            Data::Group { pill_indicator, .. } => {
                                *pill_indicator = None;
                            }
                            _ => unreachable!(),
                        }
                    }
                    _ => {}
                }
            }

            // If anything was changed, push updated tree
            if let Some(mut new_tree) = new_tree {
                let gaps = self.gaps();
                let blocker = self.update_positions_for(&mut new_tree, gaps);
                if self.tiling_engine == TilingEngine::Scrolling {
                    self.queue.push_direct_tree(new_tree, blocker);
                } else {
                    self.queue.push_tree(new_tree, ANIMATION_DURATION, blocker);
                }
            }
        }
    }

    pub fn drop_window(
        &mut self,
        window: CosmicMapped,
        source_geometry: Rectangle<i32, Local>,
    ) -> (CosmicMapped, Point<i32, Local>) {
        let gaps = self.gaps();
        let dropped_window = window.clone();

        let mut tree = self.queue.trees.back().unwrap().0.copy_clone();
        let update_scrolling_model =
            self.tiling_engine == TilingEngine::Scrolling || self.scrolling.has_pending_drag();
        let mut column_drop_target = scrolling_drop_target(
            &tree,
            self.last_overview_hover.as_ref().map(|(_, zone)| zone),
        );
        if self.tiling_engine != TilingEngine::Scrolling
            && column_drop_target != ColumnDropTarget::Discard
        {
            // Classic drops keep their normal tree behavior. If this window
            // already had persistent scrolling state, only its replaced node
            // identity is carried across the dormant Classic round trip.
            column_drop_target = ColumnDropTarget::Original;
        }

        window.output_enter(&self.output, window.bbox());
        {
            let layer_map = layer_map_for_output(&self.output);
            window.set_bounds(layer_map.non_exclusive_zone().size);
        }

        let mapped = match self.last_overview_hover.as_ref().map(|(_, zone)| zone) {
            Some(TargetZone::GroupEdge(group_id, direction)) if tree.get(group_id).is_ok() => {
                let new_id = tree
                    .insert(
                        Node::new(Data::Mapped {
                            mapped: window.clone(),
                            last_geometry: Rectangle::from_size((100, 100).into()),
                            minimize_rect: None,
                        }),
                        InsertBehavior::UnderNode(group_id),
                    )
                    .unwrap();

                let orientation = if matches!(direction, Direction::Left | Direction::Right) {
                    Orientation::Vertical
                } else {
                    Orientation::Horizontal
                };
                if tree.get(group_id).unwrap().data().orientation() != orientation {
                    TilingLayout::new_group(&mut tree, group_id, &new_id, orientation).unwrap();
                } else {
                    let data = tree.get_mut(group_id).unwrap().data_mut();
                    let len = data.len();
                    data.add_window(if matches!(direction, Direction::Left | Direction::Up) {
                        0
                    } else {
                        len
                    });
                }
                if matches!(direction, Direction::Left | Direction::Up) {
                    tree.make_first_sibling(&new_id).unwrap();
                } else {
                    tree.make_last_sibling(&new_id).unwrap();
                }
                *window.tiling_node_id.lock().unwrap() = Some(new_id);
                window
            }
            Some(TargetZone::GroupInterior(group_id, idx)) if tree.get(group_id).is_ok() => {
                let new_id = tree
                    .insert(
                        Node::new(Data::Mapped {
                            mapped: window.clone(),
                            last_geometry: Rectangle::from_size((100, 100).into()),
                            minimize_rect: None,
                        }),
                        InsertBehavior::UnderNode(group_id),
                    )
                    .unwrap();
                let idx = {
                    let data = tree.get_mut(group_id).unwrap().data_mut();
                    let bound_idx = data.len().min(*idx + 1);
                    data.add_window(bound_idx);
                    bound_idx
                };
                tree.make_nth_sibling(&new_id, idx).unwrap();
                *window.tiling_node_id.lock().unwrap() = Some(new_id);
                window
            }
            Some(TargetZone::InitialPlaceholder(node_id)) if tree.get(node_id).is_ok() => {
                let data = tree.get_mut(node_id).unwrap().data_mut();
                let geo = *data.geometry();

                *data = Data::Mapped {
                    mapped: window.clone(),
                    last_geometry: geo,
                    minimize_rect: None,
                };
                *window.tiling_node_id.lock().unwrap() = Some(node_id.clone());
                window
            }
            Some(TargetZone::WindowSplit(window_id, direction)) if tree.get(window_id).is_ok() => {
                let new_id = tree
                    .insert(
                        Node::new(Data::Mapped {
                            mapped: window.clone(),
                            last_geometry: Rectangle::from_size((100, 100).into()),
                            minimize_rect: None,
                        }),
                        InsertBehavior::UnderNode(window_id),
                    )
                    .unwrap();
                let orientation = if matches!(direction, Direction::Left | Direction::Right) {
                    Orientation::Vertical
                } else {
                    Orientation::Horizontal
                };
                TilingLayout::new_group(&mut tree, window_id, &new_id, orientation).unwrap();
                if matches!(direction, Direction::Left | Direction::Up) {
                    tree.make_first_sibling(&new_id).unwrap();
                }
                *window.tiling_node_id.lock().unwrap() = Some(new_id.clone());
                window
            }
            Some(TargetZone::WindowStack(window_id, _)) if tree.get(window_id).is_ok() => {
                match tree.get_mut(window_id).unwrap().data_mut() {
                    Data::Mapped { mapped, .. } => {
                        mapped.convert_to_stack(
                            (&self.output, mapped.bbox()),
                            self.theme.clone(),
                            self.appearance,
                        );
                        let Some(stack) = mapped.stack_ref() else {
                            unreachable!()
                        };
                        for surface in window.windows().map(|s| s.0) {
                            stack.add_window(surface, None, None);
                        }
                        mapped.clone()
                    }
                    _ => unreachable!(),
                }
            }
            _ => {
                TilingLayout::map_to_tree(
                    &mut tree,
                    window.clone(),
                    &self.output,
                    None,
                    None,
                    None,
                );
                window
            }
        };

        if let Some(root) = tree.root_node_id() {
            for id in tree
                .traverse_pre_order_ids(root)
                .unwrap()
                .collect::<Vec<_>>()
                .into_iter()
            {
                match tree.get_mut(&id).map(|node| node.data_mut()) {
                    Ok(Data::Placeholder { .. }) => TilingLayout::unmap_internal(&mut tree, &id),
                    Ok(Data::Group { pill_indicator, .. }) if pill_indicator.is_some() => {
                        pill_indicator.take();
                    }
                    _ => {}
                }
            }
        }

        let final_node_id = mapped.tiling_node_id.lock().unwrap().clone();
        if self.tiling_engine == TilingEngine::Scrolling
            && mapped == dropped_window
            && let Some(final_node_id) = final_node_id.as_ref()
            && let Ok(node) = tree.get_mut(final_node_id)
            && let Data::Mapped { minimize_rect, .. } = node.data_mut()
        {
            *minimize_rect = Some(source_geometry);
        }
        if update_scrolling_model {
            self.scrolling
                .finish_drag(final_node_id.as_ref(), column_drop_target);
        }
        let blocker = if self.tiling_engine == TilingEngine::Scrolling {
            self.scrolling.update_positions_ensuring(
                &self.output,
                &mut tree,
                gaps,
                final_node_id.as_ref(),
            )
        } else {
            self.update_positions_for(&mut tree, gaps)
        };
        if self.tiling_engine == TilingEngine::Scrolling {
            self.push_scrolling_animation(tree, blocker);
        } else {
            self.queue.push_tree(tree, ANIMATION_DURATION, blocker);
        }

        let location = self.element_geometry(&mapped).unwrap().loc;
        (mapped, location)
    }

    fn last_active_window<'a>(
        tree: &Tree<Data>,
        mut focus_stack: impl Iterator<Item = &'a FocusTarget>,
    ) -> Option<(NodeId, CosmicMapped)> {
        focus_stack.find_map(|target| {
            tree.root_node_id().and_then(|root| {
                tree.traverse_pre_order_ids(root).unwrap().find_map(|id| {
                    let Ok(Data::Mapped { mapped, .. }) = tree.get(&id).map(|n| n.data()) else {
                        return None;
                    };

                    if target == mapped {
                        Some((id, mapped.clone()))
                    } else {
                        None
                    }
                })
            })
        })
    }

    fn currently_focused_node(
        tree: &Tree<Data>,
        mut target: KeyboardFocusTarget,
    ) -> Option<(NodeId, FocusedNodeData)> {
        // if the focus is currently on a popup, treat it's toplevel as the target
        if let KeyboardFocusTarget::Popup(popup) = target {
            let toplevel_surface = match popup {
                PopupKind::Xdg(_) => get_popup_toplevel(&popup),
                PopupKind::InputMethod(_) => unreachable!(),
            }?;
            let root_id = tree.root_node_id()?;
            let node =
                tree.traverse_pre_order(root_id)
                    .unwrap()
                    .find(|node| match node.data() {
                        Data::Mapped { mapped, .. } => mapped
                            .windows()
                            .any(|(w, _)| w.wl_surface().as_deref() == Some(&toplevel_surface)),
                        _ => false,
                    })?;

            target = KeyboardFocusTarget::Element(match node.data() {
                Data::Mapped { mapped, .. } => mapped.clone(),
                _ => unreachable!(),
            });
        }

        match target {
            KeyboardFocusTarget::Element(mapped) => {
                let node_id = mapped.tiling_node_id.lock().unwrap().clone()?;
                let node = tree.get(&node_id).ok()?;
                let data = node.data();
                if data.is_mapped(Some(&mapped)) {
                    return Some((node_id, FocusedNodeData::Window(mapped)));
                }
            }
            KeyboardFocusTarget::Group(window_group) => {
                let node = tree.get(&window_group.node).ok()?;
                if node.data().is_group() {
                    return Some((
                        window_group.node,
                        FocusedNodeData::Group(window_group.focus_stack, window_group.alive),
                    ));
                }
            }
            _ => {}
        };

        None
    }

    fn new_group(
        tree: &mut Tree<Data>,
        old_id: &NodeId,
        new_id: &NodeId,
        orientation: Orientation,
    ) -> Result<NodeId, NodeIdError> {
        let new_group = Node::new(Data::new_group(
            orientation,
            Rectangle::from_size((100, 100).into()),
        ));
        let old = tree.get(old_id)?;
        let parent_id = old.parent().cloned();
        let pos = parent_id.as_ref().and_then(|parent_id| {
            tree.children_ids(parent_id)
                .unwrap()
                .position(|id| id == old_id)
        });

        let group_id = tree
            .insert(
                new_group,
                if let Some(parent) = parent_id.as_ref() {
                    InsertBehavior::UnderNode(parent)
                } else {
                    InsertBehavior::AsRoot
                },
            )
            .unwrap();

        tree.move_node(old_id, MoveBehavior::ToParent(&group_id))
            .unwrap();
        // keep position
        if let Some(old_pos) = pos {
            tree.make_nth_sibling(&group_id, old_pos).unwrap();
        }
        tree.move_node(new_id, MoveBehavior::ToParent(&group_id))
            .unwrap();

        Ok(group_id)
    }

    fn has_adjacent_node(tree: &Tree<Data>, node: &NodeId, direction: Direction) -> bool {
        let mut search_node = node;
        match tree.ancestor_ids(node) {
            Ok(mut iter) => iter.any(|parent_id| {
                let parent = tree.get(parent_id).unwrap();
                let children = parent.children();
                let idx = children.iter().position(|id| id == search_node).unwrap();
                search_node = parent_id;

                match direction {
                    Direction::Up => {
                        parent.data().orientation() == Orientation::Horizontal && idx > 0
                    }
                    Direction::Down => {
                        parent.data().orientation() == Orientation::Horizontal
                            && idx < (children.len() - 1)
                    }
                    Direction::Left => {
                        parent.data().orientation() == Orientation::Vertical && idx > 0
                    }
                    Direction::Right => {
                        parent.data().orientation() == Orientation::Vertical
                            && idx < (children.len() - 1)
                    }
                }
            }),
            Err(_) => false,
        }
    }

    fn has_sibling_node(tree: &Tree<Data>, node: &NodeId, direction: Direction) -> bool {
        match tree.get(node).ok().and_then(|node| node.parent()) {
            Some(parent_id) => {
                let parent = tree.get(parent_id).unwrap();
                let children = parent.children();
                let idx = children.iter().position(|id| id == node).unwrap();

                match direction {
                    Direction::Up => {
                        parent.data().orientation() == Orientation::Horizontal && idx > 0
                    }
                    Direction::Down => {
                        parent.data().orientation() == Orientation::Horizontal
                            && idx < (children.len() - 1)
                    }
                    Direction::Left => {
                        parent.data().orientation() == Orientation::Vertical && idx > 0
                    }
                    Direction::Right => {
                        parent.data().orientation() == Orientation::Vertical
                            && idx < (children.len() - 1)
                    }
                }
            }
            None => false,
        }
    }

    #[profiling::function]
    fn update_positions_for(
        &mut self,
        tree: &mut Tree<Data>,
        gaps: (i32, i32),
    ) -> Option<TilingBlocker> {
        TilingLayout::update_positions_with(
            &self.output,
            self.tiling_engine,
            &mut self.scrolling,
            tree,
            gaps,
        )
    }

    fn update_positions_with(
        output: &Output,
        tiling_engine: TilingEngine,
        scrolling: &mut ScrollingLayout,
        tree: &mut Tree<Data>,
        gaps: (i32, i32),
    ) -> Option<TilingBlocker> {
        match tiling_engine {
            TilingEngine::Classic => TilingLayout::update_positions(output, tree, gaps),
            TilingEngine::Scrolling => scrolling.update_positions(output, tree, gaps),
        }
    }

    #[profiling::function]
    fn update_positions(
        output: &Output,
        tree: &mut Tree<Data>,
        gaps: (i32, i32),
    ) -> Option<TilingBlocker> {
        if let Some(root_id) = tree.root_node_id() {
            let mut configures = Vec::new();

            let (outer, inner) = gaps;
            let mut geo = layer_map_for_output(output).non_exclusive_zone().as_local();
            geo.loc.x += outer;
            geo.loc.y += outer;
            geo.size.w -= outer * 2;
            geo.size.h -= outer * 2;
            let mut stack = vec![geo];

            for node_id in tree
                .traverse_pre_order_ids(root_id)
                .unwrap()
                .collect::<Vec<_>>()
                .into_iter()
            {
                let node = tree.get_mut(&node_id).unwrap();
                let data = node.data_mut();

                // flatten tree
                if data.is_group() && data.len() == 1 {
                    // RemoveBehavior::LiftChildren sadly does not what we want: lifting them into the same place.
                    // So we need to fix that manually..
                    let idx = node.parent().cloned().map(|parent_id| {
                        tree.children_ids(&parent_id)
                            .unwrap()
                            .position(|id| id == &node_id)
                            .unwrap()
                    });
                    let child_id = tree
                        .children_ids(&node_id)
                        .unwrap()
                        .next()
                        .cloned()
                        .unwrap();
                    tree.remove_node(node_id, RemoveBehavior::LiftChildren)
                        .unwrap();
                    if let Some(idx) = idx {
                        tree.make_nth_sibling(&child_id, idx).unwrap();
                    } else {
                        // additionally `RemoveBehavior::LiftChildren` doesn't work, when removing the root-node,
                        // even with just one child. *sigh*
                        tree.move_node(&child_id, MoveBehavior::ToRoot).unwrap();
                    }

                    continue;
                }

                if let Some(mut geo) = stack.pop() {
                    let node = tree.get(&node_id).unwrap();
                    let data = node.data();
                    if data.is_mapped(None) {
                        let gap = (
                            (
                                if TilingLayout::has_adjacent_node(tree, &node_id, Direction::Left)
                                {
                                    inner / 2
                                } else {
                                    inner
                                },
                                if TilingLayout::has_adjacent_node(tree, &node_id, Direction::Up) {
                                    inner / 2
                                } else {
                                    inner
                                },
                            ),
                            (
                                if TilingLayout::has_adjacent_node(tree, &node_id, Direction::Right)
                                {
                                    inner / 2
                                } else {
                                    inner
                                },
                                if TilingLayout::has_adjacent_node(tree, &node_id, Direction::Down)
                                {
                                    inner / 2
                                } else {
                                    inner
                                },
                            ),
                        );
                        geo.loc += gap.0.into();
                        geo.size -= gap.0.into();
                        geo.size -= gap.1.into();
                    }

                    let node = tree.get_mut(&node_id).unwrap();
                    let data = node.data_mut();
                    data.update_geometry(geo);

                    match data {
                        Data::Group {
                            orientation, sizes, ..
                        } => match orientation {
                            Orientation::Horizontal => {
                                let mut previous: i32 = sizes.iter().sum();
                                for size in sizes.iter().rev() {
                                    previous -= *size;
                                    stack.push(Rectangle::new(
                                        (geo.loc.x, geo.loc.y + previous).into(),
                                        (geo.size.w, *size).into(),
                                    ));
                                }
                            }
                            Orientation::Vertical => {
                                let mut previous: i32 = sizes.iter().sum();
                                for size in sizes.iter().rev() {
                                    previous -= *size;
                                    stack.push(Rectangle::new(
                                        (geo.loc.x + previous, geo.loc.y).into(),
                                        (*size, geo.size.h).into(),
                                    ));
                                }
                            }
                        },
                        Data::Mapped { mapped, .. } => {
                            if !(mapped.is_fullscreen(true) || mapped.is_maximized(true)) {
                                mapped.set_tiled(true);
                                let internal_geometry = geo.to_global(output);
                                mapped.set_geometry(internal_geometry);
                                if let Some(serial) = mapped.configure() {
                                    configures.push((mapped.active_window(), serial));
                                }
                            }
                        }
                        Data::Placeholder { .. } => {}
                    }
                }
            }

            if !configures.is_empty() {
                let blocker = TilingBlocker::new(configures);
                for (surface, _) in &blocker.necessary_acks {
                    if let Some(surface) = surface.wl_surface() {
                        add_blocker(&surface, blocker.clone());
                    }
                }
                return Some(blocker);
            }
        }

        None
    }

    pub fn popup_element_under(
        &self,
        location_f64: Point<f64, Local>,
        seat: &Seat<State>,
    ) -> Option<KeyboardFocusTarget> {
        let location = location_f64.to_i32_floor();

        for (mapped, geo) in self.mapped() {
            if !mapped.bbox().contains((location - geo.loc).as_logical()) {
                continue;
            }

            if mapped
                .focus_under(
                    (location_f64 - geo.loc.to_f64()).as_logical() + mapped.geometry().loc.to_f64(),
                    WindowSurfaceType::POPUP | WindowSurfaceType::SUBSURFACE,
                    seat,
                )
                .is_some()
            {
                return Some(mapped.clone().into());
            }
        }

        None
    }

    pub fn toplevel_element_under(
        &self,
        location_f64: Point<f64, Local>,
        seat: &Seat<State>,
    ) -> Option<KeyboardFocusTarget> {
        let location = location_f64.to_i32_floor();

        for (mapped, geo) in self.mapped() {
            // Tiled windows are rendered cropped to their tile (`geo`), so input must be bound to the tile as well
            if !geo.contains(location) {
                continue;
            }
            if !mapped.bbox().contains((location - geo.loc).as_logical()) {
                continue;
            }

            if mapped
                .focus_under(
                    (location_f64 - geo.loc.to_f64()).as_logical() + mapped.geometry().loc.to_f64(),
                    WindowSurfaceType::TOPLEVEL | WindowSurfaceType::SUBSURFACE,
                    seat,
                )
                .is_some()
            {
                return Some(mapped.clone().into());
            }
        }

        None
    }

    pub fn popup_surface_under(
        &self,
        location_f64: Point<f64, Local>,
        overview: OverviewMode,
        seat: &Seat<State>,
    ) -> Option<(PointerFocusTarget, Point<f64, Local>)> {
        let location = location_f64.to_i32_floor();

        if matches!(overview, OverviewMode::None) {
            for (mapped, geo) in self.mapped() {
                if !mapped.bbox().contains((location - geo.loc).as_logical()) {
                    continue;
                }
                if mapped.is_maximized(false) {
                    continue;
                }
                if let Some((target, surface_offset)) = mapped.focus_under(
                    (location_f64 - geo.loc.to_f64()).as_logical() + mapped.geometry().loc.to_f64(),
                    WindowSurfaceType::POPUP | WindowSurfaceType::SUBSURFACE,
                    seat,
                ) {
                    return Some((
                        target,
                        geo.loc.to_f64() - mapped.geometry().loc.as_local().to_f64()
                            + surface_offset.as_local(),
                    ));
                }
            }
        }

        None
    }

    pub fn toplevel_surface_under(
        &self,
        location_f64: Point<f64, Local>,
        overview: OverviewMode,
        seat: &Seat<State>,
    ) -> Option<(PointerFocusTarget, Point<f64, Local>)> {
        let tree = &self.queue.trees.back().unwrap().0;
        let root = tree.root_node_id()?;
        let location = location_f64.to_i32_floor();

        if matches!(overview, OverviewMode::None) {
            for (mapped, geo) in self.mapped() {
                // Tiled windows are rendered cropped to their tile (`geo`), so input must be bound to the tile as well
                if !geo.contains(location) {
                    continue;
                }
                if !mapped.bbox().contains((location - geo.loc).as_logical()) {
                    continue;
                }
                if mapped.is_maximized(false) {
                    continue;
                }
                if let Some((target, surface_offset)) = mapped.focus_under(
                    (location_f64 - geo.loc.to_f64()).as_logical() + mapped.geometry().loc.to_f64(),
                    WindowSurfaceType::TOPLEVEL | WindowSurfaceType::SUBSURFACE,
                    seat,
                ) {
                    return Some((
                        target,
                        geo.loc.to_f64() - mapped.geometry().loc.as_local().to_f64()
                            + surface_offset.as_local(),
                    ));
                }
            }

            let mut result = None;
            let mut lookup = Some(root.clone());
            while let Some(node) = lookup {
                let data = tree.get(&node).unwrap().data();
                if data.geometry().contains(location) {
                    result = Some(node.clone());
                }

                lookup = None;
                if result.is_some() && data.is_group() {
                    for child_id in tree.children_ids(&node).unwrap() {
                        if tree
                            .get(child_id)
                            .unwrap()
                            .data()
                            .geometry()
                            .contains(location)
                        {
                            lookup = Some(child_id.clone());
                            break;
                        }
                    }
                }
            }

            match result.map(|id| (id.clone(), tree.get(&id).unwrap().data().clone())) {
                Some((
                    _,
                    Data::Mapped {
                        mapped,
                        last_geometry,
                        ..
                    },
                )) => {
                    let test_point = (location_f64 - last_geometry.loc.to_f64()
                        + mapped.geometry().loc.to_f64().as_local())
                    .as_logical();
                    mapped
                        .focus_under(
                            test_point,
                            WindowSurfaceType::TOPLEVEL | WindowSurfaceType::SUBSURFACE,
                            seat,
                        )
                        .map(|(surface, surface_offset)| {
                            (
                                surface,
                                last_geometry.loc.to_f64()
                                    - mapped.geometry().loc.as_local().to_f64()
                                    + surface_offset.as_local(),
                            )
                        })
                }
                Some((
                    id,
                    Data::Group {
                        orientation,
                        last_geometry,
                        ..
                    },
                )) => {
                    if self.tiling_engine == TilingEngine::Scrolling {
                        let tile = self.scrolling_focused_tile(seat)?;
                        if let Some((upper, lower, boundary)) =
                            self.scrolling.row_resize_edge_at(tree, &tile, location_f64)
                        {
                            let x = tree.get(&tile).ok()?.data().geometry().loc.x;
                            return Some((
                                ResizeForkTarget {
                                    node: id,
                                    output: self.output.downgrade(),
                                    left_up_idx: 0,
                                    orientation: Orientation::Horizontal,
                                    scrolling_resize: Some(ScrollingResizeTarget::Rows {
                                        upper,
                                        lower,
                                    }),
                                }
                                .into(),
                                Point::<i32, Local>::from((x, boundary)).to_f64(),
                            ));
                        }

                        let (anchor, boundary) =
                            self.scrolling.resize_edge_at(tree, &tile, location_f64.x)?;
                        return Some((
                            ResizeForkTarget {
                                node: id,
                                output: self.output.downgrade(),
                                left_up_idx: 0,
                                orientation: Orientation::Vertical,
                                scrolling_resize: Some(ScrollingResizeTarget::Column {
                                    tile,
                                    anchor,
                                }),
                            }
                            .into(),
                            Point::<i32, Local>::from((boundary, last_geometry.loc.y)).to_f64(),
                        ));
                    }

                    let idx = tree
                        .children(&id)
                        .unwrap()
                        .position(|node| {
                            let data = node.data();
                            match orientation {
                                Orientation::Vertical => location.x < data.geometry().loc.x,
                                Orientation::Horizontal => location.y < data.geometry().loc.y,
                            }
                        })
                        .and_then(|x| x.checked_sub(1))?;
                    Some((
                        ResizeForkTarget {
                            node: id.clone(),
                            output: self.output.downgrade(),
                            left_up_idx: idx,
                            orientation,
                            scrolling_resize: None,
                        }
                        .into(),
                        (last_geometry.loc
                            + tree
                                .children(&id)
                                .unwrap()
                                .nth(idx)
                                .map(|node| {
                                    let geo = node.data().geometry();
                                    geo.loc + geo.size
                                })
                                .unwrap())
                        .to_f64(),
                    ))
                }
                _ => None,
            }
        } else {
            None
        }
    }

    pub fn update_pointer_position(
        &mut self,
        location_f64: Option<Point<f64, Local>>,
        overview: OverviewMode,
    ) {
        let gaps = self.gaps();
        let last_overview_hover = &mut self.last_overview_hover;
        let tree = &self.queue.trees.back().unwrap().0;
        let Some(root) = tree.root_node_id() else {
            if matches!(
                overview.active_trigger(),
                Some(Trigger::Pointer(_) | Trigger::Touch(_))
            ) && location_f64.is_some()
            {
                let mut tree = tree.copy_clone();
                tree.insert(
                    Node::new(Data::Placeholder {
                        id: Id::new(),
                        last_geometry: Rectangle::from_size((100, 100).into()),
                        type_: PlaceholderType::DropZone,
                    }),
                    InsertBehavior::AsRoot,
                )
                .unwrap();
                let blocker = TilingLayout::update_positions_with(
                    &self.output,
                    self.tiling_engine,
                    &mut self.scrolling,
                    &mut tree,
                    gaps,
                );
                if self.tiling_engine == TilingEngine::Scrolling {
                    self.queue.push_direct_tree(tree, blocker);
                } else {
                    self.queue.push_tree(tree, ANIMATION_DURATION, blocker);
                }
            }
            return;
        };

        if !matches!(
            overview,
            OverviewMode::Started(_, _) | OverviewMode::Active(_)
        ) || location_f64.is_none()
        {
            last_overview_hover.take();
            return;
        }

        let location_f64 = location_f64.unwrap();
        let location = location_f64.to_i32_floor();

        if matches!(
            overview.active_trigger(),
            Some(Trigger::Pointer(_) | Trigger::Touch(_))
        ) {
            let non_exclusive_zone = layer_map_for_output(&self.output)
                .non_exclusive_zone()
                .as_local();
            let geometries = geometries_for_groupview(
                tree,
                Option::<&mut GlowRenderer>::None,
                non_exclusive_zone,
                None,
                self.output.current_scale().fractional_scale(),
                1.0,
                overview.alpha().unwrap(),
                &self.backdrop_id,
                Some(None),
                None,
                None,
                self.theme.cosmic(),
                &mut |_| {},
            );

            let mut result = None;
            let mut lookup = Some(root.clone());
            while let Some(node) = lookup {
                let data = tree.get(&node).unwrap().data();
                if geometries
                    .get(&node)
                    .map(|geo| geo.contains(location))
                    .unwrap_or(false)
                {
                    result = Some(node.clone());
                }

                lookup = None;
                if result.is_some() && data.is_group() {
                    if tree.children(&node).unwrap().any(|child| {
                        matches!(
                            child.data(),
                            Data::Placeholder {
                                type_: PlaceholderType::DropZone,
                                ..
                            }
                        )
                    }) {
                        break;
                    }
                    for child_id in tree.children_ids(&node).unwrap() {
                        if geometries
                            .get(child_id)
                            .map(|geo| geo.contains(location))
                            .unwrap_or(false)
                        {
                            lookup = Some(child_id.clone());
                            break;
                        }
                    }
                }
            }

            if let Some(res_id) = result {
                let Some(mut last_geometry) = geometries.get(&res_id).copied() else {
                    return;
                };
                let node = tree.get(&res_id).unwrap();
                let data = node.data().clone();

                let group_zone = if let Data::Group { orientation, .. } = &data {
                    if node.children().iter().any(|child_id| {
                        tree.get(child_id)
                            .ok()
                            .map(|child| {
                                matches!(
                                    child.data(),
                                    Data::Placeholder {
                                        type_: PlaceholderType::DropZone,
                                        ..
                                    }
                                )
                            })
                            .unwrap_or(false)
                    }) {
                        None
                    } else {
                        let left_edge = match &*last_overview_hover {
                            Some((_, TargetZone::GroupEdge(id, Direction::Left)))
                                if *id == res_id =>
                            {
                                let zone = Rectangle::new(
                                    last_geometry.loc,
                                    (80, last_geometry.size.h).into(),
                                );
                                last_geometry.loc.x += 80;
                                last_geometry.size.w -= 80;
                                zone
                            }
                            _ => {
                                let zone = Rectangle::new(
                                    last_geometry.loc,
                                    (32, last_geometry.size.h).into(),
                                );
                                last_geometry.loc.x += 32;
                                last_geometry.size.w -= 32;
                                zone
                            }
                        };
                        let top_edge = match &*last_overview_hover {
                            Some((_, TargetZone::GroupEdge(id, Direction::Up)))
                                if *id == res_id =>
                            {
                                let zone = Rectangle::new(
                                    last_geometry.loc,
                                    (last_geometry.size.w, 80).into(),
                                );
                                last_geometry.loc.y += 80;
                                last_geometry.size.h -= 80;
                                zone
                            }
                            _ => {
                                let zone = Rectangle::new(
                                    last_geometry.loc,
                                    (last_geometry.size.w, 32).into(),
                                );
                                last_geometry.loc.y += 32;
                                last_geometry.size.h -= 32;
                                zone
                            }
                        };
                        let right_edge = match &*last_overview_hover {
                            Some((_, TargetZone::GroupEdge(id, Direction::Right)))
                                if *id == res_id =>
                            {
                                let zone = Rectangle::new(
                                    (
                                        last_geometry.loc.x + last_geometry.size.w - 80,
                                        last_geometry.loc.y,
                                    )
                                        .into(),
                                    (80, last_geometry.size.h).into(),
                                );
                                last_geometry.size.w -= 80;
                                zone
                            }
                            _ => {
                                let zone = Rectangle::new(
                                    (
                                        last_geometry.loc.x + last_geometry.size.w - 32,
                                        last_geometry.loc.y,
                                    )
                                        .into(),
                                    (32, last_geometry.size.h).into(),
                                );
                                last_geometry.size.w -= 32;
                                zone
                            }
                        };
                        let bottom_edge = match &*last_overview_hover {
                            Some((_, TargetZone::GroupEdge(id, Direction::Down)))
                                if *id == res_id =>
                            {
                                let zone = Rectangle::new(
                                    (
                                        last_geometry.loc.x,
                                        last_geometry.loc.y + last_geometry.size.h - 80,
                                    )
                                        .into(),
                                    (last_geometry.size.w, 80).into(),
                                );
                                last_geometry.size.h -= 80;
                                zone
                            }
                            _ => {
                                let zone = Rectangle::new(
                                    (
                                        last_geometry.loc.x,
                                        last_geometry.loc.y + last_geometry.size.h - 32,
                                    )
                                        .into(),
                                    (last_geometry.size.w, 32).into(),
                                );
                                last_geometry.size.h -= 32;
                                zone
                            }
                        };

                        if left_edge.contains(location) {
                            Some(TargetZone::GroupEdge(res_id.clone(), Direction::Left))
                        } else if right_edge.contains(location) {
                            Some(TargetZone::GroupEdge(res_id.clone(), Direction::Right))
                        } else if top_edge.contains(location) {
                            Some(TargetZone::GroupEdge(res_id.clone(), Direction::Up))
                        } else if bottom_edge.contains(location) {
                            Some(TargetZone::GroupEdge(res_id.clone(), Direction::Down))
                        } else {
                            let idx = tree
                                .children_ids(&res_id)
                                .unwrap()
                                .position(|node| {
                                    let Some(geo) = geometries.get(node) else {
                                        return false;
                                    };
                                    match orientation {
                                        Orientation::Vertical => location.x < geo.loc.x,
                                        Orientation::Horizontal => location.y < geo.loc.y,
                                    }
                                })
                                .and_then(|x| x.checked_sub(1))
                                .unwrap_or(0);
                            Some(TargetZone::GroupInterior(res_id.clone(), idx))
                        }
                    }
                } else {
                    None
                };

                let target_zone = group_zone.unwrap_or_else(|| match &data {
                    Data::Placeholder { .. } => TargetZone::InitialPlaceholder(res_id),
                    Data::Group { .. } | Data::Mapped { .. } => {
                        let id = if data.is_group() {
                            tree.get(&res_id)
                                .unwrap()
                                .children()
                                .iter()
                                .find(|child_id| tree.get(child_id).unwrap().data().is_mapped(None))
                                .expect("Placeholder group without real window?")
                                .clone()
                        } else {
                            res_id
                        };

                        let third_width = (last_geometry.size.w as f64 / 3.0).round() as i32;
                        let third_height = (last_geometry.size.h as f64 / 3.0).round() as i32;
                        let stack_region = Rectangle::from_extremities(
                            (
                                last_geometry.loc.x + third_width,
                                last_geometry.loc.y + third_height,
                            ),
                            (
                                last_geometry.loc.x + 2 * third_width,
                                last_geometry.loc.y + 2 * third_height,
                            ),
                        );

                        if stack_region.contains(location) {
                            TargetZone::WindowStack(id, last_geometry)
                        } else {
                            let left_right = {
                                let relative_loc = location_f64.x - last_geometry.loc.x as f64;
                                if relative_loc < last_geometry.size.w as f64 / 2.0 {
                                    (Direction::Left, relative_loc / last_geometry.size.w as f64)
                                } else {
                                    (
                                        Direction::Right,
                                        1.0 - (relative_loc / last_geometry.size.w as f64),
                                    )
                                }
                            };
                            let up_down = {
                                let relative_loc = location_f64.y - last_geometry.loc.y as f64;
                                if relative_loc < last_geometry.size.h as f64 / 2.0 {
                                    (Direction::Up, relative_loc / last_geometry.size.h as f64)
                                } else {
                                    (
                                        Direction::Down,
                                        1.0 - (relative_loc / last_geometry.size.h as f64),
                                    )
                                }
                            };

                            let direction = if left_right.1 < up_down.1 {
                                left_right.0
                            } else {
                                up_down.0
                            };

                            TargetZone::WindowSplit(id, direction)
                        }
                    }
                });

                match &mut *last_overview_hover {
                    last_overview_hover @ None => {
                        *last_overview_hover = Some((
                            None,
                            tree.traverse_pre_order_ids(root)
                                .unwrap()
                                .find(|id| {
                                    matches!(
                                        tree.get(id).unwrap().data(),
                                        Data::Placeholder {
                                            type_: PlaceholderType::GrabbedWindow,
                                            ..
                                        }
                                    )
                                })
                                .map(TargetZone::InitialPlaceholder)
                                .unwrap_or(TargetZone::Initial),
                        ));
                    }
                    Some((instant, old_target_zone)) => {
                        if *old_target_zone != target_zone {
                            let overdue = if let Some(instant) = instant {
                                match old_target_zone {
                                    TargetZone::InitialPlaceholder(_) => {
                                        Instant::now().duration_since(*instant)
                                            > INITIAL_MOUSE_ANIMATION_DELAY
                                    }
                                    _ => {
                                        Instant::now().duration_since(*instant)
                                            > MOUSE_ANIMATION_DELAY
                                    }
                                }
                            } else {
                                *instant = Some(Instant::now());
                                false
                            };

                            if overdue {
                                let duration = if target_zone.is_window_zone()
                                    && !old_target_zone.is_window_zone()
                                {
                                    ANIMATION_DURATION * 2
                                } else {
                                    ANIMATION_DURATION
                                };

                                let mut tree = tree.copy_clone();

                                // remove old placeholders
                                let removed = if let TargetZone::InitialPlaceholder(node_id) =
                                    old_target_zone
                                {
                                    if tree.get(node_id).is_ok() {
                                        TilingLayout::unmap_internal(&mut tree, node_id);
                                    }
                                    true
                                } else if let TargetZone::WindowSplit(node_id, _) = old_target_zone
                                {
                                    if let Some(children) = tree
                                        .get(node_id)
                                        .ok()
                                        .and_then(|node| node.parent())
                                        .and_then(|parent_id| tree.get(parent_id).ok())
                                        .map(|node| node.children().clone())
                                    {
                                        for id in children {
                                            let matches = matches!(
                                                tree.get(&id).unwrap().data(),
                                                Data::Placeholder {
                                                    type_: PlaceholderType::DropZone,
                                                    ..
                                                }
                                            );

                                            if matches {
                                                TilingLayout::unmap_internal(&mut tree, &id);
                                                break;
                                            }
                                        }
                                    }
                                    true
                                } else if let TargetZone::GroupEdge(node_id, _) = old_target_zone {
                                    if let Ok(node) = tree.get_mut(node_id) {
                                        match node.data_mut() {
                                            Data::Group { pill_indicator, .. } => {
                                                *pill_indicator = None;
                                            }
                                            _ => unreachable!(),
                                        }
                                    }
                                    true
                                } else if let TargetZone::GroupInterior(node_id, _) =
                                    old_target_zone
                                {
                                    if let Ok(node) = tree.get_mut(node_id) {
                                        match node.data_mut() {
                                            Data::Group { pill_indicator, .. } => {
                                                *pill_indicator = None;
                                            }
                                            _ => unreachable!(),
                                        }
                                    }
                                    true
                                } else {
                                    false
                                };

                                // add placeholders
                                let added = if let TargetZone::WindowSplit(node_id, dir) =
                                    &target_zone
                                {
                                    let id = tree
                                        .insert(
                                            Node::new(Data::Placeholder {
                                                id: Id::new(),
                                                last_geometry: Rectangle::from_size(
                                                    (100, 100).into(),
                                                ),
                                                type_: PlaceholderType::DropZone,
                                            }),
                                            InsertBehavior::UnderNode(node_id),
                                        )
                                        .unwrap();
                                    let orientation =
                                        if matches!(dir, Direction::Left | Direction::Right) {
                                            Orientation::Vertical
                                        } else {
                                            Orientation::Horizontal
                                        };
                                    TilingLayout::new_group(&mut tree, node_id, &id, orientation)
                                        .unwrap();
                                    if matches!(dir, Direction::Left | Direction::Up) {
                                        tree.make_first_sibling(&id).unwrap();
                                    }

                                    true
                                } else if let TargetZone::GroupEdge(node_id, direction) =
                                    &target_zone
                                {
                                    if let Ok(node) = tree.get_mut(node_id) {
                                        match node.data_mut() {
                                            Data::Group { pill_indicator, .. } => {
                                                *pill_indicator =
                                                    Some(PillIndicator::Outer(*direction));
                                            }
                                            _ => unreachable!(),
                                        }
                                        true
                                    } else {
                                        false
                                    }
                                } else if let TargetZone::GroupInterior(node_id, idx) = &target_zone
                                {
                                    if let Ok(node) = tree.get_mut(node_id) {
                                        match node.data_mut() {
                                            Data::Group { pill_indicator, .. } => {
                                                *pill_indicator = Some(PillIndicator::Inner(*idx));
                                            }
                                            _ => unreachable!(),
                                        }
                                        true
                                    } else {
                                        false
                                    }
                                } else {
                                    false
                                };

                                if removed || added {
                                    let blocker = TilingLayout::update_positions_with(
                                        &self.output,
                                        self.tiling_engine,
                                        &mut self.scrolling,
                                        &mut tree,
                                        gaps,
                                    );
                                    if self.tiling_engine == TilingEngine::Scrolling {
                                        self.queue.push_direct_tree(tree, blocker);
                                    } else {
                                        self.queue.push_tree(tree, duration, blocker);
                                    }
                                }

                                *instant = None;
                                *old_target_zone = target_zone;
                            }
                        } else {
                            *instant = None;
                        }
                    }
                }
            }
        }
    }

    pub fn mapped(&self) -> impl Iterator<Item = (&CosmicMapped, Rectangle<i32, Local>)> {
        let tree = &self.queue.trees.back().unwrap().0;
        let iter = tree.root_node_id().map(|root| {
            tree.traverse_pre_order(root)
                .unwrap()
                .filter(|node| node.data().is_mapped(None))
                .filter(|node| match node.data() {
                    Data::Mapped { mapped, .. } => mapped.is_activated(false),
                    _ => unreachable!(),
                })
                .map(|node| match node.data() {
                    Data::Mapped {
                        mapped,
                        last_geometry,
                        ..
                    } => (mapped, *last_geometry),
                    _ => unreachable!(),
                })
                .chain(
                    tree.traverse_pre_order(root)
                        .unwrap()
                        .filter(|node| node.data().is_mapped(None))
                        .filter(|node| match node.data() {
                            Data::Mapped { mapped, .. } => !mapped.is_activated(false),
                            _ => unreachable!(),
                        })
                        .map(|node| match node.data() {
                            Data::Mapped {
                                mapped,
                                last_geometry,
                                ..
                            } => (mapped, *last_geometry),
                            _ => unreachable!(),
                        }),
                )
        });
        iter.into_iter().flatten()
    }

    pub fn windows(&self) -> impl Iterator<Item = (CosmicSurface, Rectangle<i32, Local>)> + '_ {
        self.mapped().flat_map(|(mapped, geo)| {
            mapped.windows().map(move |(w, p)| {
                (w, {
                    let mut geo = geo;
                    geo.loc += p.as_local();
                    geo.size -= p.to_size().as_local();
                    geo
                })
            })
        })
    }

    pub fn element_for_node(&self, node: &NodeId) -> Option<&CosmicMapped> {
        let tree = &self.queue.trees.back().unwrap().0;
        tree.get(node).ok().and_then(|node| match node.data() {
            Data::Mapped { mapped, .. } => Some(mapped),
            _ => None,
        })
    }

    pub fn has_node(&self, node: &NodeId) -> bool {
        let tree = &self.queue.trees.back().unwrap().0;
        tree.root_node_id()
            .map(|root| {
                tree.traverse_pre_order_ids(root)
                    .unwrap()
                    .any(|id| &id == node)
            })
            .unwrap_or(false)
    }

    pub fn merge(&mut self, mut other: TilingLayout) {
        let gaps = self.gaps();

        let src = other.queue.trees.pop_back().unwrap().0;
        let mut dst = self.queue.trees.back().unwrap().0.copy_clone();

        let orientation = match self.output.geometry().size {
            x if x.w >= x.h => Orientation::Vertical,
            _ => Orientation::Horizontal,
        };
        TilingLayout::merge_trees(src, &mut dst, orientation);

        let blocker = self.update_positions_for(&mut dst, gaps);
        self.queue.push_tree(dst, ANIMATION_DURATION, blocker);
    }

    fn merge_trees(src: Tree<Data>, dst: &mut Tree<Data>, orientation: Orientation) {
        if let Some(dst_root_id) = dst.root_node_id().cloned() {
            let mut stack = Vec::new();

            if let Some(src_root_id) = src.root_node_id() {
                let root_node = src.get(src_root_id).unwrap();
                let new_node = Node::new(root_node.data().clone());
                let new_id = dst
                    .insert(new_node, InsertBehavior::UnderNode(&dst_root_id))
                    .unwrap();
                if let &mut Data::Mapped { ref mut mapped, .. } =
                    dst.get_mut(&new_id).unwrap().data_mut()
                {
                    *mapped.tiling_node_id.lock().unwrap() = Some(new_id.clone());
                }
                TilingLayout::new_group(dst, &dst_root_id, &new_id, orientation).unwrap();
                stack.push((src_root_id.clone(), new_id));
            }

            while let Some((src_id, dst_id)) = stack.pop() {
                for child_id in src.children_ids(&src_id).unwrap() {
                    let src_node = src.get(child_id).unwrap();
                    let new_node = Node::new(src_node.data().clone());
                    let new_child_id = dst
                        .insert(new_node, InsertBehavior::UnderNode(&dst_id))
                        .unwrap();
                    if let &mut Data::Mapped { ref mut mapped, .. } =
                        dst.get_mut(&new_child_id).unwrap().data_mut()
                    {
                        *mapped.tiling_node_id.lock().unwrap() = Some(new_child_id.clone());
                    }
                    stack.push((child_id.clone(), new_child_id));
                }
            }
        } else {
            *dst = src;
        }
    }

    #[profiling::function]
    pub fn render<R>(
        &self,
        renderer: &mut R,
        seat: Option<&Seat<State>>,
        focused: Option<&CosmicMapped>,
        non_exclusive_zone: Rectangle<i32, Local>,
        overview: (OverviewMode, Option<(SwapIndicator, Option<&Tree<Data>>)>),
        resize_indicator: Option<(ResizeMode, ResizeIndicator)>,
        indicator_thickness: u8,
        theme: &cosmic::theme::CosmicTheme,
        scanout_node: Option<DrmNode>,
        push: &mut dyn FnMut(CosmicMappedRenderElement<R>),
    ) where
        R: AsGlowRenderer,
        R::TextureId: Send + Clone + 'static,
        CosmicMappedRenderElement<R>: RenderElement<R>,
        CosmicWindowRenderElement<R>: RenderElement<R>,
        CosmicStackRenderElement<R>: RenderElement<R>,
    {
        let output_scale = self.output.current_scale().fractional_scale();
        let live_resize_tiles = self
            .scrolling_live_resize
            .as_ref()
            .filter(|_| self.tiling_engine == TilingEngine::Scrolling)
            .map(|resize| resize.tiles.clone())
            .unwrap_or_default();

        let (target_tree, duration, _) = if self.queue.animation_start.is_some() {
            self.queue
                .trees
                .get(1)
                .expect("Animation ongoing, should have two trees")
        } else {
            self.queue.trees.front().unwrap()
        };
        let reference_tree = self
            .queue
            .animation_start
            .is_some()
            .then(|| &self.queue.trees.front().unwrap().0);

        let percentage = if let Some(animation_start) = self.queue.animation_start {
            if *duration == Duration::ZERO {
                1.0
            } else {
                let now = Instant::now();
                let total = duration.as_millis() as f32;
                let since = now.duration_since(animation_start).as_millis() as f32;
                let percentage = since / total;
                debug_assert!(!percentage.is_nan());
                debug_assert!(!percentage.is_infinite());
                ease(EaseInOutCubic, 0.0, 1.0, percentage)
            }
        } else {
            1.0
        };
        let draw_groups = overview.0.alpha();

        let is_overview = !matches!(overview.0, OverviewMode::None);
        let is_mouse_tiling = (matches!(overview.0.trigger(), Some(Trigger::Pointer(_))))
            .then(|| self.last_overview_hover.as_ref().map(|(_, zone)| zone));
        let swap_desc = if let Some(Trigger::KeyboardSwap(_, desc)) = overview.0.trigger() {
            Some(desc.clone())
        } else {
            None
        };

        // all gone windows and fade them out
        let old_geometries = if let Some(reference_tree) = reference_tree.as_ref() {
            let geometries = if let Some(transition) = draw_groups {
                Some(geometries_for_groupview(
                    reference_tree,
                    &mut *renderer,
                    non_exclusive_zone,
                    seat, // TODO: Would be better to be an old focus,
                    // but for that we have to associate focus with a tree (and animate focus changes properly)
                    output_scale,
                    1.0 - transition,
                    transition,
                    &self.backdrop_id,
                    is_mouse_tiling,
                    swap_desc.clone(),
                    overview.1.as_ref().and_then(|(_, tree)| *tree),
                    theme,
                    &mut |_elem| {},
                ))
            } else {
                None
            };

            // all old windows we want to fade out
            render_old_tree_windows(
                reference_tree,
                target_tree,
                renderer,
                geometries.clone(),
                output_scale,
                percentage,
                indicator_thickness,
                swap_desc.is_some(),
                theme,
                scanout_node,
                push,
            );

            geometries
        } else {
            None
        };

        let mut group_elements = SmallVec::<[_; 4]>::new_const();
        let geometries = if let Some(transition) = draw_groups {
            Some(geometries_for_groupview(
                target_tree,
                &mut *renderer,
                non_exclusive_zone,
                seat,
                output_scale,
                transition,
                transition,
                &self.backdrop_id,
                is_mouse_tiling,
                swap_desc.clone(),
                overview.1.as_ref().and_then(|(_, tree)| *tree),
                theme,
                &mut |elem| group_elements.push(elem),
            ))
        } else {
            None
        };

        // all alive windows
        render_new_tree_windows(
            target_tree,
            reference_tree,
            renderer,
            non_exclusive_zone,
            geometries,
            old_geometries,
            is_overview,
            seat,
            focused,
            &self.output,
            percentage,
            draw_groups,
            if let Some(transition) = draw_groups {
                let diff = (4u8.abs_diff(indicator_thickness) as f32 * transition).round() as u8;
                if 3 > indicator_thickness {
                    indicator_thickness + diff
                } else {
                    indicator_thickness - diff
                }
            } else {
                indicator_thickness
            },
            overview,
            resize_indicator,
            swap_desc.clone(),
            &live_resize_tiles,
            &self.swapping_stack_surface_id,
            &self.backdrop_id,
            theme,
            scanout_node,
            push,
        );

        // tiling hints
        for elem in group_elements.into_iter() {
            push(elem);
        }
    }

    #[profiling::function]
    pub fn render_popups<R>(
        &self,
        renderer: &mut R,
        seat: Option<&Seat<State>>,
        non_exclusive_zone: Rectangle<i32, Local>,
        overview: (OverviewMode, Option<(SwapIndicator, Option<&Tree<Data>>)>),
        theme: &cosmic::theme::CosmicTheme,
        scanout_node: Option<DrmNode>,
        push: &mut dyn FnMut(CosmicMappedRenderElement<R>),
    ) where
        R: AsGlowRenderer,
        R::TextureId: Send + Clone + 'static,
        CosmicMappedRenderElement<R>: RenderElement<R>,
        CosmicWindowRenderElement<R>: RenderElement<R>,
        CosmicStackRenderElement<R>: RenderElement<R>,
    {
        let output_scale = self.output.current_scale().fractional_scale();

        let (target_tree, duration, _) = if self.queue.animation_start.is_some() {
            self.queue
                .trees
                .get(1)
                .expect("Animation ongoing, should have two trees")
        } else {
            self.queue.trees.front().unwrap()
        };
        let reference_tree = self
            .queue
            .animation_start
            .is_some()
            .then(|| &self.queue.trees.front().unwrap().0);

        let percentage = if let Some(animation_start) = self.queue.animation_start {
            if *duration == Duration::ZERO {
                1.0
            } else {
                let percentage = Instant::now().duration_since(animation_start).as_millis() as f32
                    / duration.as_millis() as f32;
                ease(EaseInOutCubic, 0.0, 1.0, percentage)
            }
        } else {
            1.0
        };
        let draw_groups = overview.0.alpha();

        let is_mouse_tiling = (matches!(overview.0.trigger(), Some(Trigger::Pointer(_))))
            .then(|| self.last_overview_hover.as_ref().map(|(_, zone)| zone));
        let swap_desc = if let Some(Trigger::KeyboardSwap(_, desc)) = overview.0.trigger() {
            Some(desc.clone())
        } else {
            None
        };

        // all gone windows and fade them out
        let old_geometries = if let Some(reference_tree) = reference_tree.as_ref() {
            let geometries = if let Some(transition) = draw_groups {
                Some(geometries_for_groupview(
                    reference_tree,
                    &mut *renderer,
                    non_exclusive_zone,
                    seat, // TODO: Would be better to be an old focus,
                    // but for that we have to associate focus with a tree (and animate focus changes properly)
                    output_scale,
                    1.0 - transition,
                    transition,
                    &self.backdrop_id,
                    is_mouse_tiling,
                    swap_desc.clone(),
                    overview.1.as_ref().and_then(|(_, tree)| *tree),
                    theme,
                    &mut |_| {},
                ))
            } else {
                None
            };

            // all old windows we want to fade out
            render_old_tree_popups(
                reference_tree,
                target_tree,
                renderer,
                geometries.clone(),
                output_scale,
                percentage,
                swap_desc.is_some(),
                scanout_node,
                push,
            );

            geometries
        } else {
            None
        };

        let geometries = if let Some(transition) = draw_groups {
            Some(geometries_for_groupview(
                target_tree,
                &mut *renderer,
                non_exclusive_zone,
                seat,
                output_scale,
                transition,
                transition,
                &self.backdrop_id,
                is_mouse_tiling,
                swap_desc.clone(),
                overview.1.as_ref().and_then(|(_, tree)| *tree),
                theme,
                &mut |_| {},
            ))
        } else {
            None
        };

        // all alive windows
        render_new_tree_popups(
            target_tree,
            reference_tree,
            renderer,
            geometries,
            old_geometries,
            seat,
            &self.output,
            percentage,
            overview,
            swap_desc.clone(),
            scanout_node,
            push,
        );
    }

    fn gaps(&self) -> (i32, i32) {
        let g = self.theme.cosmic().gaps;
        (g.0 as i32, g.1 as i32)
    }
}

const GAP_KEYBOARD: i32 = 8;
const GAP_MOUSE: i32 = 32;
const PLACEHOLDER_GAP_MOUSE: i32 = 8;
const WINDOW_BACKDROP_BORDER: i32 = 4;
const WINDOW_BACKDROP_GAP: i32 = 12;

const MAX_SWAP_WINDOW_SIZE: (i32, i32) = (360, 240);

fn swap_factor(size: Size<i32, Logical>) -> f64 {
    let target_w = std::cmp::min(size.w, MAX_SWAP_WINDOW_SIZE.0);
    let target_h = std::cmp::min(size.h, MAX_SWAP_WINDOW_SIZE.1);
    (target_w as f64 / size.w as f64).min(target_h as f64 / size.h as f64)
}

fn swap_geometry(
    size: Size<i32, Logical>,
    relative_to: Rectangle<i32, Local>,
) -> Rectangle<i32, Local> {
    let factor = swap_factor(size);

    let new_size = Size::from((
        (size.w as f64 * factor).round() as i32,
        (size.h as f64 * factor).round() as i32,
    ));

    let loc = Point::from((
        relative_to.loc.x + relative_to.size.w - new_size.w,
        relative_to.loc.y,
    ));

    Rectangle::new(loc, new_size)
}

fn geometries_for_groupview<'a, R>(
    tree: &Tree<Data>,
    renderer: impl Into<Option<&'a mut R>>,
    non_exclusive_zone: Rectangle<i32, Local>,
    seat: Option<&Seat<State>>,
    scale: f64,
    alpha: f32,
    transition: f32,
    backdrop_id: &Id,
    mouse_tiling: Option<Option<&TargetZone>>,
    swap_desc: Option<NodeDesc>,
    swap_tree: Option<&Tree<Data>>,
    _theme: &cosmic::theme::CosmicTheme,
    push: &mut dyn FnMut(CosmicMappedRenderElement<R>),
) -> HashMap<NodeId, Rectangle<i32, Local>>
where
    R: AsGlowRenderer + 'a,
    R::TextureId: 'static,
    CosmicMappedRenderElement<R>: RenderElement<R>,
    CosmicWindowRenderElement<R>: RenderElement<R>,
{
    // we need to recalculate geometry for all elements, if we are drawing groups
    let gap: i32 = (if mouse_tiling.is_some() {
        GAP_MOUSE
    } else {
        GAP_KEYBOARD
    } as f32
        * transition)
        .round() as i32;
    let mut renderer = renderer.into();

    let root = tree.root_node_id();
    let mut stack = Vec::new();
    if swap_tree.is_some() {
        // push bogos value, that will get ignored anyway
        stack.push((Rectangle::from_size((320, 240).into()), 0));
    }

    let has_root = root.is_some();
    if has_root {
        stack.push((non_exclusive_zone, 0));
    }

    let mut push = |elem| {
        if has_root {
            push(elem)
        }
    };

    let mut geometries: HashMap<NodeId, Rectangle<i32, Local>> = HashMap::new();
    let alpha = alpha * transition;

    let focused = seat
        .and_then(|seat| {
            seat.get_keyboard()
                .unwrap()
                .current_focus()
                .and_then(|target| TilingLayout::currently_focused_node(tree, target))
        })
        .map(|(id, _)| id);
    let focused_geo = focused
        .as_ref()
        .map(|focused_id| *tree.get(focused_id).unwrap().data().geometry());

    let has_potential_groups = if let Some(focused_id) = focused.as_ref() {
        let focused_node = tree.get(focused_id).unwrap();
        if let Some(parent) = focused_node.parent() {
            let parent_node = tree.get(parent).unwrap();
            parent_node.children().len() > 2
        } else {
            false
        }
    } else {
        false
    };

    for (tree, node_id) in root
        .into_iter()
        .flat_map(|root| tree.traverse_pre_order_ids(root).unwrap())
        .map(|id| (tree, id))
        .chain(
            swap_tree
                .as_ref()
                .filter(|_| swap_desc.as_ref().unwrap().stack_window.is_none())
                .into_iter()
                .flat_map(|tree| {
                    tree.traverse_pre_order_ids(&swap_desc.as_ref().unwrap().node)
                        .unwrap()
                })
                .map(|id| (*swap_tree.as_ref().unwrap(), id)),
        )
    {
        if let Some((mut geo, depth)) = stack.pop() {
            let node: &Node<Data> = tree.get(&node_id).unwrap();
            let data = node.data();

            let is_placeholder_sibling = node
                .parent()
                .and_then(|parent_id| tree.children_ids(parent_id).ok())
                .map(|mut siblings| {
                    siblings.any(|child_id| tree.get(child_id).unwrap().data().is_placeholder())
                })
                .unwrap_or(false);

            let render_potential_group = swap_desc.is_none()
                && has_potential_groups
                && (if let Some(focused_id) = focused.as_ref() {
                    // `focused` can move into us directly
                    if let Some(parent) = node.parent() {
                        let parent_data = tree.get(parent).unwrap().data();

                        let idx = tree
                            .children_ids(parent)
                            .unwrap()
                            .position(|id| id == &node_id)
                            .unwrap();
                        if let Some(focused_idx) = tree
                            .children_ids(parent)
                            .unwrap()
                            .position(|id| id == focused_id)
                        {
                            // only direct neighbors
                            focused_idx.abs_diff(idx) == 1
                                // skip neighbors, if this is a group of two
                                && parent_data.len() > 2
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    false
                });

            let (element_gap_left, element_gap_up, element_gap_right, element_gap_down) = {
                let gap = if is_placeholder_sibling {
                    PLACEHOLDER_GAP_MOUSE
                } else {
                    gap
                };
                (
                    if TilingLayout::has_sibling_node(tree, &node_id, Direction::Left)
                        && (mouse_tiling.is_some() || depth > 0)
                    {
                        gap / 2
                    } else {
                        0
                    },
                    if TilingLayout::has_sibling_node(tree, &node_id, Direction::Up)
                        && (mouse_tiling.is_some() || depth > 0)
                    {
                        gap / 2
                    } else {
                        0
                    },
                    if TilingLayout::has_sibling_node(tree, &node_id, Direction::Right)
                        && (mouse_tiling.is_some() || depth > 0)
                    {
                        gap / 2
                    } else {
                        0
                    },
                    if TilingLayout::has_sibling_node(tree, &node_id, Direction::Down)
                        && (mouse_tiling.is_some() || depth > 0)
                    {
                        gap / 2
                    } else {
                        0
                    },
                )
            };

            let group_color = GROUP_COLOR;

            match data {
                Data::Group {
                    orientation,
                    last_geometry,
                    sizes,
                    alive,
                    pill_indicator,
                } => {
                    let render_active_child = if let Some(focused_id) = focused.as_ref() {
                        !has_potential_groups
                            && swap_desc.is_none()
                            && node
                                .children()
                                .iter()
                                .any(|child_id| child_id == focused_id)
                    } else {
                        false
                    };

                    geo.loc += (element_gap_left, element_gap_up).into();
                    geo.size -= (element_gap_left, element_gap_up).into();
                    geo.size -= (element_gap_right, element_gap_down).into();

                    geometries.insert(node_id.clone(), geo);

                    if let Some(renderer) = renderer.as_mut() {
                        if (render_potential_group || render_active_child) && Some(&node_id) != root
                        {
                            push(
                                IndicatorShader::element(
                                    *renderer,
                                    Key::Group(Arc::downgrade(alive)),
                                    geo,
                                    4,
                                    [if render_active_child { 16 } else { 8 }; 4],
                                    alpha * if render_potential_group { 0.40 } else { 1.0 },
                                    scale,
                                    group_color,
                                )
                                .into(),
                            );
                        }
                        if mouse_tiling.is_some()
                            && pill_indicator.is_some()
                            && Some(&node_id) != root
                        {
                            push(
                                IndicatorShader::element(
                                    *renderer,
                                    Key::Group(Arc::downgrade(alive)),
                                    geo,
                                    4,
                                    [8; 4],
                                    alpha * 0.40,
                                    scale,
                                    group_color,
                                )
                                .into(),
                            );
                        }

                        if mouse_tiling.is_some()
                            && node
                                .parent()
                                .map(|parent_id| {
                                    matches!(
                                        tree.get(parent_id).unwrap().data(),
                                        Data::Group {
                                            pill_indicator: Some(_),
                                            ..
                                        }
                                    )
                                })
                                .unwrap_or(false)
                        {
                            // test if parent pill-indicator is adjacent
                            let parent_id = node.parent().unwrap();
                            let parent = tree.get(parent_id).unwrap();
                            let own_idx = tree
                                .children_ids(parent_id)
                                .unwrap()
                                .position(|child_id| child_id == &node_id)
                                .unwrap();
                            let draw_outline = match parent.data() {
                                Data::Group {
                                    pill_indicator,
                                    orientation,
                                    ..
                                } => match pill_indicator {
                                    Some(PillIndicator::Inner(pill_idx)) => {
                                        *pill_idx == own_idx || pill_idx + 1 == own_idx
                                    }
                                    Some(PillIndicator::Outer(dir)) => match (dir, orientation) {
                                        (Direction::Left, Orientation::Horizontal)
                                        | (Direction::Right, Orientation::Horizontal)
                                        | (Direction::Up, Orientation::Vertical)
                                        | (Direction::Down, Orientation::Vertical) => true,
                                        (Direction::Left, Orientation::Vertical)
                                        | (Direction::Up, Orientation::Horizontal) => own_idx == 0,
                                        (Direction::Right, Orientation::Vertical)
                                        | (Direction::Down, Orientation::Horizontal) => {
                                            own_idx + 1 == parent.data().len()
                                        }
                                    },
                                    None => unreachable!(),
                                },
                                _ => unreachable!(),
                            };

                            if draw_outline {
                                push(
                                    IndicatorShader::element(
                                        *renderer,
                                        Key::Group(Arc::downgrade(alive)),
                                        geo,
                                        4,
                                        [8; 4],
                                        alpha * 0.15,
                                        scale,
                                        group_color,
                                    )
                                    .into(),
                                );
                            }
                        }
                    }

                    geo.loc += (gap, gap).into();
                    geo.size -= (gap * 2, gap * 2).into();

                    if mouse_tiling.is_some()
                        && let Some(PillIndicator::Outer(direction)) = pill_indicator
                    {
                        let (pill_geo, remaining_geo) = match direction {
                            Direction::Left => (
                                Rectangle::new(
                                    (geo.loc.x, geo.loc.y).into(),
                                    (16, geo.size.h).into(),
                                ),
                                Rectangle::new(
                                    (geo.loc.x + 48, geo.loc.y).into(),
                                    (geo.size.w - 48, geo.size.h).into(),
                                ),
                            ),
                            Direction::Up => (
                                Rectangle::new(
                                    (geo.loc.x, geo.loc.y).into(),
                                    (geo.size.w, 16).into(),
                                ),
                                Rectangle::new(
                                    (geo.loc.x, geo.loc.y + 48).into(),
                                    (geo.size.w, geo.size.h - 48).into(),
                                ),
                            ),
                            Direction::Right => (
                                Rectangle::new(
                                    (geo.loc.x + geo.size.w - 16, geo.loc.y).into(),
                                    (16, geo.size.h).into(),
                                ),
                                Rectangle::new(geo.loc, (geo.size.w - 48, geo.size.h).into()),
                            ),
                            Direction::Down => (
                                Rectangle::new(
                                    (geo.loc.x, geo.loc.y + geo.size.h - 16).into(),
                                    (geo.size.w, 16).into(),
                                ),
                                Rectangle::new(geo.loc, (geo.size.w, geo.size.h - 48).into()),
                            ),
                        };

                        if let Some(renderer) = renderer.as_mut() {
                            push(
                                BackdropShader::element(
                                    *renderer,
                                    backdrop_id.clone(),
                                    pill_geo,
                                    8.,
                                    alpha * 0.4,
                                    group_color,
                                )
                                .into(),
                            );
                        }

                        geo = remaining_geo;
                    };

                    if matches!(swap_desc, Some(ref desc) if desc.node == node_id) {
                        if let Some(renderer) = renderer.as_mut() {
                            push(
                                BackdropShader::element(
                                    *renderer,
                                    Key::Group(Arc::downgrade(alive)),
                                    geo,
                                    8.,
                                    alpha
                                        * if focused
                                            .as_ref()
                                            .map(|focused| {
                                                focused == &swap_desc.as_ref().unwrap().node
                                            })
                                            .unwrap_or(false)
                                        {
                                            0.4
                                        } else {
                                            0.15
                                        },
                                    group_color,
                                )
                                .into(),
                            );
                        }
                        let swap_geo = swap_geometry(
                            geo.size.as_logical(),
                            focused_geo.unwrap_or({
                                let mut geo = non_exclusive_zone;
                                geo.loc += (WINDOW_BACKDROP_BORDER, WINDOW_BACKDROP_BORDER).into();
                                geo.size -=
                                    (WINDOW_BACKDROP_BORDER * 2, WINDOW_BACKDROP_BORDER * 2).into();
                                geo
                            }),
                        );
                        geo = ease(
                            Linear,
                            EaseRectangle(geo),
                            EaseRectangle(swap_geo),
                            transition,
                        )
                        .unwrap();
                        geometries.insert(node_id.clone(), geo);
                    };

                    let previous_length = match orientation {
                        Orientation::Horizontal => last_geometry.size.h,
                        Orientation::Vertical => last_geometry.size.w,
                    };
                    let new_length = match orientation {
                        Orientation::Horizontal => geo.size.h,
                        Orientation::Vertical => geo.size.w,
                    };

                    let mut sizes = sizes
                        .iter()
                        .map(|len| {
                            (((*len as f64) / (previous_length as f64)) * (new_length as f64))
                                .round() as i32
                        })
                        .collect::<Vec<_>>();
                    let sum: i32 = sizes.iter().sum();
                    if sum < new_length {
                        *sizes.last_mut().unwrap() += new_length - sum;
                    }

                    match orientation {
                        Orientation::Horizontal => {
                            let mut previous: i32 = sizes.iter().sum();
                            for (idx, size) in sizes.iter().enumerate().rev() {
                                previous -= *size;
                                let mut geo = Rectangle::new(
                                    (geo.loc.x, geo.loc.y + previous).into(),
                                    (geo.size.w, *size).into(),
                                );
                                if mouse_tiling.is_some()
                                    && let Some(PillIndicator::Inner(pill_idx)) = pill_indicator
                                {
                                    if *pill_idx == idx {
                                        geo.size.h -= 32;
                                    }
                                    if idx
                                        .checked_sub(1)
                                        .map(|idx| idx == *pill_idx)
                                        .unwrap_or(false)
                                    {
                                        if let Some(renderer) = renderer.as_mut() {
                                            push(
                                                BackdropShader::element(
                                                    *renderer,
                                                    backdrop_id.clone(),
                                                    Rectangle::new(
                                                        (geo.loc.x, geo.loc.y - 8).into(),
                                                        (geo.size.w, 16).into(),
                                                    ),
                                                    8.,
                                                    alpha * 0.4,
                                                    group_color,
                                                )
                                                .into(),
                                            );
                                        }
                                        geo.loc.y += 32;
                                        geo.size.h -= 32;
                                    }
                                }
                                stack.push((geo, depth + 1));
                            }
                        }
                        Orientation::Vertical => {
                            let mut previous: i32 = sizes.iter().sum();
                            for (idx, size) in sizes.iter().enumerate().rev() {
                                previous -= *size;
                                let mut geo = Rectangle::new(
                                    (geo.loc.x + previous, geo.loc.y).into(),
                                    (*size, geo.size.h).into(),
                                );
                                if mouse_tiling.is_some()
                                    && let Some(PillIndicator::Inner(pill_idx)) = pill_indicator
                                {
                                    if *pill_idx == idx {
                                        geo.size.w -= 32;
                                    }
                                    if idx
                                        .checked_sub(1)
                                        .map(|idx| idx == *pill_idx)
                                        .unwrap_or(false)
                                    {
                                        if let Some(renderer) = renderer.as_mut() {
                                            push(
                                                BackdropShader::element(
                                                    *renderer,
                                                    backdrop_id.clone(),
                                                    Rectangle::new(
                                                        (geo.loc.x - 8, geo.loc.y).into(),
                                                        (16, geo.size.h).into(),
                                                    ),
                                                    8.,
                                                    alpha * 0.4,
                                                    group_color,
                                                )
                                                .into(),
                                            );
                                        }
                                        geo.loc.x += 32;
                                        geo.size.w -= 32;
                                    }
                                }
                                stack.push((geo, depth + 1));
                            }
                        }
                    }
                }
                Data::Mapped { mapped, .. } => {
                    geo.loc += (element_gap_left, element_gap_up).into();
                    geo.size -= (element_gap_left, element_gap_up).into();
                    geo.size -= (element_gap_right, element_gap_down).into();

                    if let Some(renderer) = renderer.as_mut() {
                        if render_potential_group {
                            push(
                                IndicatorShader::element(
                                    *renderer,
                                    Key::Window(Usage::PotentialGroupIndicator, mapped.key()),
                                    geo,
                                    4,
                                    [8; 4],
                                    alpha * 0.40,
                                    scale,
                                    group_color,
                                )
                                .into(),
                            );
                            geo.loc += (gap, gap).into();
                            geo.size -= (gap * 2, gap * 2).into();
                        }

                        let accent = ACTIVE_GROUP_COLOR;

                        if focused
                            .as_ref()
                            .map(|focused_id| {
                                !tree
                                    .ancestor_ids(&node_id)
                                    .unwrap()
                                    .any(|id| id == focused_id)
                            })
                            .unwrap_or(mouse_tiling.is_some())
                        {
                            let color = match mouse_tiling {
                                Some(Some(TargetZone::WindowStack(stack_id, _)))
                                    if *stack_id == node_id =>
                                {
                                    accent
                                }
                                _ => group_color,
                            };
                            geo.loc += (WINDOW_BACKDROP_BORDER, WINDOW_BACKDROP_BORDER).into();
                            geo.size -=
                                (WINDOW_BACKDROP_BORDER * 2, WINDOW_BACKDROP_BORDER * 2).into();
                            push(
                                BackdropShader::element(
                                    *renderer,
                                    Key::Window(Usage::OverviewBackdrop, mapped.key()),
                                    geo,
                                    8.,
                                    alpha
                                        * if focused
                                            .as_ref()
                                            .map(|focused_id| focused_id == &node_id)
                                            .unwrap_or(color == accent)
                                        {
                                            0.4
                                        } else {
                                            0.15
                                        },
                                    color,
                                )
                                .into(),
                            );
                        }

                        geo.loc += (WINDOW_BACKDROP_GAP, WINDOW_BACKDROP_GAP).into();
                        geo.size -= (WINDOW_BACKDROP_GAP * 2, WINDOW_BACKDROP_GAP * 2).into();
                    }

                    if matches!(swap_desc, Some(ref desc) if desc.node == node_id && desc.stack_window.is_none())
                    {
                        let swap_geo = swap_geometry(
                            geo.size.as_logical(),
                            focused_geo.unwrap_or({
                                let mut geo = non_exclusive_zone;
                                geo.loc += (WINDOW_BACKDROP_BORDER, WINDOW_BACKDROP_BORDER).into();
                                geo.size -=
                                    (WINDOW_BACKDROP_BORDER * 2, WINDOW_BACKDROP_BORDER * 2).into();
                                geo
                            }),
                        );
                        geo = ease(
                            Linear,
                            EaseRectangle(geo),
                            EaseRectangle(swap_geo),
                            transition,
                        )
                        .unwrap();
                    };

                    geometries.insert(node_id.clone(), geo);
                }
                Data::Placeholder { id, .. } => {
                    geo.loc += (element_gap_left, element_gap_up).into();
                    geo.size -= (element_gap_left, element_gap_up).into();
                    geo.size -= (element_gap_right, element_gap_down).into();

                    if let Some(renderer) = renderer.as_mut() {
                        geo.loc += (WINDOW_BACKDROP_BORDER, WINDOW_BACKDROP_BORDER).into();
                        geo.size -= (WINDOW_BACKDROP_BORDER * 2, WINDOW_BACKDROP_BORDER * 2).into();
                        push(
                            BackdropShader::element(
                                *renderer,
                                id.clone(),
                                geo,
                                8.,
                                alpha * 0.4,
                                group_color,
                            )
                            .into(),
                        );
                    }

                    geometries.insert(node_id.clone(), geo);
                }
            }
        }
    }

    geometries
}

fn render_old_tree_popups<R>(
    reference_tree: &Tree<Data>,
    target_tree: &Tree<Data>,
    renderer: &mut R,
    geometries: Option<HashMap<NodeId, Rectangle<i32, Local>>>,
    output_scale: f64,
    percentage: f32,
    is_swap_mode: bool,
    scanout_node: Option<DrmNode>,
    push: &mut dyn FnMut(CosmicMappedRenderElement<R>),
) where
    R: AsGlowRenderer,
    R::TextureId: Send + Clone + 'static,
    CosmicMappedRenderElement<R>: RenderElement<R>,
    CosmicWindowRenderElement<R>: RenderElement<R>,
    CosmicStackRenderElement<R>: RenderElement<R>,
{
    render_old_tree(
        reference_tree,
        target_tree,
        geometries,
        output_scale,
        percentage,
        is_swap_mode,
        |mapped, elem_geometry, geo, alpha, _| {
            mapped.push_popup_render_elements(
                renderer,
                geo.loc.as_logical().to_physical_precise_round(output_scale) - elem_geometry.loc,
                Scale::from(output_scale),
                alpha,
                scanout_node,
                push,
            )
        },
    )
}

fn render_old_tree_windows<R>(
    reference_tree: &Tree<Data>,
    target_tree: &Tree<Data>,
    renderer: &mut R,
    geometries: Option<HashMap<NodeId, Rectangle<i32, Local>>>,
    output_scale: f64,
    percentage: f32,
    indicator_thickness: u8,
    is_swap_mode: bool,
    theme: &cosmic::theme::CosmicTheme,
    scanout_node: Option<DrmNode>,
    push: &mut dyn FnMut(CosmicMappedRenderElement<R>),
) where
    R: AsGlowRenderer,
    R::TextureId: Send + Clone + 'static,
    CosmicMappedRenderElement<R>: RenderElement<R>,
    CosmicWindowRenderElement<R>: RenderElement<R>,
    CosmicStackRenderElement<R>: RenderElement<R>,
{
    let window_hint = crate::theme::active_window_hint(theme);
    let mut lower_elements = Vec::default();
    let mut shadow_elements = SmallVec::<[_; 4]>::new_const();

    let window_map =
        |elem, geo: Rectangle<i32, Local>, elem_geometry: Rectangle<i32, Physical>| match elem {
            CosmicMappedRenderElement::Stack(elem) => constrain_render_elements(
                std::iter::once(elem),
                geo.loc.as_logical().to_physical_precise_round(output_scale) - elem_geometry.loc,
                geo.as_logical().to_physical_precise_round(output_scale),
                elem_geometry,
                ConstrainScaleBehavior::Stretch,
                ConstrainAlign::CENTER,
                output_scale,
            )
            .next()
            .map(CosmicMappedRenderElement::TiledStack),
            CosmicMappedRenderElement::Window(elem) => constrain_render_elements(
                std::iter::once(elem),
                geo.loc.as_logical().to_physical_precise_round(output_scale) - elem_geometry.loc,
                geo.as_logical().to_physical_precise_round(output_scale),
                elem_geometry,
                ConstrainScaleBehavior::Stretch,
                ConstrainAlign::CENTER,
                output_scale,
            )
            .next()
            .map(CosmicMappedRenderElement::TiledWindow),
            x => Some(x),
        };

    render_old_tree(
        reference_tree,
        target_tree,
        geometries,
        output_scale,
        percentage,
        is_swap_mode,
        |mapped, elem_geometry, geo, alpha, is_minimizing| {
            let radius = mapped.corner_radius(geo.size.as_logical(), indicator_thickness);
            if is_minimizing && indicator_thickness > 0 {
                push(CosmicMappedRenderElement::FocusIndicator(
                    IndicatorShader::focus_element(
                        renderer,
                        Key::Window(Usage::FocusIndicator, mapped.clone().key()),
                        geo,
                        indicator_thickness,
                        radius,
                        alpha,
                        output_scale,
                        [window_hint.red, window_hint.green, window_hint.blue],
                    ),
                ));
            }

            mapped.push_render_elements(
                renderer,
                geo.loc.as_logical().to_physical_precise_round(output_scale) - elem_geometry.loc,
                Some(geo.size.as_logical()),
                Scale::from(output_scale),
                alpha,
                None,
                scanout_node,
                &mut |elem| {
                    if let Some(elem) = window_map(elem, geo, elem_geometry) {
                        push(elem);
                    }
                },
                &mut |elem| {
                    if let Some(elem) = window_map(elem, geo, elem_geometry) {
                        lower_elements.push(elem);
                    }
                },
            );

            shadow_elements.extend(mapped.shadow_render_element(
                renderer,
                geo.loc.as_logical().to_physical_precise_round(output_scale) - elem_geometry.loc,
                Some(geo.size.as_logical()),
                Scale::from(output_scale),
                1.,
                alpha,
            ));
        },
    );

    for elem in shadow_elements {
        push(elem);
    }
    for elem in lower_elements {
        push(elem);
    }
}

fn render_old_tree(
    reference_tree: &Tree<Data>,
    target_tree: &Tree<Data>,
    geometries: Option<HashMap<NodeId, Rectangle<i32, Local>>>,
    output_scale: f64,
    percentage: f32,
    is_swap_mode: bool,
    mut processor: impl FnMut(&CosmicMapped, Rectangle<i32, Physical>, Rectangle<i32, Local>, f32, bool),
) {
    if let Some(root) = reference_tree.root_node_id() {
        let geometries = geometries.unwrap_or_default();
        reference_tree
            .traverse_pre_order_ids(root)
            .unwrap()
            .filter(|node_id| reference_tree.get(node_id).unwrap().data().is_mapped(None))
            .map(
                |node_id| match reference_tree.get(&node_id).unwrap().data() {
                    Data::Mapped {
                        mapped,
                        last_geometry,
                        minimize_rect,
                        ..
                    } => (
                        mapped,
                        last_geometry,
                        geometries.get(&node_id).copied(),
                        minimize_rect,
                    ),
                    _ => unreachable!(),
                },
            )
            .filter(|(mapped, _, _, _)| !mapped.is_maximized(false))
            .filter(|(mapped, _, _, _)| {
                if let Some(root) = target_tree.root_node_id() {
                    is_swap_mode
                        || !target_tree
                            .traverse_pre_order(root)
                            .unwrap()
                            .any(|node| node.data().is_mapped(Some(mapped)))
                } else {
                    true
                }
            })
            .for_each(|(mapped, original_geo, mut scaled_geo, minimize_geo)| {
                if let Some(minimize_geo) = minimize_geo {
                    scaled_geo = Some(
                        ease(
                            EaseInOutCubic,
                            EaseRectangle(*original_geo),
                            EaseRectangle(*minimize_geo),
                            percentage,
                        )
                        .unwrap(),
                    );
                }

                let (scale, offset) = scaled_geo
                    .map(|adapted_geo| scale_to_center(original_geo, &adapted_geo))
                    .unwrap_or_else(|| (1.0, (0, 0).into()));
                let geo = scaled_geo
                    .map(|adapted_geo| {
                        Rectangle::new(
                            adapted_geo.loc + offset,
                            (original_geo.size.to_f64() * scale).to_i32_round(),
                        )
                    })
                    .unwrap_or(*original_geo);

                let alpha = if minimize_geo.is_some() {
                    1.0 - ((percentage - 0.5).max(0.0) * 2.0)
                } else {
                    1.0 - percentage
                };

                let elem_geometry = mapped.geometry().to_physical_precise_round(output_scale);

                processor(mapped, elem_geometry, geo, alpha, minimize_geo.is_some())
            });
    }
}

fn render_new_tree_popups<R>(
    target_tree: &Tree<Data>,
    reference_tree: Option<&Tree<Data>>,
    renderer: &mut R,
    geometries: Option<HashMap<NodeId, Rectangle<i32, Local>>>,
    old_geometries: Option<HashMap<NodeId, Rectangle<i32, Local>>>,
    seat: Option<&Seat<State>>,
    output: &Output,
    percentage: f32,
    overview: (OverviewMode, Option<(SwapIndicator, Option<&Tree<Data>>)>),
    swap_desc: Option<NodeDesc>,
    scanout_node: Option<DrmNode>,
    push: &mut dyn FnMut(CosmicMappedRenderElement<R>),
) where
    R: AsGlowRenderer,
    R::TextureId: Send + Clone + 'static,
    CosmicMappedRenderElement<R>: RenderElement<R>,
    CosmicWindowRenderElement<R>: RenderElement<R>,
    CosmicStackRenderElement<R>: RenderElement<R>,
{
    let output_scale = output.current_scale().fractional_scale();

    let is_active_output = seat
        .map(|seat| &seat.active_output() == output)
        .unwrap_or(false);

    let (_, swap_tree) = overview.1.unzip();
    let swap_desc = swap_desc.filter(|_| is_active_output);
    let swap_tree = swap_tree.flatten().filter(|_| is_active_output);

    render_new_tree(
        target_tree,
        reference_tree,
        geometries,
        old_geometries,
        percentage,
        swap_tree,
        swap_desc.as_ref(),
        |_node_id, data, geo, _original_geo, alpha, _| {
            if let Data::Mapped { mapped, .. } = data {
                let elem_geometry = mapped.geometry().to_physical_precise_round(output_scale);

                mapped.push_popup_render_elements(
                    renderer,
                    geo.loc.as_logical().to_physical_precise_round(output_scale)
                        - elem_geometry.loc,
                    Scale::from(output_scale),
                    alpha,
                    scanout_node,
                    push,
                );
            }
        },
    );
}

fn render_new_tree_windows<R>(
    target_tree: &Tree<Data>,
    reference_tree: Option<&Tree<Data>>,
    renderer: &mut R,
    non_exclusive_zone: Rectangle<i32, Local>,
    geometries: Option<HashMap<NodeId, Rectangle<i32, Local>>>,
    old_geometries: Option<HashMap<NodeId, Rectangle<i32, Local>>>,
    is_overview: bool,
    seat: Option<&Seat<State>>,
    focused: Option<&CosmicMapped>,
    output: &Output,
    percentage: f32,
    transition: Option<f32>,
    indicator_thickness: u8,
    overview: (OverviewMode, Option<(SwapIndicator, Option<&Tree<Data>>)>),
    mut resize_indicator: Option<(ResizeMode, ResizeIndicator)>,
    swap_desc: Option<NodeDesc>,
    live_resize_tiles: &[NodeId],
    swapping_stack_surface_id: &Id,
    backdrop_id: &Id,
    theme: &cosmic::theme::CosmicTheme,
    scanout_node: Option<DrmNode>,
    push: &mut dyn FnMut(CosmicMappedRenderElement<R>),
) where
    R: AsGlowRenderer,
    R::TextureId: Send + Clone + 'static,
    CosmicMappedRenderElement<R>: RenderElement<R>,
    CosmicWindowRenderElement<R>: RenderElement<R>,
    CosmicStackRenderElement<R>: RenderElement<R>,
{
    let focused = match seat.and_then(|seat| seat.get_keyboard().unwrap().current_focus()) {
        Some(target @ KeyboardFocusTarget::Group(_)) => {
            TilingLayout::currently_focused_node(target_tree, target).map(|(id, _)| id)
        }
        _ => focused.and_then(|mapped| {
            let node_id = mapped.tiling_node_id.lock().unwrap().clone()?;
            target_tree
                .get(&node_id)
                .ok()
                .filter(|node| node.data().is_mapped(Some(mapped)))
                .map(|_| node_id)
        }),
    };
    let focused_geo = if let Some(focused) = focused.as_ref() {
        geometries
            .as_ref()
            .and_then(|geometries| geometries.get(focused))
            .or_else(|| {
                target_tree
                    .get(focused)
                    .ok()
                    .map(|node| node.data().geometry())
            })
            .cloned()
    } else {
        None
    }
    .unwrap_or({
        let mut geo = non_exclusive_zone;
        geo.loc += (WINDOW_BACKDROP_BORDER, WINDOW_BACKDROP_BORDER).into();
        geo.size -= (WINDOW_BACKDROP_BORDER * 2, WINDOW_BACKDROP_BORDER * 2).into();
        geo
    });

    let is_active_output = seat
        .map(|seat| &seat.active_output() == output)
        .unwrap_or(false);

    let mut animating_window_upper_elements = Vec::new();
    let mut animating_window_lower_elements = Vec::new();
    let mut animating_shadow_elements = SmallVec::<[CosmicMappedRenderElement<R>; 4]>::new_const();

    let mut window_upper_elements = Vec::new();
    let mut window_lower_elements = Vec::new();
    let mut shadow_elements = SmallVec::<[CosmicMappedRenderElement<R>; 4]>::new_const();

    let mut group_backdrop = None;
    let mut indicators = SmallVec::<[CosmicMappedRenderElement<R>; 2]>::new_const();
    let mut resize_elements = SmallVec::<[CosmicMappedRenderElement<R>; 10]>::new_const();
    let mut swap_elements = SmallVec::<[CosmicMappedRenderElement<R>; 4]>::new_const();

    let output_scale = output.current_scale().fractional_scale();

    let (mut swap_indicator, swap_tree) = overview.1.unzip();
    let swap_desc = swap_desc.filter(|_| is_active_output);
    let swap_tree = swap_tree.flatten().filter(|_| is_active_output);
    let window_hint = crate::theme::active_window_hint(theme);
    let group_color = GROUP_COLOR;

    // render placeholder, if we are swapping to an empty workspace
    if target_tree.root_node_id().is_none() && swap_desc.is_some() {
        window_upper_elements.push(
            BackdropShader::element(
                renderer,
                backdrop_id.clone(),
                focused_geo,
                8.,
                transition.unwrap_or(1.0) * 0.4,
                group_color,
            )
            .into(),
        );
    }

    // render single stack window when swapping separately
    if let Some(window) = swap_desc
        .as_ref()
        .and_then(|desc| desc.stack_window.clone())
    {
        let window_geo = window.geometry();
        let origin = {
            let mut geo = focused_geo;
            geo.loc.x += STACK_TAB_HEIGHT;
            geo.size.h -= STACK_TAB_HEIGHT;
            geo
        };
        let target = swap_geometry(window_geo.size, focused_geo);
        let swap_geo = ease(
            Linear,
            EaseRectangle(origin),
            EaseRectangle(target),
            transition.unwrap_or(1.0),
        )
        .unwrap();
        let scale = swap_geo.size.to_f64() / origin.size.to_f64();

        let radius = theme
            .radius_s()
            .map(|x| if x < 4.0 { x } else { x + 4.0 })
            .map(|val| (val * scale.x.min(scale.y) as f32).round() as u8);
        swap_elements.push(CosmicMappedRenderElement::FocusIndicator(
            IndicatorShader::focus_element(
                renderer,
                Key::from(swapping_stack_surface_id.clone()),
                swap_geo,
                4,
                radius,
                transition.unwrap_or(1.0),
                output_scale,
                [window_hint.red, window_hint.green, window_hint.blue],
            ),
        ));

        let render_loc =
            (swap_geo.loc.as_logical() - window_geo.loc).to_physical_precise_round(output_scale);

        window.push_render_elements(
            renderer,
            render_loc,
            output_scale.into(),
            1.0,
            None,
            scanout_node,
            false,
            [0, 0, 0, 0],
            // TODO
            0,
            &mut |elem| {
                swap_elements.push(CosmicMappedRenderElement::GrabbedWindow(
                    RescaleRenderElement::from_element(
                        elem.into(),
                        swap_geo
                            .loc
                            .as_logical()
                            .to_physical_precise_round(output_scale),
                        ease(
                            Linear,
                            1.0,
                            swap_factor(window_geo.size),
                            transition.unwrap_or(1.0),
                        ),
                    ),
                ));
            },
            None,
        );
    }

    // render actual tree nodes
    render_new_tree(
        target_tree,
        reference_tree,
        geometries,
        old_geometries,
        percentage,
        swap_tree,
        swap_desc.as_ref(),
        |node_id, data, geo, original_geo, alpha, animating| {
            if swap_desc.as_ref().map(|desc| &desc.node) == Some(&node_id)
                || focused.as_ref() == Some(&node_id)
            {
                if indicator_thickness > 0 || data.is_group() {
                    let mut geo = geo;

                    let scale = geo.size.to_f64() / original_geo.size.to_f64();
                    let radius = match data {
                        Data::Mapped { mapped, .. }
                            if swap_desc
                                .as_ref()
                                .map(|desc| &desc.node)
                                .is_none_or(|n| n != &node_id) =>
                        {
                            mapped
                                .corner_radius(geo.size.as_logical(), indicator_thickness)
                                .map(|val| (val as f64 * scale.x.min(scale.y)).round() as u8)
                        }
                        _ => theme
                            .radius_s()
                            .map(|x| if x < 4.0 { x } else { x + 4.0 })
                            .map(|val| (val * scale.x.min(scale.y) as f32).round() as u8),
                    };

                    if data.is_group() {
                        let outer_gap: i32 = (if is_overview { GAP_KEYBOARD } else { 4 } as f32
                            * percentage)
                            .round() as i32;
                        geo.loc += (outer_gap, outer_gap).into();
                        geo.size -= (outer_gap * 2, outer_gap * 2).into();

                        let backdrop = BackdropShader::element(
                            renderer,
                            match data {
                                Data::Group { alive, .. } => Key::Group(Arc::downgrade(alive)),
                                _ => unreachable!(),
                            },
                            geo,
                            radius[0] as f32,
                            0.4,
                            group_color,
                        );

                        if focused.as_ref() == Some(&node_id) {
                            group_backdrop = Some(backdrop);
                        } else {
                            indicators.push(backdrop.into());
                        }
                    }
                    if !swap_desc
                        .as_ref()
                        .map(|desc| desc.stack_window.is_some())
                        .unwrap_or(false)
                        || focused.as_ref() == Some(&node_id)
                    {
                        indicators.push(CosmicMappedRenderElement::FocusIndicator(
                            IndicatorShader::focus_element(
                                renderer,
                                match data {
                                    Data::Mapped { mapped, .. } => {
                                        Key::Window(Usage::FocusIndicator, mapped.clone().key())
                                    }
                                    Data::Group { alive, .. } => Key::Group(Arc::downgrade(alive)),
                                    _ => unreachable!(),
                                },
                                geo,
                                if data.is_group() {
                                    4
                                } else {
                                    indicator_thickness
                                },
                                radius,
                                alpha,
                                output_scale,
                                [window_hint.red, window_hint.green, window_hint.blue],
                            ),
                        ));
                    }

                    if focused.as_ref() == Some(&node_id)
                        && (swap_desc.as_ref().map(|desc| &desc.node) != Some(&node_id)
                            || swap_desc
                                .as_ref()
                                .and_then(|swap_desc| swap_desc.stack_window.as_ref())
                                .zip(focused.as_ref())
                                .map(|(stack_window, focused_id)| {
                                    target_tree
                                        .get(focused_id)
                                        .ok()
                                        .map(|focused| match focused.data() {
                                            Data::Mapped { mapped, .. } => mapped
                                                .stack_ref()
                                                .map(|stack| &stack.active() != stack_window)
                                                .unwrap_or(false),
                                            _ => false,
                                        })
                                        .unwrap_or(false)
                                })
                                .unwrap_or(false))
                        && let Some(swap) = swap_indicator.as_mut()
                    {
                        let size = geo.size.as_logical();
                        swap.resize(size);
                        swap.output_enter(output);
                        swap.push_render_elements(
                            renderer,
                            geo.loc.as_logical().to_physical_precise_round(output_scale),
                            output_scale.into(),
                            alpha * overview.0.alpha().unwrap_or(1.0),
                            &mut |elem| swap_elements.push(elem.into()),
                            None,
                        );
                    }
                }

                if let Some((mode, resize)) = resize_indicator.as_mut() {
                    let mut geo = geo;
                    geo.loc -= (18, 18).into();
                    geo.size += (36, 36).into();

                    resize.resize(geo.size.as_logical());
                    resize.output_enter(output);
                    let possible_edges =
                        TilingLayout::possible_resizes(target_tree, node_id.clone());
                    if !possible_edges.is_empty() {
                        resize.set_edges(possible_edges);
                        resize.push_render_elements(
                            renderer,
                            geo.loc.as_logical().to_physical_precise_round(output_scale),
                            output_scale.into(),
                            alpha * mode.alpha().unwrap_or(1.0),
                            &mut |elem| {
                                resize_elements.push(elem.into());
                            },
                            None,
                        );
                    }
                }
            }

            if let Data::Mapped { mapped, .. } = data {
                let committed_geometry = mapped.geometry();
                let elem_geometry = committed_geometry.to_physical_precise_round(output_scale);

                let scale = geo.size.to_f64() / original_geo.size.to_f64();
                let constrain_animated_width =
                    tiled_window_has_animated_width(is_overview, animating, &geo, original_geo);
                let constrain_live_resize = !is_overview && live_resize_tiles.contains(&node_id);
                let constrain_from_source = constrain_animated_width || constrain_live_resize;
                let max_size = tiled_window_render_max_size(
                    is_overview,
                    animating,
                    constrain_live_resize,
                    &geo,
                    original_geo,
                    committed_geometry.size.as_logical(),
                );

                let (behavior, align) = if is_overview {
                    (ConstrainScaleBehavior::Fit, ConstrainAlign::CENTER)
                } else if animating || constrain_live_resize {
                    (ConstrainScaleBehavior::Stretch, ConstrainAlign::TOP_LEFT)
                } else {
                    (ConstrainScaleBehavior::CutOff, ConstrainAlign::TOP_LEFT)
                };

                let map_elem = |element| match element {
                    CosmicMappedRenderElement::Stack(elem) => constrain_render_elements(
                        std::iter::once(elem),
                        geo.loc.as_logical().to_physical_precise_round(output_scale)
                            - elem_geometry.loc,
                        geo.as_logical().to_physical_precise_round(output_scale),
                        elem_geometry,
                        behavior,
                        align,
                        output_scale,
                    )
                    .next()
                    .map(CosmicMappedRenderElement::TiledStack),
                    CosmicMappedRenderElement::Window(elem) => constrain_render_elements(
                        std::iter::once(elem),
                        geo.loc.as_logical().to_physical_precise_round(output_scale)
                            - elem_geometry.loc,
                        geo.as_logical().to_physical_precise_round(output_scale),
                        elem_geometry,
                        behavior,
                        align,
                        output_scale,
                    )
                    .next()
                    .map(CosmicMappedRenderElement::TiledWindow),
                    CosmicMappedRenderElement::Overlay(elem) => constrain_render_elements(
                        std::iter::once(elem),
                        geo.loc.as_logical().to_physical_precise_round(output_scale)
                            - elem_geometry.loc,
                        geo.as_logical().to_physical_precise_round(output_scale),
                        elem_geometry,
                        behavior,
                        align,
                        output_scale,
                    )
                    .next()
                    .map(CosmicMappedRenderElement::TiledOverlay),
                    x => Some(x),
                };

                // Animated mapped elements are generated at the authoritative
                // target size and constrained exactly once to `geo`. Generating
                // borders or shadows at `geo` before stretching them from the
                // target geometry applies the width interpolation twice and
                // leaves a target-like edge inside the window.
                let shadow_element = mapped.shadow_render_element(
                    renderer,
                    geo.loc.as_logical().to_physical_precise_round(output_scale)
                        - elem_geometry.loc,
                    max_size,
                    Scale::from(output_scale),
                    if constrain_from_source {
                        1.0
                    } else {
                        scale.x.min(scale.y)
                    },
                    alpha,
                );
                let shadow_element = if constrain_from_source {
                    shadow_element.and_then(&map_elem)
                } else {
                    shadow_element
                };

                let mut upper_elements = SmallVec::<[CosmicMappedRenderElement<R>; 4]>::new_const();
                let mut lower_elements = SmallVec::<[CosmicMappedRenderElement<R>; 4]>::new_const();
                mapped.push_render_elements(
                    renderer,
                    //original_location,
                    geo.loc.as_logical().to_physical_precise_round(output_scale)
                        - elem_geometry.loc,
                    max_size,
                    Scale::from(output_scale),
                    alpha,
                    None,
                    scanout_node,
                    &mut |elem| {
                        if let Some(elem) = map_elem(elem) {
                            upper_elements.push(elem)
                        }
                    },
                    &mut |elem| {
                        if let Some(elem) = map_elem(elem) {
                            lower_elements.push(elem)
                        }
                    },
                );

                if swap_desc
                    .as_ref()
                    .filter(|swap_desc| swap_desc.node == node_id)
                    .and_then(|swap_desc| swap_desc.stack_window.as_ref())
                    .zip(focused.as_ref())
                    .map(|(stack_window, focused_id)| {
                        target_tree
                            .get(focused_id)
                            .ok()
                            .map(|focused| match focused.data() {
                                Data::Mapped { mapped, .. } => mapped
                                    .stack_ref()
                                    .map(|stack| &stack.active() == stack_window)
                                    .unwrap_or(false),
                                _ => false,
                            })
                            .unwrap_or(false)
                    })
                    .unwrap_or(false)
                {
                    let mut active_geo = mapped.active_window_geometry().as_local();
                    active_geo.loc += geo.loc - mapped.geometry().loc.as_local();
                    upper_elements.insert(
                        0,
                        CosmicMappedRenderElement::Overlay(BackdropShader::element(
                            renderer,
                            Key::Window(Usage::Overlay, mapped.key()),
                            active_geo,
                            0.0,
                            0.3,
                            group_color,
                        )),
                    )
                }

                if swap_desc
                    .as_ref()
                    .map(|swap_desc| {
                        (swap_desc.node == node_id
                            || target_tree.ancestor_ids(&node_id).ok().is_none_or(
                                |mut ancestors| ancestors.any(|id| &swap_desc.node == id),
                            ))
                            && swap_desc.stack_window.is_none()
                    })
                    .unwrap_or(false)
                {
                    swap_elements.extend(upper_elements);
                    swap_elements.extend(shadow_element);
                    swap_elements.extend(lower_elements);
                } else if animating {
                    animating_window_upper_elements.extend(upper_elements);
                    animating_shadow_elements.extend(shadow_element);
                    animating_window_lower_elements.extend(lower_elements);
                } else {
                    window_upper_elements.extend(upper_elements);
                    shadow_elements.extend(shadow_element);
                    window_lower_elements.extend(lower_elements);
                }
            }
        },
    );

    for elem in resize_elements
        .into_iter()
        .chain(swap_elements)
        .chain(indicators)
        .chain(window_upper_elements)
        .chain(shadow_elements)
        .chain(window_lower_elements)
        .chain(animating_window_upper_elements)
        .chain(animating_shadow_elements)
        .chain(animating_window_lower_elements)
        .chain(group_backdrop.into_iter().map(Into::into))
    {
        push(elem);
    }
}

fn tiled_window_render_max_size(
    is_overview: bool,
    animating: bool,
    live_resize: bool,
    animated_geometry: &Rectangle<i32, Local>,
    target_geometry: &Rectangle<i32, Local>,
    committed_size: Size<i32, Logical>,
) -> Option<Size<i32, Logical>> {
    if is_overview {
        None
    } else if live_resize {
        Some(committed_size)
    } else if tiled_window_has_animated_width(
        is_overview,
        animating,
        animated_geometry,
        target_geometry,
    ) {
        Some(target_geometry.size.as_logical())
    } else {
        Some(animated_geometry.size.as_logical())
    }
}

fn live_resize_tile_is_pending(latest_size_committed: bool, resizing: bool) -> bool {
    !latest_size_committed || resizing
}

fn tiled_window_has_animated_width(
    is_overview: bool,
    animating: bool,
    animated_geometry: &Rectangle<i32, Local>,
    target_geometry: &Rectangle<i32, Local>,
) -> bool {
    !is_overview
        && animating
        && animated_geometry.size.w != target_geometry.size.w
        && animated_geometry.size.h == target_geometry.size.h
}

fn render_new_tree(
    target_tree: &Tree<Data>,
    reference_tree: Option<&Tree<Data>>,
    geometries: Option<HashMap<NodeId, Rectangle<i32, Local>>>,
    old_geometries: Option<HashMap<NodeId, Rectangle<i32, Local>>>,
    percentage: f32,
    swap_tree: Option<&Tree<Data>>,
    swap_desc: Option<&NodeDesc>,
    mut processor: impl FnMut(NodeId, &Data, Rectangle<i32, Local>, &Rectangle<i32, Local>, f32, bool),
) {
    let old_geometries = old_geometries.unwrap_or_default();
    let geometries = geometries.unwrap_or_default();
    target_tree
        .root_node_id()
        .into_iter()
        .flat_map(|root| target_tree.traverse_pre_order_ids(root).unwrap())
        .map(|id| (target_tree, id))
        .chain(
            swap_tree
                .into_iter()
                .flat_map(|tree| {
                    let sub_root = &swap_desc.unwrap().node;
                    if swap_desc.unwrap().stack_window.is_none() {
                        Some(
                            tree.traverse_pre_order_ids(sub_root)
                                .unwrap()
                                .map(move |id| (tree, id)),
                        )
                    } else {
                        None
                    }
                })
                .flatten(),
        )
        .for_each(|(target_tree, node_id)| {
            let data = target_tree.get(&node_id).unwrap().data();
            let (original_geo, scaled_geo) = (data.geometry(), geometries.get(&node_id));

            let (old_original_geo, old_scaled_geo) =
                if let Some(reference_tree) = reference_tree.as_ref() {
                    if let Some(root) = reference_tree.root_node_id() {
                        reference_tree
                            .traverse_pre_order_ids(root)
                            .unwrap()
                            .find(|id| &node_id == id)
                            .map(|node_id| {
                                (
                                    reference_tree.get(&node_id).unwrap().data().geometry(),
                                    old_geometries.get(&node_id),
                                )
                            })
                    } else {
                        None
                    }
                } else {
                    None
                }
                .unzip();
            let mut old_geo = old_original_geo.map(|original_geo| {
                let (scale, offset) = old_scaled_geo
                    .unwrap()
                    .map(|adapted_geo| scale_to_center(original_geo, adapted_geo))
                    .unwrap_or_else(|| (1.0, (0, 0).into()));
                (
                    old_scaled_geo
                        .unwrap()
                        .map(|adapted_geo| {
                            Rectangle::new(
                                adapted_geo.loc + offset,
                                (original_geo.size.to_f64() * scale).to_i32_round(),
                            )
                        })
                        .unwrap_or(*original_geo),
                    1.0,
                )
            });

            let was_minimized = if let Data::Mapped {
                minimize_rect: Some(minimize_rect),
                ..
            } = &data
            {
                old_geo = Some((*minimize_rect, (percentage * 2.0).min(1.0)));
                true
            } else {
                false
            };

            let (scale, offset) = scaled_geo
                .map(|adapted_geo| scale_to_center(original_geo, adapted_geo))
                .unwrap_or_else(|| (1.0, (0, 0).into()));
            let new_geo = scaled_geo
                .map(|adapted_geo| {
                    Rectangle::new(
                        adapted_geo.loc + offset,
                        (
                            (original_geo.size.w as f64 * scale).round() as i32,
                            (original_geo.size.h as f64 * scale).round() as i32,
                        )
                            .into(),
                    )
                })
                .unwrap_or(*original_geo);

            let (geo, alpha, animating) = if let Some((old_geo, alpha)) = old_geo.filter(|_| {
                swap_desc
                    .map(|desc| desc.node != node_id && desc.stack_window.is_none())
                    .unwrap_or(true)
            }) {
                (
                    if was_minimized {
                        ease(
                            EaseInOutCubic,
                            EaseRectangle(old_geo),
                            EaseRectangle(new_geo),
                            percentage,
                        )
                        .unwrap()
                    } else {
                        ease(
                            Linear,
                            EaseRectangle(old_geo),
                            EaseRectangle(new_geo),
                            percentage,
                        )
                        .unwrap()
                    },
                    alpha,
                    old_geo != new_geo,
                )
            } else {
                (new_geo, percentage, false)
            };

            if let Data::Mapped { mapped, .. } = data
                && mapped.is_maximized(false)
            {
                return;
            }
            processor(node_id, data, geo, original_geo, alpha, animating)
        });
}

fn scale_to_center<C>(
    old_geo: &Rectangle<i32, C>,
    new_geo: &Rectangle<i32, C>,
) -> (f64, Point<i32, C>) {
    let scale_w = new_geo.size.w as f64 / old_geo.size.w as f64;
    let scale_h = new_geo.size.h as f64 / old_geo.size.h as f64;

    if scale_w > scale_h {
        (
            scale_h,
            (
                ((new_geo.size.w as f64 - old_geo.size.w as f64 * scale_h) / 2.0).round() as i32,
                0,
            )
                .into(),
        )
    } else {
        (
            scale_w,
            (
                0,
                ((new_geo.size.h as f64 - old_geo.size.h as f64 * scale_w) / 2.0).round() as i32,
            )
                .into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::output::{Mode, PhysicalProperties, Subpixel};

    fn test_output() -> Output {
        Output::new(
            "tiling-test".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "COSMIC".into(),
                model: "test".into(),
                serial_number: "test".into(),
            },
        )
    }

    fn empty_layout(tiling_engine: TilingEngine) -> TilingLayout {
        TilingLayout::new(
            cosmic::Theme::default(),
            AppearanceConfig::default(),
            &test_output(),
            tiling_engine,
        )
    }

    fn sized_scrolling_layout() -> TilingLayout {
        let output = test_output();
        output.change_current_state(
            Some(Mode {
                size: (1_200, 800).into(),
                refresh: 60_000,
            }),
            None,
            None,
            Some((0, 0).into()),
        );
        TilingLayout::new(
            cosmic::Theme::default(),
            AppearanceConfig::default(),
            &output,
            TilingEngine::Scrolling,
        )
    }

    #[test]
    fn empty_layout_uses_requested_engine() {
        assert_eq!(
            empty_layout(TilingEngine::Classic).mode(),
            TilingEngine::Classic
        );
        assert_eq!(
            empty_layout(TilingEngine::Scrolling).mode(),
            TilingEngine::Scrolling
        );
    }

    #[test]
    fn confirmed_new_toplevel_is_reconciled_after_the_focused_scrolling_column() {
        let geometry = Rectangle::new((0, 0).into(), (1_200, 800).into());
        let mut layout = sized_scrolling_layout();
        let mut tree = Tree::new();
        let root = tree
            .insert(
                Node::new(Data::Group {
                    orientation: Orientation::Vertical,
                    sizes: vec![400; 3],
                    last_geometry: geometry,
                    alive: Arc::new(()),
                    pill_indicator: None,
                }),
                InsertBehavior::AsRoot,
            )
            .unwrap();
        let insert = |tree: &mut Tree<Data>| {
            tree.insert(
                Node::new(Data::Placeholder {
                    id: Id::new(),
                    last_geometry: geometry,
                    type_: PlaceholderType::GrabbedWindow,
                }),
                InsertBehavior::UnderNode(&root),
            )
            .unwrap()
        };
        let first = insert(&mut tree);
        let second = insert(&mut tree);
        let third = insert(&mut tree);
        layout.update_positions_for(&mut tree, layout.gaps());

        assert!(layout.scrolling.begin_drag(&second));
        layout
            .scrolling
            .finish_drag(Some(&second), ColumnDropTarget::Above(first.clone()));
        assert_eq!(
            layout.scrolling.column_tiles(),
            [vec![second.clone(), first.clone()], vec![third.clone()]]
        );

        if let Data::Group { sizes, .. } = tree.get_mut(&root).unwrap().data_mut() {
            sizes.push(400);
        }
        let new = insert(&mut tree);
        layout
            .scrolling
            .insert_new_toplevel(new.clone(), Some(&first));
        layout.update_positions_for(&mut tree, layout.gaps());

        assert_eq!(
            layout.scrolling.column_tiles(),
            [vec![second, first], vec![new], vec![third]]
        );
        assert_eq!(
            layout.scrolling.column_width_fractions(),
            [0.66, 0.66, 0.66]
        );
    }

    #[test]
    fn direct_geometry_updates_coalesce_before_presentation() {
        let mut layout = empty_layout(TilingEngine::Scrolling);
        let first = Tree::new();
        let second = Tree::new();

        layout
            .queue
            .push_direct_tree(first, Some(TilingBlocker::new(std::iter::empty())));
        layout
            .queue
            .push_direct_tree(second, Some(TilingBlocker::new(std::iter::empty())));

        assert_eq!(layout.queue.trees.len(), 2);
        assert_eq!(layout.queue.trees.back().unwrap().1, Duration::ZERO);
        assert!(layout.queue.trees.back().unwrap().2.is_some());
        assert!(layout.queue.retired_blocker.is_some());
    }

    #[test]
    fn scrolling_animations_retarget_from_current_visual_geometry() {
        let source_geometry = Rectangle::new((0, 0).into(), (600, 800).into());
        let mut source = Tree::new();
        let tile = source
            .insert(
                Node::new(Data::Placeholder {
                    id: Id::new(),
                    last_geometry: source_geometry,
                    type_: PlaceholderType::GrabbedWindow,
                }),
                InsertBehavior::AsRoot,
            )
            .unwrap();
        let mut target = source.copy_clone();
        target
            .get_mut(&tile)
            .unwrap()
            .data_mut()
            .update_geometry(Rectangle::new((100, 0).into(), (600, 800).into()));

        let mut queue = TreeQueue::default();
        queue.push_tree(source, Duration::ZERO, None);
        queue.push_retargetable_tree(target, ANIMATION_DURATION, None);
        queue.animation_start = Some(Instant::now() - ANIMATION_DURATION / 2);

        let mut final_target = queue.trees.back().unwrap().0.copy_clone();
        final_target
            .get_mut(&tile)
            .unwrap()
            .data_mut()
            .update_geometry(Rectangle::new((200, 0).into(), (600, 800).into()));
        queue.push_retargetable_tree(final_target, ANIMATION_DURATION, None);

        assert_eq!(queue.trees.len(), 2);
        assert_eq!(queue.trees.back().unwrap().1, ANIMATION_DURATION);
        let current_x = queue
            .trees
            .front()
            .unwrap()
            .0
            .get(&tile)
            .unwrap()
            .data()
            .geometry()
            .loc
            .x;
        assert!((0..=100).contains(&current_x));
        assert_eq!(
            queue
                .trees
                .back()
                .unwrap()
                .0
                .get(&tile)
                .unwrap()
                .data()
                .geometry()
                .loc
                .x,
            200
        );
    }

    #[test]
    fn animated_tiled_elements_are_constrained_once_from_target_geometry() {
        let widening_target = Rectangle::new((0, 0).into(), (1_000, 390).into());
        let widening_frame = Rectangle::new((0, 0).into(), (830, 390).into());
        let widening_source = tiled_window_render_max_size(
            false,
            true,
            false,
            &widening_frame,
            &widening_target,
            widening_target.size.as_logical(),
        )
        .unwrap();
        assert_eq!(widening_source, widening_target.size.as_logical());
        assert_eq!(
            (f64::from(widening_source.w) * f64::from(widening_frame.size.w)
                / f64::from(widening_target.size.w))
            .round() as i32,
            widening_frame.size.w
        );

        let narrowing_target = Rectangle::new((0, 0).into(), (660, 390).into());
        let narrowing_frame = Rectangle::new((0, 0).into(), (830, 390).into());
        let narrowing_source = tiled_window_render_max_size(
            false,
            true,
            false,
            &narrowing_frame,
            &narrowing_target,
            narrowing_target.size.as_logical(),
        )
        .unwrap();
        assert_eq!(narrowing_source, narrowing_target.size.as_logical());
        assert_eq!(
            (f64::from(narrowing_source.w) * f64::from(narrowing_frame.size.w)
                / f64::from(narrowing_target.size.w))
            .round() as i32,
            narrowing_frame.size.w
        );
        assert_eq!(narrowing_source.h, narrowing_frame.size.h);

        assert_eq!(
            tiled_window_render_max_size(
                false,
                false,
                false,
                &widening_frame,
                &widening_target,
                widening_target.size.as_logical(),
            ),
            Some(widening_frame.size.as_logical())
        );
        assert_eq!(
            tiled_window_render_max_size(
                true,
                true,
                false,
                &widening_frame,
                &widening_target,
                widening_target.size.as_logical(),
            ),
            None
        );

        let vertical_frame = Rectangle::new((0, 0).into(), (1_000, 300).into());
        assert_eq!(
            tiled_window_render_max_size(
                false,
                true,
                false,
                &vertical_frame,
                &widening_target,
                widening_target.size.as_logical(),
            ),
            Some(vertical_frame.size.as_logical())
        );
        assert!(!tiled_window_has_animated_width(
            false,
            true,
            &vertical_frame,
            &widening_target
        ));
    }

    #[test]
    fn live_resize_uses_the_committed_buffer_as_its_single_constraint_source() {
        let widening_tile = Rectangle::new((100, 0).into(), (900, 390).into());
        let narrowing_tile = Rectangle::new((100, 0).into(), (500, 390).into());
        let committed = Size::from((660, 390));

        for tile in [widening_tile, narrowing_tile] {
            let source =
                tiled_window_render_max_size(false, false, true, &tile, &tile, committed).unwrap();
            assert_eq!(source, committed);

            // The committed content is stretched or clipped exactly once to
            // the current tile, so its resulting edge is the tile edge.
            let visible_width =
                (f64::from(source.w) * f64::from(tile.size.w) / f64::from(source.w)).round() as i32;
            assert_eq!(visible_width, tile.size.w);
            assert_eq!(
                tile.loc.x.saturating_add(visible_width),
                tile.loc.x + tile.size.w
            );
        }

        let taller_upper = Rectangle::new((100, 0).into(), (660, 500).into());
        let shorter_lower = Rectangle::new((100, 510).into(), (660, 280).into());
        for tile in [taller_upper, shorter_lower] {
            let source =
                tiled_window_render_max_size(false, false, true, &tile, &tile, committed).unwrap();
            assert_eq!(source, committed);
            let visible_height =
                (f64::from(source.h) * f64::from(tile.size.h) / f64::from(source.h)).round() as i32;
            assert_eq!(visible_height, tile.size.h);
            assert_eq!(
                tile.loc.y.saturating_add(visible_height),
                tile.loc.y + tile.size.h
            );
        }

        assert_eq!(
            tiled_window_render_max_size(
                true,
                false,
                true,
                &widening_tile,
                &widening_tile,
                committed,
            ),
            None
        );

        assert!(live_resize_tile_is_pending(false, true));
        assert!(live_resize_tile_is_pending(true, true));
        assert!(!live_resize_tile_is_pending(true, false));
    }

    #[test]
    fn repeated_scrolling_animations_remain_bounded_and_preserve_blockers() {
        let mut queue = TreeQueue::default();
        queue.push_tree(Tree::new(), Duration::ZERO, None);
        queue.push_retargetable_tree(
            Tree::new(),
            ANIMATION_DURATION,
            Some(TilingBlocker::new(std::iter::empty())),
        );

        for _ in 0..32 {
            queue.push_retargetable_tree(Tree::new(), ANIMATION_DURATION, None);
            assert_eq!(queue.trees.len(), 2);
        }

        assert!(queue.retired_blocker.is_some());
        assert_eq!(queue.trees.back().unwrap().1, ANIMATION_DURATION);
    }

    #[test]
    fn direct_input_interrupts_scrolling_animation_and_stays_immediate() {
        let mut queue = TreeQueue::default();
        queue.push_tree(Tree::new(), Duration::ZERO, None);
        queue.push_retargetable_tree(Tree::new(), ANIMATION_DURATION, None);
        queue.animation_start = Some(Instant::now());

        let direct = queue.prepare_direct_tree();
        queue.push_direct_tree(direct, None);

        assert!(queue.animation_start.is_none());
        assert_eq!(queue.trees.len(), 2);
        assert_eq!(queue.trees.back().unwrap().1, Duration::ZERO);
    }

    #[test]
    fn scrolling_focus_and_center_viewport_changes_are_animated() {
        let geometry = Rectangle::new((0, 0).into(), (1_200, 800).into());
        let mut layout = sized_scrolling_layout();
        let mut tree = Tree::new();
        let root = tree
            .insert(
                Node::new(Data::Group {
                    orientation: Orientation::Vertical,
                    sizes: vec![400, 400, 400],
                    last_geometry: geometry,
                    alive: Arc::new(()),
                    pill_indicator: None,
                }),
                InsertBehavior::AsRoot,
            )
            .unwrap();
        let insert = |tree: &mut Tree<Data>| {
            tree.insert(
                Node::new(Data::Placeholder {
                    id: Id::new(),
                    last_geometry: geometry,
                    type_: PlaceholderType::GrabbedWindow,
                }),
                InsertBehavior::UnderNode(&root),
            )
            .unwrap()
        };
        let first = insert(&mut tree);
        let _second = insert(&mut tree);
        let third = insert(&mut tree);
        let blocker = layout.update_positions_for(&mut tree, layout.gaps());
        layout.queue.trees.clear();
        layout.queue.push_tree(tree, Duration::ZERO, blocker);

        assert!(!layout.scrolling.ensure_visible(&first));
        assert_eq!(layout.queue.trees.len(), 1);
        assert!(layout.scrolling.ensure_visible(&third));
        let mut focused = layout.tree().copy_clone();
        let blocker = layout.scrolling.update_positions_ensuring(
            &layout.output,
            &mut focused,
            layout.gaps(),
            Some(&third),
        );
        layout.push_scrolling_animation(focused, blocker);
        assert_eq!(layout.queue.trees.back().unwrap().1, ANIMATION_DURATION);

        assert!(layout.scrolling.request_center(&third));
        let mut centered = layout.tree().copy_clone();
        let blocker = layout.update_positions_for(&mut centered, layout.gaps());
        layout.push_scrolling_animation(centered, blocker);
        assert_eq!(layout.queue.trees.len(), 2);
        assert_eq!(layout.queue.trees.back().unwrap().1, ANIMATION_DURATION);
    }

    #[test]
    fn scrolling_vertical_focus_edges_allow_workspace_fallback() {
        assert!(matches!(
            scrolling_edge_focus_result(FocusDirection::Up),
            FocusResult::None
        ));
        assert!(matches!(
            scrolling_edge_focus_result(FocusDirection::Down),
            FocusResult::None
        ));
        assert!(matches!(
            scrolling_edge_focus_result(FocusDirection::Left),
            FocusResult::Handled
        ));
        assert!(matches!(
            scrolling_edge_focus_result(FocusDirection::Right),
            FocusResult::Handled
        ));
    }

    #[test]
    fn empty_layout_mode_transition_replaces_pending_queue_state() {
        let mut layout = empty_layout(TilingEngine::Classic);
        let mut transient_tree = Tree::new();
        transient_tree
            .insert(
                Node::new(Data::Placeholder {
                    id: Id::new(),
                    last_geometry: Rectangle::from_size((100, 100).into()),
                    type_: PlaceholderType::DropZone,
                }),
                InsertBehavior::AsRoot,
            )
            .unwrap();
        layout
            .queue
            .push_tree(transient_tree, ANIMATION_DURATION, None);
        layout.queue.animation_start = Some(Instant::now());
        layout.last_overview_hover = Some((None, TargetZone::Initial));

        layout.set_mode(TilingEngine::Scrolling);

        assert_eq!(layout.mode(), TilingEngine::Scrolling);
        assert_eq!(layout.queue.trees.len(), 1);
        assert_eq!(layout.queue.trees.front().unwrap().1, Duration::ZERO);
        assert!(layout.queue.trees.front().unwrap().2.is_none());
        assert!(layout.queue.animation_start.is_none());
        assert!(layout.last_overview_hover.is_none());
        assert!(layout.tree().root_node_id().is_none());

        layout.set_mode(TilingEngine::Classic);
        assert_eq!(layout.mode(), TilingEngine::Classic);
        assert_eq!(layout.queue.trees.len(), 1);
        assert!(layout.tree().root_node_id().is_none());
    }

    #[test]
    fn mode_transition_keeps_grabbed_window_placeholder() {
        let mut layout = empty_layout(TilingEngine::Classic);
        let mut grabbed_tree = Tree::new();
        let grabbed = grabbed_tree
            .insert(
                Node::new(Data::Placeholder {
                    id: Id::new(),
                    last_geometry: Rectangle::from_size((100, 100).into()),
                    type_: PlaceholderType::GrabbedWindow,
                }),
                InsertBehavior::AsRoot,
            )
            .unwrap();
        layout.queue.push_tree(grabbed_tree, Duration::ZERO, None);

        layout.set_mode(TilingEngine::Scrolling);

        assert_eq!(layout.tree().root_node_id(), Some(&grabbed));
        assert!(matches!(
            layout.last_overview_hover,
            Some((None, TargetZone::InitialPlaceholder(ref node_id))) if node_id == &grabbed
        ));
        assert_eq!(
            layout
                .scrolling
                .next_focus(layout.tree(), &grabbed, FocusDirection::Left),
            FocusNavigation::Edge
        );
        let Data::Placeholder { last_geometry, .. } = layout.tree().get(&grabbed).unwrap().data()
        else {
            unreachable!()
        };
        assert_ne!(*last_geometry, Rectangle::from_size((100, 100).into()));

        layout.set_mode(TilingEngine::Classic);
        assert_eq!(layout.mode(), TilingEngine::Classic);
        assert_eq!(layout.tree().root_node_id(), Some(&grabbed));
    }

    #[test]
    fn non_empty_round_trip_preserves_nodes_and_scrolling_order() {
        let geometry = Rectangle::new((0, 0).into(), (1_200, 800).into());
        let mut layout = empty_layout(TilingEngine::Scrolling);
        let mut tree = Tree::new();
        let root = tree
            .insert(
                Node::new(Data::Group {
                    orientation: Orientation::Vertical,
                    sizes: vec![400, 400, 400],
                    last_geometry: geometry,
                    alive: Arc::new(()),
                    pill_indicator: None,
                }),
                InsertBehavior::AsRoot,
            )
            .unwrap();
        let insert_column = |tree: &mut Tree<Data>| {
            tree.insert(
                Node::new(Data::Placeholder {
                    id: Id::new(),
                    last_geometry: geometry,
                    type_: PlaceholderType::GrabbedWindow,
                }),
                InsertBehavior::UnderNode(&root),
            )
            .unwrap()
        };
        let first = insert_column(&mut tree);
        let second = insert_column(&mut tree);
        let third = insert_column(&mut tree);
        let blocker = layout.update_positions_for(&mut tree, layout.gaps());
        layout.queue.trees.clear();
        layout.queue.push_tree(tree, Duration::ZERO, blocker);

        assert!(layout.scrolling.move_column(&second, Direction::Right));
        assert_eq!(
            layout.scrolling.column_order(),
            vec![first.clone(), third.clone(), second.clone()]
        );
        let initial_preorder = layout
            .tree()
            .traverse_pre_order_ids(&root)
            .unwrap()
            .collect::<Vec<_>>();

        layout.set_mode(TilingEngine::Classic);

        let mut classic_tree = layout.tree().copy_clone();
        let fourth = insert_column(&mut classic_tree);
        classic_tree
            .get_mut(&root)
            .unwrap()
            .data_mut()
            .add_window(3);
        let blocker = layout.update_positions_for(&mut classic_tree, layout.gaps());
        layout
            .queue
            .push_tree(classic_tree, Duration::ZERO, blocker);
        layout.set_mode(TilingEngine::Scrolling);

        let final_preorder = layout
            .tree()
            .traverse_pre_order_ids(&root)
            .unwrap()
            .collect::<Vec<_>>();
        assert_eq!(&final_preorder[..initial_preorder.len()], initial_preorder);
        assert_eq!(final_preorder.last(), Some(&fourth));
        assert_eq!(
            layout.scrolling.column_order(),
            vec![first, third, second, fourth]
        );
    }

    #[test]
    fn scrolling_pointer_drop_commits_preview_node_replacement_at_target() {
        let geometry = Rectangle::new((0, 0).into(), (1_200, 800).into());
        let mut layout = empty_layout(TilingEngine::Scrolling);
        let mut tree = Tree::new();
        let root = tree
            .insert(
                Node::new(Data::Group {
                    orientation: Orientation::Vertical,
                    sizes: vec![400, 400, 400],
                    last_geometry: geometry,
                    alive: Arc::new(()),
                    pill_indicator: None,
                }),
                InsertBehavior::AsRoot,
            )
            .unwrap();
        let insert_column = |tree: &mut Tree<Data>| {
            tree.insert(
                Node::new(Data::Placeholder {
                    id: Id::new(),
                    last_geometry: geometry,
                    type_: PlaceholderType::GrabbedWindow,
                }),
                InsertBehavior::UnderNode(&root),
            )
            .unwrap()
        };
        let first = insert_column(&mut tree);
        let second = insert_column(&mut tree);
        let third = insert_column(&mut tree);
        let blocker = layout.update_positions_for(&mut tree, layout.gaps());
        layout.queue.trees.clear();
        layout.queue.push_tree(tree, Duration::ZERO, blocker);

        assert!(layout.scrolling.begin_drag(&third));
        let mut preview = layout.tree().copy_clone();
        TilingLayout::unmap_internal(&mut preview, &third);
        let blocker = layout.update_positions_for(&mut preview, layout.gaps());
        layout.queue.trees.clear();
        layout.queue.push_tree(preview, Duration::ZERO, blocker);
        assert_eq!(
            layout.scrolling.column_order(),
            vec![first.clone(), second.clone()]
        );

        let mut dropped = layout.tree().copy_clone();
        let replacement = insert_column(&mut dropped);
        dropped.get_mut(&root).unwrap().data_mut().add_window(1);
        dropped.make_nth_sibling(&replacement, 1).unwrap();
        let target = scrolling_drop_target(
            &dropped,
            Some(&TargetZone::WindowSplit(second.clone(), Direction::Left)),
        );
        assert_eq!(target, ColumnDropTarget::Before(second.clone()));
        assert_eq!(
            scrolling_drop_target(
                &dropped,
                Some(&TargetZone::GroupEdge(root.clone(), Direction::Left)),
            ),
            ColumnDropTarget::Before(first.clone())
        );
        assert_eq!(
            scrolling_drop_target(&dropped, Some(&TargetZone::GroupInterior(root.clone(), 0)),),
            ColumnDropTarget::After(first.clone())
        );

        layout.scrolling.finish_drag(Some(&replacement), target);
        let blocker = layout.update_positions_for(&mut dropped, layout.gaps());
        layout.push_scrolling_animation(dropped, blocker);
        assert_eq!(layout.queue.trees.back().unwrap().1, ANIMATION_DURATION);

        assert_eq!(
            layout.scrolling.column_order(),
            vec![first, replacement, second]
        );
    }

    #[test]
    fn scrolling_pointer_drop_inserts_replacement_above_target_tile() {
        let geometry = Rectangle::new((0, 0).into(), (1_200, 800).into());
        let mut layout = empty_layout(TilingEngine::Scrolling);
        let mut tree = Tree::new();
        let root = tree
            .insert(
                Node::new(Data::Group {
                    orientation: Orientation::Vertical,
                    sizes: vec![400, 400, 400],
                    last_geometry: geometry,
                    alive: Arc::new(()),
                    pill_indicator: None,
                }),
                InsertBehavior::AsRoot,
            )
            .unwrap();
        let insert_tile = |tree: &mut Tree<Data>| {
            tree.insert(
                Node::new(Data::Placeholder {
                    id: Id::new(),
                    last_geometry: geometry,
                    type_: PlaceholderType::GrabbedWindow,
                }),
                InsertBehavior::UnderNode(&root),
            )
            .unwrap()
        };
        let first = insert_tile(&mut tree);
        let second = insert_tile(&mut tree);
        let third = insert_tile(&mut tree);
        let blocker = layout.update_positions_for(&mut tree, layout.gaps());
        layout.queue.trees.clear();
        layout.queue.push_tree(tree, Duration::ZERO, blocker);

        assert!(layout.scrolling.begin_drag(&third));
        let mut preview = layout.tree().copy_clone();
        TilingLayout::unmap_internal(&mut preview, &third);
        let blocker = layout.update_positions_for(&mut preview, layout.gaps());
        layout.queue.trees.clear();
        layout.queue.push_tree(preview, Duration::ZERO, blocker);

        let mut dropped = layout.tree().copy_clone();
        let replacement = insert_tile(&mut dropped);
        let target = scrolling_drop_target(
            &dropped,
            Some(&TargetZone::WindowSplit(second.clone(), Direction::Up)),
        );
        assert_eq!(target, ColumnDropTarget::Above(second.clone()));
        assert_eq!(
            scrolling_drop_target(
                &dropped,
                Some(&TargetZone::WindowSplit(second.clone(), Direction::Down)),
            ),
            ColumnDropTarget::Below(second.clone())
        );

        layout.scrolling.finish_drag(Some(&replacement), target);
        let blocker = layout.update_positions_for(&mut dropped, layout.gaps());
        layout.push_scrolling_animation(dropped, blocker);
        assert_eq!(layout.queue.trees.back().unwrap().1, ANIMATION_DURATION);

        assert_eq!(
            layout.scrolling.column_tiles(),
            vec![vec![first], vec![replacement, second]]
        );
    }

    #[test]
    fn scrolling_directional_focus_never_targets_grabbed_placeholders() {
        let geometry = Rectangle::new((0, 0).into(), (1_200, 800).into());
        let mut layout = empty_layout(TilingEngine::Scrolling);
        let mut tree = Tree::new();
        let root = tree
            .insert(
                Node::new(Data::Group {
                    orientation: Orientation::Vertical,
                    sizes: vec![600, 600],
                    last_geometry: geometry,
                    alive: Arc::new(()),
                    pill_indicator: None,
                }),
                InsertBehavior::AsRoot,
            )
            .unwrap();
        let insert_placeholder = |tree: &mut Tree<Data>| {
            tree.insert(
                Node::new(Data::Placeholder {
                    id: Id::new(),
                    last_geometry: geometry,
                    type_: PlaceholderType::GrabbedWindow,
                }),
                InsertBehavior::UnderNode(&root),
            )
            .unwrap()
        };
        let first = insert_placeholder(&mut tree);
        let second = insert_placeholder(&mut tree);
        let blocker = layout.update_positions_for(&mut tree, layout.gaps());
        layout.queue.trees.clear();
        layout.queue.push_tree(tree, Duration::ZERO, blocker);

        assert_eq!(
            layout
                .scrolling
                .next_focus(layout.tree(), &first, FocusDirection::Left),
            FocusNavigation::Edge
        );
        assert_eq!(
            layout
                .scrolling
                .next_focus(layout.tree(), &first, FocusDirection::Right),
            FocusNavigation::Unavailable
        );

        assert!(layout.scrolling.begin_drag(&second));
        layout
            .scrolling
            .finish_drag(Some(&second), ColumnDropTarget::Above(first.clone()));
        assert_eq!(
            layout
                .scrolling
                .next_focus(layout.tree(), &second, FocusDirection::Up),
            FocusNavigation::Edge
        );
        assert_eq!(
            layout
                .scrolling
                .next_focus(layout.tree(), &second, FocusDirection::Down),
            FocusNavigation::Unavailable
        );
    }

    #[test]
    fn scrolling_keyboard_actions_merge_extract_and_resize_columns() {
        let geometry = Rectangle::new((0, 0).into(), (1_200, 800).into());
        let mut layout = empty_layout(TilingEngine::Scrolling);
        let mut tree = Tree::new();
        let root = tree
            .insert(
                Node::new(Data::Group {
                    orientation: Orientation::Vertical,
                    sizes: vec![400, 400, 400],
                    last_geometry: geometry,
                    alive: Arc::new(()),
                    pill_indicator: None,
                }),
                InsertBehavior::AsRoot,
            )
            .unwrap();
        let insert_placeholder = |tree: &mut Tree<Data>| {
            tree.insert(
                Node::new(Data::Placeholder {
                    id: Id::new(),
                    last_geometry: geometry,
                    type_: PlaceholderType::GrabbedWindow,
                }),
                InsertBehavior::UnderNode(&root),
            )
            .unwrap()
        };
        let first = insert_placeholder(&mut tree);
        let second = insert_placeholder(&mut tree);
        let third = insert_placeholder(&mut tree);
        let blocker = layout.update_positions_for(&mut tree, layout.gaps());
        layout.queue.trees.clear();
        layout.queue.push_tree(tree, Duration::ZERO, blocker);

        assert_eq!(
            layout
                .scrolling
                .move_tile_vertical(&second, Direction::Down),
            TileMove::Moved
        );
        assert_eq!(
            layout.scrolling.column_tiles(),
            vec![vec![first.clone(), second.clone()], vec![third.clone()]]
        );
        let mut animated = layout.tree().copy_clone();
        let blocker = layout.update_positions_for(&mut animated, layout.gaps());
        layout.push_scrolling_animation(animated, blocker);
        assert_eq!(layout.queue.trees.back().unwrap().1, ANIMATION_DURATION);
        assert_eq!(
            layout
                .scrolling
                .move_tile_vertical(&second, Direction::Down),
            TileMove::Edge
        );

        assert!(layout.scrolling.move_column(&first, Direction::Left));
        assert_eq!(
            layout.scrolling.column_tiles(),
            vec![vec![first], vec![second.clone()], vec![third]]
        );
        let mut animated = layout.tree().copy_clone();
        let blocker = layout.update_positions_for(&mut animated, layout.gaps());
        layout.push_scrolling_animation(animated, blocker);
        assert_eq!(layout.queue.trees.len(), 2);
        assert_eq!(layout.queue.trees.back().unwrap().1, ANIMATION_DURATION);

        assert!(layout.cycle_scrolling_column_width_for(&second, ResizeDirection::Outwards));
        assert_eq!(layout.scrolling.column_width_fractions(), [0.66, 1.0, 0.66]);
        assert_eq!(layout.queue.trees.len(), 2);
        assert_eq!(layout.queue.trees.back().unwrap().1, ANIMATION_DURATION);
        assert!(layout.cycle_scrolling_column_width_for(&second, ResizeDirection::Inwards));
        assert_eq!(
            layout.scrolling.column_width_fractions(),
            [0.66, 0.66, 0.66]
        );
        assert_eq!(layout.queue.trees.len(), 2);
        assert_eq!(layout.queue.trees.back().unwrap().1, ANIMATION_DURATION);

        let queued = layout.queue.trees.len();
        let unavailable = layout.tree().root_node_id().unwrap().clone();
        assert!(!layout.cycle_scrolling_column_width_for(&unavailable, ResizeDirection::Outwards));
        assert_eq!(layout.queue.trees.len(), queued);

        assert_eq!(
            layout.resize_scrolling_column(&second, 1, ColumnResizeAnchor::Left),
            Some(true)
        );
        assert_eq!(layout.scrolling.column_width_fractions(), [0.66, 1.0, 0.66]);
        assert_eq!(layout.queue.trees.len(), 2);
        assert_eq!(layout.queue.trees.back().unwrap().1, Duration::ZERO);

        let mut classic = empty_layout(TilingEngine::Classic);
        assert!(!classic.cycle_scrolling_column_width_for(&second, ResizeDirection::Outwards));
        assert_eq!(classic.queue.trees.len(), 1);
        assert_eq!(classic.queue.trees.back().unwrap().1, Duration::ZERO);
    }

    #[test]
    fn scrolling_mouse_resize_keeps_the_opposite_column_edge_fixed() {
        let geometry = Rectangle::new((0, 0).into(), (1_200, 800).into());
        let mut layout = sized_scrolling_layout();
        let mut tree = Tree::new();
        let root = tree
            .insert(
                Node::new(Data::Group {
                    orientation: Orientation::Vertical,
                    sizes: vec![400, 400, 400],
                    last_geometry: geometry,
                    alive: Arc::new(()),
                    pill_indicator: None,
                }),
                InsertBehavior::AsRoot,
            )
            .unwrap();
        let insert_placeholder = |tree: &mut Tree<Data>| {
            tree.insert(
                Node::new(Data::Placeholder {
                    id: Id::new(),
                    last_geometry: geometry,
                    type_: PlaceholderType::GrabbedWindow,
                }),
                InsertBehavior::UnderNode(&root),
            )
            .unwrap()
        };
        let first = insert_placeholder(&mut tree);
        let second = insert_placeholder(&mut tree);
        let third = insert_placeholder(&mut tree);
        let blocker = layout.update_positions_for(&mut tree, layout.gaps());
        layout.queue.trees.clear();
        layout.queue.push_tree(tree, Duration::ZERO, blocker);

        assert_eq!(
            layout
                .scrolling
                .move_tile_vertical(&second, Direction::Down),
            TileMove::Moved
        );
        let mut stacked = layout.tree().copy_clone();
        let blocker = layout.update_positions_for(&mut stacked, layout.gaps());
        layout.queue.trees.clear();
        layout.queue.push_tree(stacked, Duration::ZERO, blocker);
        assert_eq!(
            layout.scrolling.column_tiles(),
            [vec![first.clone(), second.clone()], vec![third.clone()]]
        );

        let initial = *layout.tree().get(&second).unwrap().data().geometry();
        let initial_third = *layout.tree().get(&third).unwrap().data().geometry();
        let fixed_right = initial.loc.x.saturating_add(initial.size.w);
        assert_eq!(
            layout.resize_scrolling_column(&second, 100, ColumnResizeAnchor::Right),
            Some(true)
        );
        let right_anchored = *layout.tree().get(&second).unwrap().data().geometry();
        let stacked_peer = *layout.tree().get(&first).unwrap().data().geometry();
        let unrelated = *layout.tree().get(&third).unwrap().data().geometry();
        assert_eq!(
            right_anchored.loc.x.saturating_add(right_anchored.size.w),
            fixed_right
        );
        assert_eq!(right_anchored.loc.x, initial.loc.x - 100);
        assert_eq!(stacked_peer.loc.x, right_anchored.loc.x);
        assert_eq!(stacked_peer.size.w, right_anchored.size.w);
        assert_eq!(unrelated.loc.x, initial_third.loc.x);
        assert_eq!(layout.queue.trees.back().unwrap().1, Duration::ZERO);
        assert!(layout.queue.trees.back().unwrap().2.is_none());
        assert!(layout.scrolling_live_resize.as_ref().is_some_and(|resize| {
            !resize.finishing
                && matches!(
                    &resize.target,
                    ScrollingResizeTarget::Column { tile, .. } if tile == &second
                )
        }));

        let fixed_left = right_anchored.loc.x;
        assert_eq!(
            layout.resize_scrolling_column(&second, -50, ColumnResizeAnchor::Left),
            Some(true)
        );
        let left_anchored = *layout.tree().get(&second).unwrap().data().geometry();
        assert_eq!(left_anchored.loc.x, fixed_left);
        assert_eq!(left_anchored.size.w, right_anchored.size.w - 50);

        for _ in 0..64 {
            assert_eq!(
                layout.resize_scrolling_column(&second, 1, ColumnResizeAnchor::Left),
                Some(true)
            );
            assert_eq!(
                layout.resize_scrolling_column(&second, -1, ColumnResizeAnchor::Left),
                Some(true)
            );
            assert!(layout.queue.trees.len() <= 2);
            assert_eq!(layout.queue.trees.back().unwrap().1, Duration::ZERO);
            assert!(layout.queue.trees.back().unwrap().2.is_none());
        }
        let after_reversals = *layout.tree().get(&second).unwrap().data().geometry();
        assert_eq!(after_reversals, left_anchored);

        layout.finish_scrolling_resize();
        assert_eq!(
            *layout.tree().get(&second).unwrap().data().geometry(),
            left_anchored
        );
        assert_eq!(layout.queue.trees.back().unwrap().1, Duration::ZERO);
        assert!(layout.queue.trees.back().unwrap().2.is_none());
        assert!(
            layout
                .scrolling_live_resize
                .as_ref()
                .is_some_and(|resize| resize.finishing)
        );
        layout.update_animation_state();
        assert!(layout.scrolling_live_resize.is_none());
    }

    #[test]
    fn scrolling_vertical_resize_is_direct_bounded_and_changes_only_the_pair() {
        let geometry = Rectangle::new((0, 0).into(), (1_200, 1_200).into());
        let mut layout = sized_scrolling_layout();
        layout.output.change_current_state(
            Some(Mode {
                size: (1_200, 1_200).into(),
                refresh: 60_000,
            }),
            None,
            None,
            Some((0, 0).into()),
        );
        let mut tree = Tree::new();
        let root = tree
            .insert(
                Node::new(Data::Group {
                    orientation: Orientation::Vertical,
                    sizes: vec![400, 400, 400],
                    last_geometry: geometry,
                    alive: Arc::new(()),
                    pill_indicator: None,
                }),
                InsertBehavior::AsRoot,
            )
            .unwrap();
        let insert_placeholder = |tree: &mut Tree<Data>| {
            tree.insert(
                Node::new(Data::Placeholder {
                    id: Id::new(),
                    last_geometry: geometry,
                    type_: PlaceholderType::GrabbedWindow,
                }),
                InsertBehavior::UnderNode(&root),
            )
            .unwrap()
        };
        let first = insert_placeholder(&mut tree);
        let second = insert_placeholder(&mut tree);
        let third = insert_placeholder(&mut tree);
        let blocker = layout.update_positions_for(&mut tree, layout.gaps());
        layout.queue.trees.clear();
        layout.queue.push_tree(tree, Duration::ZERO, blocker);

        assert_eq!(
            layout
                .scrolling
                .move_tile_vertical(&second, Direction::Down),
            TileMove::Moved
        );
        assert_eq!(
            layout.scrolling.move_tile_vertical(&third, Direction::Down),
            TileMove::Moved
        );
        let mut stacked = layout.tree().copy_clone();
        let blocker = layout.update_positions_for(&mut stacked, layout.gaps());
        layout.queue.trees.clear();
        layout.queue.push_tree(stacked, Duration::ZERO, blocker);
        assert_eq!(
            layout.scrolling.column_tiles(),
            [vec![first.clone(), second.clone(), third.clone()]]
        );

        let initial_upper = *layout.tree().get(&first).unwrap().data().geometry();
        let initial_lower = *layout.tree().get(&second).unwrap().data().geometry();
        let initial_third = *layout.tree().get(&third).unwrap().data().geometry();
        let gap = initial_lower.loc.y - initial_upper.loc.y.saturating_add(initial_upper.size.h);
        let fixed_pair_top = initial_upper.loc.y;
        let fixed_pair_bottom = initial_lower.loc.y.saturating_add(initial_lower.size.h);

        assert_eq!(
            layout.resize_scrolling_rows(&first, &second, 50),
            Some(true)
        );
        let resized_upper = *layout.tree().get(&first).unwrap().data().geometry();
        let resized_lower = *layout.tree().get(&second).unwrap().data().geometry();
        assert_eq!(resized_upper.loc.y, fixed_pair_top);
        assert_eq!(
            resized_lower.loc.y.saturating_add(resized_lower.size.h),
            fixed_pair_bottom
        );
        assert_eq!(
            resized_lower.loc.y - resized_upper.loc.y.saturating_add(resized_upper.size.h),
            gap
        );
        assert_eq!(
            *layout.tree().get(&third).unwrap().data().geometry(),
            initial_third
        );
        assert_eq!(layout.queue.trees.back().unwrap().1, Duration::ZERO);
        assert!(layout.queue.trees.back().unwrap().2.is_none());
        assert!(layout.scrolling_live_resize.as_ref().is_some_and(|resize| {
            !resize.finishing
                && resize.tiles == [first.clone(), second.clone()]
                && matches!(
                    &resize.target,
                    ScrollingResizeTarget::Rows { upper, lower }
                        if upper == &first && lower == &second
                )
        }));

        for _ in 0..64 {
            assert_eq!(layout.resize_scrolling_rows(&first, &second, 1), Some(true));
            assert_eq!(
                layout.resize_scrolling_rows(&first, &second, -1),
                Some(true)
            );
            assert!(layout.queue.trees.len() <= 2);
            assert_eq!(layout.queue.trees.back().unwrap().1, Duration::ZERO);
            assert!(layout.queue.trees.back().unwrap().2.is_none());
        }
        assert_eq!(
            *layout.tree().get(&first).unwrap().data().geometry(),
            resized_upper
        );
        assert_eq!(
            *layout.tree().get(&second).unwrap().data().geometry(),
            resized_lower
        );

        layout.finish_scrolling_resize();
        assert!(
            layout
                .scrolling_live_resize
                .as_ref()
                .is_some_and(|resize| resize.finishing)
        );
        assert!(layout.queue.trees.back().unwrap().2.is_none());
        layout.update_animation_state();
        assert!(layout.scrolling_live_resize.is_none());
        assert_eq!(
            *layout.tree().get(&first).unwrap().data().geometry(),
            resized_upper
        );
        assert_eq!(
            *layout.tree().get(&second).unwrap().data().geometry(),
            resized_lower
        );

        assert_eq!(layout.resize_scrolling_rows(&first, &second, 5), Some(true));
        assert!(layout.scrolling_live_resize.is_some());
        assert!(layout.scrolling.begin_drag(&first));
        layout
            .scrolling
            .finish_drag(Some(&first), ColumnDropTarget::Discard);
        layout.update_scrolling_live_resize_state();
        assert!(layout.scrolling_live_resize.is_none());
    }

    #[test]
    fn single_column_recenters_when_mouse_resize_finishes() {
        let geometry = Rectangle::new((0, 0).into(), (1_200, 800).into());
        let mut layout = sized_scrolling_layout();
        let mut tree = Tree::new();
        let tile = tree
            .insert(
                Node::new(Data::Placeholder {
                    id: Id::new(),
                    last_geometry: geometry,
                    type_: PlaceholderType::GrabbedWindow,
                }),
                InsertBehavior::AsRoot,
            )
            .unwrap();
        let blocker = layout.update_positions_for(&mut tree, layout.gaps());
        layout.queue.trees.clear();
        layout.queue.push_tree(tree, Duration::ZERO, blocker);

        let initial = *layout.tree().get(&tile).unwrap().data().geometry();
        let center = initial.loc.x.saturating_mul(2) + initial.size.w;
        let right = initial.loc.x.saturating_add(initial.size.w);
        assert_eq!(
            layout.resize_scrolling_column(&tile, 100, ColumnResizeAnchor::Right),
            Some(true)
        );
        let anchored = *layout.tree().get(&tile).unwrap().data().geometry();
        assert_eq!(anchored.loc.x.saturating_add(anchored.size.w), right);

        layout.finish_scrolling_resize();
        let centered = *layout.tree().get(&tile).unwrap().data().geometry();
        assert_eq!(centered.size.w, anchored.size.w);
        assert_eq!(centered.loc.x.saturating_mul(2) + centered.size.w, center);
    }

    #[test]
    fn switching_engines_clears_an_active_scrolling_resize() {
        let geometry = Rectangle::new((0, 0).into(), (1_200, 800).into());
        let mut layout = sized_scrolling_layout();
        let mut tree = Tree::new();
        let tile = tree
            .insert(
                Node::new(Data::Placeholder {
                    id: Id::new(),
                    last_geometry: geometry,
                    type_: PlaceholderType::GrabbedWindow,
                }),
                InsertBehavior::AsRoot,
            )
            .unwrap();
        let blocker = layout.update_positions_for(&mut tree, layout.gaps());
        layout.queue.trees.clear();
        layout.queue.push_tree(tree, Duration::ZERO, blocker);

        assert_eq!(
            layout.resize_scrolling_column(&tile, 50, ColumnResizeAnchor::Left),
            Some(true)
        );
        assert!(layout.scrolling_live_resize.is_some());

        layout.set_mode(TilingEngine::Classic);
        assert!(layout.scrolling_live_resize.is_none());
        assert_eq!(layout.mode(), TilingEngine::Classic);
    }

    #[test]
    fn scrolling_keyboard_width_animation_retargets_every_preset() {
        let geometry = Rectangle::new((0, 0).into(), (1_200, 800).into());
        let mut layout = sized_scrolling_layout();
        let mut tree = Tree::new();
        let root = tree
            .insert(
                Node::new(Data::Group {
                    orientation: Orientation::Vertical,
                    sizes: vec![600, 600],
                    last_geometry: geometry,
                    alive: Arc::new(()),
                    pill_indicator: None,
                }),
                InsertBehavior::AsRoot,
            )
            .unwrap();
        let insert_placeholder = |tree: &mut Tree<Data>| {
            tree.insert(
                Node::new(Data::Placeholder {
                    id: Id::new(),
                    last_geometry: geometry,
                    type_: PlaceholderType::GrabbedWindow,
                }),
                InsertBehavior::UnderNode(&root),
            )
            .unwrap()
        };
        let first = insert_placeholder(&mut tree);
        let second = insert_placeholder(&mut tree);
        let third = insert_placeholder(&mut tree);
        let blocker = layout.update_positions_for(&mut tree, layout.gaps());
        layout.queue.trees.clear();
        layout.queue.push_tree(tree, Duration::ZERO, blocker);

        assert_eq!(
            layout
                .scrolling
                .move_tile_vertical(&second, Direction::Down),
            TileMove::Moved
        );
        let mut stacked = layout.tree().copy_clone();
        let blocker = layout.update_positions_for(&mut stacked, layout.gaps());
        layout.queue.trees.clear();
        layout.queue.push_tree(stacked, Duration::ZERO, blocker);
        assert_eq!(
            layout.scrolling.column_tiles(),
            [vec![first.clone(), second.clone()], vec![third]]
        );

        for expected in [1.0, 0.33, 0.5, 0.66] {
            assert!(layout.cycle_scrolling_column_width_for(&second, ResizeDirection::Outwards));
            assert_eq!(layout.scrolling.column_width_fractions()[0], expected);
            assert_eq!(layout.queue.trees.len(), 2);
            assert_eq!(layout.queue.trees.back().unwrap().1, ANIMATION_DURATION);

            let target = &layout.queue.trees.back().unwrap().0;
            let expected_width = layout.scrolling.column_widths()[0];
            assert_eq!(
                target.get(&first).unwrap().data().geometry().size.w,
                expected_width
            );
            assert_eq!(
                target.get(&second).unwrap().data().geometry().size.w,
                expected_width
            );
        }

        for expected in [0.5, 0.33, 1.0, 0.66] {
            assert!(layout.cycle_scrolling_column_width_for(&second, ResizeDirection::Inwards));
            assert_eq!(layout.scrolling.column_width_fractions()[0], expected);
            assert_eq!(layout.queue.trees.len(), 2);
            assert_eq!(layout.queue.trees.back().unwrap().1, ANIMATION_DURATION);
        }

        assert!(layout.cycle_scrolling_column_width_for(&second, ResizeDirection::Outwards));
        layout.queue.animation_start = Some(Instant::now() - ANIMATION_DURATION / 2);
        assert!(layout.cycle_scrolling_column_width_for(&second, ResizeDirection::Inwards));
        assert_eq!(layout.scrolling.column_width_fractions()[0], 0.66);
        assert_eq!(layout.queue.trees.len(), 2);
        let rendered_width = layout
            .queue
            .trees
            .front()
            .unwrap()
            .0
            .get(&second)
            .unwrap()
            .data()
            .geometry()
            .size
            .w;
        let target_width = layout.scrolling.column_widths()[0];
        assert!(rendered_width > target_width);
        assert_eq!(
            layout
                .queue
                .trees
                .back()
                .unwrap()
                .0
                .get(&second)
                .unwrap()
                .data()
                .geometry()
                .size
                .w,
            target_width
        );

        layout.queue.animation_start = Some(Instant::now());
        assert!(layout.pan_scrolling_viewport(100.0));
        assert_eq!(layout.queue.trees.len(), 2);
        assert_eq!(layout.queue.trees.back().unwrap().1, Duration::ZERO);

        assert_eq!(
            layout.resize_scrolling_column(&second, 50, ColumnResizeAnchor::Left),
            Some(true)
        );
        let continuous = layout.scrolling.column_width_fractions()[0];
        assert!(continuous > 0.66 && continuous < 1.0);
        assert_eq!(layout.queue.trees.back().unwrap().1, Duration::ZERO);
        assert!(layout.cycle_scrolling_column_width_for(&second, ResizeDirection::Outwards));
        assert_eq!(layout.scrolling.column_width_fractions()[0], 1.0);
        assert_eq!(layout.queue.trees.len(), 2);
        assert_eq!(layout.queue.trees.back().unwrap().1, ANIMATION_DURATION);

        layout.queue.animation_start = Some(Instant::now());
        assert_eq!(
            layout.resize_scrolling_column(&second, -50, ColumnResizeAnchor::Left),
            Some(true)
        );
        let continuous = layout.scrolling.column_width_fractions()[0];
        assert!(continuous > 0.66 && continuous < 1.0);
        assert_eq!(layout.queue.trees.back().unwrap().1, Duration::ZERO);
        assert!(layout.cycle_scrolling_column_width_for(&second, ResizeDirection::Inwards));
        assert_eq!(layout.scrolling.column_width_fractions()[0], 0.66);
        assert_eq!(layout.queue.trees.len(), 2);
        assert_eq!(layout.queue.trees.back().unwrap().1, ANIMATION_DURATION);
    }

    #[test]
    fn mode_transition_retires_pending_blockers_on_new_geometry() {
        let mut layout = empty_layout(TilingEngine::Classic);
        layout.queue.push_tree(
            Tree::new(),
            ANIMATION_DURATION,
            Some(TilingBlocker::new(std::iter::empty())),
        );

        layout.set_mode(TilingEngine::Scrolling);

        assert_eq!(layout.queue.trees.len(), 2);
        assert!(layout.queue.trees.front().unwrap().2.is_none());
        assert!(layout.queue.trees.back().unwrap().2.is_some());
        assert!(
            layout
                .queue
                .trees
                .iter()
                .all(|(tree, _, _)| tree.root_node_id().is_none())
        );

        layout.update_animation_state();
        assert!(layout.queue.animation_start.is_some());
        layout.update_animation_state();
        assert_eq!(layout.queue.trees.len(), 1);
        assert!(layout.queue.trees.front().unwrap().2.is_none());
    }
}
