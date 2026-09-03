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
    - Testing.  Today that means unit tests, run with `cargo test` and living beside the code they cover.  Integration
      and end-to-end testing against real clients is a goal and is not yet built: nothing in the repository drives the
      compositor through its socket, so conformance is currently established by running clients against it by hand.
    - Code quality, including readability, maintainability, and adherence to rust idioms and best practices.

### What is built, and what is not

This document describes a design, and parts of that design are ahead of the code.  Sections describing something not
yet written say so where they begin.  As of now:

- **Built**: the socket subsystem, the protocol layer and compositor state, scene building, the GL renderer, shm with
  zero-copy and damage tracking, dma-buf import, the clipboard and drag and drop, and the winit and null backends.
- **Not built**: the DRM backend, and with it every part of running on bare hardware — session management, `libinput`,
  and modesetting.  Subsurface `set_sync` is recorded but does not cache commits.  There is no XWayland, no layer
  shell, no decoration protocol, no primary selection, and no tablet input.  Touch is implemented but has only ever
  been exercised by tests — see `docs/notes.md`.

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

The table is about *inbound* descriptors only.  An event that carries one out — `wl_keyboard.keymap`, and
`wl_data_source.send` — builds its message with `message_with_fds` and owns the descriptor from that point, so
dropping the message closes it on every path, sent or not.

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

Messages run backend to compositor — including the one that drives rendering, since when a display can take a frame is
something only the backend knows.  A few questions run the other way, and `BackendRequest` is that direction: the
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

A `wl_buffer` is likewise a struct of what every buffer has — size, owner, and a serial identifying its contents — plus
a `BufferKind` for the part that differs.  Only an shm buffer's serial ever moves: a dma-buf is sampled where it lies,
so a client drawing into one changes the screen without anything crossing to the backend, and bumping the serial there
would have it throw away a good import once per committed frame.

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

##### The protocol on top

`zwp_linux_dmabuf_v1` is advertised at version 3.  On bind a client is told what can be imported: `modifier` events
carrying format and layout together, or plain `format` events for a client that bound version 1 or 2 and has no way to
be told about layouts.  Version 4 forbids both and replaces them with `zwp_linux_dmabuf_feedback_v1` — a format table
over a descriptor, a main device, per-surface tranches — which every client falls back from cleanly, so it can wait.

The global is not in the static `GLOBALS` table, because whether there is anything to advertise is not known at
startup: it depends on what the driver says, and the driver can only be asked once the backend has a GL context.  It is
allocated a name and broadcast when the probe comes back, and listed for clients that connect later — the same
two-path shape `wl_output` uses.  Nothing is advertised if nothing can be imported.

A client describes a buffer through `zwp_linux_buffer_params_v1`, one `add` per plane, and then asks for a `wl_buffer`.
Several planes does not mean YUV: a compressed RGB layout keeps its metadata in a second plane and is still sampled as
one ordinary texture, which is what a Vulkan swapchain on Intel hardware offers.  What would need a different sampler is
YUV, and those formats are excluded where the advertised list is built.

Refusals split in two, and the split is the whole design.  A malformed request — a plane index past the last, a plane
set twice, a gap in the planes, a buffer with no area, planes disagreeing about the layout — is a fatal protocol error,
because the client is broken.  "The driver will not take this" is the non-fatal `failed` event, because that is
something a client can recover from by falling back to shm.  Putting a case on the wrong side of that line either kills
clients that did nothing wrong or lets a bad request through.

The one bound the compositor can put on what a client makes the driver read is that a plane fits inside its descriptor,
which a dma-buf answers with `lseek(SEEK_END)`.  That check only means anything for a linear layout: under tiling or
compression a row is not `stride` bytes after the one above it, and the later planes are metadata with their own
geometry entirely.  So it is applied to the first plane of a linear or implicit buffer and skipped otherwise, where the
driver knows the real layout and does its own checking.

`create` and `create_immed` differ in who names the `wl_buffer` and therefore in what can still be refused.  `create`
lets the compositor answer with an id of its own — from the top of the id space, which is the compositor's half and
which `wl_display.delete_id` must never announce — so it can wait for the backend to try the import and answer
`created` or `failed` honestly.  `create_immed` has the client name the id up front, so there is nothing left to refuse:
a buffer that turns out not to import is registered anyway, as a buffer that draws nothing.  Tearing the object down
instead would leave the client owning an id the compositor has forgotten, and the next request naming it would
disconnect it.

That verdict arrives some frames after the request, by which time the client may have destroyed the params object,
destroyed the buffer, or disconnected, so each is checked rather than assumed — and a buffer id that has been destroyed
and reused since is caught by the content serial the pending import was stamped with.

#### Winit Backend

The winit backend provides a backend for running the compositor inside of a normal window hosted in an existing
compositor (or x session.)  This is useful for development and testing, as it allows us to run the compositor
without needing to set up a full wayland session.

It gets its GL context from glutin: an EGL display off the winit window, a GLES 3.0 context, and a window surface, all
created and made current on the winit thread.

It is also where the frame pacing described under [Rendering](#rendering) comes from in a hosted session.  A window on
someone else's compositor has no vblank of its own; what it has is `RedrawRequested`, which is the host saying that a
frame drawn now will be shown.  So that is what becomes a frame request, and a new one is asked for only after a frame
has been presented — which is what holds this backend to one frame in flight.  Vsync is off, because the pacing is
already in that loop and blocking the swap on vblank as well would only stall the thread handling input.

#### DRM Backend

*Not built.*  This section is a design, not a description of code that exists — `src/backend/` holds the winit and null
backends and nothing else.

The DRM backend is what would let the compositor run on a linux system with no compositor under it.  It would use the
Direct Rendering Manager subsystem of the kernel to drive the displays and `libinput` over evdev for input.  Its GL
context would come from EGL on a gbm device rather than from a host window — the same EGL the dma-buf import path
above already needs — and the renderer above it would be unchanged.

Frame pacing is the part that already has its shape decided.  A page flip completing on a CRTC is exactly the signal
`RedrawRequested` is in the winit backend: it says this output can take another frame.  It becomes a
`FrameRequested` for that output, and the answer comes back as a scene for that output alone.  This is why rendering
is paced per output rather than on a timer — see [Rendering](#rendering) — and it is the reason to have settled that
before writing this backend rather than after.  Two displays at different refresh rates have no shared rate to be
driven at, and a compositor that had assumed one would have to be unpicked everywhere it had assumed it.

Beyond the frame loop, this backend is where the rest of running on hardware lives, none of which the hosted backends
need: taking the session and the DRM master lease (`libseat`), enumerating connectors and choosing modes, handling
hotplug, and giving up and reclaiming the device across a VT switch.

#### Null Backend

The null backend provides a backend that does not display anything and does not capture any input events.  This is
useful for testing and benchmarking, as it allows us to run the compositor without needing to set up any graphical
output or input devices.  It has no GL context and drops the scenes it is sent; there is no software rasteriser to fall
back to, so a backend that cannot get a context has nothing to display and shuts down rather than run blind.

It reports no outputs, so it never asks for a frame and never presents one — a presentation names the output it
happened on, and this backend has none.  Its clients are still paced: with no output showing anything, every surface
falls to the offscreen path described under [Rendering](#rendering) and is served from the housekeeping timer.

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

##### When a request cannot be honoured

There are two ways a request can fail to make sense, and both end the connection:

- an **opcode the interface does not have**, which means the client and the compositor disagree about what object that
  id is, or about which version of the interface it is speaking;
- **arguments that do not decode**, which for a fixed-width wire format means the bytes were never written or the
  sender is out of step with the interface it thinks it is calling.

Neither is survivable by carrying on.  Every later request on that connection is decoded against the same disagreement,
so logging and continuing turns one recognisable fault into arbitrary behaviour some distance away, with the client
believing all the while that it was understood.

This follows from what `wl_display.error` already means rather than being a policy on top of it: the spec has a client
disconnect on receiving one, and libwayland destroys the connection as it posts it.  So the disconnect lives in
`ClientState::send_error` and there is no way to send an error without it.  A condition a client can recover from is a
different thing entirely and is not an error — those are interface-specific events, of which
`zwp_linux_buffer_params_v1.failed` is the one this compositor sends.

The compositor's half of the bargain is that a request within an interface's advertised version is always in the
handler's match, whether or not anything acts on it yet.  A request accepted and ignored gets an arm of its own, so
"not implemented" and "not a request" never look alike from the dispatcher.  Advertising a version whose requests are
not all in the match would disconnect clients that had done nothing wrong — `xdg_positioner.set_reactive` under the
`xdg_wm_base` version advertised here is exactly such a case.

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

#### Scrolling

Four events describe one scroll, and the order they go out in is the protocol's
rather than a convenience.  The source comes first, because it says what kind of
scroll this is: a wheel clicks through detents and stops between them, a
touchpad glides and is let go of, and a client cannot tell them apart from the
deltas alone — which is what decides whether the scroll may be given momentum.
Then the detent count for an axis, before the distance it explains.  Then the
distance.  Then the frame that says the picture is complete.

The detent count goes out as one of two events that are alternatives rather than
companions.  Version 8 replaced `axis_discrete` with `axis_value120`, and a
client that understands the newer one would count every detent twice if it were
sent both — so the client's version decides which it gets, and exactly one goes
out.  The `120` is the unit: a high-resolution wheel reports a fraction of a
click without needing a different event from an ordinary one, and an older
client is told in whole clicks, where a movement smaller than one rounds to
nothing because that is the best that unit can say.

`axis_stop` is what a touchpad has and a wheel does not.  A scroll that has
paused and one that has ended look identical in a stream of deltas, and only the
axes that actually moved are stopped — a stop for an axis that never scrolled
describes something that never happened.

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

#### Window states, dialogs and popups

A window has two states it can ask for and the compositor grants: maximized, which fills the output, and fullscreen,
which covers it.  They are independent, and the difference is worth keeping straight — a fullscreen window is raised
above everything on its workspace and a maximized one is an ordinary window that happens to fill the screen, so
un-fullscreening a window that was also maximized has to leave it filling the output rather than shrink it back to
the size it had before either.  The geometry to return to is captured once, on the way in from a normal window, and
spent on the way out of the last state; capturing it again on the second transition would record the maximized
geometry and lose the real one.

A configure carries the *complete* set of states, so it is built in one place from what the compositor knows rather
than assembled per call site.  The earlier shape, where activation and resizing each wrote their own short array,
could say only one thing at a time — and a configure that omits `maximized` is telling the client it is no longer
maximized.

A window is also told how much room it has, through `configure_bounds`, before the configure it is meant to size
itself against.  There are no panels or docks here so the bounds are the whole output, but the point of the event is
that the client should not have to *assume* that — a window opening larger than the display it is on is what it
prevents.  It is sent only when the answer changes, which for a window that stays on one display means once.

Not every request is granted, and `xdg_toplevel.wm_capabilities` is where that is said out loud.  A client told
nothing must assume every capability is available, so silence is a claim rather than a neutral absence: toolkits draw
the title-bar buttons and each one then does nothing.  The capabilities are a table pairing each with whether the
request behind it is implemented, which is what stops the advertisement drifting from the truth — minimize is absent
because a minimized window would have no way back with no taskbar to click, and the window menu because the
compositor has no text rendering to draw one with.

`set_parent` makes one window a dialog of another, and the only thing the compositor does with it is keep the child
above the parent.  That means raising either has to move both, since otherwise clicking the window would bury the
dialog belonging to it — so raising starts from the root of the family and walks down.  A link that would close a
loop is refused, because the walk that orders them would otherwise never end.

A popup is placed by an `xdg_positioner`, in two steps.  The anchor and gravity say where the client wants it: a
point on the anchor rectangle, and which corner of the popup hangs off that point.  The constraint adjustment then
says what to do when that lands off-screen, which is the ordinary case for a menu opened near an edge — flip to the
other side of the anchor, slide back into view, or shrink.  Each axis is settled separately, so a menu running off
the right edge slides sideways without also being dragged upwards, and a flip that would not help is discarded
rather than throwing the popup to the far side for nothing.  `reposition` runs the same placement again on a live
popup and answers with the client's own token, so a client that has asked more than once can tell which answer
belongs to which request.

#### The bell

`xdg_system_bell_v1.ring` asks the compositor to get the user's attention.  There is no audio anywhere in this
project, so the bell is visual: the output the surface is on is tinted for a moment.  That is the same answer a
terminal gives with the speaker muted, and it is deliberately translucent and brief — the alert is the change, and a
screen the user cannot read through is worse than one they can.

#### The clipboard, and dragging between windows

Four interfaces carry data between clients, and none of them carries any data.  A `wl_data_source` is a list of mime
types a client says it can produce; a `wl_data_offer` is the compositor's name for that list, handed to a client that
may ask for it; a `wl_data_device` is where a client is told which offer is the clipboard and what is being dragged
over it.  What actually moves the bytes is a pipe the two clients share, and the compositor's whole part in it is to
pass one end across.

That is worth being explicit about, because it is what keeps this feature out of the event loop entirely.  A client
pasting sends `wl_data_offer.receive` carrying the write end of a pipe it made; the compositor forwards that descriptor
verbatim as the descriptor of `wl_data_source.send` to the offering client, and the two of them talk directly.  Nothing
is buffered, nothing is read, and a hundred megabytes of pasted text never touches compositor memory.  A descriptor
that would reach a client that has gone away is dropped instead, which closes it — an immediate end of file for the
reader rather than a hang.  That is also what a paste from a stale offer or for a mime type that was never offered
gets, and it is the right answer for all three: an empty paste is what actually happened, and none of them is a fault
worth ending a connection over.

It follows that a selection dies with the client that owns it.  Copy from a terminal, close the terminal, and there is
nothing to paste — because the only thing that could have produced those bytes has gone.  Keeping the content alive
would mean the compositor reading every offered mime type into memory the moment a selection was set, which is both
the asynchronous pipe machinery this design avoids and a policy decision about which clipboards are worth spending
memory on.  A clipboard manager is a client, and is where that belongs.

An offer is named by the compositor, from the top of the id space, because there is no round trip in which the client
could name it.  That has a consequence the id-space rules make sharp: `wl_display.delete_id` is never sent for a server
id, so nothing but the client's own `wl_data_offer.destroy` takes the id out of its object map.  So when the compositor
stops caring about an offer — the clipboard has been replaced, the drag has left the window — it forgets the offer's
*contents* and keeps its *identity*.  Tearing the object down instead would leave the client holding an id the
compositor had forgotten, and the destroy it is about to send would disconnect it.

The clipboard follows keyboard focus.  There is one selection at a time, and the client that has focus is the one told
about it: on a focus change it is sent a fresh offer, and on binding a data device while already focused it is sent one
then.  That second case is the one that matters at startup — a client usually gets its data device well after its first
window is focused, and a compositor that only offered the selection from the focus change would leave it with an empty
clipboard for the rest of its life.  A client losing focus is told nothing, because an offer it still holds stops
working the moment the selection is replaced, and it is not going to be asked for a paste in the meantime.  A client
that owns the selection is offered it back like anyone else: copying and pasting within one application goes through
exactly this path, and a shortcut that skipped the owner would break it.

A drag takes the pointer the way a move does, and for the same reason: the compositor owns it until the button comes
up, and the client the pointer was over is sent a leave so it is not left drawing a hover state it will hear no more
about.  It is a separate piece of state from an interactive grab rather than a third kind of one, because the two have
nothing in common past that sentence — a move writes a window's position and sends it a configure, while a drag writes
no geometry at all and delivers protocol to a third client.  They are mutually exclusive by a check where each begins,
which is also what stops a client starting a move mid-drag off the very button press that started the drag.

What the pointer crosses over is delivered as `wl_data_device.enter`, `motion` and `leave` to the surface underneath,
with a fresh offer per surface entered — and one per data device, since `enter` is a per-device event and an offer
belongs to the device it arrived on.  No `wl_pointer` event reaches anyone for the duration; the target learns where
the pointer is from `wl_data_device.motion`.  A drag with no source is a client dragging within itself and is delivered
only to that client, with a null offer: there is nothing to hand anyone else.

The two sides then negotiate.  The source names the actions it will allow, the target names the ones it will take and
which of them it would rather have, and the compositor settles it — the preference where both sides offer it, failing
that the lowest of copy, move and ask that they agree on, and `none` when they agree on nothing.  Both are told, and
only when the answer changes.  A client too old for `set_actions` is not the same as one that has it and never called
it: a version 1 or 2 source can only mean a copy, and a version 1 or 2 target has no way to refuse anything, so it is
taken to accept whatever is offered.  That is exactly how those clients behaved before actions existed.

On release, a target that has accepted a mime type *and* settled on an action gets `drop`; anything else cancels.  A
target that took the content but agreed to do nothing with it has not accepted the drop.  The drop is where the drag
ends and the offer does not: the pointer is free immediately, and the offer outlives it so the target can still read
from it and then say it is done with `finish`, which is what tells the source `dnd_finished`.  Keeping the drag alive
to mean "dropped but not finished" was the alternative, and it would put a second condition on every check of whether
the pointer is spoken for — where the one place that forgot it would swallow the pointer for good.  A version 1 or 2
target never sends `finish`, so its source never hears `dnd_finished`; that is what the protocol gives it.

The drag icon is the cursor-surface path with a different position: a surface role, permanent like the cursor's, and
one more quad pushed into the scene above every window and below the cursor.  It sits at the pointer plus whatever
offset the client attached it with, which is the only means a client has to position it — a toolkit centres its icon
under the cursor by attaching at a negative `dx` and `dy`.  That offset is why `wl_surface.attach`'s displacement and
`wl_surface.offset` are tracked at all; nothing else reads them yet, and applying them to ordinary surfaces is a
larger question about how a surface repositions itself on attach that is deliberately left alone.

Serials are how a client proves the user asked for something.  Starting a drag must quote a pointer button press that
is still held — the same rule an interactive move follows, and for the same reason, since a drag beginning after the
user let go has nothing to follow.  Setting the clipboard is looser, because it is a keypress far more often than a
click, and because a client working through a batch of events quotes the serial of the event it is handling rather
than the newest one that arrived.  So the compositor keeps a short history of the serials it has sent each client
rather than only the last, and honours a request quoting any of them.  A serial from neither is refused in silence: a
client that lost that race has done nothing that warrants `wl_display.error`, which would end its connection.

#### Touch

Touch is multi-point, and every event names which finger it is about.  A point belongs to the surface it *started*
on for its whole life, however far it then travels: dragging a finger off a window keeps reporting to the client
that owns it, which is what makes a swipe leaving the window still reach the thing being swiped.  A touch that lands
on nothing is not tracked at all, so its motion and lift are dropped rather than delivered to whatever is touched
next.

`cancel` is not the same as every finger lifting.  It says something else has taken the sequence over, and a client
receiving it must undo what the gesture was doing rather than complete it — so it goes to every client holding a
point, not only the one under the last finger, since a two-finger gesture spanning two windows leaves both waiting
to hear how it ended.

The seat capability appears on the first touch rather than at startup.  winit reports touch *events* and never touch
*devices*, so the first event is the only evidence a touchscreen exists — and a capability can therefore appear
after clients have already bound the seat, which is why they are all told again rather than only whoever binds next.

#### What a client says about its buffer

Two requests describe the buffer rather than the surface, and both reach further than they look.

`set_buffer_transform` says how the client has *already* rotated or flipped its contents, so drawing it correctly
means undoing that — the compositor applies the inverse as an affine map over the unit quad in the vertex shader.
The map is column-major, matching how GL reads a `mat2`, and writing it row-major transposes exactly the two quarter
turns while leaving the symmetric transforms looking correct, which is a bug that hides well and is what the test
covering all eight exists to catch.  A quarter turn also exchanges the surface's width and height, so this reaches
hit testing and layout and not only sampling.

`set_opaque_region` is a promise, not a description: the client says what is fully opaque and the compositor may
skip the alpha blend there.  It is acted on only when the region covers the whole surface, because a partial promise
would mean splitting the quad along the region's edges, and one quad per surface is what keeps this renderer simple.

#### Rendering

The compositor subsystem does not rasterise anything itself.  It builds a *scene* per output: a flat, back-to-front list
of textured quads, each one a source rectangle in some texture paired with a destination rectangle in output pixels.
Surface position, subsurface offsets, `wp_viewport` cropping and scaling, and buffer scale are all resolved here, into
that pair of rectangles.

##### What paces it

The backend does, per output.  A scene is composed for an output when two things are true at once: the backend has said
that output can show another frame — `BackendMessage::FrameRequested` — and what it was last given no longer reflects
compositor state.

Only a backend can know the first of those.  A display takes a frame when a page flip completes, and a hosted window
takes one when the host says so; neither is a rate the compositor could pick.  Two displays at different refresh rates
make that concrete, because there is no single rate that is right for both, and a compositor that had picked one would
be wrong on at least one display forever.  The earlier design here ran a 16ms timer and composed every output against
it, which is the assumption that does not survive contact with real hardware — hence settling it before the DRM backend
rather than after.

A request outlives the moment it is made.  An output that asks while nothing has changed is not turned away; it is
served as soon as something does, so the first thing that moves on an idle desktop is not made to wait a refresh period
for the display to ask again.

The two halves are checked together once per pass of the event loop rather than in whichever arm settled the second of
them, because either one — a page flip, a client commit — can be the one that completes the pair.

##### What crosses to the backend

A `Frame` is the newest scene for *every* output.  It is not a moment in time: outputs are paced apart, so the scenes in
one frame were composed at whatever moment each output last asked.  It carries them all because the slot holds a single
value and a new publication replaces it — a frame carrying only the output being served would drop the scene of an
output whose backend had not drawn it yet.

Each scene carries a serial that rises with every one composed, and that is how a backend tells the one scene that is
new to it from the ones being carried along.  A backend draws a scene whose serial it has not drawn, and skips the rest:
redrawing them would cost a swap for no change, and would have that output report a presentation it did not make —
which is what fires its clients' frame callbacks.

What crosses is a notification, not the frame itself: the backend is woken and reads the slot when it is ready to draw,
so wake-ups that arrive while it is busy collapse into a single draw of the newest frame rather than a backlog of stale
ones.  This matters more than it looks, because a queued frame pins the shm pixel copies it references; bounding the
queue at one bounds that memory too.

Rendering being paced by the backend leaves work that no display paces — noticing a buffer has stopped being read,
re-confining windows to their outputs — with nothing to run it.  That is what the compositor's own 16ms timer is now
for, and it is upkeep rather than frame pacing.

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

That report names an output, and settles only the callbacks of the surfaces that output was showing.  A client with a
window on each of two displays is paced by each of them separately, which is what per-output pacing is for.  Which
surfaces those are is `Surface::visible_on`, which is what the compositor knows rather than `entered_outputs`, which is
what the client has been *told* — a client is only told about an output it has bound, so pacing on the latter would
leave a client that never binds `wl_output` waiting on a callback that could not arrive.

Surfaces no output is showing are settled from the housekeeping timer instead, since no output will ever report
presenting them.  This is not only windows the user has hidden: a client's first commit typically carries no buffer and
a frame request, and it draws nothing until that callback comes back.  Their presentation feedback is `discarded`
rather than `presented`, which is what it is — nothing reached a screen, and a `presented` naming a time would be a
plain untruth about the one thing this protocol exists to be accurate about.

Callbacks live in surface state until they fire, which is what makes this safe against the single-slot frame channel: a
frame that is superseded before the backend draws it costs a client some latency, never a lost callback.  Every backend
reports presentation, including the headless one — a backend that went quiet here would strand every client waiting for
a callback that could not arrive.

