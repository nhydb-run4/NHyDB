# KQ-10M reference results

Reference runs of the [KQ-10M benchmark](../README.md) on our own machines.
All runs: **NHyDB 0.28.2**, benchmark protocol **1.0.0**, 10,000,000 KQ
records, batches of 10,000 records with **one data commit** per import,
**3 runs** per workload (reading: 3 repetitions on one indexed database).
Every run passed all validation checks (**VALID**). Only NHyDB is measured.

| Platform | Import 10 M | Build 3 indexes | Import throughput | IDs `tq BETWEEN …` (12,002, cold) | NumPy range read (12,002, cold) | Report |
|---|---:|---:|---:|---:|---:|---|
| Apple M1 Max, macOS 27 | 0.543 s | 4.335 s | 18.42 M records/s | 0.530 ms | 2.47 ms | [md](0.28.2/mac-m1-max.md) · [json](0.28.2/mac-m1-max.json) |
| Intel i7-8850H, Windows 11 | 1.295 s | 7.077 s | 7.72 M records/s | 1.24 ms | 6.19 ms | [md](0.28.2/p52-windows11.md) · [json](0.28.2/p52-windows11.json) |
| Intel i7-8850H, Ubuntu 24.04 (Hyper-V VM) | 1.188 s | 6.539 s | 8.42 M records/s | 0.673 ms | 4.04 ms | [md](0.28.2/p52-ubuntu-vm.md) · [json](0.28.2/p52-ubuntu-vm.json) |

Values are means of the 3 runs; the reports show min–max and all other
measurements (write calls, commit, file sizes, memory, counting, reading
complete records, single-call queries).

## Machines

- **Apple M1 Max** – MacBook Pro 16-inch (2021), 10 cores (8 performance +
  2 efficiency), 64 GB memory, internal SSD, macOS 27.0.1.
- **Lenovo ThinkPad P52** – Intel Core i7-8850H (6 cores, 12 threads), 64 GB
  memory, Samsung SSD. Measured twice on the **same laptop**:
  - **Windows 11** directly on the hardware;
  - **Ubuntu 24.04** in a **Hyper-V virtual machine** on this Windows host
    (8 virtual CPUs, 24 GB memory, virtual disk on the NVMe SSD).

  The Windows and Ubuntu results are therefore two operating systems on one
  physical machine, not two different computers.

All runs used CPython 3.14.5 and NumPy 2.5.4, with NHyDB installed from PyPI.

## How to read these numbers

These are single measurements on specific hardware, operating systems and
storage devices. They show what NHyDB achieves in these environments and are
not a general ranking. Results on other machines can be higher or lower; run
the [benchmark](../README.md) on your own machine to compare.
