# scr-axum-cranker-router

*A Rust port of [mu-cranker-router](https://github.com/hsbc/mu-cranker-router)*

"scr" means from scratch, there was a "cranker-ruster" previously when Rust was too hard for me. Rust is still too hard,
but it's not so painful now, that's why this one is written from scratch again.

## TODO
* [x] Replace `VecDeque` with a timeout-able mpsc channel for polling router socket
* [ ] Define errors with `thiserror` crate
* ~~[ ] Wrap the whole thing into a tower service~~ Looks like tower http not designed for WebSocket so no way
* [ ] V3 support
* [x] Handle invalid router socket
* [x] ~~Invoke listeners at hooking point~~ Invoke `on_failure_to_acquire_proxy_socket` in `ProxyListener`
* [x] ~~Add health map for observability.~~ ~~To achieve this we need to change the current `Arc<dyn RouteSocket>` in
  `route_to_socket_chan` to `Weak<_>`, and store the only Arc in a global map in the AppState. Reference `WebSocketFarm`
  in mu cranker router.~~ WebSocketFarm done. The /health/connectors is available by default.
* [x] Deregister handler
* ~~[ ] Replace all unnecessary async mpmc unbounded channel with tokio's mpsc unbounded channel.~~ Abandoned.
* [x] Dark mode manager
* [ ] Tidy up access modifiers, expose as little implementation details as possible.
* [ ] Consider `tracing` crate for logging
* [x] Get rid of the `rfc7239` crate which has bug in parsing host with port
* [ ] Documentation
* [ ] Graceful shutdown