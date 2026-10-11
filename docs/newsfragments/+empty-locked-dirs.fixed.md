`empty` no longer stops at a directory you own but cannot list or change, such as a makepkg `pkg` tree at mode 0311.
It gives the owner `rwx` back and removes it, and it never changes what another account owns.
An item that still cannot go stays in the trash with its `.trashinfo`, while the rest goes.
`empty` then lists each item left with the reason and exits 1.
`list`, `status`, and `rm` now show and match payloads that have no `.trashinfo`.
