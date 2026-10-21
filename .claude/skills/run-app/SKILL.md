---
name: run-app
description: Build and launch Rustmail with a throwaway profile so real mail data is never touched.
---

Prefer the release binary (`target/release/rustmail`); dev builds feel slow.

```bash
export PATH="$HOME/.local/share/mise/shims:$HOME/.cargo/bin:$PATH"
cd /home/ben/Projects/rustmail
CARGO_BUILD_JOBS=8 cargo build --release   # 12-19 min: run in background
P=<scratchpad>/rustmail-run     # absolute path in the session scratchpad
XDG_DATA_HOME=$P/data XDG_CACHE_HOME=$P/cache XDG_CONFIG_HOME=$P/config target/release/rustmail
```

Never launch without the three XDG variables. Do not enter real credentials in the throwaway profile unless the user does so themselves. Known: scroll direction is inverted for natural-scrolling mice (see ROADMAP).
