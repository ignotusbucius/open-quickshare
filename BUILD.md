The project is divided into two parts:

- **core_lib:** A Rust library with all the logic for discovering, connecting to,
  and transferring files with Quick Share clients — over BLE, Wi-Fi LAN and Wi-Fi
  Direct.
- **app/main:** The Tauri desktop application built on top of core_lib.

How to build
--------------------------

### core_lib

Install the `protobuf-compiler` system package, then run `cargo build` or
`cargo build --release` from the `core_lib` folder.

### app/main

The app is a Tauri v2 application; pnpm is the recommended package manager.
(All commands are run inside the `app/main` folder.)

Install the dependencies:

```
pnpm install
```

- To run the debug version:

```
pnpm dev
```

- To build release packages (.deb, .rpm & .AppImage):

```
pnpm build
```

For more detail see the [Tauri documentation](https://v2.tauri.app/start).
