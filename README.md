This is a small tray app to alert if the PCIe link on your GPU is degraded by checking the width of the connection against your baseline.

Made this because my PCIe slot is broken, making the GPU slowly sag and the link silently degrade.

Nvidia GPUs only (rely on NVML), Windows only. Requires Rust. 


# Build + Run
```cargo build --release```

Run at ```target\release\pciewatch.exe```

Configure via tray icon menu
