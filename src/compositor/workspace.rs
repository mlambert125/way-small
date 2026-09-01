//! Workspaces: the layer between an output and the windows on it.
//!
//! An output owns one or more workspaces, and a workspace owns the toplevel
//! windows shown together on it. Exactly one workspace per output is on screen
//! at a time, so the windows of the others stay in state — positions, stacking
//! and all — while being drawn nowhere and hit-testable by nothing.
//!
//! Every output gets one workspace by default and nothing yet creates a
//! second, but the collection is a `Vec` rather than a single field so that
//! adding, removing and switching workspaces later is a matter of policy
//! rather than another change of shape.
//!
//! A window lives in exactly one workspace, and that membership is the only
//! record of which output it is on: the workspace holding it belongs to one
//! output, so there is nowhere for a second, disagreeing answer to be stored.
//! Windows mapped while there is no output at all are held aside as unplaced
//! and re-homed as soon as one appears.

use super::protocol::state::ClientObjectId;
use crate::shared::{Output, OutputId};

#[cfg(test)]
mod tests;

/// Where the first window on a workspace is placed, relative to its output.
const CASCADE_START: i32 = 50;
/// How far each subsequent window is offset from the last.
const CASCADE_STEP: i32 = 50;

/// A unique workspace id.
///
/// Drawn from a counter that never repeats, so an id belonging to a workspace
/// that has gone can never name a later one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkspaceId(pub u32);

/// One workspace: the windows shown together on an output.
#[derive(Debug)]
pub struct Workspace {
    /// The workspace id
    pub id: WorkspaceId,
    /// Toplevel draw order, bottom to top. Popups and subsurfaces are not
    /// here: they hang off a toplevel and travel with it.
    pub surface_stack: Vec<ClientObjectId>,
    /// Where the next window opening here is placed, in coordinates local to
    /// the output. Per workspace, so each one cascades from its own top-left.
    next_position: (i32, i32),
}

impl Workspace {
    /// Create an empty workspace with the given id
    fn new(id: WorkspaceId) -> Self {
        Self {
            id,
            surface_stack: Vec::new(),
            next_position: (CASCADE_START, CASCADE_START),
        }
    }

    /// The position for the next window to open here, in output-local
    /// coordinates, and advance the cascade.
    ///
    /// Restarts at the top-left rather than marching off the far edge of the
    /// output. The limits are the caller's to choose because only it knows how
    /// big the output currently is.
    pub fn next_cascade_slot(&mut self, limit_x: i32, limit_y: i32) -> (i32, i32) {
        let (x, y) = self.next_position;
        let (x, y) = if x >= limit_x || y >= limit_y {
            (CASCADE_START, CASCADE_START)
        } else {
            (x, y)
        };
        self.next_position = (x + CASCADE_STEP, y + CASCADE_STEP);
        (x, y)
    }

    /// Put a window on top of this workspace's stack, adding it if it is new.
    pub fn raise(&mut self, key: ClientObjectId) {
        self.surface_stack.retain(|k| *k != key);
        self.surface_stack.push(key);
    }

    /// The topmost window here, if any.
    pub fn top(&self) -> Option<ClientObjectId> {
        self.surface_stack.last().copied()
    }
}

/// The workspaces belonging to one output.
///
/// Never empty: an output always has the workspace it was given when it
/// appeared, so there is always somewhere for a window to go.
#[derive(Debug)]
pub struct OutputWorkspaces {
    /// The output these belong to.
    pub output: OutputId,
    /// The workspaces on this output, in the order they were created.
    workspaces: Vec<Workspace>,
    /// Index into `workspaces` of the one on screen.
    active: usize,
}

impl OutputWorkspaces {
    /// Start an output off with a single workspace.
    fn new(output: OutputId, first: Workspace) -> Self {
        Self {
            output,
            workspaces: vec![first],
            active: 0,
        }
    }

    /// The workspace currently on screen.
    pub fn active(&self) -> Option<&Workspace> {
        self.workspaces.get(self.active)
    }

    /// The workspace currently on screen, for modification.
    pub fn active_mut(&mut self) -> Option<&mut Workspace> {
        self.workspaces.get_mut(self.active)
    }

    /// Every workspace on this output, on screen or not.
    pub fn iter(&self) -> impl Iterator<Item = &Workspace> {
        self.workspaces.iter()
    }

    /// Every workspace on this output, for modification.
    fn iter_mut(&mut self) -> impl Iterator<Item = &mut Workspace> {
        self.workspaces.iter_mut()
    }
}

/// Every workspace the compositor has, arranged under the outputs that own
/// them, plus the windows that have nowhere to live yet.
#[derive(Debug, Default)]
pub struct Workspaces {
    /// One entry per output, in the order the outputs arrived.
    outputs: Vec<OutputWorkspaces>,
    /// Windows mapped before any output existed, or left behind by one that
    /// has gone. They are drawn nowhere until re-homed.
    unplaced: Vec<ClientObjectId>,
    /// Source of workspace ids. Only ever incremented.
    next_id: u32,
}

impl Workspaces {
    /// Create the collection for a compositor with no outputs yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Give every output a workspace, and take back the windows of any output
    /// that has gone. Returns true if anything changed.
    ///
    /// Idempotent, and cheap when nothing has: outputs come and go rarely, but
    /// this runs every frame so that a hotplug needs no separate path.
    pub fn sync_outputs(&mut self, outputs: &[Output]) -> bool {
        let mut changed = false;

        if self
            .outputs
            .iter()
            .any(|entry| !outputs.iter().any(|o| o.id == entry.output))
        {
            let (live, gone): (Vec<_>, Vec<_>) = std::mem::take(&mut self.outputs)
                .into_iter()
                .partition(|entry| outputs.iter().any(|o| o.id == entry.output));
            self.outputs = live;
            for entry in gone {
                for workspace in entry.workspaces {
                    self.unplaced.extend(workspace.surface_stack);
                }
            }
            changed = true;
        }

        for output in outputs {
            if self.outputs.iter().any(|entry| entry.output == output.id) {
                continue;
            }
            let id = self.allocate_id();
            self.outputs
                .push(OutputWorkspaces::new(output.id, Workspace::new(id)));
            changed = true;
        }

        changed
    }

    /// Hand out the next workspace id.
    fn allocate_id(&mut self) -> WorkspaceId {
        self.next_id += 1;
        WorkspaceId(self.next_id)
    }

    /// The workspaces of one output.
    pub fn get(&self, output: OutputId) -> Option<&OutputWorkspaces> {
        self.outputs.iter().find(|entry| entry.output == output)
    }

    /// The workspace on screen on one output.
    pub fn active(&self, output: OutputId) -> Option<&Workspace> {
        self.get(output).and_then(OutputWorkspaces::active)
    }

    /// The workspace on screen on one output, for modification.
    pub fn active_mut(&mut self, output: OutputId) -> Option<&mut Workspace> {
        self.outputs
            .iter_mut()
            .find(|entry| entry.output == output)
            .and_then(OutputWorkspaces::active_mut)
    }

    /// The windows drawn on one output, bottom to top. Empty for an output
    /// that has no workspaces, which is one the compositor has not seen.
    pub fn visible_stack(&self, output: OutputId) -> &[ClientObjectId] {
        match self.active(output) {
            Some(workspace) => &workspace.surface_stack,
            None => &[],
        }
    }

    /// Every workspace, with the output it belongs to.
    pub fn iter(&self) -> impl Iterator<Item = (OutputId, &Workspace)> {
        self.outputs
            .iter()
            .flat_map(|entry| entry.iter().map(move |w| (entry.output, w)))
    }

    /// Every window the compositor is managing, with the output it is on.
    ///
    /// Unplaced windows are not here: they are on no output, which is exactly
    /// what makes them unplaced.
    pub fn windows(&self) -> impl Iterator<Item = (OutputId, ClientObjectId)> {
        self.iter()
            .flat_map(|(output, w)| w.surface_stack.iter().map(move |&key| (output, key)))
    }

    /// Which output and workspace a window is in.
    pub fn find(&self, key: ClientObjectId) -> Option<(OutputId, WorkspaceId)> {
        self.iter()
            .find(|(_, w)| w.surface_stack.contains(&key))
            .map(|(output, w)| (output, w.id))
    }

    /// Which output a window is on, if it is on one.
    pub fn output_of(&self, key: ClientObjectId) -> Option<OutputId> {
        self.find(key).map(|(output, _)| output)
    }

    /// Whether a window is in a workspace that is currently on screen.
    pub fn is_visible(&self, key: ClientObjectId) -> bool {
        self.outputs
            .iter()
            .filter_map(OutputWorkspaces::active)
            .any(|w| w.surface_stack.contains(&key))
    }

    /// Put a window on top of the workspace showing on an output, taking it
    /// out of wherever it was before. Returns false if that output has no
    /// workspaces, in which case the window has not been moved.
    pub fn place(&mut self, output: OutputId, key: ClientObjectId) -> bool {
        if self.active(output).is_none() {
            return false;
        }
        self.remove(key);
        if let Some(workspace) = self.active_mut(output) {
            workspace.raise(key);
        }
        true
    }

    /// Raise a window to the top of its own workspace. Does nothing for a
    /// window that is in none.
    pub fn raise(&mut self, key: ClientObjectId) {
        for workspace in self.workspaces_mut() {
            if workspace.surface_stack.contains(&key) {
                workspace.raise(key);
                return;
            }
        }
    }

    /// Take a window out of whatever workspace holds it.
    pub fn remove(&mut self, key: ClientObjectId) {
        for workspace in self.workspaces_mut() {
            workspace.surface_stack.retain(|k| *k != key);
        }
        self.unplaced.retain(|k| *k != key);
    }

    /// Take out every window belonging to a client that has gone.
    pub fn remove_client(&mut self, client_id: u32) {
        for workspace in self.workspaces_mut() {
            workspace.surface_stack.retain(|(cid, _)| *cid != client_id);
        }
        self.unplaced.retain(|(cid, _)| *cid != client_id);
    }

    /// Hold a window aside until there is an output to put it on.
    pub fn hold_unplaced(&mut self, key: ClientObjectId) {
        if !self.unplaced.contains(&key) {
            self.unplaced.push(key);
        }
    }

    /// Take the windows waiting for somewhere to live, leaving none behind.
    pub fn take_unplaced(&mut self) -> Vec<ClientObjectId> {
        std::mem::take(&mut self.unplaced)
    }

    /// Every workspace on every output, for modification.
    fn workspaces_mut(&mut self) -> impl Iterator<Item = &mut Workspace> {
        self.outputs.iter_mut().flat_map(OutputWorkspaces::iter_mut)
    }
}
