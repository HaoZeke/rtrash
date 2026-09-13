`disk_usage` parallelises only when a directory has 32+ children, so small trees stay serial (db8407b).
