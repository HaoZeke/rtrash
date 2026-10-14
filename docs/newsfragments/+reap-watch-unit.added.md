`rtrash reap --install-timer --watch=EVERY --when-below=SPEC` installs a user service that keeps `reap --watch` running (systemd on Linux, launchd KeepAlive on macOS) instead of a timer.
