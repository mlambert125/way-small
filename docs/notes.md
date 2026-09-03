# TODO Next...

- DRM backend.  The frame loop it needs is in place — outputs are paced individually by `FrameRequested`, so a page
  flip completing becomes a request for that output and nothing assumes a shared rate.  What is left is the hardware:
  `libseat` for the session and the DRM master lease, connector enumeration and modesetting, hotplug, `libinput` for
  input, and giving the device up and taking it back across a VT switch.
- Integration tests that drive the compositor through its socket.  Everything today is a unit test; conformance
  against real clients is established by hand (`wayland-info`, `foot`, `gtk4-demo`), which is not something CI can do.
- dma-buf: `zwp_linux_dmabuf_feedback_v1` (version 4+), which needs a format table over a descriptor and the DRM device
  as a `dev_t`; version 3 is advertised today and clients fall back to it cleanly
- dma-buf: an external-sampler program, for the YUV formats a video decoder produces — also needs
  `wp_color_representation_v1` to know which matrix and range to convert with
- dma-buf: `y_invert`, currently refused. It is a `TextureSource::Dmabuf { image, y_invert }` field and a swap of the
  two v coordinates in the `u_src` uniform
- Primary selection (`zwp_primary_selection_v1`), the middle-click clipboard.  It is a near-copy of `wl_data_device`
  with its own three interfaces and the same descriptor relay behind it
- `wlr-data-control`, which is what a clipboard manager binds.  Without it a selection dies with the client that set
  it, which is what the protocol says should happen and is not always what a user wants
- Drag and drop over touch (`wl_data_device.start_drag` quoting a `wl_touch.down` serial), once there is touch input
  at all
- The drag icon reads `wl_surface.attach`'s offset; ordinary surfaces still ignore it.  Applying it everywhere is the
  larger question of how a surface repositions itself on attach
- Requests still accepted and ignored, as of an audit against `wayland.xml` and wayland-protocols 1.49.  Every
  request an advertised version requires is in its dispatch table; these are the ones with no behaviour behind them:
    - `xdg_toplevel.set_minimized`.  There is no taskbar, dock or window list, so a minimized window would have no
      way back onto the screen.  `wm_capabilities` reports it as unsupported, so clients hide the button
    - `xdg_toplevel.show_window_menu`.  The compositor has no text rendering and no widgets, so there is no menu to
      show; `wm_capabilities` says so and clients draw their own
    - `xdg_positioner.set_parent_configure`.  Placement is not deferred until the parent acknowledges a configure,
      so the serial has nothing to be matched against
- Touch is implemented but has never run against hardware — there is no touchscreen on the development machine, so
  `wl_touch` is covered by unit tests alone.  winit reports no touch *devices*, only touch *events*, so the seat
  capability appears on the first touch rather than at startup
- `wl_surface.set_opaque_region` is honoured only when the region covers the whole surface.  A partial one is real
  information, but acting on it means splitting the quad along the region's edges, and one quad per surface is what
  keeps the renderer simple
- Workspace hotkeys: add, remove and switch the workspaces on an output (the hierarchy is there, the policy is not)
- Client provided mouse cursors
- Themed server mouse cursor support (hardcoded white arrow right now)
- Make CSD work
- Animation testing - simple window open animation to see how this should be approached
- Window box shadows
- Configurable xkb options 
- Configurable CSD disable/enable (implement xdg-decoration-v1 wayland protocol)
- Scaling
- Look at fractional scaling wayland protocol


