# Way Small

## Goals

This is a small wayland compositor written in rust "from scratch." 

The primary goals:

- To implement the wayland protocol, and a complete compositor without relying on a library like smithay or wlroots.
- To take advantage of modern rust features that were not exploited in earlier wayland libraries.
- To avoid unnecessary abstractions and layers of indirection, and to embrace the reality of both the wayland protocol
  and rust as they are, rather than trying to shoehorn them into a particular design pattern or architecture (e.g. OOP, ECS, etc).
- To demonstrate good stewardship of an open source software project, including in areas not related to code quality that
  I have found lacking in other projects:
    - Documentation, including design documentation and code comments.
    - Testing, including unit tests, integration tests, and end-to-end tests.
    - Code quality, including readability, maintainability, and adherence to rust idioms and best practices.

## Top Level Architecture

At the very top level, the idea is to have several subsystems, each running on a tokio task that communicate
with each other via channels. The subsystems are:

- Wayland Socket: responsible for accepting new clients and pulling raw wayland messages from the socket with
  associated file descriptors, and sending them to the compositor subsytem for processing.
- Compositor: responsible for processing wayland messages, managing the state of the compositor, sending messsages
  back to clients, and sending and receiving messages to and from the backend subsystem.
- Backend: responsible for displaying the compositors output and capturing input events and sending them to the
  compositor subsystem.

This separation allows for a clean separation of concerns, and allows each subsystem to be developed and tested
independently. This relies heavily on rust's async features and channels for communication between subsystems,
which allows for a clean and efficient design without the need for complex synchronization primitives or shared state.

Some shared state is still necessary for performance reasons, and these generally use `Arc<Mutex<T>>` to allow for
sending large and/or mutable state across subsystem boundaries without the need for complex synchronization primitives.
This is kept to a minimum.j:w


