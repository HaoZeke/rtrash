On Windows, rtrash now handles file names that hold an unpaired surrogate: arguments and names pass through WTF-8 and round-trip exactly, and output shows the bad unit as U+FFFD.
