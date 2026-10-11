The musl build no longer warns that `libc::time_t` is deprecated: the restore code that copies timestamps now lets the compiler pick the width of `tv_sec`.
