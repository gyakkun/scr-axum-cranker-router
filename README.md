# scr-axum-cranker-router

"scr" means from scratch, there was a "cranker-ruster" previously when Rust was too hard for me. Rust is still too hard,
but it's not so painful now, that's why this one is written from scratch again.

## TODO
* [ ] Replace `VecDeque` with a timeout-able mpsc channel for polling router socket
* [ ] Define errors with `anyhow` crate
* [ ] Wrap the whole thing into a tower service
* [ ] V3 support
