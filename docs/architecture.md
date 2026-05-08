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
    pub fds: Vec<i32>,  // Always empty for inbound messages, see "File Descriptors" section below
}
```

Each of these messages represents a single wayland message that the compositor should process and possibly
respond to through the client's sender channel.


#### File Descriptors

Some wayland messages include one or more accompanying file descriptors that refer to shared memory buffers
that are shared between the compositor and the client application.  These file descriptors are not sent in the
normal socket stream, but are sent out-of-band as ancillary data on the unix socket.  The rust sendfd library
is used to provide unix sockets that include this ancillary data so that we can get the file descriptors with
the messages.

By specification, each file descriptor is associated with a particular message.  However, the wayland socket
subsystem can not associate the file descriptors with a particular message without parsing the message and
considering compositor state, which is the responsibility of the compositor subsystem.  

Instead, file descriptors are places on a queue in the client struct as they are received, and the compositor
subsystem is responsible for associating them with the correct message when processing messages from that client. 
Since messages are guaranteed to be processed in order, this works as long as the compositor subsystem is careful
to always pull the correct number of file descriptors from the queue for each message.

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

#### Winit Backend

The winit backend provides a backend for running the compositor inside of a normal window hosted in an existing
compositor (or x session.)  This is useful for development and testing, as it allows us to run the compositor
without needing to set up a full wayland session.

#### DRM Backend

The DRM backend provides a backend for running the compositor on a linux system without an existing compositor.  This
is the "real" backend that allows the compositor to be used as a standalone compositor for a linux system.  It uses
the Direct Rendering Manager (DRM) subsystem of the linux kernel to display the compositors output and capture input
events using the evdev and libinput libraries.

#### Null Backend

The null backend provides a backend that does not display anything and does not capture any input events.  This is
useful for testing and benchmarking, as it allows us to run the compositor without needing to set up any graphical
output or input devices.

## Compositor Architecture

The compositor keeps it's state in a single struct that is shared across the compositor subsystem.  This state includes
"global" state that is not specific to any particular client, and a HashMap of client IDs to client state for each
connected client.

### Global State

The global state primarily stores windows, surfaces, and other objects needed to track the items needed to track input
and compose and display output frames to the backend. This includes things like:

- A list of outputs (monitors) that the compositor is currently displaying on.
- A list of input devices that the compositor is currently capturing input from.
- A list of windows that the compositor is currently managing, including their position, size, and other metadata.
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

#### Rendering

Rendering is triggered at 60 fps and looks at the current state of the compositor and draws the appropriate output frames
and sends them to the backend for display.  This includes compositing the windows and surfaces together, applying any
effects or transformations, and sending the final frame to the backend for display.

