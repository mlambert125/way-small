//! Tests for the output → workspace → window hierarchy: which workspaces
//! exist, which window is in which, and what happens to the windows of an
//! output that goes away.

use crate::compositor::state::ClientObjectId;
use crate::compositor::workspace::{CASCADE_START, CASCADE_STEP, Workspaces};
use crate::shared::{
    OUTPUT_MODE_CURRENT, Output, OutputGeometry, OutputId, OutputMode, OutputSubpixel,
    OutputTransform,
};

const FIRST: OutputId = OutputId(1);
const SECOND: OutputId = OutputId(2);
const WINDOW: ClientObjectId = (1, 10);
const OTHER_WINDOW: ClientObjectId = (1, 20);

/// A 100x100 output at the given x.
fn output(id: OutputId, x: i32) -> Output {
    Output {
        id,
        geometry: OutputGeometry {
            x,
            y: 0,
            physical_width: 100,
            physical_height: 100,
            subpixel: OutputSubpixel::None,
            make: String::new(),
            model: String::new(),
            transform: OutputTransform::Normal,
        },
        modes: vec![OutputMode {
            flags: OUTPUT_MODE_CURRENT,
            width: 100,
            height: 100,
            refresh_mhz: 60000,
        }],
        scale: 1,
        name: String::new(),
        description: String::new(),
    }
}

/// Two outputs, each with the workspace it starts life with.
fn two_outputs() -> (Vec<Output>, Workspaces) {
    let outputs = vec![output(FIRST, 0), output(SECOND, 100)];
    let mut workspaces = Workspaces::new();
    workspaces.sync_outputs(&outputs);
    (outputs, workspaces)
}

#[test]
fn every_output_starts_with_one_workspace() {
    let (_, workspaces) = two_outputs();

    assert_eq!(workspaces.iter().count(), 2);
    for id in [FIRST, SECOND] {
        let entry = workspaces.get(id).expect("output should have workspaces");
        assert_eq!(entry.iter().count(), 1);
        assert!(entry.active().is_some(), "and one of them is showing");
    }
    // Ids are unique across outputs, not just within one.
    let first = workspaces.active(FIRST).expect("no workspace").id;
    let second = workspaces.active(SECOND).expect("no workspace").id;
    assert_ne!(first, second);
}

#[test]
fn syncing_the_same_outputs_again_changes_nothing() {
    let (outputs, mut workspaces) = two_outputs();
    let ids: Vec<_> = workspaces.iter().map(|(_, w)| w.id).collect();

    assert!(
        !workspaces.sync_outputs(&outputs),
        "an unchanged set of outputs is not a change"
    );

    let after: Vec<_> = workspaces.iter().map(|(_, w)| w.id).collect();
    assert_eq!(ids, after, "existing workspaces are kept, not rebuilt");
}

#[test]
fn a_window_is_in_one_workspace_at_a_time() {
    let (_, mut workspaces) = two_outputs();

    assert!(workspaces.place(FIRST, WINDOW));
    assert_eq!(workspaces.output_of(WINDOW), Some(FIRST));

    // Moving it to the other output takes it off the first, rather than
    // leaving it on both and letting them disagree about where it is.
    assert!(workspaces.place(SECOND, WINDOW));
    assert_eq!(workspaces.output_of(WINDOW), Some(SECOND));
    assert!(workspaces.visible_stack(FIRST).is_empty());
    assert_eq!(workspaces.visible_stack(SECOND), [WINDOW]);
}

#[test]
fn a_window_cannot_be_placed_on_an_output_the_compositor_has_never_seen() {
    let (_, mut workspaces) = two_outputs();

    assert!(!workspaces.place(OutputId(99), WINDOW));
    assert_eq!(workspaces.output_of(WINDOW), None);
}

#[test]
fn raising_reorders_only_the_windows_own_workspace() {
    let (_, mut workspaces) = two_outputs();
    workspaces.place(FIRST, WINDOW);
    workspaces.place(FIRST, OTHER_WINDOW);
    workspaces.place(SECOND, (2, 30));
    assert_eq!(workspaces.visible_stack(FIRST), [WINDOW, OTHER_WINDOW]);

    workspaces.raise(WINDOW);

    assert_eq!(workspaces.visible_stack(FIRST), [OTHER_WINDOW, WINDOW]);
    assert_eq!(workspaces.visible_stack(SECOND), [(2, 30)]);
}

#[test]
fn an_output_that_goes_away_gives_its_windows_back() {
    let (outputs, mut workspaces) = two_outputs();
    workspaces.place(FIRST, WINDOW);
    workspaces.place(SECOND, OTHER_WINDOW);

    // The first output is unplugged, taking its workspace with it.
    let remaining: Vec<Output> = outputs.into_iter().filter(|o| o.id != FIRST).collect();
    assert!(workspaces.sync_outputs(&remaining));

    assert_eq!(workspaces.output_of(WINDOW), None);
    assert_eq!(
        workspaces.take_unplaced(),
        vec![WINDOW],
        "its window is handed back to be re-homed"
    );
    // The output that is left keeps its own workspace and window untouched.
    assert_eq!(workspaces.visible_stack(SECOND), [OTHER_WINDOW]);
}

#[test]
fn a_window_held_aside_is_not_on_any_output_and_is_not_visible() {
    let mut workspaces = Workspaces::new();
    workspaces.hold_unplaced(WINDOW);
    workspaces.hold_unplaced(WINDOW);

    assert_eq!(workspaces.output_of(WINDOW), None);
    assert!(!workspaces.is_visible(WINDOW));
    assert_eq!(
        workspaces.take_unplaced(),
        vec![WINDOW],
        "held once, however many times it was offered"
    );
    assert!(workspaces.take_unplaced().is_empty(), "and taken only once");
}

#[test]
fn a_windows_last_trace_goes_with_the_client() {
    let (_, mut workspaces) = two_outputs();
    workspaces.place(FIRST, WINDOW);
    workspaces.place(SECOND, (2, 30));
    workspaces.hold_unplaced((1, 99));

    workspaces.remove_client(1);

    assert!(workspaces.visible_stack(FIRST).is_empty());
    assert!(workspaces.take_unplaced().is_empty());
    assert_eq!(
        workspaces.visible_stack(SECOND),
        [(2, 30)],
        "another client's window stays put"
    );
}

#[test]
fn the_cascade_restarts_rather_than_running_off_the_output() {
    let (_, mut workspaces) = two_outputs();
    let workspace = workspaces.active_mut(FIRST).expect("no workspace");

    assert_eq!(
        workspace.next_cascade_slot(1000, 1000),
        (CASCADE_START, CASCADE_START)
    );
    assert_eq!(
        workspace.next_cascade_slot(1000, 1000),
        (CASCADE_START + CASCADE_STEP, CASCADE_START + CASCADE_STEP)
    );
    // Past the limit the cascade wraps back to where it began instead of
    // placing the window off the edge.
    assert_eq!(
        workspace.next_cascade_slot(CASCADE_START, CASCADE_START),
        (CASCADE_START, CASCADE_START)
    );
}

#[test]
fn each_workspace_cascades_from_its_own_top_left() {
    let (_, mut workspaces) = two_outputs();

    let first = workspaces
        .active_mut(FIRST)
        .expect("no workspace")
        .next_cascade_slot(1000, 1000);
    let second = workspaces
        .active_mut(SECOND)
        .expect("no workspace")
        .next_cascade_slot(1000, 1000);

    assert_eq!(
        first, second,
        "one workspace's windows do not push another's"
    );
}
