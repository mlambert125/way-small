# Way Small

## Goals

This is a small wayland compositor written in rust "from scratch." 

The primary goals:

- To implement the wayland protocol, and a complete compositor without relying on an existing library like
  smithay or wlroots.
- To take advantage of modern rust features that were not exploited in earlier wayland libraries.
- To avoid unnecessary abstractions and layers of indirection, and to embrace the reality of both the wayland
  protocol and rust as they are, rather than trying to shoehorn them into a particular design pattern or
  architecture (e.g. OOP, ECS, etc).
- To demonstrate good stewardship of an open source software project, including:
    - Documentation, including design documentation and code comments.
    - Testing, including unit tests, integration tests, and end-to-end tests.
    - Code quality, including readability, maintainability, and adherence to rust idioms and best practices.

## Source Layout

Each subsystem is a module folder under `src/`, and the folders match the sections below:

```
src/
  main.rs            wiring: config, channels, task startup
  shared/            the vocabulary the subsystems share
    scene.rs         Frame, Scene, SceneElement
    texture.rs       what a backend uploads or imports
    buffer.rs        client buffer memory, and its SIGBUS net
    dmabuf.rs        client GPU buffers, described for import
    output.rs        how a backend describes displays
  wayland_socket/    the socket subsystem
  compositor/        the event loop
    scene.rs         scene building
    workspace.rs     outputs, their workspaces, and the windows on them
    protocol/        request dispatch and all compositor state
  backend/           each backend, and the GL renderer they share
    dmabuf.rs        importing client GPU buffers through EGL
```

Every subsystem is a folder with a `mod.rs`, whether or not it has grown a second file yet.

`protocol/` sits inside `compositor/` because it is part of that subsystem, not a peer of it: the compositor task is the
only thing that touches protocol state, and `CompositorState` itself lives there.

`shared/` sits above the subsystems because it belongs to none of them. `Frame` is produced by the compositor and
consumed by a backend; `BackendMessage` goes the other way. Putting either in one subsystem makes the other depend on
its internals for a type it owns half of, which is how the backend previously ended up reaching into the compositor's
protocol state for `Output` and `BufferGuard`. `shared/` depends on nothing else in the crate, and everything else
depends on it:

```
shared  <-  backend
shared  <-  compositor  ->  wayland_socket
```

`compositor` and `backend` no longer reference each other at all — they meet only in `shared`.

Tests live in their own `tests.rs` beside the code they cover, declared with `#[cfg(test)] mod tests;`. A child module
still sees its parent's private items, so nothing has to be made public to test it, and the source files stay free of
fixtures.

## Top Level Architecture

At the very top level, there are several subsystems (tokio tasks), that communicate with each other via channels.
The subsystems are:

- **Wayland Socket**: responsible for accepting new clients and pulling raw wayland messages from the socket with
  associated file descriptors, and sending them to the compositor subsytem for processing.
- **Compositor**: responsible for processing wayland messages, managing the state of the compositor, sending messsages
  back to clients, and sending and receiving messages to and from the backend subsystem.
- **Backend**: responsible for displaying the compositors output and capturing input events and sending them to the
  compositor subsystem.

This allows for a clean separation of concerns, and allows each subsystem to be developed and tested
independently. This relies heavily on rust's async features and channels for communication between subsystems,
which allows for a clean and efficient design.

Some shared state between subsystems is still necessary for performance reasons, and these generally use `Arc<Mutex<T>>`
to allow for sending large and/or mutable state across subsystem boundaries without the need for complex synchronization
primitives. This is kept to a minimum.

### Wayland Socket

The wayland socket subsystem is responsible for accepting new wayland clients, reading wayland messages off of
a Unix socket and dispatching those messages to the compositor subsystem.

When a new client connects to the wayland socket, a new client struct is created with an ID and sent to the compositor
subsystem so that it can correalate later messages with the client.  The client struct contains a channel for
sending messages back on the client socket, a queue of file descriptors currently pending from the client, other
metadata about the client, and state specific to the client that is maintained by the compositor subsystem.

```rust
pub struct ClientState {
    // Data from the wayland socket subsystem:
    pub sender: Sender<WaylandProtocolMessage>,     // <-- Channel for sending messages back to the client socket
    pub fd_queue: Arc<Mutex<VecDeque<i32>>>,        // <-- Queue of pending file descriptors from the client

    // State managed by the compositor subsystem:
    pub objects: HashMap<u32, ObjectType>,
    pub object_versions: HashMap<u32, u32>,

    // ...
}
```

After the initial connection, the wayland socket subsystem continuously reads from the socket for each client.

A single read on the socket may contain one or more messages, so this subsystem breaks the payload into individual
messages before dispatching them to the compositor subsystem.

```rust
pub struct WaylandProtocolMessage {
    pub object_id: u32,
    pub op_code: u16,
    pub args: Vec<u8>,
    pub fds: Vec<OwnedFd>,  // Always empty for inbound messages, see "File Descriptors" section below
}
```

Each of these messages represents a single wayland message that the compositor should process and possibly
respond to through the client's sender channel.

#### Outgoing Messages and Unresponsive Clients

The compositor subsystem is a single task that owns all compositor state, so it must never block. In particular it
must never wait on a client: a client that stops reading its socket will fill the kernel's socket buffer, which stalls
that client's send task, which in turn fills the send channel. If the compositor awaited that channel, one wedged
client would freeze input handling, rendering, and every other client along with it.

Sends to a client are therefore non-blocking. `ClientState::send` uses `try_send` and, when a client's send channel
reaches `CLIENT_SEND_QUEUE_LIMIT` messages, the client is assumed to be wedged and is disconnected rather than waited
on. This is the same policy libwayland applies once a client's output buffer grows past its threshold.

Frames follow the same rule for the same reason.  The compositor publishes each frame into a single-slot channel and
never awaits it, so a backend that has fallen behind cannot stall protocol handling.  Replacing the slot's contents is
also the behaviour you want on its own merits: a frame the backend has not picked up yet has been overtaken by a newer
one, and presenting it would only put a stale frame in front of a fresh one.  A queue of frames is never useful.

Disconnection is signalled through a per-client `CancellationToken`, created as a child of the global shutdown token
and handed to the compositor in the `WaylandNewClientMessage`. Cancelling it stops that client's read and send tasks
without affecting any other client; the read task then emits the usual `ClientDisconnected` message so the compositor
cleans up the client's resources through the normal path.

Because sends cannot block, the entire protocol handling layer is synchronous. This keeps the message handlers plain
state-machine code that is straightforward to unit test without a runtime.


#### File Descriptors

Some wayland messages include one or more accompanying file descriptors that refer to shared memory buffers
that are shared between the compositor and the client application.  These file descriptors are not sent in the
normal socket stream, but are sent out-of-band as ancillary data on the unix socket.  The rust sendfd library
is used to provide unix sockets that include this ancillary data so that we can get the file descriptors with
the messages.

By specification, each file descriptor is associated with a particular message.  However, the wayland socket
subsystem can not associate the file descriptors with a particular message without parsing the message and
considering compositor state, which is the responsibility of the compositor subsystem.

Instead, file descriptors are placed on a queue in the client struct as they are received, and the compositor
subsystem associates them with the correct message when processing messages from that client.  Messages are
processed in order, so this works as long as exactly the right number of file descriptors is pulled from the queue
for each message.

That last condition is the fragile part, so it is enforced in one place rather than left to individual handlers.
`request_fd_count` in `protocol/mod.rs` is a table of how many file descriptors each request carries, and
`handle_message` applies it to every request before dispatch: it moves that many descriptors off the queue into a
`Vec<OwnedFd>` and passes them to the handler.  Only the interfaces that can actually receive a descriptor take that
parameter.

Making this the dispatcher's job rather than each handler's is what keeps the accounting honest.  A handler that
ignores its descriptors, returns early, or is an unimplemented stub simply drops the `Vec<OwnedFd>`, which closes
them.  It cannot leave a descriptor on the queue to be mispaired with some later request, which would otherwise
corrupt every remaining file descriptor on that connection — a failure that surfaces far away from its cause.

Adding a new request that takes an `fd` argument therefore means adding it to `request_fd_count` and widening its
interface's arm in `handle_message`.  Forgetting the arm closes the descriptor and makes the request a no-op;
forgetting the table entry desyncs the queue, which is why the table carries a comment saying so.

`OwnedFd` is used throughout rather than a raw `i32` so that ownership is explicit and closing is automatic.  The
one `unsafe` conversion happens where the kernel hands the descriptors over, in the socket read loop.

#### Ordering

Treating a missing file descriptor as a client protocol violation — which the compositor does, by disconnecting the
client — relies on a descriptor never arriving *after* the message it belongs to.  It cannot: for a `SOCK_STREAM`
socket the kernel stops a `recvmsg` at the boundary where ancillary data is attached, so a descriptor always arrives
with the first byte of its own `sendmsg`.  The read loop then queues the descriptors from a read before forwarding
that read's messages to the compositor.  A descriptor may arrive early, batched ahead of its message, but never
late.

Note that the ancillary buffer is fixed at `MAX_FDS_IN` (28, matching libwayland's `MAX_FDS_OUT`).  A client that
attaches more than that to a single `sendmsg` overflows it, and the kernel closes the excess rather than requeueing
it.  The `sendfd` crate does not surface `MSG_CTRUNC`, so this would be silent.

#### Limits

Requests that take no file descriptor never drain the queue, so a client that attaches descriptors to them would
grow it without bound.  Because descriptors are a process-wide resource, that is not merely the offending client's
problem: exhausting `RLIMIT_NOFILE` would break every other client, along with `mmap` and `memfd_create`.  The queue
is therefore capped at `MAX_PENDING_FDS`, and a client that exceeds it is disconnected and its queued descriptors
closed.  libwayland bounds the same queue the same way.

The cap is set well above any legitimate burst.  Only two requests carry descriptors, and the compositor drains its
entire message channel on each pass of its loop, so a well-behaved client never accumulates more than a handful.

Received descriptors are marked close-on-exec as they arrive.  `sendfd` does not pass `MSG_CMSG_CLOEXEC`, so this is
not atomic with the `recvmsg`, but way-small never forks, so there is no window in which they could leak into a
child.  The keymap memfd the compositor creates for `wl_keyboard.keymap` is likewise created with `MFD_CLOEXEC`.

### Compositor

The compositor subsystem is responsible for processing wayland messages from the wayland socket subsystem and input
events from the backend system.  It maintains the state of the compositor based on the messages and events received
and sends messages back to clients through the wayland socket subsystem and graphical frame data to the backend
subsystem as necessary.

The compositor is the center of the system, and basically what makes the compositor unique.

### Backend

The backend subsystem has a few implementations for different scenarios.  The backend is responsible for displaying
the compositor's output and capturing input events and sending them to the compositor subsystem.  The backend connects
the compositor to a set of I/O devices so that users can see and interact with the compositor.

Every backend that displays anything is responsible for rasterising the scenes it receives.  The GL renderer that does
this is shared rather than reimplemented per backend: it takes a scene and a drawable size and issues one textured quad
per element, keeping a GPU texture per texture id so an unchanged surface costs no upload.  What differs between
backends is only how the GL context and its drawable are obtained.

Textures are cached per texture id and evicted against the whole frame rather than a single output's scene: with more
than one output the same buffer can appear in one scene and not another, and evicting per scene would drop and re-upload
it every frame.

The drawable size comes from the backend, never from the scene.  During a resize the two disagree for a frame or two —
the backend learns its new size before the compositor has composed for it — and the backend's answer is the correct one.

#### Asking the backend a question

Messages run backend to compositor.  A few questions run the other way, and `BackendRequest` is that direction: the
compositor asks, and the answer arrives later as an ordinary `BackendMessage`.  It is deliberately not a call.  The
compositor task blocks on nothing, and what makes these questions worth asking at all is that only the backend thread
can answer them — anything touching the GL context has to run there, because a GL context belongs to one thread and
cannot be borrowed across.

A hosted backend may not be able to answer immediately: winit has no context until its window exists, so a request that
arrives first is remembered and answered from `resumed`.  That shape is the point rather than an accident of startup —
it is the same shape a client's `zwp_linux_buffer_params_v1.create` will need, where the compositor cannot say whether a
buffer imported until the backend has tried.

#### dma-buf

A client that renders on the GPU has its pixels there already.  Handing them over as shm means reading them back,
copying them through a shared mapping, and uploading them again — three trips for pixels that never needed to leave the
card.  A dma-buf is the kernel's handle on that memory instead: the client passes a file descriptor, the backend imports
it as a texture through `EGL_EXT_image_dma_buf_import`, and nothing is copied.

A `TextureImage` is therefore a struct of what every texture has — id, size, and whether its alpha means anything —
plus a `TextureSource` for the part that differs.  The fields that only an upload can have, `previous_serial` and
`damage`, live inside that variant rather than beside it: an imported buffer has nothing to patch and nothing to patch
against, and putting them at the top would mean giving them a meaningless value for half the images that exist.  The
drawing path reads only the common fields and never asks which kind it has.

The compositor never imports anything.  It knows a buffer's size, so it can lay it out in a scene, and treats the rest
as opaque — the description travels to the backend as a `TextureSource::Dmabuf` and is imported there, on the thread
that owns the context, exactly as the shm path already divides the work.  For the renderer this is one more source of a
texture: a `TEXTURE_2D` bound to an imported `EGLImage` rather than one filled by `tex_image_2d`.  The `EGLImage` is
kept alive alongside the texture that samples it, so the import's lifetime is something the texture cache decides
rather than a side effect of a local going out of scope.

Two things differ from an upload and both are in the shader.  An uploaded client buffer is `[B, G, R, A]` going up as
`RGBA` and swizzled when sampled; an imported one is described to the driver by its real fourcc and comes back in the
right order already, so `u_swizzle` turns the fix off for it.  Damage does not apply either: a client drawing into a
dma-buf changes what the texture samples without anything crossing back through the compositor at all.

What the driver will accept is enumerated with `EGL_EXT_image_dma_buf_import_modifiers` and filtered down to what can
actually be drawn.  Modifiers marked `external_only` need `samplerExternalOES`, which this renderer has no program for,
and a format left with no modifiers after that filtering is dropped entirely — on Mesa that removes every YUV layout,
taking the list from 65 formats to 24.  A compositor that advertises what it cannot draw is worse than one that
advertises nothing: the client allocates against the list and then has nothing to fall back to.

For the same reason the format list is not taken on trust.  `EGL_MESA_image_dma_buf_export` runs the path backwards, so
the backend can make a real dma-buf out of a texture it filled itself, import that back through the production path,
and read the pixels out again.  That round trip runs once at startup and is the difference between "the entry points
are there" and "importing works" — a driver can advertise the extension and still refuse every buffer.

None of this is reachable by a client yet: `zwp_linux_dmabuf_v1` is not implemented, so nothing constructs a
`TextureSource::Dmabuf`.  What exists is the path underneath it, proven end to end.

#### Winit Backend

The winit backend provides a backend for running the compositor inside of a normal window hosted in an existing
compositor (or x session.)  This is useful for development and testing, as it allows us to run the compositor
without needing to set up a full wayland session.

It gets its GL context from glutin: an EGL display off the winit window, a GLES 3.0 context, and a window surface, all
created and made current on the winit thread.  Vsync is off, because the compositor already paces frames on its own
16ms timer and waiting for vblank here would only stall the thread handling input.

#### DRM Backend

The DRM backend provides a backend for running the compositor on a linux system without an existing compositor.  This
is the "real" backend that allows the compositor to be used as a standalone compositor for a linux system.  It uses
the Direct Rendering Manager (DRM) subsystem of the linux kernel to display the compositors output and capture input
events using the evdev and libinput libraries.  Its GL context comes from EGL on a gbm device rather than from a host
window, which is the same EGL the dma-buf import path above needs; the renderer above it is unchanged.

#### Null Backend

The null backend provides a backend that does not display anything and does not capture any input events.  This is
useful for testing and benchmarking, as it allows us to run the compositor without needing to set up any graphical
output or input devices.  It has no GL context and drops the scenes it is sent; there is no software rasteriser to fall
back to, so a backend that cannot get a context has nothing to display and shuts down rather than run blind.

## Compositor Architecture

The compositor keeps it's state in a single struct that is shared across the compositor subsystem.  This state includes
"global" state that is not specific to any particular client, and a HashMap of client IDs to client state for each
connected client.

### Global State

The global state primarily stores windows, surfaces, and other objects needed to track the items needed to track input
and compose and display output frames to the backend. This includes things like:

- A list of outputs (monitors) that the compositor is currently displaying on.
- A list of input devices that the compositor is currently capturing input from.
- The workspaces of each output, and the windows on each workspace, including their position, size, and other metadata.
- A list of surfaces that the compositor is currently managing, including their position, size, and other metadata.
- A list of shared memory buffers that the compositor is currently managing, including their file descriptors, size, and other metadata.

### Client State

The client state primarily stores objects that are specific to a particular client.  This includes the socket for
sending messages back to the client, a queue of pending file descriptors from the client, and a list of wayland objects
that the client has created and is using.

Wayland objects at this level are typically just an ID and a type.  These are primarily stored and kept to correalate
messages from a client with items that the client has previously created or requested.  The actual state of these objects
is usually stored in the global state as it is relevant to the compositor at large and needed for rendering and input
handling.  For example, a client may create a surface object, and the compositor will store the surface's state in the global
state, but the client state will just store the surface's ID and type so that it can correalate future messages from the client
with the surface object that the client created.

Storing the IDs and types of objects created by a client at client level also ensures that the client only has access to 
the objects that it has created or requested, and can not access or manipulate objects created by other clients.  It also
lets the compositor easily clean up all of a client's objects when the client disconnects by just looking at that client's state.

#### Object IDs

Clients choose the ids for the objects they create, and must not reuse one until the compositor has acknowledged the
previous object's destruction with `wl_display.delete_id`.  `ClientState::register` enforces that: reusing a live id
is a protocol error, so the client is sent an error and disconnected.

This matters because most compositor state is keyed by object id but lives in the *global* state rather than in the
client's object map — shm pools, surfaces, pointer and keyboard bindings.  Letting a reused id silently replace its
predecessor would leave that state in place with nothing pointing at it: unreachable to the client, and invisible to
the disconnect-time cleanup, which finds a client's resources by walking its object map.  An `mmap`ed pool stranded
this way would stay mapped for the compositor's lifetime.

`register` returns a `Result` rather than handling the error silently so that the check cannot be bypassed by
accident.  `Result` is `#[must_use]`, so a caller that creates an object without considering the rejected case is a
compiler warning rather than a leak discovered much later.  Callers only need to stop what they were doing; the error
and the disconnect have already been sent.

The converse invariant is what keeps well-behaved clients working: an id is removed from the object map and
`wl_display.delete_id` is sent together, in `ClientState::unregister`, and nowhere else.  A client is therefore told
an id is free at exactly the moment the compositor stops considering it live.

### Dispatch Loop

The main process of the compositor subsystem is a dispatch loop that waits for any of several events to occur, and handles those
events as they come in.  The events include:

- **Wayland Messages** - From a connected client, received through the wayland socket subsystem
- **Backend Messages** - Input events, output events (e.g. monitor hotplug events), and other messages from the backend subsystem
- **Render Events** - Triggered at 60 fps

#### Wayland Message Processing

Wayland messages are processed by looking at the message's object ID and op code, and dispatching to the appropriate handler
function based on the object type and op code.  The handler function will then update the compositor's state as necessary, and
send any messages back to the client through the wayland socket subsystem as necessary.

#### Backend Message Processing

Backend messages are processed by updating the compositor's state as necessary based on the message type and content, and
sending notifications to the appropriate clients based on focus, activation, etc.  For example, if the compositor receives
a keyboard input event from the backend, it will look at which client is currently focused for keyboard input, and then
will translate and send a wayland keyboard event message to that client through the wayland socket subsystem.

#### The pointer

Two kinds of motion reach the compositor. `MouseMovedTo` carries a position, which is what a hosted backend reports —
the host owns the pointer and says where it put it — and what a touchscreen or tablet produces. `MouseMovedBy` carries a
delta, which is what a mouse produces: at the `libinput` layer a mouse has no position at all, only movement. The
pointer position belongs to the compositor, not to any device.

Which means the compositor has to keep it somewhere useful. Every position, however it arrives, is constrained onto the
outputs. Without that a relative device walks the pointer off the desktop and it never returns, and the failure is
silent rather than loud: the cursor stops being drawn, hit testing finds nothing, and clicks land nowhere, with no edge
for the user to push against. It also keeps the position inside `i32`, which everything downstream converts to without
checking.

The constraint is the union of the outputs, not their bounding box. Two outputs of different heights side by side leave
a notch belonging to no display, and a pointer parked there would be exactly as lost. A position off every output snaps
to the nearest point of the nearest one.

#### Outputs, workspaces and windows

The three nest: an output owns one or more workspaces, and a workspace owns the toplevel windows shown together on it.
Exactly one workspace per output is on screen at a time, so the windows of the others keep their positions and stacking
order in state while being drawn nowhere and hit-testable by nothing.

Every output gets one workspace when it appears, and nothing yet creates a second — there is no hotkey to add, remove or
switch one. The collection is a `Vec` per output rather than a single field so that when there is, it is a question of
policy rather than another change of shape.

An `xdg_toplevel` lives in exactly one workspace, and that membership is the only record of which output it is on:
the workspace holding it belongs to one output, so there is nowhere for a second, disagreeing answer to be stored. A
window is confined to that output and never straddles two; popups and subsurfaces hang off a toplevel and travel with
it. Windows mapped while there is no output at all — the compositor hears about outputs from the backend, and a client
could in principle map a window first — are held aside as unplaced and re-homed as soon as one appears.

A window opens on the workspace showing on the output under the pointer, or on the first output if the pointer is not
over one. Placement cascades from the top-left, per workspace, restarting rather than marching off the far edge.
Positions are stored globally — so input hit-testing, subsurface trees and `wl_surface.enter` all keep working in one
coordinate space — but the scene for an output contains only the windows of the workspace it is showing, drawn relative
to its origin. That is what stops every output showing the same top-left corner of the desktop.

Windows are re-confined every tick, which covers a client resizing itself out of bounds, an output changing size, and an
output being unplugged: its workspaces go with it and their windows come back as unplaced, to be re-homed on the same
pass. A window larger than its output is pinned to the top-left, since no position fits and the top-left is the part
worth showing.

Stacking and focus follow the hierarchy rather than spanning it: each workspace has its own stack, and alt-tab cycles
within one — the workspace holding the focused window, or the one showing under the pointer. Tabbing into another
workspace would move focus to a window that is not on screen.

#### Interactive move and resize

Clients draw their own decorations — no decoration manager is advertised — so dragging a title bar or an edge reaches the
compositor as `xdg_toplevel.move` or `.resize`. Both start a *grab*: for as long as one is held the compositor owns the
pointer, and motion and buttons drive the window instead of reaching any client. The client is sent a pointer leave when
the grab begins, so it is not left drawing a hover state for a pointer it will hear no more about, and re-enters
naturally on the next motion after the grab ends.

A client may only start a grab off the back of real user input, or any client could seize the pointer whenever it liked.
The serial it quotes must be one minted for a `wl_pointer.button` press it was sent, that button must still be held, and
the window named must be its own.

A move keeps the grip point under the cursor. A resize is measured from where the drag began rather than accumulated
per event, so the window cannot drift, and the opposite edge stays put — dragging the left edge changes the width and the
origin together. Size changes are requests: the compositor sends `xdg_toplevel.configure` carrying the `resizing` state
along with the matching `xdg_surface.configure`, and the client resizes when it acknowledges. An unchanged size sends
nothing, so a drag does not flood the client.

A drag is clamped at both ends, from three sources, and the narrowest wins where they overlap:

- the compositor's own floor, enough to keep a title bar and its buttons reachable — that is what makes a window
  recoverable, since one dragged to nothing has no edge left to grab;
- the client's `set_min_size` and `set_max_size`, which say what it can actually render at;
- the size of the output the window is on, so a window can never be dragged larger than the display showing it. One a
  client has already made larger than its display is brought back within it by the first drag.

Where the three contradict each other the range widens rather than inverting, because a clamp needs a floor no higher
than its ceiling. A client insisting on a minimum larger than the display gets it: the compositor cannot make it render
smaller, and configuring a size it will refuse achieves nothing. A client whose minimum and maximum are equal has told us
it has one size, and a drag leaves it alone.

When a clamp bites, the origin is derived from the clamped size rather than the pointer, so the anchored edge stays
exactly where it was instead of sliding away.

The size hints are applied when they arrive rather than on the next commit as the protocol specifies. The difference is
only visible to a client that sets a limit and starts a resize before committing, and there is no other double-buffered
toplevel state to hang it off. A negative limit is refused with `invalid_size` rather than stored, since it would poison
every later resize. A minimum above the current maximum is *not* an error: the two arrive in separate requests, so a
client raising both would momentarily look inconsistent through no fault of its own.

Dragging a window towards another output hands it over rather than letting it straddle: the window moves to the
workspace showing on the pointer's output, and the tick's confinement then pulls it wholly inside the new one.

The same grabs are available without the client's cooperation, which is the only way to move a window whose decorations
offer no handle: Alt+left-drag moves, and Alt+right-drag resizes from whichever corner of the window the pointer is
nearest.

#### Rendering

Rendering is triggered at 60 fps and looks at the current state of the compositor, but the compositor subsystem does not
rasterise anything itself.  It builds a *scene* per output: a flat, back-to-front list of textured quads, each one a
source rectangle in some texture paired with a destination rectangle in output pixels.  Surface position, subsurface
offsets, `wp_viewport` cropping and scaling, and buffer scale are all resolved here, into that pair of rectangles.

A frame is every output's scene from one tick, published together, because a frame is a moment in time rather than a
per-output event — and because only a whole frame can be meaningfully superseded by a newer one.

The frame goes to the backend, which draws it on the GPU.  What crosses is a notification, not the frame itself: the
backend is woken and reads the slot when it is ready to draw, so wake-ups that arrive while it is busy collapse into a
single draw of the newest frame rather than a backlog of stale ones.  This matters more than it looks, because a queued
frame pins the shm pixel copies it references; bounding the queue at one bounds that memory too.

The split is forced by GL: a context belongs to one thread, and that thread is the backend's.  It is also the useful
split, because everything that needs compositor state is on this side and everything that needs a GPU is on the other.

Client pixels are not copied.  A texture handed to the backend points straight into the client's shm mapping, and the
GPU uploads from there.  What the compositor tracks instead is *identity*: every `wl_buffer` carries a `content_serial`
from a counter that never repeats, bumped by any commit that attaches it and by anything that moves the pool mapping
underneath it.  Scene building compares serials and re-reads nothing when they match; because the serial never repeats, a
buffer id reused after destruction cannot be mistaken for the one it replaced.

`wl_shm` is not a legacy path that `linux-dmabuf` will retire.  It is a core global every compositor must offer, and
plenty of clients will never use anything else — software-rendered clients, machines with no GPU driver, and client-side
cursors among them.  dmabuf adds a texture source the backend imports rather than uploads; it does not remove this one.
Both paths are permanent, so both are worth making good.

##### Buffer lifetime

Not copying moves a cost into a correctness requirement.  A client may not draw into a buffer it has committed until the
compositor sends `wl_buffer.release`, and with a copy that could be sent as soon as the copy was taken.  Reading the
mapping directly means the release has to wait for the *backend*, which is on another thread and a frame or two behind.

Each buffer therefore has a guard: a reference-counted handle on the pool mapping that every texture borrowing that
buffer holds a clone of.  The compositor keeps one handle of its own for as long as the buffer exists, so a count of one
means nobody is reading.  A commit that replaces a buffer only marks it as wanted-back; the release goes out on a later
tick, once the count has fallen and the buffer is no longer attached to anything on screen.  That check runs on every
tick, not only the ones that draw, because the last reader is usually the previous frame — dropped when a newer frame
replaces it or when the backend finishes with it, neither of which has anything to do with whether compositor state has
changed.

Counting references rather than watching for a drop is deliberate.  It keeps the compositor's own handle in place for the
whole life of the buffer, so a client that re-attaches a buffer before hearing it was released still renders, and it
keeps the release decision on the compositor thread where the client's socket already lives.

Pool mappings are reference counted for the same reason.  A resize replaces the mapping rather than unmapping it, and a
destroyed pool forgets it; the `munmap` happens when the last texture borrowing it goes.

##### Truncated pools

A pool is a file the client owns, and nothing stops it shrinking that file after the compositor has mapped it.  Reading a
mapped page with no file behind it raises `SIGBUS` — and because buffers are read in place rather than copied, that read
happens inside the GL driver on the backend thread.  Unhandled, any client could take down the compositor and everyone
else's windows with it, by accident or on purpose.  Three defences, cheapest first:

- A pool larger than the file behind it is refused outright, and the client gets `wl_shm.error.invalid_fd`.  This catches
  the ordinary bug of declaring the wrong size, which would otherwise become a page that faults on first read.
- The pool file is sealed against shrinking.  Clients using libwayland's shm helpers hand over a `memfd` that accepts
  `F_SEAL_SHRINK`, and for those the fault becomes impossible for the life of the pool.  It is best effort: a file that
  cannot be sealed is still mapped.
- Whatever is left is caught by a `SIGBUS` handler that maps a page of zeroes over the hole, so the faulting read retries
  and succeeds.  The client sees black where its buffer used to be, which is the right outcome for one that broke its own
  promise.  The handler consults a fixed, lock-free table of live mappings, so it allocates nothing and takes no locks; a
  fault outside those ranges is re-raised with the default handler rather than masked.  Blanked pages are counted and
  logged by the compositor, since a signal handler cannot log.

The rare buffer GL cannot address in place — a row stride that is not a whole number of pixels — falls back to a copy.
Cursors are copies too, having no client buffer behind them.

##### Damage

Alongside the serial, each buffer carries the damage accumulated since it was last read — the regions the client says it
drew into.  This is a promise about what did *not* change, so it is tracked in three states rather than two: exact
rectangles, nothing-changed-yet, and *unknown*.  Anything the compositor cannot place accurately collapses to unknown,
which means the whole buffer, and unknown is sticky until the damage is read: once one change could not be described, no
later rectangle can narrow the window back down.

The two damage requests are in different coordinate spaces and cannot share a list.  `wl_surface.damage_buffer` is
already in buffer pixels; `wl_surface.damage` is surface-local, and only means something once run backwards through the
same viewport-and-scale mapping the scene draws forwards with.  Both are resolved at commit, after the commit's own
viewport state has been applied, and rounded outward with a pixel of padding — uploading slightly more than changed is
always safe, uploading less is not.

Damage travels to the backend as a hint attached to the pixel copy, along with the serial the copy was derived from.  The
copy itself is always complete, so a backend that cannot use the hint can always fall back to the whole thing.  It can
only patch if it holds a texture at exactly that previous serial, at the same dimensions; a texture it never had, or one
several serials behind, gets a full upload.  That is what keeps partial uploads correct even though the backend evicts
textures without telling anyone.  For a client redrawing a small part of a large window — a terminal appending a line —
this is the difference between megabytes and kilobytes of upload per frame.

Damage and zero-copy compound: a client redrawing one line of a terminal causes no read on the compositor side at all,
and an upload of just the changed rows on the backend side.

Frame callbacks follow presentation, not hand-off.  A backend reports `FramePresented` once a frame has reached the
screen, and that is what fires `wl_surface.frame` and `wp_presentation_feedback.presented`; the timestamp a client is
told is the one the backend read at that moment, not one measured a channel hop later.

Callbacks live in surface state until they fire, which is what makes this safe against the single-slot frame channel: a
frame that is superseded before the backend draws it costs a client some latency, never a lost callback.  Every backend
reports presentation, including the headless one — a backend that went quiet here would strand every client waiting for
a callback that could not arrive.

