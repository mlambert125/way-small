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
- Drag and drop over touch (`wl_data_device.start_drag` quoting a `wl_touch.down` serial); only a pointer serial is
  accepted today
- The drag icon reads `wl_surface.attach`'s offset; ordinary surfaces still ignore it.  Applying it everywhere is the
  larger question of how a surface repositions itself on attach
- Subsurface `set_sync`.  The flag is recorded on the surface but commits are never cached against it, so a
  synchronised subsurface applies immediately like a desynchronised one.
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
- Workspace hotkeys: add, remove and switch the workspaces on an output (the hierarchy is there, the policy is not).
  Bindings today are Alt+F4 and Alt+Tab, matched on physical evdev keys in `match_binding`.
- Make CSD work.  Alt+drag to move and resize is the stand-in for a client that draws no usable decorations.
- Animation testing - simple window open animation to see how this should be approached
- Window box shadows
- Configurable xkb options.  `config.toml` carries only `backend` and `socket_path`; the keymap is built from the
  default names.  Once layouts are configurable, `match_binding` should move to the `mods_depressed` mask so a
  remapped Alt still works.
- Configurable CSD disable/enable (implement xdg-decoration-v1 wayland protocol)
- Scaling.  `wl_surface.set_buffer_scale` is honoured for surface size and hit testing and `wl_output.scale` is
  advertised, but every output is scale 1 and nothing renders at a scale.
- Look at fractional scaling wayland protocol
- Tablet input.  The seat carries `wl_pointer`, `wl_keyboard` and `wl_touch`; there is no tablet.
- Layer shell, for panels and lock screens.  Not started.
- X11 apps, tentatively by way of `xwayland-satellite` rather than an X window manager built into the compositor.
  Satellite runs Xwayland and the `xwm` in a process of its own and hands each X window to the compositor as an
  ordinary `xdg_toplevel` or `xdg_popup`, so nothing here grows an X11 protocol implementation, and the window model
  does not have to span two kinds of toplevel — hit testing, focus, stacking and workspaces keep working untouched.
  What it requires of the host is already advertised: the core interfaces, the whole of `xdg_shell` (`xdg_wm_base`,
  `xdg_surface`, `xdg_popup`, `xdg_toplevel`), and `wp_viewporter`.  dma-buf, xdg-activation, xdg-decoration, primary
  selection, pointer constraints, tablet input and fractional scale are all optional and each only makes it better.
  Needs Xwayland 23.1 or newer.  So the work is to try it, and to fix whatever it finds — popup placement is the
  likely sore spot, since X override-redirect windows become `xdg_popup`s and have to live with a positioner they
  never agreed to.  Clipboard between X and Wayland clients goes through `wl_data_device`, which works; what
  satellite cannot carry across is the primary selection, until that is implemented.
- The satellite decision is tentative.  What it costs is that the compositor cannot tell an X window from a native
  one and so can apply no X-specific policy, and that an X client which positions itself at a screen coordinate
  cannot be told the truth about where it is.  Writing a real `xwm` stays open, and is a far better idea once the
  socket-level test harness above exists to check it with.
