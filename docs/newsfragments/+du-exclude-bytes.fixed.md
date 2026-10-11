On Unix, `rtrash du --exclude` and `-X` patterns with bytes that are not UTF-8 now match names with the same bytes, since the match works on raw bytes.
An ignore file or pattern file holding such a byte used to fail to read and drop every rule in it; now each rule still applies.
