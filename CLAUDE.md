This directory contains the code for GoopieLauncher, a Rust + Tauri v2 launcher for PC games built with the ReXGlue SDK.
The launcher normally points to goopie.xyz, but it also ships a statically-built version of it as the GoopieWebsite submodule.

The launcher communicates with the website through `src-tauri\src\bridge\shim.js` and `src-tauri\src\bridge\mod.rs`.

There's a version-gating helper in `GoopieWebsite\src\app\utils\launcherVersion.ts`. Use it if you're adding new features to the launcher code, so that the website does not break for users who are running old launcher versions. You can create a new version by running `python set-version.py <major|minor|patch>`. It will tag the new version and push everything if you include `--push` so be careful.

Commit your changes granularly. Never include yourself as a co-author when you commit.
