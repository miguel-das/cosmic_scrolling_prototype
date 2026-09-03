// SPDX-License-Identifier: GPL-3.0-only

//! Minimal scrolling tiling model.
//!
//! Column order and positions live in virtual strip coordinates. Only the
//! rectangles written into the tiling tree are translated into output-local
//! screen coordinates by subtracting `viewport_x`.

use std::collections::HashMap;

use cosmic_settings_config::shortcuts::action::{Direction, FocusDirection, ResizeDirection};
use id_tree::{NodeId, Tree};
use smithay::{
    desktop::layer_map_for_output,
    output::Output,
    utils::{Point, Rectangle},
    wayland::{compositor::add_blocker, seat::WaylandFocus},
};

use crate::utils::prelude::*;

use super::tiling::{Data, PlaceholderType, TilingBlocker};

const DEFAULT_COLUMN_FRACTION: f64 = 0.66;
const COLUMN_WIDTH_PRESETS: [f64; 4] = [0.33, 0.5, 0.66, 1.0];
const MIN_COLUMN_FRACTION: f64 = 0.33;
const MAX_COLUMN_FRACTION: f64 = 1.0;
const MIN_TILE_HEIGHT: i32 = 240;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum FocusNavigation {
    Target(NodeId),
    Edge,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TileMove {
    Moved,
    Edge,
    Unavailable,
}

/// The horizontal edge which remains fixed while pointer motion resizes a
/// Scrolling column from the opposite edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnResizeAnchor {
    Left,
    Right,
}

impl ColumnResizeAnchor {
    pub(super) fn width_delta(self, pointer_delta_x: f64) -> f64 {
        match self {
            Self::Left => pointer_delta_x,
            Self::Right => -pointer_delta_x,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ColumnDropTarget<T> {
    Original,
    Before(T),
    After(T),
    Above(T),
    Below(T),
    First,
    Last,
    Discard,
}

#[derive(Debug, Clone)]
pub(super) struct ScrollingLayout {
    model: ColumnModel<NodeId>,
    pending_center: Option<NodeId>,
}

impl Default for ScrollingLayout {
    fn default() -> Self {
        Self::new()
    }
}

impl ScrollingLayout {
    pub(super) fn new() -> Self {
        Self {
            model: ColumnModel::default(),
            pending_center: None,
        }
    }

    /// Reconcile the stable column order with the mapped leaves and apply the
    /// scrolling geometry to the tiling tree.
    pub(super) fn update_positions(
        &mut self,
        output: &Output,
        tree: &mut Tree<Data>,
        gaps: (i32, i32),
    ) -> Option<TilingBlocker> {
        self.update_positions_ensuring(output, tree, gaps, None)
    }

    /// Apply scrolling geometry while preferring an explicitly requested focus
    /// target over the currently activated surface. Focus activation is updated
    /// after layout geometry, so the activated surface can briefly be stale.
    pub(super) fn update_positions_ensuring(
        &mut self,
        output: &Output,
        tree: &mut Tree<Data>,
        gaps: (i32, i32),
        requested_focus: Option<&NodeId>,
    ) -> Option<TilingBlocker> {
        self.update_positions_internal(output, tree, gaps, requested_focus, false, None)
    }

    /// Apply geometry without focus visibility or single-tile centering moving
    /// the viewport chosen by a live direct-manipulation grab.
    pub(super) fn update_positions_preserving_viewport(
        &mut self,
        output: &Output,
        tree: &mut Tree<Data>,
        gaps: (i32, i32),
    ) -> Option<TilingBlocker> {
        self.update_positions_internal(output, tree, gaps, None, true, None)
    }

    /// Apply direct pointer-resize geometry immediately while limiting client
    /// configures to the affected tiles. During motion each tile receives a
    /// new configure only after its preceding size was committed; release
    /// always sends the final requested sizes.
    pub(super) fn update_positions_for_live_resize(
        &mut self,
        output: &Output,
        tree: &mut Tree<Data>,
        gaps: (i32, i32),
        resized_tiles: &[NodeId],
        final_configure: bool,
    ) -> Option<TilingBlocker> {
        self.update_positions_internal(
            output,
            tree,
            gaps,
            None,
            true,
            Some((resized_tiles, final_configure)),
        )
    }

    fn update_positions_internal(
        &mut self,
        output: &Output,
        tree: &mut Tree<Data>,
        gaps: (i32, i32),
        requested_focus: Option<&NodeId>,
        preserve_viewport: bool,
        live_resize: Option<(&[NodeId], bool)>,
    ) -> Option<TilingBlocker> {
        let requested_center = self.pending_center.take();
        let preserved_viewport = preserve_viewport.then_some(self.model.viewport_x);
        let Some(root_id) = tree.root_node_id().cloned() else {
            self.model.reconcile(std::iter::empty::<NodeId>());
            return None;
        };

        let node_ids = tree
            .traverse_pre_order_ids(&root_id)
            .expect("root node must be traversable")
            .collect::<Vec<_>>();
        let focused_id = node_ids
            .iter()
            .find(|node_id| {
                matches!(
                    tree.get(node_id).map(|node| node.data()),
                    Ok(Data::Mapped { mapped, .. }) if mapped.is_activated(false)
                )
            })
            .cloned();
        let tile_ids = node_ids
            .iter()
            .filter(|node_id| {
                matches!(
                    tree.get(node_id).map(|node| node.data()),
                    Ok(Data::Mapped { .. })
                        | Ok(Data::Placeholder {
                            type_: PlaceholderType::GrabbedWindow,
                            ..
                        })
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        self.model.reconcile(tile_ids);

        let is_live_resize = live_resize.is_some();

        let usable = layer_map_for_output(output).non_exclusive_zone().as_local();
        let viewport = scrolling_viewport(usable, gaps);
        let inner = gaps.1.max(0);
        let viewport_width = viewport.size.w;
        let column_height = viewport.size.h;
        let origin = (viewport.loc.x, viewport.loc.y);

        self.model.set_metrics(viewport_width, inner);
        self.model.set_column_height(column_height);
        if let Some(viewport_x) = preserved_viewport.filter(|viewport_x| viewport_x.is_finite()) {
            if !self.model.columns.is_empty() {
                self.model.viewport_x = viewport_x;
            }
        } else {
            self.model
                .ensure_preferred_visible(requested_focus, focused_id.as_ref());
            if let Some(current) = requested_center
                .as_ref()
                .filter(|current| self.model.contains(current))
            {
                self.model.center_column(current);
            } else {
                self.model.center_only_tile();
            }
        }

        let mut geometries = HashMap::new();
        let mut configures = Vec::new();

        for (index, column) in self.model.columns.iter().enumerate() {
            let x =
                f64_to_i32(origin.0 as f64 + self.model.layout_x(index) - self.model.viewport_x);
            let column_width = self.model.column_width(index);
            let weights = column
                .tiles
                .iter()
                .map(|tile| self.model.tile_weight(tile))
                .collect::<Vec<_>>();
            let rows = weighted_vertical_tile_rows(column_height, inner, &weights);

            for (node_id, (offset_y, tile_height)) in column.tiles.iter().zip(rows) {
                let geometry: Rectangle<i32, Local> = Rectangle::new(
                    (x, origin.1.saturating_add(offset_y)).into(),
                    (column_width, tile_height).into(),
                );
                let Ok(node) = tree.get_mut(node_id) else {
                    continue;
                };
                match node.data_mut() {
                    Data::Mapped {
                        mapped,
                        last_geometry,
                        ..
                    } => {
                        let size_changed = last_geometry.size != geometry.size;
                        *last_geometry = geometry;
                        geometries.insert(node_id.clone(), geometry);

                        if !(mapped.is_fullscreen(true) || mapped.is_maximized(true)) {
                            mapped.set_tiled(true);
                            mapped.set_geometry(geometry.to_global(output));
                            let should_configure = if is_live_resize {
                                should_configure_live_resize(
                                    live_resize.is_some_and(|(resized_tiles, _)| {
                                        resized_tiles.contains(node_id)
                                    }),
                                    live_resize.is_some_and(|(_, final_configure)| final_configure),
                                    size_changed,
                                    mapped.active_window().latest_size_committed(),
                                )
                            } else {
                                true
                            };
                            if should_configure && let Some(serial) = mapped.configure() {
                                configures.push((mapped.active_window(), serial));
                            }
                        }
                    }
                    Data::Placeholder {
                        last_geometry,
                        type_: PlaceholderType::GrabbedWindow,
                        ..
                    } => {
                        *last_geometry = geometry;
                        geometries.insert(node_id.clone(), geometry);
                    }
                    _ => {}
                }
            }
        }

        update_group_bounds(tree, &node_ids, &mut geometries);

        if configures.is_empty() {
            return None;
        }

        let blocker = TilingBlocker::new(configures);
        for (surface, _) in &blocker.necessary_acks {
            if let Some(surface) = surface.wl_surface() {
                add_blocker(&surface, blocker.clone());
            }
        }
        Some(blocker)
    }

    pub(super) fn tiles_in_column(&self, tile: &NodeId) -> Option<Vec<NodeId>> {
        self.model
            .tile_position(tile)
            .map(|(index, _)| self.model.columns[index].tiles.clone())
    }

    pub(super) fn next_focus(
        &self,
        tree: &Tree<Data>,
        current: &NodeId,
        direction: FocusDirection,
    ) -> FocusNavigation {
        let target = match direction {
            FocusDirection::Left => self.model.adjacent_column(current, -1),
            FocusDirection::Right => self.model.adjacent_column(current, 1),
            FocusDirection::Up => self.model.adjacent_tile(current, -1),
            FocusDirection::Down => self.model.adjacent_tile(current, 1),
            _ => return FocusNavigation::Unavailable,
        };

        let Some(target) = target.cloned() else {
            return if self.model.contains(current) {
                FocusNavigation::Edge
            } else {
                FocusNavigation::Unavailable
            };
        };

        if matches!(
            tree.get(&target).map(|node| node.data()),
            Ok(Data::Mapped { .. })
        ) {
            FocusNavigation::Target(target)
        } else {
            FocusNavigation::Unavailable
        }
    }

    /// Mark a column focused and move the viewport just enough to reveal it.
    /// Returns whether the viewport changed.
    pub(super) fn ensure_visible(&mut self, current: &NodeId) -> bool {
        self.model.ensure_visible(current)
    }

    /// Directly pan the horizontal strip without changing focus or model order.
    pub(super) fn pan_viewport(&mut self, delta_x: f64) -> bool {
        self.model.pan_viewport(delta_x)
    }

    /// Recover the virtual viewport represented by an interrupted visual tree.
    /// Prefer the focused tile because it is the stable anchor during keyboard
    /// movement; fall back to the first modeled tile still present.
    pub(super) fn sync_viewport_from_tree(
        &mut self,
        output: &Output,
        tree: &Tree<Data>,
        gaps: (i32, i32),
    ) -> bool {
        let usable = layer_map_for_output(output).non_exclusive_zone().as_local();
        let origin_x = scrolling_viewport(usable, gaps).loc.x;
        let focused = self.model.focused.clone();
        let candidates = focused.into_iter().chain(
            self.model
                .columns
                .iter()
                .flat_map(|column| column.tiles.iter().cloned()),
        );
        for tile in candidates {
            let Some((column, _)) = self.model.tile_position(&tile) else {
                continue;
            };
            let Some(geometry) = tile_geometry(tree, &tile) else {
                continue;
            };
            let viewport_x =
                f64::from(origin_x) + self.model.layout_x(column) - f64::from(geometry.loc.x);
            if viewport_x.is_finite() {
                self.model.viewport_x = viewport_x;
                return true;
            }
        }
        false
    }

    /// Request exact horizontal centering during the next geometry update.
    pub(super) fn request_center(&mut self, current: &NodeId) -> bool {
        if !self.model.centering_changes_viewport(current) {
            return false;
        }
        self.pending_center = Some(current.clone());
        true
    }

    pub(super) fn begin_drag(&mut self, current: &NodeId) -> bool {
        self.model.begin_drag(current)
    }

    pub(super) fn has_pending_drag(&self) -> bool {
        self.model.pending_drag.is_some()
    }

    pub(super) fn finish_drag(
        &mut self,
        final_tile: Option<&NodeId>,
        target: ColumnDropTarget<NodeId>,
    ) {
        self.model.finish_drag(final_tile, target);
    }

    pub(super) fn cancel_drag(&mut self) {
        self.model.cancel_drag();
    }

    /// Move horizontally in the stable strip. A tile in a multi-tile column is
    /// extracted first; an independent tile moves together with its column.
    pub(super) fn move_column(&mut self, current: &NodeId, direction: Direction) -> bool {
        let delta = match direction {
            Direction::Left => -1,
            Direction::Right => 1,
            Direction::Up | Direction::Down => return false,
        };

        if !self.model.move_by(current, delta) {
            return false;
        }
        self.model.ensure_visible(current);
        true
    }

    pub(super) fn move_tile_vertical(
        &mut self,
        current: &NodeId,
        direction: Direction,
    ) -> TileMove {
        let delta = match direction {
            Direction::Up => -1,
            Direction::Down => 1,
            Direction::Left | Direction::Right => return TileMove::Unavailable,
        };
        let result = self.model.move_tile_vertical(current, delta);
        if result == TileMove::Moved {
            self.model.ensure_visible(current);
        }
        result
    }

    pub(super) fn cycle_column_width(
        &mut self,
        current: &NodeId,
        direction: ResizeDirection,
    ) -> bool {
        if !self.model.cycle_column_width(current, direction) {
            return false;
        }
        self.model.ensure_visible(current);
        true
    }

    pub(super) fn contains(&self, current: &NodeId) -> bool {
        self.model.contains(current)
    }

    /// Place a genuinely new tiled toplevel in its own column immediately
    /// after the focused column. Generic reconciliation deliberately does not
    /// use this policy because unknown identities can also be restored tiles,
    /// drag replacements, or leaves first observed after an engine switch.
    pub(super) fn insert_new_toplevel(&mut self, tile: NodeId, focused: Option<&NodeId>) {
        self.model.insert_new_toplevel(tile, focused);
    }

    pub(super) fn resize_column_by_pixels(
        &mut self,
        current: &NodeId,
        delta: i32,
        anchor: ColumnResizeAnchor,
    ) -> Option<bool> {
        self.model
            .resize_column_by_pixels(current, delta, anchor)
            .map(|(changed, _)| changed)
    }

    pub(super) fn finish_column_resize(&mut self) {
        self.model.center_only_tile();
    }

    pub(super) fn row_pair(&self, current: &NodeId, delta: isize) -> Option<(NodeId, NodeId)> {
        self.model.row_pair(current, delta)
    }

    pub(super) fn resize_rows_by_pixels(
        &mut self,
        upper: &NodeId,
        lower: &NodeId,
        delta: i32,
    ) -> Option<bool> {
        self.model.resize_rows_by_pixels(upper, lower, delta)
    }

    /// Resolve a horizontal tile separator adjacent to the focused tile. The
    /// pointer must also be horizontally inside that focused column.
    pub(super) fn row_resize_edge_at(
        &self,
        tree: &Tree<Data>,
        current: &NodeId,
        location: Point<f64, Local>,
    ) -> Option<(NodeId, NodeId, i32)> {
        let (column, tile) = self.model.tile_position(current)?;
        let current_geometry = tile_geometry(tree, current)?;
        let x = location.x.floor() as i32;
        if x < current_geometry.loc.x
            || x >= current_geometry
                .loc
                .x
                .saturating_add(current_geometry.size.w)
        {
            return None;
        }

        let tolerance = f64::from(self.model.gap.max(1).saturating_add(1));
        let mut candidates = Vec::with_capacity(2);
        if tile > 0 {
            let upper = self.model.columns[column].tiles[tile - 1].clone();
            let upper_geometry = tile_geometry(tree, &upper)?;
            let boundary = upper_geometry.loc.y.saturating_add(upper_geometry.size.h);
            candidates.push((upper, current.clone(), boundary));
        }
        if tile + 1 < self.model.columns[column].tiles.len() {
            let lower = self.model.columns[column].tiles[tile + 1].clone();
            let boundary = current_geometry
                .loc
                .y
                .saturating_add(current_geometry.size.h);
            candidates.push((current.clone(), lower, boundary));
        }

        candidates
            .into_iter()
            .filter(|(_, _, boundary)| (f64::from(*boundary) - location.y).abs() <= tolerance)
            .min_by_key(|(_, _, boundary)| (f64::from(*boundary) - location.y).abs().round() as i64)
    }

    /// Resolve a visible separator adjacent to `current`.
    ///
    /// The returned anchor identifies the opposite edge which must remain
    /// stable while the pointer drags the selected boundary. Separators
    /// unrelated to the focused column are deliberately rejected.
    pub(super) fn resize_edge_at(
        &self,
        tree: &Tree<Data>,
        current: &NodeId,
        x: f64,
    ) -> Option<(ColumnResizeAnchor, i32)> {
        let (column, _) = self.model.tile_position(current)?;
        let geometry_for = |column: usize| {
            self.model
                .columns
                .get(column)?
                .tiles
                .iter()
                .find_map(|tile| {
                    let geometry = match tree.get(tile).ok()?.data() {
                        Data::Mapped { last_geometry, .. }
                        | Data::Placeholder { last_geometry, .. } => last_geometry,
                        Data::Group { .. } => return None,
                    };
                    Some(*geometry)
                })
        };
        let tolerance = f64::from(self.model.gap.max(1).saturating_add(1));
        let mut candidates = Vec::with_capacity(2);

        if let Some(previous) = column.checked_sub(1).and_then(geometry_for) {
            candidates.push((
                ColumnResizeAnchor::Right,
                previous.loc.x.saturating_add(previous.size.w),
            ));
        }
        if let Some(current) = geometry_for(column) {
            candidates.push((
                ColumnResizeAnchor::Left,
                current.loc.x.saturating_add(current.size.w),
            ));
        }

        candidates
            .into_iter()
            .filter(|(_, boundary)| (f64::from(*boundary) - x).abs() <= tolerance)
            .min_by_key(|(_, boundary)| (f64::from(*boundary) - x).abs().round() as i64)
    }

    #[cfg(test)]
    pub(super) fn column_order(&self) -> Vec<NodeId> {
        self.model
            .columns
            .iter()
            .filter_map(|column| column.tiles.first().cloned())
            .collect()
    }

    #[cfg(test)]
    pub(super) fn column_tiles(&self) -> Vec<Vec<NodeId>> {
        self.model
            .columns
            .iter()
            .map(|column| column.tiles.clone())
            .collect()
    }

    #[cfg(test)]
    pub(super) fn column_width_fractions(&self) -> Vec<f64> {
        self.model
            .columns
            .iter()
            .map(|column| column.width.fraction)
            .collect()
    }

    #[cfg(test)]
    pub(super) fn column_widths(&self) -> Vec<i32> {
        (0..self.model.columns.len())
            .map(|column| self.model.column_width(column))
            .collect()
    }
}

fn scrolling_viewport(usable: Rectangle<i32, Local>, gaps: (i32, i32)) -> Rectangle<i32, Local> {
    let (outer, inner) = (gaps.0.max(0), gaps.1.max(0));
    let boundary = outer.saturating_add(inner);

    Rectangle::new(
        (
            usable.loc.x.saturating_add(boundary),
            usable.loc.y.saturating_add(boundary),
        )
            .into(),
        (
            usable
                .size
                .w
                .saturating_sub(boundary.saturating_mul(2))
                .max(1),
            usable
                .size
                .h
                .saturating_sub(boundary.saturating_mul(2))
                .max(1),
        )
            .into(),
    )
}

fn update_group_bounds(
    tree: &mut Tree<Data>,
    node_ids: &[NodeId],
    geometries: &mut HashMap<NodeId, Rectangle<i32, Local>>,
) {
    for node_id in node_ids.iter().rev() {
        let children = match tree.get(node_id) {
            Ok(node) if matches!(node.data(), Data::Group { .. }) => node.children().to_vec(),
            _ => continue,
        };

        let Some(bounds) = children
            .iter()
            .filter_map(|child| geometries.get(child).copied())
            .reduce(|bounds, geometry| bounds.merge(geometry))
        else {
            continue;
        };

        if let Ok(node) = tree.get_mut(node_id)
            && matches!(node.data(), Data::Group { .. })
        {
            node.data_mut().update_geometry(bounds);
            geometries.insert(node_id.clone(), bounds);
        }
    }
}

fn f64_to_i32(value: f64) -> i32 {
    value.round().clamp(i32::MIN as f64, i32::MAX as f64) as i32
}

fn tile_geometry(tree: &Tree<Data>, tile: &NodeId) -> Option<Rectangle<i32, Local>> {
    match tree.get(tile).ok()?.data() {
        Data::Mapped { last_geometry, .. } | Data::Placeholder { last_geometry, .. } => {
            Some(*last_geometry)
        }
        Data::Group { .. } => None,
    }
}

fn should_configure_live_resize(
    is_resized_tile: bool,
    final_configure: bool,
    size_changed: bool,
    latest_size_committed: bool,
) -> bool {
    is_resized_tile && (final_configure || (size_changed && latest_size_committed))
}

#[cfg(test)]
fn vertical_tile_rows(column_height: i32, gap: i32, tile_count: usize) -> Vec<(i32, i32)> {
    weighted_vertical_tile_rows(column_height, gap, &vec![1.0; tile_count])
}

fn weighted_vertical_tile_rows(column_height: i32, gap: i32, weights: &[f64]) -> Vec<(i32, i32)> {
    if weights.is_empty() {
        return Vec::new();
    }

    let height = i64::from(column_height.max(0));
    let gap_count = i64::try_from(weights.len().saturating_sub(1)).unwrap_or(i64::MAX);
    let requested_gap = i64::from(gap.max(0));
    let effective_gap = if gap_count == 0 {
        0
    } else {
        requested_gap.min(height / gap_count)
    };
    let available = height.saturating_sub(effective_gap.saturating_mul(gap_count));
    let valid_weights = weights
        .iter()
        .map(|weight| {
            if weight.is_finite() && *weight > 0.0 {
                *weight
            } else {
                1.0
            }
        })
        .collect::<Vec<_>>();
    let total = valid_weights.iter().sum::<f64>().max(f64::EPSILON);
    let mut heights = valid_weights
        .iter()
        .map(|weight| ((available as f64 * *weight / total).floor() as i64).clamp(0, available))
        .collect::<Vec<_>>();
    let assigned = heights.iter().fold(0_i64, |sum, height| {
        sum.saturating_add(*height).min(available)
    });
    let remainder = available.saturating_sub(assigned);
    for index in 0..usize::try_from(remainder)
        .unwrap_or(usize::MAX)
        .min(heights.len())
    {
        heights[index] = heights[index].saturating_add(1);
    }
    let mut y = 0_i64;

    heights
        .into_iter()
        .map(|tile_height| {
            let row = (y as i32, tile_height as i32);
            y = y.saturating_add(tile_height).saturating_add(effective_gap);
            row
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ColumnWidth {
    fraction: f64,
}

impl ColumnWidth {
    fn new(fraction: f64) -> Self {
        debug_assert!(fraction.is_finite() && fraction > 0.0);
        Self { fraction }
    }

    fn pixels(self, viewport_width: i32) -> i32 {
        f64_to_i32(viewport_width.max(1) as f64 * self.fraction).max(1)
    }
}

impl Default for ColumnWidth {
    fn default() -> Self {
        Self::new(DEFAULT_COLUMN_FRACTION)
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Column<T> {
    /// Tiles are kept in their stable vertical order. This phase only creates
    /// single-tile columns, but reconciliation preserves a modeled group.
    tiles: Vec<T>,
    width: ColumnWidth,
}

#[derive(Debug, Clone)]
struct PendingDrag<T> {
    source: T,
    source_column: usize,
    source_tile: usize,
    source_width: ColumnWidth,
    source_tiles: Vec<T>,
    source_weight: f64,
    focused: Option<T>,
    viewport_x: f64,
    user_positioned_viewport: bool,
}

impl<T> Column<T> {
    fn single(tile: T) -> Self {
        Self {
            tiles: vec![tile],
            width: ColumnWidth::default(),
        }
    }
}

/// Persistent scrolling-strip state.
///
/// After reconciliation, every tile occurs at most once, columns are non-empty,
/// existing column and tile order is stable, and unknown tiles have appended as
/// deterministic single-tile columns. Width belongs to the column, while focus
/// belongs to a tile identity rather than to a position in either vector.
#[derive(Debug, Clone)]
struct ColumnModel<T> {
    columns: Vec<Column<T>>,
    tile_weights: Vec<(T, f64)>,
    focused: Option<T>,
    pending_drag: Option<PendingDrag<T>>,
    viewport_x: f64,
    /// A touchpad pan is authoritative until an explicit focus/layout action
    /// requests visibility again. Ordinary reconciliation must not snap back
    /// to the activated tile while this is set.
    user_positioned_viewport: bool,
    viewport_width: i32,
    gap: i32,
    column_height: i32,
}

impl<T> Default for ColumnModel<T> {
    fn default() -> Self {
        Self {
            columns: Vec::new(),
            tile_weights: Vec::new(),
            focused: None,
            pending_drag: None,
            viewport_x: 0.0,
            user_positioned_viewport: false,
            viewport_width: 0,
            gap: 0,
            column_height: 0,
        }
    }
}

impl<T> ColumnModel<T>
where
    T: Clone + Eq,
{
    fn reconcile(&mut self, mapped: impl IntoIterator<Item = T>) {
        let mut mapped_tiles = Vec::new();
        for tile in mapped {
            if !mapped_tiles.contains(&tile) {
                mapped_tiles.push(tile);
            }
        }

        // Retaining in model order preserves both surviving column order and
        // surviving tile order. `retained` also repairs any accidental duplicate
        // by keeping only its first modeled occurrence.
        let mut retained = Vec::new();
        for column in &mut self.columns {
            column.tiles.retain(|tile| {
                if !mapped_tiles.contains(tile) || retained.contains(tile) {
                    false
                } else {
                    retained.push(tile.clone());
                    true
                }
            });
        }
        self.columns.retain(|column| !column.tiles.is_empty());
        let mut weighted = Vec::new();
        self.tile_weights.retain(|(tile, _)| {
            if retained.contains(tile) && !weighted.contains(tile) {
                weighted.push(tile.clone());
                true
            } else {
                false
            }
        });
        for tile in &retained {
            if !self.tile_weights.iter().any(|(known, _)| known == tile) {
                self.tile_weights.push((tile.clone(), 1.0));
            }
        }

        // Clearing focus when its tile disappears matches the old flat model.
        // The compositor's requested or activated tile establishes the next
        // focus without reconciliation choosing a surprising neighbor.
        if self
            .focused
            .as_ref()
            .is_some_and(|focused| !retained.contains(focused))
        {
            self.focused = None;
        }

        for tile in mapped_tiles {
            if !retained.contains(&tile) {
                retained.push(tile.clone());
                self.tile_weights.push((tile.clone(), 1.0));
                self.columns.push(Column::single(tile));
            }
        }

        if self.columns.is_empty() {
            self.focused = None;
            self.tile_weights.clear();
            self.viewport_x = 0.0;
            self.user_positioned_viewport = false;
        } else {
            self.clamp_viewport();
        }
    }

    /// Insert a confirmed new toplevel without making tree preorder
    /// authoritative for ordinary reconciliation.
    fn insert_new_toplevel(&mut self, tile: T, focused: Option<&T>) {
        // Be defensive if a caller repeats notification for the same identity:
        // each tile must still belong to exactly one column.
        self.remove_tile(&tile);

        let insertion = focused
            .and_then(|focused| self.tile_position(focused))
            .map(|(column, _)| column + 1)
            .unwrap_or(self.columns.len());
        self.columns.insert(insertion, Column::single(tile.clone()));
        self.set_tile_weight(tile, 1.0);
        self.clamp_viewport();
    }

    /// Snapshot one tile and its source position before pointer previews
    /// temporarily remove its grabbed-window placeholder from the tiling tree.
    fn begin_drag(&mut self, current: &T) -> bool {
        if self.pending_drag.is_some() {
            return false;
        }
        let Some(pending_drag) = self.drag_snapshot(current) else {
            return false;
        };

        self.pending_drag = Some(pending_drag);
        true
    }

    /// Commit a deliberate pointer drop without making ordinary tree
    /// reconciliation authoritative for scrolling order.
    fn finish_drag(&mut self, final_tile: Option<&T>, target: ColumnDropTarget<T>) {
        let pending = self.pending_drag.take();

        if target == ColumnDropTarget::Discard {
            if let Some(pending) = pending {
                self.remove_tile(&pending.source);
            }
            self.focused = final_tile.filter(|tile| self.contains(tile)).cloned();
            self.clamp_viewport();
            return;
        }

        let Some(final_tile) = final_tile.cloned() else {
            if let Some(pending) = pending {
                self.pending_drag = Some(pending);
                self.cancel_drag();
            }
            return;
        };

        let drag = if let Some(pending) = pending {
            pending
        } else if let Some(snapshot) = self.drag_snapshot(&final_tile) {
            snapshot
        } else {
            PendingDrag {
                source: final_tile.clone(),
                source_column: self.columns.len(),
                source_tile: 0,
                source_width: ColumnWidth::default(),
                source_tiles: vec![final_tile.clone()],
                source_weight: 1.0,
                focused: self.focused.clone(),
                viewport_x: self.viewport_x,
                user_positioned_viewport: self.user_positioned_viewport,
            }
        };

        self.remove_tile(&drag.source);
        if final_tile != drag.source {
            self.remove_tile(&final_tile);
        }

        match target {
            ColumnDropTarget::Original => {
                self.restore_dragged_tile(&drag, final_tile.clone());
            }
            ColumnDropTarget::Above(anchor) => {
                if let Some((column, tile)) = self.tile_position(&anchor) {
                    let weight = if drag.source_tiles.contains(&anchor) {
                        drag.source_weight
                    } else {
                        self.mean_column_weight(column)
                    };
                    self.columns[column].tiles.insert(tile, final_tile.clone());
                    self.set_tile_weight(final_tile.clone(), weight);
                } else {
                    self.restore_dragged_tile(&drag, final_tile.clone());
                }
            }
            ColumnDropTarget::Below(anchor) => {
                if let Some((column, tile)) = self.tile_position(&anchor) {
                    let weight = if drag.source_tiles.contains(&anchor) {
                        drag.source_weight
                    } else {
                        self.mean_column_weight(column)
                    };
                    self.columns[column]
                        .tiles
                        .insert(tile + 1, final_tile.clone());
                    self.set_tile_weight(final_tile.clone(), weight);
                } else {
                    self.restore_dragged_tile(&drag, final_tile.clone());
                }
            }
            ColumnDropTarget::Before(anchor) => {
                let fallback = drag.source_column.min(self.columns.len());
                let insertion = self
                    .tile_position(&anchor)
                    .map(|(column, _)| column)
                    .unwrap_or(fallback);
                let width = if drag.source_tiles.len() == 1 {
                    drag.source_width
                } else {
                    ColumnWidth::default()
                };
                self.columns.insert(
                    insertion,
                    Column {
                        tiles: vec![final_tile.clone()],
                        width,
                    },
                );
                self.set_tile_weight(final_tile.clone(), 1.0);
            }
            ColumnDropTarget::After(anchor) => {
                let fallback = drag.source_column.min(self.columns.len());
                let insertion = self
                    .tile_position(&anchor)
                    .map(|(column, _)| column + 1)
                    .unwrap_or(fallback);
                let width = if drag.source_tiles.len() == 1 {
                    drag.source_width
                } else {
                    ColumnWidth::default()
                };
                self.columns.insert(
                    insertion,
                    Column {
                        tiles: vec![final_tile.clone()],
                        width,
                    },
                );
                self.set_tile_weight(final_tile.clone(), 1.0);
            }
            ColumnDropTarget::First => {
                let width = if drag.source_tiles.len() == 1 {
                    drag.source_width
                } else {
                    ColumnWidth::default()
                };
                self.columns.insert(
                    0,
                    Column {
                        tiles: vec![final_tile.clone()],
                        width,
                    },
                );
                self.set_tile_weight(final_tile.clone(), 1.0);
            }
            ColumnDropTarget::Last => {
                let width = if drag.source_tiles.len() == 1 {
                    drag.source_width
                } else {
                    ColumnWidth::default()
                };
                self.columns.push(Column {
                    tiles: vec![final_tile.clone()],
                    width,
                });
                self.set_tile_weight(final_tile.clone(), 1.0);
            }
            ColumnDropTarget::Discard => unreachable!(),
        }
        self.focused = Some(final_tile.clone());
        self.ensure_visible(&final_tile);
    }

    fn cancel_drag(&mut self) {
        if let Some(pending) = self.pending_drag.take() {
            self.remove_tile(&pending.source);
            self.restore_dragged_tile(&pending, pending.source.clone());
            self.focused = pending.focused.filter(|tile| self.contains(tile));
            self.viewport_x = pending.viewport_x;
            self.user_positioned_viewport = pending.user_positioned_viewport;
            self.clamp_viewport();
        }
    }

    fn drag_snapshot(&self, current: &T) -> Option<PendingDrag<T>> {
        let (source_column, source_tile) = self.tile_position(current)?;
        let column = &self.columns[source_column];
        Some(PendingDrag {
            source: current.clone(),
            source_column,
            source_tile,
            source_width: column.width,
            source_tiles: column.tiles.clone(),
            source_weight: self.tile_weight(current),
            focused: self.focused.clone(),
            viewport_x: self.viewport_x,
            user_positioned_viewport: self.user_positioned_viewport,
        })
    }

    fn restore_dragged_tile(&mut self, pending: &PendingDrag<T>, tile: T) {
        let source_column = pending
            .source_tiles
            .iter()
            .filter(|peer| *peer != &pending.source)
            .find_map(|peer| self.tile_position(peer).map(|(column, _)| column));

        if let Some(column) = source_column {
            self.columns[column].width = pending.source_width;
            let tile_index = pending.source_tile.min(self.columns[column].tiles.len());
            self.columns[column].tiles.insert(tile_index, tile.clone());
        } else {
            let column_index = pending.source_column.min(self.columns.len());
            self.columns.insert(
                column_index,
                Column {
                    tiles: vec![tile.clone()],
                    width: pending.source_width,
                },
            );
        }
        self.set_tile_weight(tile, pending.source_weight);
    }

    fn remove_tile(&mut self, tile: &T) -> bool {
        let Some((column, tile_index)) = self.tile_position(tile) else {
            return false;
        };
        self.columns[column].tiles.remove(tile_index);
        self.tile_weights.retain(|(known, _)| known != tile);
        if self.columns[column].tiles.is_empty() {
            self.columns.remove(column);
        }
        true
    }

    fn set_metrics(&mut self, viewport_width: i32, gap: i32) {
        self.viewport_width = viewport_width.max(1);
        self.gap = gap.max(0);
        self.clamp_viewport();

        if !self.user_positioned_viewport
            && let Some(focused) = self.focused.clone()
        {
            self.ensure_visible(&focused);
        }
    }

    fn set_column_height(&mut self, column_height: i32) {
        self.column_height = column_height.max(0);
    }

    fn tile_weight(&self, tile: &T) -> f64 {
        self.tile_weights
            .iter()
            .find_map(|(known, weight)| (known == tile).then_some(*weight))
            .filter(|weight| weight.is_finite() && *weight > 0.0)
            .unwrap_or(1.0)
    }

    fn mean_column_weight(&self, column: usize) -> f64 {
        let Some(column) = self.columns.get(column) else {
            return 1.0;
        };
        if column.tiles.is_empty() {
            return 1.0;
        }
        column
            .tiles
            .iter()
            .map(|tile| self.tile_weight(tile))
            .sum::<f64>()
            / column.tiles.len() as f64
    }

    fn set_tile_weight(&mut self, tile: T, weight: f64) {
        let weight = if weight.is_finite() && weight > 0.0 {
            weight
        } else {
            1.0
        };
        if let Some((_, current)) = self
            .tile_weights
            .iter_mut()
            .find(|(known, _)| known == &tile)
        {
            *current = weight;
        } else {
            self.tile_weights.push((tile, weight));
        }
    }

    fn contains(&self, current: &T) -> bool {
        self.columns
            .iter()
            .any(|column| column.tiles.contains(current))
    }

    fn tile_position(&self, current: &T) -> Option<(usize, usize)> {
        self.columns.iter().enumerate().find_map(|(column, data)| {
            data.tiles
                .iter()
                .position(|tile| tile == current)
                .map(|tile| (column, tile))
        })
    }

    fn adjacent_column(&self, current: &T, delta: isize) -> Option<&T> {
        let (column, tile) = self.tile_position(current)?;
        let target = column.checked_add_signed(delta)?;
        let target = self.columns.get(target)?;

        // Matching the vertical slot is useful once columns can contain more
        // than one tile; single-tile columns retain today's navigation exactly.
        target.tiles.get(tile).or_else(|| target.tiles.last())
    }

    fn adjacent_tile(&self, current: &T, delta: isize) -> Option<&T> {
        let (column, tile) = self.tile_position(current)?;
        let target = tile.checked_add_signed(delta)?;
        self.columns[column].tiles.get(target)
    }

    fn row_pair(&self, current: &T, delta: isize) -> Option<(T, T)> {
        let (column, tile) = self.tile_position(current)?;
        match delta {
            -1 => Some((
                self.columns[column]
                    .tiles
                    .get(tile.checked_sub(1)?)?
                    .clone(),
                current.clone(),
            )),
            1 => Some((
                current.clone(),
                self.columns[column].tiles.get(tile + 1)?.clone(),
            )),
            _ => None,
        }
    }

    fn move_by(&mut self, current: &T, delta: isize) -> bool {
        let Some((column, tile)) = self.tile_position(current) else {
            return false;
        };

        if self.columns[column].tiles.len() > 1 {
            let tile = self.columns[column].tiles.remove(tile);
            let insertion = if delta < 0 { column } else { column + 1 };
            self.columns.insert(insertion, Column::single(tile.clone()));
            self.set_tile_weight(tile, 1.0);
            self.focused = Some(current.clone());
            return true;
        }

        let Some(target) = column.checked_add_signed(delta) else {
            return false;
        };
        if target >= self.columns.len() {
            return false;
        }

        self.columns.swap(column, target);
        self.focused = Some(current.clone());
        true
    }

    fn move_tile_vertical(&mut self, current: &T, delta: isize) -> TileMove {
        let Some((column, tile)) = self.tile_position(current) else {
            return TileMove::Unavailable;
        };

        if self.columns[column].tiles.len() > 1 {
            let Some(target) = tile.checked_add_signed(delta) else {
                return TileMove::Edge;
            };
            if target >= self.columns[column].tiles.len() {
                return TileMove::Edge;
            }

            self.columns[column].tiles.swap(tile, target);
            self.focused = Some(current.clone());
            return TileMove::Moved;
        }

        let destination = if column > 0 {
            column - 1
        } else if self.columns.len() > 1 {
            1
        } else {
            return TileMove::Edge;
        };
        let source = self.columns.remove(column);
        let destination = if destination > column {
            destination - 1
        } else {
            destination
        };
        let insertion = if delta < 0 {
            0
        } else {
            self.columns[destination].tiles.len()
        };
        let weight = self.mean_column_weight(destination);
        let tile = source.tiles.into_iter().next().unwrap();
        self.columns[destination]
            .tiles
            .insert(insertion, tile.clone());
        self.set_tile_weight(tile, weight);
        self.focused = Some(current.clone());
        TileMove::Moved
    }

    fn cycle_column_width(&mut self, current: &T, direction: ResizeDirection) -> bool {
        let Some((column, _)) = self.tile_position(current) else {
            return false;
        };
        let current_fraction = self.columns[column].width.fraction;
        let next = match direction {
            ResizeDirection::Outwards => COLUMN_WIDTH_PRESETS
                .iter()
                .copied()
                .find(|preset| *preset > current_fraction + f64::EPSILON)
                .unwrap_or(COLUMN_WIDTH_PRESETS[0]),
            ResizeDirection::Inwards => COLUMN_WIDTH_PRESETS
                .iter()
                .rev()
                .copied()
                .find(|preset| *preset < current_fraction - f64::EPSILON)
                .unwrap_or(*COLUMN_WIDTH_PRESETS.last().unwrap()),
        };
        self.columns[column].width = ColumnWidth::new(next);
        self.focused = Some(current.clone());
        true
    }

    fn resize_column_by_pixels(
        &mut self,
        current: &T,
        delta: i32,
        anchor: ColumnResizeAnchor,
    ) -> Option<(bool, i32)> {
        let (column, _) = self.tile_position(current)?;
        if self.viewport_width <= 0 {
            return None;
        }

        let old_fraction = self.columns[column].width.fraction;
        let old_width = self.columns[column].width.pixels(self.viewport_width);
        let new_width = old_width.saturating_add(delta).max(1);
        let new_fraction = (f64::from(new_width) / f64::from(self.viewport_width))
            .clamp(MIN_COLUMN_FRACTION, MAX_COLUMN_FRACTION);
        if (new_fraction - old_fraction).abs() <= f64::EPSILON {
            return Some((false, 0));
        }

        self.columns[column].width = ColumnWidth::new(new_fraction);
        let applied = self.column_width(column).saturating_sub(old_width);
        if anchor == ColumnResizeAnchor::Right {
            self.viewport_x += f64::from(applied);
        }
        self.user_positioned_viewport = true;
        self.focused = Some(current.clone());
        Some((true, applied))
    }

    fn resize_rows_by_pixels(&mut self, upper: &T, lower: &T, delta: i32) -> Option<bool> {
        let (column, upper_index) = self.tile_position(upper)?;
        let (lower_column, lower_index) = self.tile_position(lower)?;
        if lower_column != column || lower_index != upper_index + 1 || self.column_height <= 0 {
            return None;
        }

        let tiles = self.columns[column].tiles.clone();
        let weights = tiles
            .iter()
            .map(|tile| self.tile_weight(tile))
            .collect::<Vec<_>>();
        let rows = weighted_vertical_tile_rows(self.column_height, self.gap, &weights);
        let upper_height = rows.get(upper_index)?.1;
        let lower_height = rows.get(lower_index)?.1;
        let pair_height = upper_height.saturating_add(lower_height);
        if pair_height <= 1 {
            return Some(false);
        }

        let minimum = if pair_height >= MIN_TILE_HEIGHT.saturating_mul(2) {
            MIN_TILE_HEIGHT
        } else {
            (pair_height / 4).max(1)
        };
        let new_upper = upper_height
            .saturating_add(delta)
            .clamp(minimum, pair_height.saturating_sub(minimum));
        if new_upper == upper_height {
            return Some(false);
        }
        let new_lower = pair_height.saturating_sub(new_upper);

        for (index, (tile, (_, height))) in tiles.iter().zip(rows).enumerate() {
            let height = if index == upper_index {
                new_upper
            } else if index == lower_index {
                new_lower
            } else {
                height
            };
            self.set_tile_weight(tile.clone(), f64::from(height.max(1)));
        }
        Some(true)
    }

    fn ensure_visible(&mut self, current: &T) -> bool {
        let Some((column, _)) = self.tile_position(current) else {
            return false;
        };
        self.focused = Some(current.clone());
        self.user_positioned_viewport = false;

        if self.viewport_width <= 0 {
            return false;
        }

        let old_viewport = self.viewport_x;
        let left = self.layout_x(column);
        let right = left + self.column_width(column) as f64;
        if left < self.viewport_x {
            self.viewport_x = left;
            self.clamp_viewport();
        } else if right > self.viewport_x + self.viewport_width as f64 {
            self.viewport_x = right - self.viewport_width as f64;
            self.clamp_viewport();
        }

        (self.viewport_x - old_viewport).abs() > f64::EPSILON
    }

    /// Center the complete column containing `current` without changing strip
    /// order or clamping edge columns back against the strip boundary.
    fn center_column(&mut self, current: &T) -> bool {
        let Some(target) = self.centered_viewport(current) else {
            return false;
        };

        let old_viewport = self.viewport_x;
        self.user_positioned_viewport = false;
        self.viewport_x = target;
        self.focused = Some(current.clone());
        (self.viewport_x - old_viewport).abs() > f64::EPSILON
    }

    fn centered_viewport(&self, current: &T) -> Option<f64> {
        let (column, _) = self.tile_position(current)?;
        (self.viewport_width > 0).then(|| {
            self.layout_x(column) + self.column_width(column) as f64 / 2.0
                - self.viewport_width as f64 / 2.0
        })
    }

    fn centering_changes_viewport(&self, current: &T) -> bool {
        self.centered_viewport(current)
            .is_some_and(|target| (target - self.viewport_x).abs() > f64::EPSILON)
    }

    /// Keep a workspace with exactly one modeled tile centered. Grabbed-window
    /// placeholders remain modeled, so dragging the only tile does not flicker.
    fn center_only_tile(&mut self) -> bool {
        let mut tiles = self.columns.iter().flat_map(|column| column.tiles.iter());
        let Some(tile) = tiles.next().cloned() else {
            return false;
        };
        if tiles.next().is_some() {
            return false;
        }
        self.center_column(&tile)
    }

    fn ensure_preferred_visible(&mut self, requested: Option<&T>, activated: Option<&T>) -> bool {
        if let Some(current) = requested {
            return self.ensure_visible(current);
        }
        let Some(current) = activated else {
            return false;
        };
        if self.user_positioned_viewport {
            if self.contains(current) {
                self.focused = Some(current.clone());
            }
            false
        } else {
            self.ensure_visible(current)
        }
    }

    /// Pan the virtual strip directly. The first and last columns may each be
    /// centered, matching explicit centering at the strip edges.
    fn pan_viewport(&mut self, delta_x: f64) -> bool {
        if !delta_x.is_finite() || self.viewport_width <= 0 || self.columns.is_empty() {
            return false;
        }
        if self
            .columns
            .iter()
            .map(|column| column.tiles.len())
            .sum::<usize>()
            == 1
        {
            return self.center_only_tile();
        }

        let old_viewport = self.viewport_x;
        self.user_positioned_viewport = true;
        self.viewport_x += delta_x;
        self.clamp_viewport();
        (self.viewport_x - old_viewport).abs() > f64::EPSILON
    }

    fn column_width(&self, index: usize) -> i32 {
        self.columns[index].width.pixels(self.viewport_width)
    }

    fn layout_x(&self, index: usize) -> f64 {
        self.columns
            .iter()
            .take(index)
            .map(|column| column.width.pixels(self.viewport_width) as f64)
            .sum::<f64>()
            + index as f64 * self.gap as f64
    }

    fn strip_width(&self) -> f64 {
        self.columns
            .iter()
            .map(|column| column.width.pixels(self.viewport_width) as f64)
            .sum::<f64>()
            + self.columns.len().saturating_sub(1) as f64 * self.gap as f64
    }

    fn clamp_viewport(&mut self) {
        if !self.viewport_x.is_finite() {
            self.viewport_x = 0.0;
        }
        if self.user_positioned_viewport && !self.columns.is_empty() {
            let viewport_width = self.viewport_width.max(0) as f64;
            let first_center = self.column_width(0) as f64 / 2.0;
            let last = self.columns.len() - 1;
            let last_center = self.layout_x(last) + self.column_width(last) as f64 / 2.0;
            let min_viewport = first_center - viewport_width / 2.0;
            let max_viewport = last_center - viewport_width / 2.0;
            self.viewport_x = self.viewport_x.clamp(min_viewport, max_viewport);
        } else {
            let max_viewport = (self.strip_width() - self.viewport_width.max(0) as f64).max(0.0);
            self.viewport_x = self.viewport_x.clamp(0.0, max_viewport);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::layout::Orientation;
    use id_tree::{InsertBehavior, Node};
    use smithay::backend::renderer::element::Id;
    use std::sync::Arc;

    fn assert_viewport(model: &ColumnModel<i32>, expected: f64) {
        assert!((model.viewport_x - expected).abs() < f64::EPSILON);
    }

    fn tile_columns(model: &ColumnModel<i32>) -> Vec<Vec<i32>> {
        model
            .columns
            .iter()
            .map(|column| column.tiles.clone())
            .collect()
    }

    fn width_fractions(model: &ColumnModel<i32>) -> Vec<f64> {
        model
            .columns
            .iter()
            .map(|column| column.width.fraction)
            .collect()
    }

    fn column_row_heights(model: &ColumnModel<i32>, column: usize) -> Vec<i32> {
        let weights = model.columns[column]
            .tiles
            .iter()
            .map(|tile| model.tile_weight(tile))
            .collect::<Vec<_>>();
        weighted_vertical_tile_rows(model.column_height, model.gap, &weights)
            .into_iter()
            .map(|(_, height)| height)
            .collect()
    }

    #[test]
    fn initial_reconciliation_creates_default_width_single_tile_columns() {
        let mut model = ColumnModel::default();
        model.reconcile([1, 2, 3]);

        assert_eq!(tile_columns(&model), [[1], [2], [3]]);
        assert_eq!(
            width_fractions(&model),
            [
                DEFAULT_COLUMN_FRACTION,
                DEFAULT_COLUMN_FRACTION,
                DEFAULT_COLUMN_FRACTION
            ]
        );
    }

    #[test]
    fn new_toplevel_opens_after_the_focused_multi_tile_column() {
        let mut model = ColumnModel {
            columns: vec![
                Column {
                    tiles: vec![1, 2],
                    width: ColumnWidth::new(0.5),
                },
                Column {
                    tiles: vec![3, 4],
                    width: ColumnWidth::new(0.8),
                },
            ],
            tile_weights: vec![(1, 2.0), (2, 1.0), (3, 1.0), (4, 3.0)],
            focused: Some(2),
            ..ColumnModel::default()
        };

        model.insert_new_toplevel(5, Some(&2));

        assert_eq!(tile_columns(&model), [vec![1, 2], vec![5], vec![3, 4]]);
        assert_eq!(width_fractions(&model), [0.5, DEFAULT_COLUMN_FRACTION, 0.8]);
        assert_eq!(model.tile_weight(&1), 2.0);
        assert_eq!(model.tile_weight(&2), 1.0);
        assert_eq!(model.tile_weight(&3), 1.0);
        assert_eq!(model.tile_weight(&4), 3.0);
        assert_eq!(model.tile_weight(&5), 1.0);
        assert_eq!(model.focused, Some(2));
    }

    #[test]
    fn new_toplevel_after_last_column_or_invalid_focus_appends_deterministically() {
        let mut model = ColumnModel::default();
        model.reconcile([1, 2]);

        model.insert_new_toplevel(3, Some(&2));
        model.insert_new_toplevel(4, Some(&99));
        model.insert_new_toplevel(5, None);

        assert_eq!(tile_columns(&model), [[1], [2], [3], [4], [5]]);
        assert_eq!(width_fractions(&model), [DEFAULT_COLUMN_FRACTION; 5]);
    }

    #[test]
    fn repeated_new_toplevel_notification_never_duplicates_a_tile() {
        let mut model = ColumnModel::default();
        model.reconcile([1, 2, 3]);

        model.insert_new_toplevel(4, Some(&1));
        model.insert_new_toplevel(4, Some(&3));

        assert_eq!(tile_columns(&model), [[1], [2], [3], [4]]);
        assert_eq!(
            model
                .columns
                .iter()
                .flat_map(|column| column.tiles.iter())
                .filter(|tile| **tile == 4)
                .count(),
            1
        );
    }

    #[test]
    fn reconciliation_preserves_strip_order_and_appends_in_supplied_order() {
        let mut model = ColumnModel::default();
        model.reconcile([1, 2, 3]);

        assert!(model.move_by(&2, 1));
        model.reconcile([3, 1, 5, 2, 4]);
        assert_eq!(tile_columns(&model), [[1], [3], [2], [5], [4]]);

        model.reconcile([4, 3, 2]);
        assert_eq!(tile_columns(&model), [[3], [2], [4]]);
    }

    #[test]
    fn reconciliation_prevents_input_and_modeled_duplicates() {
        let mut model = ColumnModel {
            columns: vec![
                Column {
                    tiles: vec![1, 2],
                    width: ColumnWidth::new(0.4),
                },
                Column {
                    tiles: vec![2, 3, 1],
                    width: ColumnWidth::new(0.7),
                },
            ],
            ..ColumnModel::default()
        };

        model.reconcile([1, 1, 2, 3, 3]);

        assert_eq!(tile_columns(&model), [vec![1, 2], vec![3]]);
        assert_eq!(width_fractions(&model), [0.4, 0.7]);
    }

    #[test]
    fn reconciliation_removes_stale_tiles_and_empty_columns() {
        let mut model = ColumnModel {
            columns: vec![
                Column {
                    tiles: vec![1, 2],
                    width: ColumnWidth::new(0.4),
                },
                Column::single(3),
                Column {
                    tiles: vec![4, 5],
                    width: ColumnWidth::new(0.8),
                },
            ],
            ..ColumnModel::default()
        };

        model.reconcile([4, 2]);

        assert_eq!(tile_columns(&model), [[2], [4]]);
        assert_eq!(width_fractions(&model), [0.4, 0.8]);
    }

    #[test]
    fn reconciliation_preserves_a_modeled_multi_tile_column() {
        let mut model = ColumnModel {
            columns: vec![
                Column {
                    tiles: vec![1, 2, 3],
                    width: ColumnWidth::new(0.75),
                },
                Column {
                    tiles: vec![4],
                    width: ColumnWidth::new(0.5),
                },
            ],
            ..ColumnModel::default()
        };

        model.reconcile([3, 2, 4, 5]);

        assert_eq!(tile_columns(&model), [vec![2, 3], vec![4], vec![5]]);
        assert_eq!(
            width_fractions(&model),
            [0.75, 0.5, DEFAULT_COLUMN_FRACTION]
        );
    }

    #[test]
    fn focus_follows_tile_identity_and_clears_when_the_tile_is_removed() {
        let mut model = ColumnModel::default();
        model.reconcile([1, 2, 3]);
        model.set_metrics(1_000, 10);
        assert!(model.ensure_visible(&2));
        assert_eq!(model.focused, Some(2));

        assert!(model.move_by(&2, 1));
        model.reconcile([3, 2, 1]);
        assert_eq!(tile_columns(&model), [[1], [3], [2]]);
        assert_eq!(model.focused, Some(2));

        model.reconcile([1, 3]);
        assert_eq!(model.focused, None);
        assert_viewport(&model, 330.0);
    }

    #[test]
    fn horizontal_focus_finds_neighbors_and_edges() {
        let mut model = ColumnModel::default();
        model.reconcile([10, 20, 30]);

        assert_eq!(model.adjacent_column(&20, -1), Some(&10));
        assert_eq!(model.adjacent_column(&20, 1), Some(&30));
        assert_eq!(model.adjacent_column(&10, -1), None);
        assert_eq!(model.adjacent_column(&30, 1), None);
    }

    #[test]
    fn vertical_focus_finds_neighbors_and_stops_at_column_edges() {
        let model = ColumnModel {
            columns: vec![
                Column {
                    tiles: vec![10, 20, 30],
                    width: ColumnWidth::new(0.4),
                },
                Column::single(40),
            ],
            ..ColumnModel::default()
        };

        assert_eq!(model.adjacent_tile(&20, -1), Some(&10));
        assert_eq!(model.adjacent_tile(&20, 1), Some(&30));
        assert_eq!(model.adjacent_tile(&10, -1), None);
        assert_eq!(model.adjacent_tile(&30, 1), None);
        assert_eq!(model.adjacent_tile(&40, -1), None);
        assert_eq!(model.adjacent_tile(&40, 1), None);
        assert_eq!(model.adjacent_tile(&99, 1), None);
    }

    #[test]
    fn vertical_focus_follows_pointer_order_and_replacement_identity() {
        let mut model = ColumnModel {
            columns: vec![
                Column::single(1),
                Column {
                    tiles: vec![2, 3, 4],
                    width: ColumnWidth::new(0.75),
                },
                Column::single(5),
            ],
            ..ColumnModel::default()
        };
        model.set_metrics(1_000, 10);
        model.ensure_visible(&3);

        assert!(model.begin_drag(&3));
        model.reconcile([1, 2, 4, 5]);
        model.finish_drag(Some(&6), ColumnDropTarget::Below(4));
        assert!(model.begin_drag(&6));
        model.reconcile([1, 2, 4, 5]);
        model.finish_drag(Some(&7), ColumnDropTarget::Above(2));

        let columns = model.columns.clone();
        let viewport = model.viewport_x;
        assert_eq!(tile_columns(&model), [vec![1], vec![7, 2, 4], vec![5]]);
        assert_eq!(model.adjacent_tile(&2, -1), Some(&7));
        assert_eq!(model.adjacent_tile(&2, 1), Some(&4));
        assert!(!model.ensure_visible(&4));
        assert_eq!(model.columns, columns);
        assert_viewport(&model, viewport);
        assert_eq!(model.focused, Some(4));
    }

    #[test]
    fn adding_a_column_keeps_existing_virtual_geometry() {
        let mut model = ColumnModel::default();
        model.reconcile([1, 2]);
        model.set_metrics(1_000, 10);
        let before = [model.layout_x(0), model.layout_x(1)];

        model.reconcile([1, 2, 3]);
        model.set_metrics(1_000, 10);

        assert_eq!(model.column_width(0), 660);
        assert_eq!([model.layout_x(0), model.layout_x(1)], before);
        assert_viewport(&model, 0.0);
    }

    #[test]
    fn viewport_follows_focus_without_moving_visible_columns() {
        let mut model = ColumnModel::default();
        model.reconcile([1, 2, 3]);
        model.set_metrics(1_000, 10);

        assert!(!model.ensure_visible(&1));
        assert_viewport(&model, 0.0);

        assert!(model.ensure_visible(&2));
        assert_viewport(&model, 330.0);

        assert!(model.ensure_visible(&3));
        assert_viewport(&model, 1_000.0);

        assert!(model.ensure_visible(&2));
        assert_viewport(&model, 670.0);
    }

    #[test]
    fn requested_focus_fully_reveals_edge_columns_before_activation_updates() {
        let mut model = ColumnModel::default();
        model.reconcile([1, 2, 3]);
        model.set_metrics(1_000, 10);

        assert!(model.ensure_visible(&2));
        assert_viewport(&model, 330.0);

        // During Shell::set_focus, 2 is still activated while 3 is the new
        // requested focus. The request must win so the last column is fully
        // visible rather than leaving the viewport between both columns.
        assert!(model.ensure_preferred_visible(Some(&3), Some(&2)));
        assert_viewport(&model, 1_000.0);

        // The same rule must fully reveal the first column at the other edge.
        assert!(model.ensure_preferred_visible(Some(&1), Some(&3)));
        assert_viewport(&model, 0.0);
    }

    #[test]
    fn reorder_moves_only_the_focused_column() {
        let mut model = ColumnModel::default();
        model.reconcile([1, 2, 3]);
        model.set_metrics(1_000, 10);
        let first = model.columns[0].clone();
        let third = model.columns[2].clone();

        assert!(model.move_by(&2, 1));
        assert_eq!(tile_columns(&model), [[1], [3], [2]]);
        assert_eq!(model.columns[0], first);
        assert_eq!(model.columns[1], third);
        assert_eq!(model.focused, Some(2));
        assert!(!model.move_by(&2, 1));

        assert!(model.move_by(&2, -1));
        assert_eq!(tile_columns(&model), [[1], [2], [3]]);
        assert_eq!(model.focused, Some(2));
    }

    #[test]
    fn width_state_follows_a_reordered_column() {
        let mut model = ColumnModel::default();
        model.reconcile([1, 2, 3]);
        model.columns[1].width = ColumnWidth::new(0.35);

        assert!(model.move_by(&2, 1));
        assert_eq!(tile_columns(&model), [[1], [3], [2]]);
        assert_eq!(
            width_fractions(&model),
            [DEFAULT_COLUMN_FRACTION, DEFAULT_COLUMN_FRACTION, 0.35]
        );
    }

    #[test]
    fn horizontal_move_extracts_only_the_focused_tile_from_a_stack() {
        let columns = vec![
            Column::single(1),
            Column {
                tiles: vec![2, 3, 4],
                width: ColumnWidth::new(0.75),
            },
            Column {
                tiles: vec![5],
                width: ColumnWidth::new(0.8),
            },
        ];
        let mut move_left = ColumnModel {
            columns: columns.clone(),
            ..ColumnModel::default()
        };
        let mut move_right = ColumnModel {
            columns,
            ..ColumnModel::default()
        };

        assert!(move_left.move_by(&3, -1));
        assert_eq!(
            tile_columns(&move_left),
            [vec![1], vec![3], vec![2, 4], vec![5]]
        );
        assert_eq!(
            width_fractions(&move_left),
            [DEFAULT_COLUMN_FRACTION, DEFAULT_COLUMN_FRACTION, 0.75, 0.8]
        );
        assert_eq!(move_left.focused, Some(3));

        assert!(move_right.move_by(&3, 1));
        assert_eq!(
            tile_columns(&move_right),
            [vec![1], vec![2, 4], vec![3], vec![5]]
        );
        assert_eq!(
            width_fractions(&move_right),
            [DEFAULT_COLUMN_FRACTION, 0.75, DEFAULT_COLUMN_FRACTION, 0.8]
        );
        assert_eq!(move_right.focused, Some(3));

        assert!(move_right.move_by(&3, 1));
        assert_eq!(
            tile_columns(&move_right),
            [vec![1], vec![2, 4], vec![5], vec![3]]
        );
        assert_eq!(
            width_fractions(&move_right),
            [DEFAULT_COLUMN_FRACTION, 0.75, 0.8, DEFAULT_COLUMN_FRACTION]
        );
    }

    #[test]
    fn horizontal_move_can_extract_at_both_strip_edges() {
        let columns = vec![
            Column {
                tiles: vec![1, 2],
                width: ColumnWidth::new(0.4),
            },
            Column {
                tiles: vec![3, 4],
                width: ColumnWidth::new(0.8),
            },
        ];
        let mut left = ColumnModel {
            columns: columns.clone(),
            ..ColumnModel::default()
        };
        let mut right = ColumnModel {
            columns,
            ..ColumnModel::default()
        };

        assert!(left.move_by(&1, -1));
        assert_eq!(tile_columns(&left), [vec![1], vec![2], vec![3, 4]]);
        assert_eq!(width_fractions(&left), [DEFAULT_COLUMN_FRACTION, 0.4, 0.8]);

        assert!(right.move_by(&4, 1));
        assert_eq!(tile_columns(&right), [vec![1, 2], vec![3], vec![4]]);
        assert_eq!(width_fractions(&right), [0.4, 0.8, DEFAULT_COLUMN_FRACTION]);
    }

    #[test]
    fn vertical_move_reorders_tiles_until_the_focused_tile_reaches_an_edge() {
        let mut model = ColumnModel {
            columns: vec![
                Column::single(1),
                Column {
                    tiles: vec![2, 3, 4],
                    width: ColumnWidth::new(0.75),
                },
                Column::single(5),
            ],
            ..ColumnModel::default()
        };
        model.set_metrics(1_000, 10);
        model.ensure_visible(&3);
        let viewport = model.viewport_x;
        let widths = width_fractions(&model);

        assert_eq!(model.move_tile_vertical(&3, 1), TileMove::Moved);
        assert_eq!(tile_columns(&model), [vec![1], vec![2, 4, 3], vec![5]]);
        assert_eq!(model.focused, Some(3));
        assert_viewport(&model, viewport);
        assert_eq!(width_fractions(&model), widths);
        assert_eq!(model.move_tile_vertical(&3, 1), TileMove::Edge);

        assert_eq!(model.move_tile_vertical(&3, -1), TileMove::Moved);
        assert_eq!(tile_columns(&model), [vec![1], vec![2, 3, 4], vec![5]]);
    }

    #[test]
    fn vertical_move_first_merges_a_single_tile_into_its_left_neighbor() {
        let mut model = ColumnModel {
            columns: vec![
                Column {
                    tiles: vec![1],
                    width: ColumnWidth::new(0.4),
                },
                Column {
                    tiles: vec![2],
                    width: ColumnWidth::new(0.35),
                },
                Column {
                    tiles: vec![3, 4],
                    width: ColumnWidth::new(0.8),
                },
            ],
            ..ColumnModel::default()
        };

        assert_eq!(model.move_tile_vertical(&2, 1), TileMove::Moved);
        assert_eq!(tile_columns(&model), [vec![1, 2], vec![3, 4]]);
        assert_eq!(width_fractions(&model), [0.4, 0.8]);
        assert_eq!(model.focused, Some(2));
        assert_eq!(model.move_tile_vertical(&2, 1), TileMove::Edge);
    }

    #[test]
    fn vertical_move_uses_the_right_neighbor_for_the_first_column() {
        let columns = vec![
            Column {
                tiles: vec![1],
                width: ColumnWidth::new(0.35),
            },
            Column {
                tiles: vec![2, 3],
                width: ColumnWidth::new(0.8),
            },
        ];
        let mut move_up = ColumnModel {
            columns: columns.clone(),
            ..ColumnModel::default()
        };
        let mut move_down = ColumnModel {
            columns,
            ..ColumnModel::default()
        };

        assert_eq!(move_up.move_tile_vertical(&1, -1), TileMove::Moved);
        assert_eq!(tile_columns(&move_up), [[1, 2, 3]]);
        assert_eq!(width_fractions(&move_up), [0.8]);
        assert_eq!(move_up.move_tile_vertical(&1, -1), TileMove::Edge);

        assert_eq!(move_down.move_tile_vertical(&1, 1), TileMove::Moved);
        assert_eq!(tile_columns(&move_down), [[2, 3, 1]]);
        assert_eq!(width_fractions(&move_down), [0.8]);
        assert_eq!(move_down.move_tile_vertical(&1, 1), TileMove::Edge);
    }

    #[test]
    fn vertical_move_handles_an_only_column_and_stale_identity_safely() {
        let mut model = ColumnModel::default();
        model.reconcile([1]);

        assert_eq!(model.move_tile_vertical(&1, -1), TileMove::Edge);
        assert_eq!(model.move_tile_vertical(&1, 1), TileMove::Edge);
        assert_eq!(model.move_tile_vertical(&2, 1), TileMove::Unavailable);
        assert_eq!(tile_columns(&model), [[1]]);
    }

    #[test]
    fn pointer_drop_places_columns_before_after_and_at_strip_edges() {
        let mut model = ColumnModel::default();
        model.reconcile([1, 2, 3, 4]);

        assert!(model.begin_drag(&3));
        model.finish_drag(Some(&3), ColumnDropTarget::Before(1));
        assert_eq!(tile_columns(&model), [[3], [1], [2], [4]]);

        assert!(model.begin_drag(&3));
        model.finish_drag(Some(&3), ColumnDropTarget::After(4));
        assert_eq!(tile_columns(&model), [[1], [2], [4], [3]]);

        assert!(model.begin_drag(&3));
        model.finish_drag(Some(&3), ColumnDropTarget::First);
        assert_eq!(tile_columns(&model), [[3], [1], [2], [4]]);

        assert!(model.begin_drag(&3));
        model.finish_drag(Some(&3), ColumnDropTarget::Last);
        assert_eq!(tile_columns(&model), [[1], [2], [4], [3]]);
    }

    #[test]
    fn pointer_drop_at_current_position_is_a_noop() {
        let mut model = ColumnModel::default();
        model.reconcile([1, 2, 3, 4]);
        let before = model.columns.clone();

        assert!(model.begin_drag(&2));
        model.finish_drag(Some(&2), ColumnDropTarget::Before(3));

        assert_eq!(model.columns, before);
        assert_eq!(model.focused, Some(2));
    }

    #[test]
    fn pointer_drop_above_and_below_builds_one_ordered_column() {
        let mut model = ColumnModel {
            columns: vec![
                Column {
                    tiles: vec![1],
                    width: ColumnWidth::new(0.4),
                },
                Column {
                    tiles: vec![2],
                    width: ColumnWidth::new(0.8),
                },
                Column::single(3),
            ],
            ..ColumnModel::default()
        };

        assert!(model.begin_drag(&1));
        model.reconcile([2, 3]);
        model.finish_drag(Some(&4), ColumnDropTarget::Above(2));
        assert_eq!(tile_columns(&model), [vec![4, 2], vec![3]]);
        assert_eq!(width_fractions(&model), [0.8, DEFAULT_COLUMN_FRACTION]);

        assert!(model.begin_drag(&3));
        model.reconcile([4, 2]);
        model.finish_drag(Some(&5), ColumnDropTarget::Below(2));
        assert_eq!(tile_columns(&model), [vec![4, 2, 5]]);
        assert_eq!(width_fractions(&model), [0.8]);
        assert_eq!(model.focused, Some(5));
    }

    #[test]
    fn pointer_drop_reorders_stably_within_a_column() {
        let mut model = ColumnModel {
            columns: vec![Column {
                tiles: vec![1, 2, 3, 4],
                width: ColumnWidth::new(0.75),
            }],
            ..ColumnModel::default()
        };

        assert!(model.begin_drag(&2));
        model.reconcile([1, 3, 4]);
        model.finish_drag(Some(&5), ColumnDropTarget::Below(4));
        assert_eq!(tile_columns(&model), [[1, 3, 4, 5]]);

        assert!(model.begin_drag(&4));
        model.reconcile([1, 3, 5]);
        model.finish_drag(Some(&6), ColumnDropTarget::Above(1));
        assert_eq!(tile_columns(&model), [[6, 1, 3, 5]]);
        assert_eq!(width_fractions(&model), [0.75]);
        assert_eq!(model.focused, Some(6));
    }

    #[test]
    fn vertical_drop_moves_only_one_tile_between_columns() {
        let mut model = ColumnModel {
            columns: vec![
                Column {
                    tiles: vec![1, 2],
                    width: ColumnWidth::new(0.4),
                },
                Column {
                    tiles: vec![3, 4],
                    width: ColumnWidth::new(0.8),
                },
            ],
            ..ColumnModel::default()
        };

        assert!(model.begin_drag(&2));
        model.reconcile([1, 3, 4]);
        model.finish_drag(Some(&5), ColumnDropTarget::Above(4));

        assert_eq!(tile_columns(&model), [vec![1], vec![3, 5, 4]]);
        assert_eq!(width_fractions(&model), [0.4, 0.8]);
        assert_eq!(model.focused, Some(5));
    }

    #[test]
    fn horizontal_drop_extracts_one_tile_with_default_width() {
        let mut model = ColumnModel {
            columns: vec![
                Column {
                    tiles: vec![1, 2],
                    width: ColumnWidth::new(0.4),
                },
                Column {
                    tiles: vec![3, 4],
                    width: ColumnWidth::new(0.8),
                },
            ],
            ..ColumnModel::default()
        };

        assert!(model.begin_drag(&2));
        model.reconcile([1, 3, 4]);
        model.finish_drag(Some(&5), ColumnDropTarget::Before(3));

        assert_eq!(tile_columns(&model), [vec![1], vec![5], vec![3, 4]]);
        assert_eq!(width_fractions(&model), [0.4, DEFAULT_COLUMN_FRACTION, 0.8]);
    }

    #[test]
    fn cancelling_multi_tile_drag_restores_the_exact_model() {
        let mut model = ColumnModel {
            columns: vec![
                Column {
                    tiles: vec![1, 2, 3],
                    width: ColumnWidth::new(0.4),
                },
                Column {
                    tiles: vec![4],
                    width: ColumnWidth::new(0.8),
                },
            ],
            ..ColumnModel::default()
        };
        model.set_metrics(1_000, 10);
        model.ensure_visible(&2);
        let before = model.clone();

        assert!(model.begin_drag(&2));
        model.reconcile([1, 3, 4]);
        model.cancel_drag();

        assert_eq!(model.columns, before.columns);
        assert_eq!(model.focused, before.focused);
        assert_viewport(&model, before.viewport_x);
        assert!(model.pending_drag.is_none());
    }

    #[test]
    fn pointer_drop_transfers_width_and_focus_to_a_replaced_node() {
        let mut model = ColumnModel::default();
        model.reconcile([1, 2, 3]);
        model.columns[1].width = ColumnWidth::new(0.35);
        model.set_metrics(1_000, 10);
        model.ensure_visible(&2);

        assert!(model.begin_drag(&2));
        model.reconcile([1, 3]);
        model.finish_drag(Some(&4), ColumnDropTarget::After(3));

        assert_eq!(tile_columns(&model), [[1], [3], [4]]);
        assert_eq!(
            width_fractions(&model),
            [DEFAULT_COLUMN_FRACTION, DEFAULT_COLUMN_FRACTION, 0.35]
        );
        assert_eq!(model.focused, Some(4));
        assert_eq!(
            model
                .columns
                .iter()
                .map(|column| column.tiles.len())
                .sum::<usize>(),
            3
        );
    }

    #[test]
    fn cancelling_pointer_drag_restores_order_width_focus_and_viewport() {
        let mut model = ColumnModel::default();
        model.reconcile([1, 2, 3]);
        model.columns[1].width = ColumnWidth::new(0.35);
        model.set_metrics(1_000, 10);
        model.ensure_visible(&2);
        let viewport = model.viewport_x;

        assert!(model.begin_drag(&2));
        model.reconcile([1, 3]);
        model.cancel_drag();

        assert_eq!(tile_columns(&model), [[1], [2], [3]]);
        assert_eq!(
            width_fractions(&model),
            [DEFAULT_COLUMN_FRACTION, 0.35, DEFAULT_COLUMN_FRACTION]
        );
        assert_eq!(model.focused, Some(2));
        assert_viewport(&model, viewport);
    }

    #[test]
    fn repeated_pointer_drops_do_not_leave_duplicates_or_phantom_columns() {
        let mut model = ColumnModel::default();
        model.reconcile([1, 2, 3]);

        assert!(model.begin_drag(&2));
        model.reconcile([1, 3]);
        model.finish_drag(Some(&4), ColumnDropTarget::After(3));
        model.reconcile([1, 3, 4, 4]);
        assert_eq!(tile_columns(&model), [[1], [3], [4]]);

        assert!(model.begin_drag(&4));
        model.reconcile([1, 3]);
        model.finish_drag(Some(&5), ColumnDropTarget::Above(3));
        model.reconcile([5, 1, 3]);
        assert_eq!(tile_columns(&model), [vec![1], vec![5, 3]]);

        assert!(model.begin_drag(&5));
        model.reconcile([1, 3]);
        model.finish_drag(Some(&6), ColumnDropTarget::Before(1));
        model.reconcile([6, 1, 3]);

        assert_eq!(tile_columns(&model), [[6], [1], [3]]);
        assert!(!model.contains(&2));
        assert!(!model.contains(&4));
        assert!(!model.contains(&5));
        assert_eq!(model.focused, Some(6));
    }

    #[test]
    fn stacking_drop_discards_only_the_dragged_tile() {
        let mut model = ColumnModel {
            columns: vec![
                Column {
                    tiles: vec![1, 2],
                    width: ColumnWidth::new(0.4),
                },
                Column::single(3),
            ],
            ..ColumnModel::default()
        };

        assert!(model.begin_drag(&2));
        model.finish_drag(Some(&3), ColumnDropTarget::Discard);
        model.reconcile([1, 3]);

        assert_eq!(tile_columns(&model), [[1], [3]]);
        assert_eq!(width_fractions(&model), [0.4, DEFAULT_COLUMN_FRACTION]);
        assert_eq!(model.focused, Some(3));
    }

    #[test]
    fn vertical_tile_geometry_preserves_single_tile_and_equal_two_tile_layouts() {
        assert_eq!(vertical_tile_rows(100, 10, 1), [(0, 100)]);
        assert_eq!(vertical_tile_rows(100, 10, 2), [(0, 45), (55, 45)]);
    }

    #[test]
    fn vertical_tile_geometry_distributes_remainder_from_the_top() {
        let rows = vertical_tile_rows(100, 10, 3);

        assert_eq!(rows, [(0, 27), (37, 27), (74, 26)]);
        assert_eq!(rows.last().map(|(y, height)| y + height), Some(100));

        let weighted = weighted_vertical_tile_rows(100, 10, &[2.0, 1.0, 1.0]);
        assert_eq!(weighted, [(0, 40), (50, 20), (80, 20)]);
        assert_eq!(weighted.last().map(|(y, height)| y + height), Some(100));
    }

    #[test]
    fn vertical_tile_geometry_is_safe_when_the_viewport_is_smaller_than_the_gaps() {
        let rows = vertical_tile_rows(2, 10, 3);

        assert_eq!(rows, [(0, 0), (1, 0), (2, 0)]);
        assert!(rows.iter().all(|(_, height)| *height >= 0));
        assert_eq!(vertical_tile_rows(100, 10, 0), []);
    }

    #[test]
    fn variable_widths_drive_strip_positions_and_total_width() {
        let mut model = ColumnModel::default();
        model.reconcile([1, 2, 3]);
        model.columns[0].width = ColumnWidth::new(0.4);
        model.columns[1].width = ColumnWidth::new(0.7);
        model.columns[2].width = ColumnWidth::new(0.5);
        model.set_metrics(1_000, 10);

        assert_eq!(model.column_width(0), 400);
        assert_eq!(model.column_width(1), 700);
        assert_eq!(model.column_width(2), 500);
        assert_eq!(
            [model.layout_x(0), model.layout_x(1), model.layout_x(2)],
            [0.0, 410.0, 1_120.0]
        );
        assert_eq!(model.strip_width(), 1_620.0);
    }

    #[test]
    fn column_width_presets_cycle_forwards_and_backwards_with_wrapping() {
        let mut forwards = ColumnModel::default();
        forwards.reconcile([1]);
        forwards.set_metrics(1_000, 10);
        for (fraction, pixels) in [(1.0, 1_000), (0.33, 330), (0.5, 500), (0.66, 660)] {
            assert!(forwards.cycle_column_width(&1, ResizeDirection::Outwards));
            assert_eq!(forwards.columns[0].width.fraction, fraction);
            assert_eq!(forwards.column_width(0), pixels);
        }

        let mut backwards = ColumnModel::default();
        backwards.reconcile([1]);
        backwards.set_metrics(1_000, 10);
        for (fraction, pixels) in [(0.5, 500), (0.33, 330), (1.0, 1_000), (0.66, 660)] {
            assert!(backwards.cycle_column_width(&1, ResizeDirection::Inwards));
            assert_eq!(backwards.columns[0].width.fraction, fraction);
            assert_eq!(backwards.column_width(0), pixels);
        }
    }

    #[test]
    fn width_cycle_changes_the_whole_focused_column_only() {
        let mut model = ColumnModel {
            columns: vec![
                Column {
                    tiles: vec![1, 2, 3],
                    width: ColumnWidth::new(0.4),
                },
                Column {
                    tiles: vec![4],
                    width: ColumnWidth::new(0.8),
                },
            ],
            ..ColumnModel::default()
        };
        model.set_metrics(1_000, 10);

        assert!(model.cycle_column_width(&2, ResizeDirection::Outwards));
        assert_eq!(tile_columns(&model), [vec![1, 2, 3], vec![4]]);
        assert_eq!(width_fractions(&model), [0.5, 0.8]);
        assert_eq!(model.focused, Some(2));
        assert!(!model.cycle_column_width(&5, ResizeDirection::Outwards));
        assert_eq!(width_fractions(&model), [0.5, 0.8]);
    }

    #[test]
    fn continuous_width_selects_the_adjacent_preset_in_each_direction() {
        let mut model = ColumnModel {
            columns: vec![
                Column {
                    tiles: vec![1, 2],
                    width: ColumnWidth::new(0.58),
                },
                Column::single(3),
            ],
            tile_weights: vec![(1, 3.0), (2, 2.0), (3, 1.0)],
            focused: Some(2),
            ..ColumnModel::default()
        };
        model.set_metrics(1_000, 10);
        let columns = tile_columns(&model);
        let weights = model.tile_weights.clone();

        assert!(model.cycle_column_width(&2, ResizeDirection::Outwards));
        assert_eq!(width_fractions(&model), [0.66, 0.66]);
        assert_eq!(tile_columns(&model), columns);
        assert_eq!(model.tile_weights, weights);
        assert_eq!(model.focused, Some(2));

        model.columns[0].width = ColumnWidth::new(0.58);
        assert!(model.cycle_column_width(&2, ResizeDirection::Inwards));
        assert_eq!(width_fractions(&model), [0.5, 0.66]);
        assert_eq!(tile_columns(&model), columns);
        assert_eq!(model.tile_weights, weights);
        assert_eq!(model.focused, Some(2));
    }

    #[test]
    fn mouse_resize_changes_column_width_continuously_and_clamps_safely() {
        let mut model = ColumnModel::default();
        model.reconcile([1, 2]);
        model.set_metrics(1_000, 10);

        assert_eq!(
            model.resize_column_by_pixels(&1, 100, ColumnResizeAnchor::Left),
            Some((true, 100))
        );
        assert_eq!(model.columns[0].width.fraction, 0.76);
        assert_eq!(model.column_width(0), 760);
        assert_eq!(
            model.resize_column_by_pixels(&1, -260, ColumnResizeAnchor::Left),
            Some((true, -260))
        );
        assert_eq!(model.columns[0].width.fraction, 0.5);

        assert_eq!(
            model.resize_column_by_pixels(&1, -10_000, ColumnResizeAnchor::Left),
            Some((true, -170))
        );
        assert_eq!(model.columns[0].width.fraction, MIN_COLUMN_FRACTION);
        assert_eq!(
            model.resize_column_by_pixels(&1, -1, ColumnResizeAnchor::Left),
            Some((false, 0))
        );
        assert_eq!(
            model.resize_column_by_pixels(&1, 10_000, ColumnResizeAnchor::Left),
            Some((true, 670))
        );
        assert_eq!(model.columns[0].width.fraction, MAX_COLUMN_FRACTION);
        assert_eq!(
            model.resize_column_by_pixels(&1, 1, ColumnResizeAnchor::Left),
            Some((false, 0))
        );
        assert_eq!(model.focused, Some(1));
        assert_eq!(model.columns[1].width.fraction, DEFAULT_COLUMN_FRACTION);
        assert_eq!(
            model.resize_column_by_pixels(&3, 100, ColumnResizeAnchor::Left),
            None
        );
    }

    #[test]
    fn live_pointer_resize_coalesces_each_tiles_configures_and_flushes_release() {
        assert!(should_configure_live_resize(true, false, true, true));

        // Pointer geometry continues changing while the preceding size is in
        // flight, but those intermediate sizes do not produce configures.
        for _ in 0..1_000 {
            assert!(!should_configure_live_resize(true, false, true, false));
        }

        // Once the previous size commits, the newest pending geometry is the
        // only next configure. Release is forced even without another delta.
        assert!(should_configure_live_resize(true, false, true, true));
        assert!(should_configure_live_resize(true, true, false, false));
        assert!(!should_configure_live_resize(false, true, true, true));
        assert!(!should_configure_live_resize(true, false, false, true));

        // Adjacent rows make their decisions independently: a fast upper tile
        // may advance while the lower tile still has a configure in flight.
        let upper_ready = should_configure_live_resize(true, false, true, true);
        let lower_ready = should_configure_live_resize(true, false, true, false);
        assert!(upper_ready);
        assert!(!lower_ready);
        assert!(should_configure_live_resize(true, true, false, false));
    }

    #[test]
    fn mouse_resize_applies_to_every_tile_in_the_target_column() {
        let mut model = ColumnModel {
            columns: vec![
                Column {
                    tiles: vec![1, 2, 3],
                    width: ColumnWidth::new(0.5),
                },
                Column {
                    tiles: vec![4],
                    width: ColumnWidth::new(0.8),
                },
            ],
            ..ColumnModel::default()
        };
        model.set_metrics(1_000, 10);

        assert_eq!(
            model.resize_column_by_pixels(&2, 100, ColumnResizeAnchor::Left),
            Some((true, 100))
        );
        assert_eq!(tile_columns(&model), [vec![1, 2, 3], vec![4]]);
        assert_eq!(width_fractions(&model), [0.6, 0.8]);
        assert_eq!(model.focused, Some(2));
        assert_eq!(model.adjacent_column(&2, -1), None);
        assert_eq!(model.adjacent_column(&4, -1), Some(&1));
    }

    #[test]
    fn mouse_resize_anchors_the_opposite_horizontal_edge() {
        let mut left_fixed = ColumnModel::default();
        left_fixed.reconcile([1, 2]);
        left_fixed.set_metrics(1_000, 10);
        left_fixed.viewport_x = 120.0;
        let old_left = left_fixed.layout_x(0) - left_fixed.viewport_x;

        assert_eq!(
            left_fixed.resize_column_by_pixels(&1, 100, ColumnResizeAnchor::Left),
            Some((true, 100))
        );
        assert_eq!(left_fixed.layout_x(0) - left_fixed.viewport_x, old_left);
        assert_eq!(left_fixed.viewport_x, 120.0);

        let mut right_fixed = ColumnModel::default();
        right_fixed.reconcile([1, 2]);
        right_fixed.set_metrics(1_000, 10);
        right_fixed.viewport_x = 120.0;
        let old_right = right_fixed.layout_x(0) + f64::from(right_fixed.column_width(0))
            - right_fixed.viewport_x;

        assert_eq!(
            right_fixed.resize_column_by_pixels(&1, 100, ColumnResizeAnchor::Right),
            Some((true, 100))
        );
        assert_eq!(right_fixed.viewport_x, 220.0);
        assert_eq!(
            right_fixed.layout_x(0) + f64::from(right_fixed.column_width(0))
                - right_fixed.viewport_x,
            old_right
        );
        assert_eq!(
            right_fixed.resize_column_by_pixels(&1, -60, ColumnResizeAnchor::Right),
            Some((true, -60))
        );
        assert_eq!(right_fixed.viewport_x, 160.0);
        assert_eq!(
            right_fixed.layout_x(0) + f64::from(right_fixed.column_width(0))
                - right_fixed.viewport_x,
            old_right
        );
        assert!(right_fixed.user_positioned_viewport);
    }

    #[test]
    fn left_edge_anchor_uses_the_applied_clamped_width_delta() {
        let mut model = ColumnModel {
            columns: vec![
                Column {
                    tiles: vec![1, 2],
                    width: ColumnWidth::new(0.95),
                },
                Column::single(3),
            ],
            tile_weights: vec![(1, 3.0), (2, 2.0), (3, 1.0)],
            focused: Some(2),
            viewport_x: 200.0,
            ..ColumnModel::default()
        };
        model.set_metrics(1_000, 10);
        let columns = tile_columns(&model);
        let weights = model.tile_weights.clone();
        let old_viewport = model.viewport_x;
        let right = model.layout_x(0) + f64::from(model.column_width(0)) - model.viewport_x;

        assert_eq!(
            model.resize_column_by_pixels(&2, 500, ColumnResizeAnchor::Right),
            Some((true, 50))
        );
        assert_eq!(model.viewport_x, old_viewport + 50.0);
        assert_eq!(
            model.layout_x(0) + f64::from(model.column_width(0)) - model.viewport_x,
            right
        );
        assert_eq!(
            model.resize_column_by_pixels(&2, 1, ColumnResizeAnchor::Right),
            Some((false, 0))
        );
        assert_eq!(model.viewport_x, old_viewport + 50.0);
        assert_eq!(tile_columns(&model), columns);
        assert_eq!(model.tile_weights, weights);
        assert_eq!(model.focused, Some(2));
        assert_eq!(width_fractions(&model), [1.0, 0.66]);

        assert_eq!(ColumnResizeAnchor::Left.width_delta(40.0), 40.0);
        assert_eq!(ColumnResizeAnchor::Right.width_delta(-40.0), 40.0);
    }

    #[test]
    fn vertical_mouse_resize_changes_only_the_adjacent_pair() {
        let mut model = ColumnModel {
            columns: vec![
                Column {
                    tiles: vec![1, 2, 3],
                    width: ColumnWidth::new(0.66),
                },
                Column {
                    tiles: vec![4, 5],
                    width: ColumnWidth::new(0.5),
                },
            ],
            ..ColumnModel::default()
        };
        model.set_metrics(1_000, 10);
        model.set_column_height(800);
        model.focused = Some(2);
        model.viewport_x = 75.0;

        assert_eq!(model.row_pair(&1, -1), None);
        assert_eq!(model.row_pair(&1, 1), Some((1, 2)));
        assert_eq!(model.row_pair(&3, -1), Some((2, 3)));
        assert_eq!(model.row_pair(&3, 1), None);
        assert_eq!(model.row_pair(&4, -1), None);
        assert_eq!(column_row_heights(&model, 0), [260, 260, 260]);
        assert_eq!(column_row_heights(&model, 1), [395, 395]);
        assert_eq!(model.resize_rows_by_pixels(&1, &2, 10), Some(true));
        assert_eq!(column_row_heights(&model, 0), [270, 250, 260]);
        assert_eq!(column_row_heights(&model, 1), [395, 395]);
        assert_eq!(model.focused, Some(2));
        assert_viewport(&model, 75.0);
        assert_eq!(model.resize_rows_by_pixels(&1, &3, 10), None);
        assert_eq!(model.resize_rows_by_pixels(&1, &4, 10), None);
    }

    #[test]
    fn vertical_mouse_resize_clamps_and_reverses_without_releasing() {
        let mut model = ColumnModel {
            columns: vec![Column {
                tiles: vec![1, 2],
                width: ColumnWidth::default(),
            }],
            ..ColumnModel::default()
        };
        model.set_metrics(1_000, 10);
        model.set_column_height(800);

        assert_eq!(model.resize_rows_by_pixels(&1, &2, 10_000), Some(true));
        assert_eq!(column_row_heights(&model, 0), [550, 240]);
        assert_eq!(model.resize_rows_by_pixels(&1, &2, 1), Some(false));
        assert_eq!(model.resize_rows_by_pixels(&1, &2, -50), Some(true));
        assert_eq!(column_row_heights(&model, 0), [500, 290]);
        assert_eq!(column_row_heights(&model, 0).iter().sum::<i32>(), 790);
    }

    #[test]
    fn vertical_weights_follow_identity_through_reorder_extract_and_insert() {
        let mut model = ColumnModel {
            columns: vec![
                Column::single(1),
                Column {
                    tiles: vec![2, 3],
                    width: ColumnWidth::new(0.75),
                },
            ],
            ..ColumnModel::default()
        };
        model.set_metrics(1_000, 10);
        model.set_column_height(800);
        assert_eq!(model.resize_rows_by_pixels(&2, &3, 100), Some(true));
        assert_eq!(column_row_heights(&model, 1), [495, 295]);

        assert_eq!(model.move_tile_vertical(&1, 1), TileMove::Moved);
        assert_eq!(tile_columns(&model), [vec![2, 3, 1]]);
        assert_eq!(model.tile_weight(&2), 495.0);
        assert_eq!(model.tile_weight(&3), 295.0);
        assert_eq!(model.tile_weight(&1), 395.0);

        assert!(model.move_by(&3, 1));
        assert_eq!(tile_columns(&model), [vec![2, 1], vec![3]]);
        assert_eq!(model.tile_weight(&2), 495.0);
        assert_eq!(model.tile_weight(&1), 395.0);
        assert_eq!(model.tile_weight(&3), 1.0);
    }

    #[test]
    fn vertical_weight_transfers_to_pointer_replacement_and_cancel_restores_it() {
        let mut model = ColumnModel {
            columns: vec![Column {
                tiles: vec![1, 2],
                width: ColumnWidth::default(),
            }],
            focused: Some(1),
            ..ColumnModel::default()
        };
        model.set_metrics(1_000, 10);
        model.set_column_height(800);
        assert_eq!(model.resize_rows_by_pixels(&1, &2, 100), Some(true));

        assert!(model.begin_drag(&1));
        model.reconcile([2]);
        model.finish_drag(Some(&3), ColumnDropTarget::Above(2));
        assert_eq!(tile_columns(&model), [vec![3, 2]]);
        assert_eq!(model.tile_weight(&3), 495.0);

        assert!(model.begin_drag(&3));
        model.reconcile([2]);
        model.cancel_drag();
        assert_eq!(tile_columns(&model), [vec![3, 2]]);
        assert_eq!(model.tile_weight(&3), 495.0);
        assert_eq!(model.focused, Some(3));
    }

    #[test]
    fn centering_uses_each_columns_virtual_center_without_clamping_edges() {
        let mut model = ColumnModel {
            columns: vec![
                Column {
                    tiles: vec![1],
                    width: ColumnWidth::new(0.33),
                },
                Column {
                    tiles: vec![2, 3],
                    width: ColumnWidth::new(0.5),
                },
                Column {
                    tiles: vec![4],
                    width: ColumnWidth::new(0.66),
                },
            ],
            ..ColumnModel::default()
        };
        model.set_metrics(1_000, 10);
        let columns = tile_columns(&model);
        let widths = width_fractions(&model);

        assert!(model.center_column(&1));
        assert_viewport(&model, -335.0);
        assert!(!model.ensure_visible(&1));
        assert!(model.center_column(&3));
        assert_viewport(&model, 90.0);
        assert!(model.center_column(&4));
        assert_viewport(&model, 680.0);
        assert!(!model.centering_changes_viewport(&4));
        assert_eq!(tile_columns(&model), columns);
        assert_eq!(width_fractions(&model), widths);
        assert_eq!(model.focused, Some(4));
        assert!(!model.center_column(&99));
    }

    #[test]
    fn centering_supports_every_preset_and_continuous_width() {
        for fraction in [0.33, 0.5, 0.66, 1.0, 0.73] {
            let mut model = ColumnModel {
                columns: vec![Column {
                    tiles: vec![1],
                    width: ColumnWidth::new(fraction),
                }],
                ..ColumnModel::default()
            };
            model.set_metrics(1_000, 10);

            assert!(model.center_column(&1) || fraction == 1.0);
            assert_viewport(&model, (model.column_width(0) as f64 - 1_000.0) / 2.0);
        }
    }

    #[test]
    fn automatic_centering_tracks_exactly_one_modeled_tile() {
        let mut model = ColumnModel::default();
        model.reconcile([1]);
        model.set_metrics(1_000, 10);

        assert!(model.center_only_tile());
        assert_viewport(&model, -170.0);

        model.reconcile([1, 2]);
        assert_viewport(&model, 0.0);
        assert!(!model.center_only_tile());

        model.columns = vec![Column {
            tiles: vec![1, 2],
            width: ColumnWidth::new(0.66),
        }];
        assert!(!model.center_only_tile());

        model.reconcile([1]);
        assert!(model.center_only_tile());
        assert_viewport(&model, -170.0);
    }

    #[test]
    fn touchpad_pan_is_direct_reversible_and_clamped_to_centered_edges() {
        let mut model = ColumnModel::default();
        model.reconcile([1, 2, 3]);
        model.set_metrics(1_000, 10);
        let columns = tile_columns(&model);
        let widths = width_fractions(&model);

        assert!(model.pan_viewport(200.0));
        assert_viewport(&model, 200.0);
        assert!(model.pan_viewport(-50.0));
        assert_viewport(&model, 150.0);
        assert!(model.pan_viewport(-10_000.0));
        assert_viewport(&model, -170.0);
        assert!(model.pan_viewport(10_000.0));
        assert_viewport(&model, 1_170.0);

        assert_eq!(tile_columns(&model), columns);
        assert_eq!(width_fractions(&model), widths);
        assert_eq!(model.focused, None);
    }

    #[test]
    fn touchpad_position_survives_reconciliation_until_explicit_focus() {
        let mut model = ColumnModel::default();
        model.reconcile([1, 2, 3]);
        model.set_metrics(1_000, 10);
        assert!(model.pan_viewport(900.0));
        assert_viewport(&model, 900.0);

        model.reconcile([1, 2, 3]);
        model.set_metrics(1_000, 10);
        assert!(!model.ensure_preferred_visible(None, Some(&1)));
        assert_viewport(&model, 900.0);
        assert_eq!(model.focused, Some(1));

        assert!(model.ensure_preferred_visible(Some(&1), Some(&3)));
        assert_viewport(&model, 0.0);
        assert!(!model.user_positioned_viewport);
        assert_eq!(model.focused, Some(1));
    }

    #[test]
    fn touchpad_pan_rejects_invalid_deltas_and_centers_a_single_tile() {
        let mut model = ColumnModel::default();
        model.reconcile([1, 2]);
        model.set_metrics(1_000, 10);

        assert!(!model.pan_viewport(f64::NAN));
        assert!(!model.pan_viewport(f64::INFINITY));
        assert_viewport(&model, 0.0);

        model.reconcile([1]);
        assert!(model.pan_viewport(100.0));
        assert_viewport(&model, -170.0);
        assert!(!model.user_positioned_viewport);
    }

    #[test]
    fn mouse_resize_edges_are_limited_to_the_focused_columns_boundaries() {
        let geometry = Rectangle::new((0, 0).into(), (1_340, 800).into());
        let mut tree = Tree::new();
        let root = tree
            .insert(
                Node::new(Data::Group {
                    orientation: Orientation::Vertical,
                    sizes: vec![670, 670],
                    last_geometry: geometry,
                    alive: Arc::new(()),
                    pill_indicator: None,
                }),
                InsertBehavior::AsRoot,
            )
            .unwrap();
        let insert = |tree: &mut Tree<Data>, geometry| {
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
        let second = insert(
            &mut tree,
            Rectangle::new((670, 0).into(), (660, 800).into()),
        );
        let first = insert(&mut tree, Rectangle::new((0, 0).into(), (660, 800).into()));
        let mut scrolling = ScrollingLayout {
            model: ColumnModel {
                columns: vec![
                    Column::single(first.clone()),
                    Column::single(second.clone()),
                ],
                ..ColumnModel::default()
            },
            pending_center: None,
        };
        scrolling.model.set_metrics(1_340, 10);

        assert_eq!(
            scrolling.resize_edge_at(&tree, &first, 665.0),
            Some((ColumnResizeAnchor::Left, 660))
        );
        assert_eq!(
            scrolling.resize_edge_at(&tree, &second, 665.0),
            Some((ColumnResizeAnchor::Right, 660))
        );
        assert_eq!(
            scrolling.resize_edge_at(&tree, &second, 1_325.0),
            Some((ColumnResizeAnchor::Left, 1_330))
        );
        assert_eq!(scrolling.resize_edge_at(&tree, &first, 1_325.0), None);
    }

    #[test]
    fn vertical_resize_hit_testing_only_accepts_focused_adjacent_separators() {
        let geometry = Rectangle::new((0, 0).into(), (1_340, 800).into());
        let mut tree = Tree::new();
        let root = tree
            .insert(
                Node::new(Data::Group {
                    orientation: Orientation::Vertical,
                    sizes: vec![670, 670],
                    last_geometry: geometry,
                    alive: Arc::new(()),
                    pill_indicator: None,
                }),
                InsertBehavior::AsRoot,
            )
            .unwrap();
        let insert = |tree: &mut Tree<Data>, geometry| {
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
        let top = insert(&mut tree, Rectangle::new((0, 0).into(), (660, 395).into()));
        let bottom = insert(
            &mut tree,
            Rectangle::new((0, 405).into(), (660, 395).into()),
        );
        let other = insert(
            &mut tree,
            Rectangle::new((670, 0).into(), (660, 800).into()),
        );
        let mut scrolling = ScrollingLayout {
            model: ColumnModel {
                columns: vec![
                    Column {
                        tiles: vec![top.clone(), bottom.clone()],
                        width: ColumnWidth::default(),
                    },
                    Column::single(other),
                ],
                ..ColumnModel::default()
            },
            pending_center: None,
        };
        scrolling.model.set_metrics(1_340, 10);
        scrolling.model.set_column_height(800);

        assert_eq!(
            scrolling.row_resize_edge_at(&tree, &top, (100.0, 400.0).into()),
            Some((top.clone(), bottom.clone(), 395))
        );
        assert_eq!(
            scrolling.row_resize_edge_at(&tree, &bottom, (100.0, 400.0).into()),
            Some((top.clone(), bottom.clone(), 395))
        );
        assert_eq!(
            scrolling.row_resize_edge_at(&tree, &top, (700.0, 400.0).into()),
            None
        );
        assert_eq!(
            scrolling.row_resize_edge_at(&tree, &top, (100.0, 800.0).into()),
            None
        );
    }

    #[test]
    fn first_and_last_columns_become_fully_visible() {
        let mut model = ColumnModel::default();
        model.reconcile([1, 2, 3]);
        model.set_metrics(1_000, 10);

        assert!(model.ensure_visible(&3));
        assert_viewport(&model, 1_000.0);
        assert_eq!(model.layout_x(2), 1_340.0);
        assert_eq!(model.layout_x(2) + model.column_width(2) as f64, 2_000.0);

        assert!(model.ensure_visible(&1));
        assert_viewport(&model, 0.0);
        assert_eq!(model.layout_x(0), 0.0);
    }

    #[test]
    fn empty_model_resets_viewport_focus_and_navigation_safely() {
        let mut model = ColumnModel::default();
        model.reconcile([1, 2, 3]);
        model.set_metrics(1_000, 10);
        assert!(model.ensure_visible(&3));

        model.reconcile([]);

        assert!(model.columns.is_empty());
        assert_eq!(model.focused, None);
        assert_viewport(&model, 0.0);
        assert_eq!(model.strip_width(), 0.0);
        assert_eq!(model.adjacent_column(&1, 1), None);
        assert_eq!(model.adjacent_tile(&1, 1), None);
        assert!(!model.ensure_visible(&1));
        assert!(!model.move_by(&1, 1));

        model.set_metrics(1_000, -10);
        assert_eq!(model.gap, 0);
        assert_viewport(&model, 0.0);
    }

    #[test]
    fn scrolling_order_and_widths_survive_a_classic_round_trip() {
        let mut model = ColumnModel::default();
        model.reconcile([1, 2, 3]);
        model.columns[0].width = ColumnWidth::new(0.4);
        model.columns[1].width = ColumnWidth::new(0.7);
        model.columns[2].width = ColumnWidth::new(0.5);
        assert!(model.begin_drag(&2));
        model.finish_drag(Some(&2), ColumnDropTarget::After(3));
        assert_eq!(tile_columns(&model), [[1], [3], [2]]);

        // While classic tiling is active the scrolling model is intentionally
        // left dormant. On return, surviving columns keep their relative
        // order and widths, while newly mapped leaves append with the default.
        model.reconcile([3, 4, 2, 1]);
        assert_eq!(tile_columns(&model), [[1], [3], [2], [4]]);
        assert_eq!(
            width_fractions(&model),
            [0.4, 0.5, 0.7, DEFAULT_COLUMN_FRACTION]
        );
    }

    #[test]
    fn multi_tile_order_and_widths_survive_a_classic_round_trip() {
        let mut model = ColumnModel {
            columns: vec![
                Column {
                    tiles: vec![1, 2, 3],
                    width: ColumnWidth::new(0.4),
                },
                Column {
                    tiles: vec![4, 5],
                    width: ColumnWidth::new(0.8),
                },
            ],
            ..ColumnModel::default()
        };

        model.reconcile([5, 3, 2, 4, 1, 6]);

        assert_eq!(tile_columns(&model), [vec![1, 2, 3], vec![4, 5], vec![6]]);
        assert_eq!(width_fractions(&model), [0.4, 0.8, DEFAULT_COLUMN_FRACTION]);
    }

    #[test]
    fn scrolling_group_bounds_preserve_classic_split_ratios() {
        fn group(sizes: Vec<i32>, geometry: Rectangle<i32, Local>) -> Data {
            Data::Group {
                orientation: Orientation::Vertical,
                sizes,
                last_geometry: geometry,
                alive: Arc::new(()),
                pill_indicator: None,
            }
        }

        let classic_geometry = Rectangle::new((0, 0).into(), (1_000, 600).into());
        let mut tree = Tree::new();
        let root = tree
            .insert(
                Node::new(group(vec![300, 700], classic_geometry)),
                InsertBehavior::AsRoot,
            )
            .unwrap();
        let left = tree
            .insert(
                Node::new(group(Vec::new(), classic_geometry)),
                InsertBehavior::UnderNode(&root),
            )
            .unwrap();
        let right = tree
            .insert(
                Node::new(group(Vec::new(), classic_geometry)),
                InsertBehavior::UnderNode(&root),
            )
            .unwrap();
        let node_ids = vec![root.clone(), left.clone(), right.clone()];

        for _ in 0..3 {
            let mut geometries = HashMap::from([
                (
                    left.clone(),
                    Rectangle::new((0, 0).into(), (360, 600).into()),
                ),
                (
                    right.clone(),
                    Rectangle::new((370, 0).into(), (840, 600).into()),
                ),
            ]);
            update_group_bounds(&mut tree, &node_ids, &mut geometries);
            tree.get_mut(&root)
                .unwrap()
                .data_mut()
                .update_geometry(classic_geometry);

            let Data::Group {
                sizes,
                last_geometry,
                ..
            } = tree.get(&root).unwrap().data()
            else {
                unreachable!()
            };
            assert_eq!(sizes, &[300, 700]);
            assert_eq!(*last_geometry, classic_geometry);
        }
    }

    #[test]
    fn viewport_keeps_inner_gap_at_every_workspace_boundary() {
        let usable = Rectangle::new((0, 32).into(), (1_920, 1_000).into());

        let viewport = scrolling_viewport(usable, (4, 8));

        assert_eq!(
            viewport,
            Rectangle::new((12, 44).into(), (1_896, 976).into())
        );
        assert_eq!(viewport.loc.x - usable.loc.x, 12);
        assert_eq!(
            usable.size.w - viewport.size.w - (viewport.loc.x - usable.loc.x),
            12
        );
        assert_eq!(viewport.loc.y - usable.loc.y, 12);
        assert_eq!(
            usable.size.h - viewport.size.h - (viewport.loc.y - usable.loc.y),
            12
        );
    }
}
