`rtrash rm` drops the stored trash total after it removes items, so the trash history and the size checks after it no longer read a total from before the rm when it ran within the same clock tick.
