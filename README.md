# scr-axum-cranker-router

"scr" means from scratch, there was a "cranker-ruster" previously when Rust was too hard for me. Rust is still too hard,
but it's not so painful now, that's why this one is written from scratch again.

## TODO
* [x] Replace `VecDeque` with a timeout-able mpsc channel for polling router socket
* [ ] Define errors with `thiserror` crate
* ~~[ ] Wrap the whole thing into a tower service~~ Looks like tower http not designed for WebSocket so no way
* [ ] V3 support
* [x] Handle invalid router socket.
* [ ] ~~Invoke listeners at hooking point~~ Invoke `on_failure_to_acquire_proxy_socket` in `ProxyListener`
* [ ] Add health map for observability. ~~To achieve this we need to change the current `Arc<dyn RouteSocket>` in 
`route_to_socket_chan` to `Weak<_>`, and store the only Arc in a global map in the AppState. Reference `WebSocketFarm`
in mu cranker router.~~ WebSocketFarm done
* [x] Deregister handler