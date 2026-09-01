# TODO Next...

- dma-buf: `zwp_linux_dmabuf_v1` and `zwp_linux_buffer_params_v1`, so a client can actually send one (the import path
  underneath is done and proven; what is missing is the protocol, and a `wl_buffer` that can be either kind)
- dma-buf: an external-sampler program, for the YUV formats a video decoder produces
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


