In debug builds, the `RTRASH_TEST_FS` seam reads a Windows directory such as `D:\a\fs` whole, so `du --devices` and the space checks see the test file system there too instead of the real drives.
