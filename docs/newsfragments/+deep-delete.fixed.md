`rtrash rm` and `rtrash empty` no longer abort with a stack overflow on a tree some thousands of levels deep; they remove trees of any depth.
The walk keeps its own stack and at most 16 open directories, reopening parents through `..` after checking they are still the same directory.
